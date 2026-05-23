//! Records a WAV file using the default input device with echo cancellation requested.
//!
//! On Windows, sets `StreamConfig.windows_input_aec`. On macOS, passes
//! `MacosVoiceProcessingInputConfig::screenpipe_aec()`. Other platforms record normally.

use clap::Parser;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample};
use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Record a WAV file with platform AEC requested when available"
)]
struct Opt {
    /// The audio input device to use.
    #[arg(short, long, default_value_t = String::from("default"))]
    device: String,

    /// Output WAV path.
    #[arg(short, long, default_value = "aec-recorded.wav")]
    output: PathBuf,

    /// Recording duration in seconds.
    #[arg(short = 't', long, default_value_t = 10)]
    duration_secs: u64,
}

fn main() -> Result<(), anyhow::Error> {
    let opt = Opt::parse();

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    eprintln!("AEC is only requested on Windows WASAPI and macOS VoiceProcessingIO; recording normally here.");

    let host = cpal::default_host();
    let device = if opt.device == "default" {
        host.default_input_device()
    } else {
        host.input_devices()?.find(|device| {
            device
                .name()
                .map(|name| name == opt.device)
                .unwrap_or(false)
        })
    }
    .ok_or_else(|| anyhow::anyhow!("failed to find input device '{}'", opt.device))?;

    println!("Input device: {}", device.name()?);

    let config = device.default_input_config()?;
    println!("Default input config: {:?}", config);
    println!("AEC request: enabled in cpal::StreamConfig");

    let spec = wav_spec_from_config(&config);
    let writer = hound::WavWriter::create(&opt.output, spec)?;
    let writer = Arc::new(Mutex::new(Some(writer)));

    let writer_2 = writer.clone();
    let err_fn = move |err| eprintln!("an error occurred on stream: {err}");
    let stream_config = aec_stream_config(&config);

    let stream = match config.sample_format() {
        cpal::SampleFormat::I8 => {
            build_aec_recording_stream::<i8, i8>(&device, &stream_config, writer_2, err_fn)?
        }
        cpal::SampleFormat::I16 => {
            build_aec_recording_stream::<i16, i16>(&device, &stream_config, writer_2, err_fn)?
        }
        cpal::SampleFormat::I32 => {
            build_aec_recording_stream::<i32, i32>(&device, &stream_config, writer_2, err_fn)?
        }
        cpal::SampleFormat::F32 => {
            build_aec_recording_stream::<f32, f32>(&device, &stream_config, writer_2, err_fn)?
        }
        sample_format => {
            return Err(anyhow::anyhow!(
                "unsupported sample format '{sample_format}'"
            ));
        }
    };

    println!(
        "Recording {} second(s) to {}...",
        opt.duration_secs,
        opt.output.display()
    );

    stream.play()?;
    std::thread::sleep(Duration::from_secs(opt.duration_secs));
    drop(stream);

    writer.lock().unwrap().take().unwrap().finalize()?;
    println!("Recording complete: {}", opt.output.display());
    Ok(())
}

fn aec_stream_config(config: &cpal::SupportedStreamConfig) -> cpal::StreamConfig {
    #[cfg(target_os = "windows")]
    {
        let mut stream_config = config.config();
        stream_config.windows_input_aec = true;
        stream_config
    }
    #[cfg(not(target_os = "windows"))]
    {
        config.config()
    }
}

fn sample_format(format: cpal::SampleFormat) -> hound::SampleFormat {
    if format.is_float() {
        hound::SampleFormat::Float
    } else {
        hound::SampleFormat::Int
    }
}

fn wav_spec_from_config(config: &cpal::SupportedStreamConfig) -> hound::WavSpec {
    hound::WavSpec {
        channels: config.channels() as _,
        sample_rate: config.sample_rate().0 as _,
        bits_per_sample: (config.sample_format().sample_size() * 8) as _,
        sample_format: sample_format(config.sample_format()),
    }
}

type WavWriterHandle = Arc<Mutex<Option<hound::WavWriter<BufWriter<File>>>>>;

fn build_aec_recording_stream<T, U>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    writer: WavWriterHandle,
    err_fn: impl FnMut(cpal::StreamError) + Send + 'static,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: Sample + cpal::SizedSample,
    U: Sample + hound::Sample + FromSample<T>,
{
    #[cfg(target_os = "macos")]
    {
        device.build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| write_input_data::<T, U>(data, &writer),
            err_fn,
            None,
            Some(cpal::MacosVoiceProcessingInputConfig::screenpipe_aec()),
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        device.build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| write_input_data::<T, U>(data, &writer),
            err_fn,
            None,
        )
    }
}

fn write_input_data<T, U>(input: &[T], writer: &WavWriterHandle)
where
    T: Sample,
    U: Sample + hound::Sample + FromSample<T>,
{
    if let Ok(mut guard) = writer.try_lock() {
        if let Some(writer) = guard.as_mut() {
            for &sample in input {
                writer.write_sample(U::from_sample(sample)).ok();
            }
        }
    }
}

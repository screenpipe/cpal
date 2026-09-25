// screenpipe — AI that knows everything you've seen, said, or heard
// https://screenpipe.com
//! Bounded macOS microphone capture through the public dynamically dispatched API.
//! Usage: cargo run --release --example vpio_probe -- vpio 18 /tmp/vpio.wav
//! Modes: vpio, bypass (native processing bypassed), raw (AUHAL).
//! Records the real microphone. Keep generated WAV files private.

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use anyhow::{bail, ensure, Context};
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use ringbuf::{
        traits::{Consumer, Producer, Split},
        HeapRb,
    };
    use std::{
        io::Write,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };

    let args: Vec<_> = std::env::args().collect();
    ensure!(
        args.len() == 4,
        "usage: vpio_probe <vpio|bypass|raw> <1..60 seconds> <output.wav>"
    );
    let seconds: usize = args[2].parse()?;
    ensure!(
        (1..=60).contains(&seconds),
        "duration must be 1..60 seconds"
    );
    let mut voice = cpal::MacosVoiceProcessingInputConfig::screenpipe_aec();
    let option = match args[1].as_str() {
        "raw" => None,
        "vpio" => Some(voice),
        "bypass" => {
            voice.voice_processing_bypass = Some(true);
            Some(voice)
        }
        _ => bail!("unknown mode"),
    };
    let device = cpal::default_host()
        .default_input_device()
        .context("no default microphone")?;
    let config = cpal::StreamConfig {
        channels: 1,
        sample_rate: cpal::SampleRate(48000),
        buffer_size: cpal::BufferSize::Default,
    };
    // Preallocate the entire bounded recording. The callback has no file IO,
    // allocations, locks, logging, or pointers to memory owned outside the stream.
    let capacity = (seconds + 3) * 48000;
    let (mut producer, mut consumer) = HeapRb::<f32>::new(capacity).split();
    let dropped = Arc::new(AtomicUsize::new(0));
    let overflow = dropped.clone();
    let errors = Arc::new(AtomicUsize::new(0));
    let failed = errors.clone();
    let stream = device.build_input_stream(
        &config,
        move |samples: &[f32], _| {
            let pushed = producer.push_slice(samples);
            overflow.fetch_add(samples.len() - pushed, Ordering::Relaxed);
        },
        move |_| {
            failed.fetch_add(1, Ordering::Relaxed);
        },
        None,
        option,
    )?;
    stream.play()?;
    println!("READY mode={} rate=48000 channels=1", args[1]);
    std::io::stdout().flush()?;
    std::thread::sleep(Duration::from_secs(seconds as u64));
    stream.pause()?;
    drop(stream);
    let mut samples = vec![0f32; capacity];
    let count = consumer.pop_slice(&mut samples);
    samples.truncate(count);
    ensure!(
        errors.load(Ordering::Relaxed) == 0,
        "capture callback failed"
    );
    ensure!(
        dropped.load(Ordering::Relaxed) == 0,
        "capture buffer overflow"
    );
    ensure!(
        count > 0 && samples.iter().all(|s| s.is_finite()),
        "invalid capture"
    );
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 48000,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    // Refuse to overwrite another recording. No output file is opened until the
    // device has stopped, so filesystem latency cannot block its audio callback.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&args[3])?;
    let mut writer = hound::WavWriter::new(std::io::BufWriter::new(file), spec)?;
    for sample in samples {
        writer.write_sample(sample)?;
    }
    writer.finalize()?;
    println!("samples={count} callback_errors=0 dropped_samples=0");
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("vpio_probe requires macOS");
    std::process::exit(1);
}

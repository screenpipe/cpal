//! macOS ScreenCaptureKit capture of system audio via display-backed virtual input devices.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cidre::{
    arc::Retained,
    cm, define_obj_type, dispatch, ns, objc,
    sc::{self, StreamOutput, StreamOutputImpl},
};

use crate::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Data, DeviceDescription, DeviceDescriptionBuilder, DeviceDirection, DeviceId,
    DeviceType, Error, ErrorKind, FrameCount, InputCallbackInfo, InputStreamTimestamp,
    InterfaceType, OutputCallbackInfo, SampleFormat, StreamConfig, StreamInstant,
    SupportedBufferSize, SupportedStreamConfig, SupportedStreamConfigRange,
};

pub use enumerate::{
    default_input_device, default_output_device, Devices, SupportedInputConfigs,
    SupportedOutputConfigs,
};

mod enumerate;

crate::assert_stream_send!(Stream);
crate::assert_stream_sync!(Stream);

#[derive(Debug)]
pub struct Host;

impl Host {
    pub fn new() -> Result<Self, Error> {
        Ok(Host)
    }
}

impl HostTrait for Host {
    type Devices = Devices;
    type Device = Device;

    fn is_available() -> bool {
        true
    }

    fn devices(&self) -> Result<Self::Devices, Error> {
        Devices::new()
    }

    fn default_input_device(&self) -> Option<Self::Device> {
        default_input_device()
    }

    fn default_output_device(&self) -> Option<Self::Device> {
        default_output_device()
    }
}

#[derive(Clone)]
pub struct Device {
    display: Retained<sc::Display>,
}

impl DeviceTrait for Device {
    type SupportedInputConfigs = SupportedInputConfigs;
    type SupportedOutputConfigs = SupportedOutputConfigs;
    type Stream = Stream;

    fn description(&self) -> Result<DeviceDescription, Error> {
        Ok(DeviceDescriptionBuilder::new(self.display_name())
            .direction(DeviceDirection::Input)
            .device_type(DeviceType::Virtual)
            .interface_type(InterfaceType::Virtual)
            .add_extended_line("ScreenCaptureKit display audio capture".to_string())
            .build())
    }

    fn id(&self) -> Result<DeviceId, Error> {
        Ok(DeviceId(
            crate::platform::HostId::ScreenCaptureKit,
            format!("display_{}", self.display.display_id().0),
        ))
    }

    fn supported_input_configs(&self) -> Result<Self::SupportedInputConfigs, Error> {
        Device::supported_input_configs(self)
    }

    fn supported_output_configs(&self) -> Result<Self::SupportedOutputConfigs, Error> {
        Device::supported_output_configs(self)
    }

    fn default_input_config(&self) -> Result<SupportedStreamConfig, Error> {
        Device::default_input_config(self)
    }

    fn default_output_config(&self) -> Result<SupportedStreamConfig, Error> {
        Device::default_output_config(self)
    }

    fn build_input_stream_raw<D, E>(
        &self,
        config: StreamConfig,
        sample_format: SampleFormat,
        data_callback: D,
        error_callback: E,
        _timeout: Option<Duration>,
    ) -> Result<Self::Stream, Error>
    where
        D: FnMut(&Data, &InputCallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        Device::build_input_stream(self, config, sample_format, data_callback, error_callback)
    }

    fn build_output_stream_raw<D, E>(
        &self,
        _config: StreamConfig,
        _sample_format: SampleFormat,
        _data_callback: D,
        _error_callback: E,
        _timeout: Option<Duration>,
    ) -> Result<Self::Stream, Error>
    where
        D: FnMut(&mut Data, &OutputCallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        Err(Error::new(ErrorKind::UnsupportedOperation))
    }
}

impl Device {
    pub(crate) fn new(display: Retained<sc::Display>) -> Self {
        Self { display }
    }

    fn display_name(&self) -> String {
        format!("Display {}", self.display.display_id().0)
    }

    fn supported_input_configs(&self) -> Result<SupportedInputConfigs, Error> {
        let channels = 2;
        let min_sample_rate = 48_000;
        let max_sample_rate = 48_000;
        let buffer_size = SupportedBufferSize::Unknown;
        let sample_format = SampleFormat::F32;
        let supported_configs = vec![SupportedStreamConfigRange {
            channels,
            min_sample_rate,
            max_sample_rate,
            buffer_size,
            sample_format,
        }];
        Ok(supported_configs.into_iter())
    }

    fn supported_output_configs(&self) -> Result<SupportedOutputConfigs, Error> {
        Ok(Vec::new().into_iter())
    }

    fn default_input_config(&self) -> Result<SupportedStreamConfig, Error> {
        let mut it = Self::supported_input_configs(self)?;
        let Some(range) = it.next() else {
            return Err(Error::new(ErrorKind::UnsupportedConfig));
        };
        Ok(range.with_max_sample_rate())
    }

    fn default_output_config(&self) -> Result<SupportedStreamConfig, Error> {
        Err(Error::new(ErrorKind::UnsupportedOperation))
    }

    fn build_input_stream<D, E>(
        &self,
        config: StreamConfig,
        sample_format: SampleFormat,
        data_callback: D,
        error_callback: E,
    ) -> Result<Stream, Error>
    where
        D: FnMut(&Data, &InputCallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        if sample_format != SampleFormat::F32 {
            return Err(Error::with_message(
                ErrorKind::UnsupportedConfig,
                "ScreenCaptureKit capture supports F32 samples only",
            ));
        }

        let queue = dispatch::Queue::serial_with_ar_pool();
        let mut cfg = sc::StreamCfg::new();
        cfg.set_captures_audio(true);
        cfg.set_excludes_current_process_audio(false);
        let windows = ns::Array::new();
        let filter = sc::ContentFilter::with_display_excluding_windows(&self.display, &windows);
        let sc_stream = sc::Stream::new(&filter, &cfg);
        let callback_frames = Arc::new(AtomicU32::new(0));
        let inner = CapturerInner {
            current_data: vec![],
            config,
            sample_format,
            data_callback: Box::new(data_callback),
            error_callback: Box::new(error_callback),
            last_frames: callback_frames.clone(),
        };
        let capturer = Capturer::with(inner);
        sc_stream
            .add_stream_output(capturer.as_ref(), sc::OutputType::Audio, Some(&queue))
            .map_err(|e| Error::with_message(ErrorKind::Other, format!("{e}")))?;

        Ok(Stream::new(StreamInner {
            _capturer: capturer,
            sc_stream,
            playing: false,
            callback_frames,
        }))
    }
}

struct StreamInner {
    _capturer: Retained<Capturer>,
    sc_stream: Retained<sc::Stream>,
    playing: bool,
    callback_frames: Arc<AtomicU32>,
}

pub struct Stream {
    inner: Arc<Mutex<StreamInner>>,
}

impl Stream {
    fn new(inner: StreamInner) -> Self {
        Self {
            inner: Arc::new(Mutex::new(inner)),
        }
    }
}

impl StreamTrait for Stream {
    fn play(&self) -> Result<(), Error> {
        let mut stream = self.inner.lock().map_err(|_| {
            Error::with_message(ErrorKind::StreamInvalidated, "stream lock poisoned")
        })?;
        if !stream.playing {
            let (tx, rx) = std::sync::mpsc::channel();
            stream.sc_stream.start_with_ch(move |e| {
                let res = if let Some(e) = e {
                    Err(Error::with_message(ErrorKind::Other, format!("{e}")))
                } else {
                    Ok(())
                };
                let _ = tx.send(res);
            });
            rx.recv()
                .map_err(|_| {
                    Error::with_message(
                        ErrorKind::Other,
                        "ScreenCaptureKit stream start callback never fired",
                    )
                })??;
            stream.playing = true;
        }
        Ok(())
    }

    fn pause(&self) -> Result<(), Error> {
        let mut stream = self.inner.lock().map_err(|_| {
            Error::with_message(ErrorKind::StreamInvalidated, "stream lock poisoned")
        })?;
        if stream.playing {
            let (tx, rx) = std::sync::mpsc::channel();
            stream.sc_stream.stop_with_ch(move |e| {
                let res = if let Some(e) = e {
                    Err(Error::with_message(ErrorKind::Other, format!("{e}")))
                } else {
                    Ok(())
                };
                let _ = tx.send(res);
            });
            rx.recv()
                .map_err(|_| {
                    Error::with_message(
                        ErrorKind::Other,
                        "ScreenCaptureKit stream stop callback never fired",
                    )
                })??;
            stream.playing = false;
        }
        Ok(())
    }

    fn buffer_size(&self) -> Result<FrameCount, Error> {
        let stream = self.inner.lock().map_err(|_| {
            Error::with_message(ErrorKind::StreamInvalidated, "stream lock poisoned")
        })?;
        let n = stream.callback_frames.load(Ordering::Relaxed);
        if n > 0 {
            Ok(n as FrameCount)
        } else {
            Err(Error::new(ErrorKind::UnsupportedOperation))
        }
    }

    fn now(&self) -> StreamInstant {
        let t = unsafe { mach2::mach_time::mach_absolute_time() };
        crate::host::coreaudio::host_time_to_stream_instant(t).unwrap_or(StreamInstant::ZERO)
    }
}

#[repr(C)]
struct CapturerInner {
    current_data: Vec<f32>,
    config: StreamConfig,
    sample_format: SampleFormat,
    data_callback: Box<dyn FnMut(&Data, &InputCallbackInfo) + Send + 'static>,
    error_callback: Box<dyn FnMut(Error) + Send + 'static>,
    last_frames: Arc<AtomicU32>,
}

impl CapturerInner {
    fn handle_audio(&mut self, sample_buf: &mut cm::SampleBuf) {
        let start = std::time::Instant::now();
        let buf_list = match sample_buf.audio_buf_list::<2>() {
            Ok(res) => res,
            Err(e) => {
                (self.error_callback)(Error::with_message(ErrorKind::Other, format!("{e}")));
                return;
            }
        };
        let buf_list = buf_list.list();
        let buf_cnt = buf_list.number_buffers as usize;
        let buf_len =
            buf_list.buffers[0].data_bytes_size as usize / self.sample_format.sample_size();
        self.last_frames.store(buf_len as u32, Ordering::Relaxed);
        let required_len = buf_cnt * buf_len;

        if required_len > self.current_data.len() {
            self.current_data.resize(required_len, 0.0);
        }

        for (i, buf) in buf_list.buffers.iter().enumerate() {
            let buf_data = unsafe { std::slice::from_raw_parts(buf.data as *const f32, buf_len) };
            for (item, v) in self
                .current_data
                .iter_mut()
                .skip(i)
                .step_by(2)
                .zip(buf_data.iter())
            {
                *item = *v;
            }
        }

        let data = self.current_data.as_mut_ptr() as *mut ();
        let data = unsafe { Data::from_parts(data, required_len, self.sample_format) };

        let capture = cm_time_to_stream_instant(sample_buf.pts());
        let duration = local_frames_to_duration(buf_len, self.config.sample_rate);
        let elapsed = start.elapsed();
        let callback = capture
            .checked_add(duration)
            .and_then(|c| c.checked_add(elapsed))
            .unwrap_or(capture);
        let timestamp = InputStreamTimestamp { callback, capture };
        let info = InputCallbackInfo { timestamp };
        (self.data_callback)(&data, &info);
    }
}

define_obj_type!(Capturer + StreamOutputImpl, CapturerInner, CAPTURER);

impl StreamOutput for Capturer {}

#[objc::add_methods]
impl StreamOutputImpl for Capturer {
    extern "C" fn impl_stream_did_output_sample_buf(
        &mut self,
        _cmd: Option<&cidre::objc::Sel>,
        _stream: &sc::Stream,
        sample_buf: &mut cm::SampleBuf,
        kind: sc::OutputType,
    ) {
        match kind {
            sc::OutputType::Audio => self.inner_mut().handle_audio(sample_buf),
            _ => {}
        }
    }
}

fn cm_time_to_stream_instant(cm_time: cm::Time) -> StreamInstant {
    if cm_time.scale == 0 {
        return StreamInstant::ZERO;
    }
    let secs = cm_time.value / cm_time.scale as i64;
    let subsec_nanos =
        (cm_time.value % cm_time.scale as i64) * 1_000_000_000 / cm_time.scale as i64;
    StreamInstant::new(secs.max(0) as u64, subsec_nanos.max(0) as u32)
}

fn local_frames_to_duration(frames: usize, rate: crate::SampleRate) -> Duration {
    if rate == 0 {
        return Duration::ZERO;
    }
    let secsf = frames as f64 / rate as f64;
    let secs = secsf as u64;
    let nanos = ((secsf - secs as f64) * 1_000_000_000.0) as u32;
    Duration::new(secs, nanos)
}

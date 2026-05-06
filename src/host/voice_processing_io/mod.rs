//! macOS Voice Processing I/O host — hardware voice processing including AEC when playback samples
//! are pushed via [`crate::Stream::push_voice_processing_playback_f32`].

mod enumerate;

use std::collections::VecDeque;
use std::ffi::c_void;
use std::mem;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use objc2_audio_toolbox::AudioUnit as AuAudioUnit;
use objc2_core_audio_types::{AudioBuffer, AudioBufferList, AudioTimeStamp};

use coreaudio::audio_unit::voice_processing_io::{
    AudioUnitRender, AudioUnitRenderActionFlags, AURenderCallbackStruct, VoiceProcessingIo,
    VoiceProcessingIoBuilder,
};

pub use enumerate::Devices;

use crate::host::coreaudio::host_time_to_stream_instant;
use crate::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Data, DeviceDescription, DeviceDescriptionBuilder, DeviceDirection,
    DeviceId, DeviceType, Error, ErrorKind, FrameCount, InputCallbackInfo, InputStreamTimestamp,
    InterfaceType, OutputCallbackInfo, SampleFormat, StreamConfig, StreamInstant,
    SupportedBufferSize, SupportedStreamConfig, SupportedStreamConfigRange,
};

crate::assert_stream_send!(Stream);

/// Voice Processing I/O host (macOS).
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
        enumerate::devices()
    }

    fn default_input_device(&self) -> Option<Self::Device> {
        self.devices().ok()?.next()
    }

    fn default_output_device(&self) -> Option<Self::Device> {
        None
    }
}

#[derive(Clone)]
pub struct Device {
    inner: Arc<DeviceInner>,
}

struct DeviceInner {
    audio_device_id: u32,
    label: String,
    busy: Mutex<bool>,
}

impl Device {
    pub(crate) fn from_audio_device_info(id: u32, name: String) -> Self {
        Self {
            inner: Arc::new(DeviceInner {
                audio_device_id: id,
                label: name,
                busy: Mutex::new(false),
            }),
        }
    }
}

impl DeviceTrait for Device {
    type SupportedInputConfigs = std::vec::IntoIter<SupportedStreamConfigRange>;
    type SupportedOutputConfigs = std::vec::IntoIter<SupportedStreamConfigRange>;
    type Stream = Stream;

    fn name(&self) -> Result<String, Error> {
        self.description().map(|d| d.name().to_string())
    }

    fn description(&self) -> Result<DeviceDescription, Error> {
        Ok(
            DeviceDescriptionBuilder::new(self.inner.label.clone())
                .direction(DeviceDirection::Input)
                .device_type(DeviceType::Microphone)
                .interface_type(InterfaceType::Unknown)
                .add_extended_line("VoiceProcessingIO (AEC-capable microphone)".to_string())
                .build(),
        )
    }

    fn id(&self) -> Result<DeviceId, Error> {
        Ok(DeviceId(
            crate::platform::HostId::VoiceProcessingIo,
            format!("{}", self.inner.audio_device_id),
        ))
    }

    fn supported_input_configs(&self) -> Result<Self::SupportedInputConfigs, Error> {
        let native = self.native_rate_hz();
        let ranges = vec![SupportedStreamConfigRange {
            channels: 1,
            min_sample_rate: native,
            max_sample_rate: native,
            buffer_size: SupportedBufferSize::Unknown,
            sample_format: SampleFormat::F32,
        }];
        Ok(ranges.into_iter())
    }

    fn supported_output_configs(&self) -> Result<Self::SupportedOutputConfigs, Error> {
        Ok(Vec::new().into_iter())
    }

    fn default_input_config(&self) -> Result<SupportedStreamConfig, Error> {
        let native = self.native_rate_hz();
        Ok(SupportedStreamConfig::new(
            1,
            native,
            SupportedBufferSize::Unknown,
            SampleFormat::F32,
        ))
    }

    fn default_output_config(&self) -> Result<SupportedStreamConfig, Error> {
        Err(Error::new(ErrorKind::UnsupportedOperation))
    }

    fn build_input_stream_raw<D, E>(
        &self,
        config: StreamConfig,
        sample_format: SampleFormat,
        data_callback: D,
        error_callback: E,
        _timeout: Option<std::time::Duration>,
    ) -> Result<Self::Stream, Error>
    where
        D: FnMut(&Data, &InputCallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        Self::build_input_stream(self, config, sample_format, data_callback, error_callback)
    }

    fn build_output_stream_raw<D, E>(
        &self,
        _config: StreamConfig,
        _sample_format: SampleFormat,
        _data_callback: D,
        _error_callback: E,
        _timeout: Option<std::time::Duration>,
    ) -> Result<Self::Stream, Error>
    where
        D: FnMut(&mut Data, &OutputCallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        Err(Error::new(ErrorKind::UnsupportedOperation))
    }
}

impl Device {
    fn native_rate_hz(&self) -> u32 {
        let b = VoiceProcessingIoBuilder::default().input_device_id(Some(self.inner.audio_device_id));
        match b.build() {
            Ok(u) => u.native_sample_rate_hz(),
            Err(_) => 48_000,
        }
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
                "VoiceProcessingIO host supports F32 input only",
            ));
        }
        if config.channels != 1 {
            return Err(Error::with_message(
                ErrorKind::UnsupportedConfig,
                "VoiceProcessingIO host supports mono input only",
            ));
        }

        let mut busy = self.inner.busy.lock().map_err(|_| {
            Error::with_message(ErrorKind::StreamInvalidated, "device mutex poisoned")
        })?;
        if *busy {
            return Err(Error::new(ErrorKind::DeviceBusy));
        }

        let native = {
            let b = VoiceProcessingIoBuilder::default().input_device_id(Some(self.inner.audio_device_id));
            let u = b.build().map_err(|e| {
                Error::with_message(ErrorKind::Other, format!("VoiceProcessingIO setup: {e}"))
            })?;
            u.native_sample_rate_hz()
        };

        if config.sample_rate != native {
            return Err(Error::with_message(
                ErrorKind::UnsupportedConfig,
                format!(
                    "VoiceProcessingIO native sample rate is {native} Hz; requested {:?}",
                    config.sample_rate
                ),
            ));
        }

        let playback = Arc::new(Mutex::new(VecDeque::<f32>::with_capacity(48_000)));

        let b = VoiceProcessingIoBuilder::default().input_device_id(Some(self.inner.audio_device_id));
        let mut vpio_unit = b.build().map_err(|e| {
            Error::with_message(ErrorKind::Other, format!("VoiceProcessingIO build: {e}"))
        })?;

        let buffer_frames = vpio_unit.maximum_frames_per_slice().unwrap_or(512);

        let input_cb_holder: Arc<Mutex<InputCb>> = Arc::new(Mutex::new(InputCb {
            data_cb: Box::new(data_callback),
            err_cb: Box::new(error_callback),
        }));

        let input_state = Box::new(InputCallbackState {
            audio_unit: vpio_unit.instance(),
            cb: input_cb_holder.clone(),
        });
        let input_ref = (&*input_state as *const InputCallbackState).cast_mut() as *mut c_void;

        let render_state = Box::new(RenderCallbackState {
            playback: playback.clone(),
        });
        let render_ref = (&*render_state as *const RenderCallbackState).cast_mut() as *mut c_void;

        let input_cb = AURenderCallbackStruct {
            inputProc: Some(input_proc_trampoline),
            inputProcRefCon: input_ref,
        };
        unsafe {
            vpio_unit.set_input_callback(&input_cb).map_err(|e| {
                Error::with_message(ErrorKind::Other, format!("set input callback: {e}"))
            })?;
        }

        let render_cb = AURenderCallbackStruct {
            inputProc: Some(render_proc_trampoline),
            inputProcRefCon: render_ref,
        };
        unsafe {
            vpio_unit.set_render_callback(&render_cb).map_err(|e| {
                Error::with_message(ErrorKind::Other, format!("set render callback: {e}"))
            })?;
        }

        vpio_unit.initialize().map_err(|e| {
            Error::with_message(ErrorKind::Other, format!("VoiceProcessingIO initialize: {e}"))
        })?;
        vpio_unit.start().map_err(|e| {
            Error::with_message(ErrorKind::Other, format!("VoiceProcessingIO start: {e}"))
        })?;

        *busy = true;
        drop(busy);

        let runtime = VpioRuntime {
            _input_state: input_state,
            _render_state: render_state,
            vpio_unit,
        };

        Ok(Stream {
            inner: Arc::new(Mutex::new(StreamInner {
                playback,
                buffer_frames,
                playing: true,
                runtime: Some(runtime),
                device_busy: self.inner.clone(),
                _input_cb: input_cb_holder,
            })),
        })
    }
}

struct InputCb {
    data_cb: Box<dyn FnMut(&Data, &InputCallbackInfo) + Send + 'static>,
    err_cb: Box<dyn FnMut(Error) + Send + 'static>,
}

struct InputCallbackState {
    audio_unit: AuAudioUnit,
    cb: Arc<Mutex<InputCb>>,
}

unsafe impl Send for InputCallbackState {}

struct RenderCallbackState {
    playback: Arc<Mutex<VecDeque<f32>>>,
}

struct VpioRuntime {
    _input_state: Box<InputCallbackState>,
    _render_state: Box<RenderCallbackState>,
    vpio_unit: VoiceProcessingIo,
}

struct StreamInner {
    playback: Arc<Mutex<VecDeque<f32>>>,
    buffer_frames: u32,
    playing: bool,
    runtime: Option<VpioRuntime>,
    device_busy: Arc<DeviceInner>,
    _input_cb: Arc<Mutex<InputCb>>,
}

pub struct Stream {
    inner: Arc<Mutex<StreamInner>>,
}

unsafe impl Send for Stream {}

impl Stream {
    pub(crate) fn extend_playback(&self, samples: &[f32]) {
        if let Ok(g) = self.inner.lock() {
            if let Ok(mut pb) = g.playback.lock() {
                pb.extend(samples.iter().copied());
            }
        }
    }
}

impl Drop for StreamInner {
    fn drop(&mut self) {
        if let Ok(mut busy) = self.device_busy.busy.lock() {
            *busy = false;
        }
        self.runtime.take();
    }
}

unsafe extern "C-unwind" fn input_proc_trampoline(
    in_ref_con: NonNull<c_void>,
    io_action_flags: NonNull<AudioUnitRenderActionFlags>,
    in_time_stamp: NonNull<AudioTimeStamp>,
    in_bus_number: u32,
    in_number_frames: u32,
    _io_data: *mut AudioBufferList,
) -> i32 {
    match catch_unwind(AssertUnwindSafe(|| unsafe {
        input_proc_inner(
            in_ref_con,
            io_action_flags,
            in_time_stamp,
            in_bus_number,
            in_number_frames,
        )
    })) {
        Ok(v) => v,
        Err(_) => 0,
    }
}

unsafe fn input_proc_inner(
    in_ref_con: NonNull<c_void>,
    io_action_flags: NonNull<AudioUnitRenderActionFlags>,
    in_time_stamp: NonNull<AudioTimeStamp>,
    in_bus_number: u32,
    in_number_frames: u32,
) -> i32 {
    let state = &*(in_ref_con.as_ptr() as *const InputCallbackState);
    let sample_count = in_number_frames as usize;
    let mut samples = vec![0.0f32; sample_count];
    let audio_buffer = AudioBuffer {
        mNumberChannels: 1,
        mDataByteSize: (sample_count * mem::size_of::<f32>()) as u32,
        mData: samples.as_mut_ptr() as *mut c_void,
    };
    let mut audio_buffer_list = AudioBufferList {
        mNumberBuffers: 1,
        mBuffers: [audio_buffer],
    };

    let status = AudioUnitRender(
        state.audio_unit,
        io_action_flags.as_ptr(),
        in_time_stamp,
        in_bus_number,
        in_number_frames,
        NonNull::from(&mut audio_buffer_list),
    );
    if status != 0 {
        return status;
    }

    let t = unsafe { mach2::mach_time::mach_absolute_time() };
    let instant =
        host_time_to_stream_instant(t).unwrap_or(StreamInstant::ZERO);
    let info = InputCallbackInfo::new(InputStreamTimestamp {
        callback: instant,
        capture: instant,
    });

    let data = unsafe {
        Data::from_parts(
            samples.as_mut_ptr() as *mut (),
            samples.len(),
            SampleFormat::F32,
        )
    };

    if let Ok(mut cb) = state.cb.lock() {
        let r = catch_unwind(AssertUnwindSafe(|| {
            (cb.data_cb)(&data, &info);
        }));
        if r.is_err() {
            (cb.err_cb)(Error::with_message(
                ErrorKind::Other,
                "panic in VoiceProcessingIO input callback",
            ));
        }
    }
    0
}

unsafe extern "C-unwind" fn render_proc_trampoline(
    in_ref_con: NonNull<c_void>,
    _io_action_flags: NonNull<AudioUnitRenderActionFlags>,
    _in_time_stamp: NonNull<AudioTimeStamp>,
    _in_bus_number: u32,
    _in_number_frames: u32,
    io_data: *mut AudioBufferList,
) -> i32 {
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
        render_proc_inner(in_ref_con, io_data);
    }));
    0
}

unsafe fn render_proc_inner(in_ref_con: NonNull<c_void>, io_data: *mut AudioBufferList) {
    if io_data.is_null() {
        return;
    }

    let state = &*(in_ref_con.as_ptr() as *const RenderCallbackState);
    let output_buffer = &mut (*io_data).mBuffers[0];
    let sample_count = (output_buffer.mDataByteSize as usize) / mem::size_of::<f32>();
    let output_samples =
        std::slice::from_raw_parts_mut(output_buffer.mData as *mut f32, sample_count);

    if let Ok(mut buffer) = state.playback.try_lock() {
        for sample in output_samples.iter_mut() {
            *sample = buffer.pop_front().unwrap_or(0.0);
        }
    } else {
        output_samples.fill(0.0);
    }
}

impl StreamTrait for Stream {
    fn play(&self) -> Result<(), Error> {
        let mut g = self.inner.lock().map_err(|_| {
            Error::with_message(ErrorKind::StreamInvalidated, "stream mutex poisoned")
        })?;
        if !g.playing {
            if let Some(r) = g.runtime.as_mut() {
                r.vpio_unit.start().map_err(|e| {
                    Error::with_message(ErrorKind::Other, format!("VoiceProcessingIO start: {e}"))
                })?;
            }
            g.playing = true;
        }
        Ok(())
    }

    fn pause(&self) -> Result<(), Error> {
        let mut g = self.inner.lock().map_err(|_| {
            Error::with_message(ErrorKind::StreamInvalidated, "stream mutex poisoned")
        })?;
        if g.playing {
            if let Some(r) = g.runtime.as_mut() {
                r.vpio_unit.stop().map_err(|e| {
                    Error::with_message(ErrorKind::Other, format!("VoiceProcessingIO stop: {e}"))
                })?;
            }
            g.playing = false;
        }
        Ok(())
    }

    fn buffer_size(&self) -> Result<FrameCount, Error> {
        let g = self.inner.lock().map_err(|_| {
            Error::with_message(ErrorKind::StreamInvalidated, "stream mutex poisoned")
        })?;
        Ok(g.buffer_frames as FrameCount)
    }

    fn now(&self) -> StreamInstant {
        let t = unsafe { mach2::mach_time::mach_absolute_time() };
        host_time_to_stream_instant(t).unwrap_or(StreamInstant::ZERO)
    }
}

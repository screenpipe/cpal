// screenpipe — AI that knows everything you've seen, said, or heard
// https://screenpipe.com
//! Native VPIO lifetime and a preallocated input callback. coreaudio-rs 0.11's
//! input helper queries format and reallocates whenever frame counts change.
//! VPIO uses varying frame counts; that query can deadlock with AudioOutputUnitStop.
use coreaudio::audio_unit::{
    self,
    render_callback::{self, data},
    Element, Scope,
};
use coreaudio::{sys, Error};
use std::{ffi::c_void, ptr};

type Capture = dyn FnMut(render_callback::Args<data::Raw>) -> Result<(), ()> + Send;
struct Input {
    unit: sys::AudioUnit,
    callback: Box<Capture>,
    on_error: Box<dyn FnMut(sys::OSStatus) + Send>,
    // Eight-byte alignment supports every PCM sample type accepted by CPAL.
    storage: Vec<u64>,
    bytes_per_frame: usize,
    channels: u32,
    max_frames: u32,
}

impl Input {
    fn buffer_list(&mut self, frames: u32) -> Result<sys::AudioBufferList, sys::OSStatus> {
        if frames > self.max_frames {
            return Err(sys::kAudioUnitErr_TooManyFramesToProcess);
        }
        Ok(sys::AudioBufferList {
            mNumberBuffers: 1,
            mBuffers: [sys::AudioBuffer {
                mNumberChannels: self.channels,
                mDataByteSize: (frames as usize * self.bytes_per_frame) as u32,
                mData: self.storage.as_mut_ptr() as *mut c_void,
            }],
        })
    }
}

pub(super) struct VoiceProcessingUnit {
    unit: sys::AudioUnit,
    input: Option<Box<Input>>,
}
// The owning Stream mutex serializes control operations. The callback box stays
// at a stable address and is only accessed by CoreAudio until stop/dispose joins IO.
unsafe impl Send for VoiceProcessingUnit {}

impl VoiceProcessingUnit {
    pub fn new() -> Result<Self, Error> {
        let description = sys::AudioComponentDescription {
            componentType: sys::kAudioUnitType_Output,
            componentSubType: sys::kAudioUnitSubType_VoiceProcessingIO,
            componentManufacturer: sys::kAudioUnitManufacturer_Apple,
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        unsafe {
            let component = sys::AudioComponentFindNext(ptr::null_mut(), &description);
            if component.is_null() {
                return Err(Error::NoMatchingDefaultAudioUnitFound);
            }
            let mut unit = ptr::null_mut();
            Error::from_os_status(sys::AudioComponentInstanceNew(component, &mut unit))?;
            // Deliberately uninitialized: configure topology and callbacks first.
            Ok(Self { unit, input: None })
        }
    }
    pub fn set_property<T>(
        &mut self,
        id: u32,
        scope: Scope,
        element: Element,
        value: Option<&T>,
    ) -> Result<(), Error> {
        audio_unit::set_property(self.unit, id, scope, element, value)
    }
    pub fn get_property<T>(&self, id: u32, scope: Scope, element: Element) -> Result<T, Error> {
        audio_unit::get_property(self.unit, id, scope, element)
    }
    pub fn initialize(&mut self) -> Result<(), Error> {
        unsafe {
            Error::from_os_status(sys::AudioUnitInitialize(self.unit))?;
        }
        // Initialization can increase MaximumFramesPerSlice for rate conversion.
        // Resize before start, never from the real-time callback.
        let max_frames: u32 = self.get_property(
            sys::kAudioUnitProperty_MaximumFramesPerSlice,
            Scope::Global,
            Element::Output,
        )?;
        if let Some(input) = self.input.as_mut() {
            let bytes = (max_frames as usize)
                .checked_mul(input.bytes_per_frame)
                .filter(|bytes| *bytes > 0 && *bytes <= 16 * 1024 * 1024)
                .ok_or(Error::AudioUnit(
                    coreaudio::error::AudioUnitError::FormatNotSupported,
                ))?;
            input.storage.resize((bytes + 7) / 8, 0);
            input.max_frames = max_frames;
        }
        Ok(())
    }
    pub fn start(&mut self) -> Result<(), Error> {
        unsafe { Error::from_os_status(sys::AudioOutputUnitStart(self.unit)) }
    }
    pub fn stop(&mut self) -> Result<(), Error> {
        unsafe { Error::from_os_status(sys::AudioOutputUnitStop(self.unit)) }
    }
    pub fn set_silent_render_callback(&mut self) -> Result<(), Error> {
        self.set_property(
            sys::kAudioUnitProperty_SetRenderCallback,
            Scope::Input,
            Element::Output,
            Some(&sys::AURenderCallbackStruct {
                inputProc: Some(silent_render),
                inputProcRefCon: ptr::null_mut(),
            }),
        )
    }
    pub fn set_input_callback<F, E>(&mut self, callback: F, on_error: E) -> Result<(), Error>
    where
        F: FnMut(render_callback::Args<data::Raw>) -> Result<(), ()> + Send + 'static,
        E: FnMut(sys::OSStatus) + Send + 'static,
    {
        let format: sys::AudioStreamBasicDescription = self.get_property(
            sys::kAudioUnitProperty_StreamFormat,
            Scope::Output,
            Element::Input,
        )?;
        let max_frames: u32 = self.get_property(
            sys::kAudioUnitProperty_MaximumFramesPerSlice,
            Scope::Global,
            Element::Output,
        )?;
        let bytes = (max_frames as usize)
            .checked_mul(format.mBytesPerFrame as usize)
            .filter(|bytes| *bytes > 0 && *bytes <= 16 * 1024 * 1024)
            .ok_or(Error::AudioUnit(
                coreaudio::error::AudioUnitError::FormatNotSupported,
            ))?;
        let mut input = Box::new(Input {
            unit: self.unit,
            callback: Box::new(callback),
            on_error: Box::new(on_error),
            storage: vec![0u64; (bytes + 7) / 8],
            bytes_per_frame: format.mBytesPerFrame as usize,
            channels: format.mChannelsPerFrame,
            max_frames,
        });
        self.set_property(
            sys::kAudioOutputUnitProperty_SetInputCallback,
            Scope::Global,
            Element::Output,
            Some(&sys::AURenderCallbackStruct {
                inputProc: Some(capture),
                inputProcRefCon: &mut *input as *mut Input as *mut c_void,
            }),
        )?;
        self.input = Some(input);
        Ok(())
    }
}

impl Drop for VoiceProcessingUnit {
    fn drop(&mut self) {
        unsafe {
            // Keep input storage/callback alive until CoreAudio can no longer call it.
            let _ = sys::AudioOutputUnitStop(self.unit);
            let _ = sys::AudioUnitUninitialize(self.unit);
            let _ = sys::AudioComponentInstanceDispose(self.unit);
        }
    }
}

extern "C" fn silent_render(
    _: *mut c_void,
    flags: *mut sys::AudioUnitRenderActionFlags,
    _: *const sys::AudioTimeStamp,
    _: u32,
    _: u32,
    data: *mut sys::AudioBufferList,
) -> sys::OSStatus {
    unsafe {
        if !data.is_null() {
            let list = &mut *data;
            for buffer in std::slice::from_raw_parts_mut(
                list.mBuffers.as_mut_ptr(),
                list.mNumberBuffers as usize,
            ) {
                if !buffer.mData.is_null() {
                    ptr::write_bytes(buffer.mData as *mut u8, 0, buffer.mDataByteSize as usize);
                }
            }
        }
        *flags |= sys::kAudioUnitRenderAction_OutputIsSilence;
    }
    0
}

extern "C" fn capture(
    context: *mut c_void,
    flags: *mut sys::AudioUnitRenderActionFlags,
    timestamp: *const sys::AudioTimeStamp,
    bus: u32,
    frames: u32,
    _: *mut sys::AudioBufferList,
) -> sys::OSStatus {
    unsafe {
        let input = &mut *(context as *mut Input);
        let mut buffers = match input.buffer_list(frames) {
            Ok(buffers) => buffers,
            Err(status) => {
                (input.on_error)(status);
                return status;
            }
        };
        let status = sys::AudioUnitRender(input.unit, flags, timestamp, bus, frames, &mut buffers);
        if status != 0 {
            (input.on_error)(status);
            return status;
        }
        let args = render_callback::Args {
            data: data::Raw { data: &mut buffers },
            time_stamp: *timestamp,
            flags: render_callback::action_flags::Handle::from_ptr(flags),
            bus_number: bus,
            num_frames: frames as usize,
        };
        match (input.callback)(args) {
            Ok(()) => 0,
            Err(()) => -1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn variable_frames_reuse_aligned_storage_and_reject_overflow() {
        let mut input = Input {
            unit: ptr::null_mut(),
            callback: Box::new(|_| Ok(())),
            on_error: Box::new(|_| {}),
            storage: vec![0u64; 2400],
            bytes_per_frame: 8,
            channels: 2,
            max_frames: 2400,
        };
        let address = input.storage.as_mut_ptr() as *mut c_void;
        let capacity = input.storage.capacity();
        for frames in [0, 511, 512, 160, 1024, 2400, 512] {
            let buffers = input.buffer_list(frames).unwrap();
            assert_eq!(buffers.mBuffers[0].mData, address);
            assert_eq!(buffers.mBuffers[0].mDataByteSize, frames * 8);
            assert_eq!(buffers.mBuffers[0].mNumberChannels, 2);
            assert_eq!(input.storage.capacity(), capacity);
        }
        assert_eq!(
            input.buffer_list(2401).err(),
            Some(sys::kAudioUnitErr_TooManyFramesToProcess)
        );
    }
}

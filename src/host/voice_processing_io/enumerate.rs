//! Device enumeration for the Voice Processing I/O host (macOS).

use super::Device;
use crate::{Error, ErrorKind};

/// Iterator returned by [`super::Host::devices`](super::Host::devices).
pub struct Devices {
    inner: std::vec::IntoIter<Device>,
}

impl Iterator for Devices {
    type Item = Device;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

impl Devices {
    pub(crate) fn from_hosts(hosts: Vec<Device>) -> Self {
        Self {
            inner: hosts.into_iter(),
        }
    }
}

pub(crate) fn devices() -> Result<Devices, Error> {
    let list = coreaudio::audio_unit::voice_processing_io::list_audio_devices().map_err(|e| {
        Error::with_message(ErrorKind::Other, format!("CoreAudio device list failed: {e}"))
    })?;

    let mut out = Vec::new();
    for d in list {
        if d.is_input {
            out.push(Device::from_audio_device_info(d.id, d.name));
        }
    }

    Ok(Devices::from_hosts(out))
}

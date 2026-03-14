use std::vec::IntoIter as VecIntoIter;

use cidre::sc;

use crate::{BackendSpecificError, DevicesError, SupportedStreamConfigRange};

use super::Device;

// CoreGraphics FFI for display enumeration fallback
#[allow(non_upper_case_globals)]
const kCGErrorSuccess: i32 = 0;

extern "C" {
    fn CGGetOnlineDisplayList(
        max_displays: u32,
        online_displays: *mut u32,
        display_count: *mut u32,
    ) -> i32;
}

pub struct Devices(VecIntoIter<Device>);

impl Devices {
    pub fn new() -> Result<Self, DevicesError> {
        // First try the standard SCK enumeration
        let res = Self::enumerate_sck()?;
        if !res.is_empty() {
            return Ok(Devices(res.into_iter()));
        }

        // SCK returned 0 displays — check if CoreGraphics sees any.
        // After sleep/wake, SCK can return stale/empty results even though
        // the display is fully active. CG is lower-level and more reliable.
        let cg_count = Self::cg_online_display_count();
        if cg_count == 0 {
            // No displays at CG level either — genuinely no display
            return Ok(Devices(Vec::new().into_iter()));
        }

        // CG sees displays but SCK doesn't — SCK is stale after wake.
        // Retry with increasing delays to give SCK time to resync.
        for delay_ms in [200, 500, 1000, 2000, 3000] {
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            let res = Self::enumerate_sck()?;
            if !res.is_empty() {
                return Ok(Devices(res.into_iter()));
            }
        }

        // Still empty after ~7s of retries — return empty (caller retries later)
        Ok(Devices(Vec::new().into_iter()))
    }

    /// Enumerate displays via SCShareableContent
    fn enumerate_sck() -> Result<Vec<Device>, DevicesError> {
        let (tx, rx) = std::sync::mpsc::channel();
        sc::ShareableContent::current_with_ch(move |sc, e| {
            let res = if let Some(err) = e {
                Result::Err(BackendSpecificError {
                    description: format!("{err}"),
                })
            } else if let Some(sc) = sc {
                Result::Ok(sc.retained())
            } else {
                Result::Err(BackendSpecificError {
                    description: "Failed to get current shareable content".to_string(),
                })
            };
            let _ = tx.send(res);
        });
        let sc_shareable_content = rx.recv().map_err(|_| DevicesError::BackendSpecific {
            err: BackendSpecificError {
                description: "ScreenCaptureKit callback never fired (channel closed)".to_string(),
            },
        })??;

        let mut res = Vec::new();
        for display in sc_shareable_content.displays().iter() {
            res.push(Device::new(display.retained()));
        }
        Ok(res)
    }

    /// Ask CoreGraphics how many displays are online.
    /// More reliable than SCK after sleep/wake since CG talks directly
    /// to the WindowServer/IOKit display stack.
    fn cg_online_display_count() -> u32 {
        let mut count: u32 = 0;
        let err = unsafe { CGGetOnlineDisplayList(0, std::ptr::null_mut(), &mut count) };
        if err == kCGErrorSuccess {
            count
        } else {
            0
        }
    }
}

unsafe impl Send for Devices {}
unsafe impl Sync for Devices {}

impl Iterator for Devices {
    type Item = Device;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }
}

pub fn default_input_device() -> Option<Device> {
    let devices = Devices::new().ok()?;
    devices.into_iter().next()
}

pub fn default_output_device() -> Option<Device> {
    None
}

pub type SupportedInputConfigs = VecIntoIter<SupportedStreamConfigRange>;
pub type SupportedOutputConfigs = VecIntoIter<SupportedStreamConfigRange>;

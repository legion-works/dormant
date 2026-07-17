#![deny(missing_docs)]

use crate::error::Error;
use crate::iokit::IoObject;
use crate::{arm, intel};
use core_foundation::base::{CFType, TCFType};
use core_foundation::data::CFData;
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::display::CGDisplay;
use ddc::{
    DdcCommand, DdcCommandMarker, DdcCommandRaw, DdcCommandRawMarker, DdcHost, Delay, ErrorCode,
    I2C_ADDRESS_DDC_CI, SUB_ADDRESS_DDC_CI,
};
use std::time::Duration;
use std::{fmt, iter};

/// DDC access method for a monitor
#[derive(Debug)]
enum MonitorService {
    Intel(IoObject),
    Arm(arm::AvService),
}

/// A handle to an attached monitor that allows the use of DDC/CI operations.
#[derive(Debug)]
pub struct Monitor {
    monitor: CGDisplay,
    service: MonitorService,
    i2c_address: u16,
    delay: Delay,
}

impl fmt::Display for Monitor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.description())
    }
}

impl Monitor {
    /// Create a new monitor from the specified handle.
    fn new(monitor: CGDisplay, service: MonitorService, i2c_address: u16) -> Self {
        Monitor {
            monitor,
            service,
            i2c_address,
            delay: Default::default(),
        }
    }

    /// Enumerate all connected physical monitors returning [Vec<Monitor>]
    pub fn enumerate() -> Result<Vec<Self>, Error> {
        let monitors = CGDisplay::active_displays()
            .map_err(Error::from)?
            .into_iter()
            .filter_map(|display_id| {
                let display = CGDisplay::new(display_id);
                return if let Some(service) = intel::get_io_framebuffer_port(display) {
                    Some(Self::new(
                        display,
                        MonitorService::Intel(service),
                        I2C_ADDRESS_DDC_CI,
                    ))
                } else if let Ok((service, i2c_address)) = arm::get_display_av_service(display) {
                    Some(Self::new(
                        display,
                        MonitorService::Arm(service),
                        i2c_address,
                    ))
                } else {
                    None
                };
            })
            .collect();
        Ok(monitors)
    }

    /// Physical monitor description string. If it cannot get the product's name it will use
    /// the vendor number and model number to form a description
    pub fn description(&self) -> String {
        self.product_name().unwrap_or(format!(
            "{:04x}:{:04x}",
            self.monitor.vendor_number(),
            self.monitor.model_number()
        ))
    }

    /// Serial number for this [Monitor]
    pub fn serial_number(&self) -> Option<String> {
        let serial = self.monitor.serial_number();
        match serial {
            0 => None,
            _ => Some(format!("{}", serial)),
        }
    }

    /// Product name for this [Monitor], if available
    pub fn product_name(&self) -> Option<String> {
        let info: CFDictionary<CFString, CFType> = unsafe {
            CFDictionary::wrap_under_create_rule(
                arm::display_create_info_dictionary(self.monitor.id).ok()?,
            )
        };

        let display_product_name_key = CFString::from_static_string("DisplayProductName");
        let display_product_names_dict = info
            .find(&display_product_name_key)?
            .downcast::<CFDictionary>()?;
        let (_, localized_product_names) = display_product_names_dict.get_keys_and_values();
        localized_product_names
            .first()
            .map(|name| unsafe { CFString::wrap_under_get_rule(*name as CFStringRef) }.to_string())
    }

    /// Returns Extended display identification data (EDID) for this [Monitor] as raw bytes data
    pub fn edid(&self) -> Option<Vec<u8>> {
        let info: CFDictionary<CFString, CFType> = unsafe {
            CFDictionary::wrap_under_create_rule(
                arm::display_create_info_dictionary(self.monitor.id).ok()?,
            )
        };
        let display_product_name_key = CFString::from_static_string("IODisplayEDIDOriginal");
        let edid_data = info.find(&display_product_name_key)?.downcast::<CFData>()?;
        Some(edid_data.bytes().into())
    }

    /// CoreGraphics display handle for this monitor
    pub fn handle(&self) -> CGDisplay {
        self.monitor
    }

    fn encode_command<'a>(&self, data: &[u8], packet: &'a mut [u8]) -> &'a [u8] {
        Self::encode_command_with_address(self.i2c_address, data, packet)
    }

    /// Test-only accessor for the packet encoder. [`Monitor`] normally requires a live
    /// CoreGraphics display handle and DDC service, which unit tests cannot construct without
    /// real hardware; this lets fork tests exercise the byte-transparent encoding (including
    /// arbitrary VCP opcodes, e.g. 0xC0 usage hours) without one. Not part of the public API.
    #[cfg(test)]
    pub(crate) fn encode_command_for_test<'a>(
        i2c_address: u16,
        data: &[u8],
        packet: &'a mut [u8],
    ) -> &'a [u8] {
        Self::encode_command_with_address(i2c_address, data, packet)
    }

    fn encode_command_with_address<'a>(
        i2c_address: u16,
        data: &[u8],
        packet: &'a mut [u8],
    ) -> &'a [u8] {
        packet[0] = SUB_ADDRESS_DDC_CI;
        packet[1] = 0x80 | data.len() as u8;
        packet[2..2 + data.len()].copy_from_slice(data);
        packet[2 + data.len()] = Self::checksum(
            iter::once((i2c_address as u8) << 1).chain(packet[..2 + data.len()].iter().cloned()),
        );
        &packet[..3 + data.len()]
    }

    fn decode_response<'a>(
        &self,
        response: &'a mut [u8],
    ) -> Result<&'a mut [u8], crate::error::Error> {
        if response.is_empty() {
            return Ok(response);
        };
        let len = (response[1] & 0x7f) as usize;
        if len + 2 >= response.len() {
            return Err(Error::Ddc(ErrorCode::InvalidLength));
        }
        let checksum = Self::checksum(
            iter::once(((self.i2c_address << 1) | 1) as u8)
                .chain(iter::once(SUB_ADDRESS_DDC_CI))
                .chain(response[1..2 + len].iter().cloned()),
        );
        if response[2 + len] != checksum {
            return Err(Error::Ddc(ErrorCode::InvalidChecksum));
        }
        Ok(&mut response[2..2 + len])
    }
}

impl DdcHost for Monitor {
    type Error = Error;

    fn sleep(&mut self) {
        self.delay.sleep()
    }
}

impl DdcCommandRaw for Monitor {
    fn execute_raw<'a>(
        &mut self,
        data: &[u8],
        out: &'a mut [u8],
        response_delay: Duration,
    ) -> Result<&'a mut [u8], Self::Error> {
        assert!(data.len() <= 36);
        let mut packet = [0u8; 36 + 3];
        let packet = self.encode_command(data, &mut packet);
        let response = match &self.service {
            MonitorService::Intel(service) => {
                intel::execute(service, self.i2c_address, packet, out, response_delay)
            }
            MonitorService::Arm(service) => {
                arm::execute(service, self.i2c_address, packet, out, response_delay)
            }
        }?;
        self.decode_response(response)
    }
}

impl DdcCommandMarker for Monitor {}

impl DdcCommandRawMarker for Monitor {
    fn set_sleep_delay(&mut self, delay: Delay) {
        self.delay = delay;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_command_surface_accepts_usage_hours_opcode() {
        // Compile-time proof that `Monitor` exposes the generic, byte-transparent raw VCP
        // surface -- not a bespoke "read usage hours" API.
        fn assert_raw<T: DdcCommandRaw>() {}
        assert_raw::<Monitor>();

        // Usage hours (0xC0) is deliberately not one of the more commonly exercised
        // 0x10/0xD6 opcodes: the encoder must stay opaque to the opcode value.
        let mut packet = [0u8; 36 + 3];
        let encoded = Monitor::encode_command_for_test(0x37, &[0x01, 0xc0], &mut packet);

        // Hand-derived expected packet for i2c_address = 0x37, data = [0x01, 0xC0], per
        // `encode_command_with_address` (mirrors `ddc::DdcCommand::encode_command` /
        // `checksum` from the upstream `ddc` crate, ddc-0.2.2 src/lib.rs ~line 101-118):
        //   packet[0] = SUB_ADDRESS_DDC_CI                = 0x51
        //   packet[1] = 0x80 | data.len() as u8            = 0x80 | 0x02 = 0x82
        //   packet[2..4] = data                            = [0x01, 0xC0]
        //   packet[4] = checksum(iter::once((i2c_address as u8) << 1)
        //                   .chain(packet[..4].iter().cloned()))
        //             = checksum([0x37 << 1, 0x51, 0x82, 0x01, 0xC0])
        //             = checksum([0x6E, 0x51, 0x82, 0x01, 0xC0])
        // `checksum` (ddc::DdcCommand::checksum default impl) XOR-folds all bytes starting
        // from 0:
        //   0x00 ^ 0x6E = 0x6E
        //   0x6E ^ 0x51 = 0x3F
        //   0x3F ^ 0x82 = 0xBD
        //   0xBD ^ 0x01 = 0xBC
        //   0xBC ^ 0xC0 = 0x7C  <- final checksum byte
        let expected: [u8; 5] = [0x51, 0x82, 0x01, 0xC0, 0x7C];

        assert_eq!(
            encoded, expected,
            "encoded packet {encoded:?} does not match the hand-derived expected packet {expected:?}"
        );
    }
}

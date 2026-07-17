#![allow(non_upper_case_globals, unused)]

/// Selective translation of IOKit/graphics/IOGraphicsLib.h
use core_foundation::dictionary::CFDictionaryRef;
use core_graphics::display::CGDirectDisplayID;
use io_kit_sys::types::{io_service_t, IOOptionBits};
use mach2::port::mach_port_t;

pub const kIODisplayMatchingInfo: IOOptionBits = 0x00000100;
pub const kIODisplayOnlyPreferredName: IOOptionBits = 0x00000200;
pub const kIODisplayNoProductName: IOOptionBits = 0x00000400;

extern "C" {
    // For some reason, this is missing from io_kit_sys
    pub static kIOMainPortDefault: mach_port_t;
    #[link(name = "IOKit", kind = "framework")]
    pub fn IODisplayCreateInfoDictionary(
        framebuffer: io_service_t,
        options: IOOptionBits,
    ) -> CFDictionaryRef;

}

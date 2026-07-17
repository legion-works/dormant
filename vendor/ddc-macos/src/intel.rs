use crate::error::{verify_io, Error};
use crate::iokit::{
    kIODisplayOnlyPreferredName, kIOI2CNoTransactionType, IODisplayCreateInfoDictionary,
};
use crate::iokit::{
    kIOI2CDDCciReplyTransactionType, kIOI2CSimpleTransactionType, IOFBCopyI2CInterfaceForBus,
    IOFBGetI2CInterfaceCount, IOI2CRequest, IoI2CInterfaceConnection,
};
use crate::iokit::{IoIterator, IoObject};
use core_foundation::base::{CFType, TCFType};
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_foundation_sys::base::kCFAllocatorDefault;
use core_graphics::display::CGDisplay;
use ddc::SUB_ADDRESS_DDC_CI;
use io_kit_sys::ret::kIOReturnSuccess;
use io_kit_sys::types::{io_service_t, IOItemCount};
use io_kit_sys::IORegistryEntryCreateCFProperties;
use mach2::kern_return::KERN_FAILURE;
use std::time::Duration;

pub(crate) fn execute<'a>(
    service: &IoObject,
    i2c_address: u16,
    request_data: &[u8],
    out: &'a mut [u8],
    response_delay: Duration,
) -> Result<&'a mut [u8], crate::error::Error> {
    let reply_transaction_type = if out.is_empty() {
        kIOI2CNoTransactionType
    } else {
        unsafe { get_supported_transaction_type().unwrap_or(kIOI2CNoTransactionType) }
    };
    execute_with(
        i2c_address,
        request_data,
        out,
        response_delay,
        reply_transaction_type,
        |request| unsafe { send_request(service, request) },
    )
}

fn execute_with<'a, F>(
    i2c_address: u16,
    request_data: &[u8],
    out: &'a mut [u8],
    response_delay: Duration,
    reply_transaction_type: u32,
    send: F,
) -> Result<&'a mut [u8], Error>
where
    F: FnOnce(&mut IOI2CRequest) -> Result<(), Error>,
{
    let mut request: IOI2CRequest = unsafe { std::mem::zeroed() };

    request.commFlags = 0;
    request.sendAddress = (i2c_address << 1) as u32;
    request.sendTransactionType = kIOI2CSimpleTransactionType;
    request.sendBuffer = request_data.as_ptr() as usize;
    request.sendBytes = request_data.len() as u32;
    request.minReplyDelay = response_delay.as_nanos() as u64;
    request.result = -1;

    request.replyTransactionType = reply_transaction_type;
    request.replyAddress = ((i2c_address << 1) | 1) as u32;
    request.replySubAddress = SUB_ADDRESS_DDC_CI;

    request.replyBuffer = out.as_mut_ptr() as usize;
    request.replyBytes = out.len() as u32;

    send(&mut request)?;
    if request.replyTransactionType == kIOI2CNoTransactionType {
        return Ok(&mut []);
    }
    let reply_len = request.replyBytes as usize;
    if reply_len > out.len() {
        return Err(Error::ReplyLengthOutOfBounds {
            reported: reply_len,
            capacity: out.len(),
        });
    }
    Ok(&mut out[..reply_len])
}

fn display_info_dict(frame_buffer: &IoObject) -> Option<CFDictionary<CFString, CFType>> {
    unsafe {
        let info = IODisplayCreateInfoDictionary(frame_buffer.into(), kIODisplayOnlyPreferredName)
            .as_ref()?;
        Some(CFDictionary::<CFString, CFType>::wrap_under_create_rule(
            info,
        ))
    }
}

/// Get supported I2C / DDC transaction types
/// DDCciReply is what we want, but Simple will also work
unsafe fn get_supported_transaction_type() -> Option<u32> {
    let transaction_types_key = CFString::from_static_string("IOI2CTransactionTypes");

    for io_service in IoIterator::for_service_names("IOFramebufferI2CInterface")? {
        let mut service_properties = std::ptr::null_mut();
        if IORegistryEntryCreateCFProperties(
            (&io_service).into(),
            &mut service_properties,
            kCFAllocatorDefault as _,
            0,
        ) == kIOReturnSuccess
        {
            let info =
                CFDictionary::<CFString, CFType>::wrap_under_create_rule(service_properties as _);
            let transaction_types = info
                .find(&transaction_types_key)?
                .downcast::<CFNumber>()?
                .to_i64()?;
            if ((1 << kIOI2CDDCciReplyTransactionType) & transaction_types) != 0 {
                return Some(kIOI2CDDCciReplyTransactionType);
            } else if ((1 << kIOI2CSimpleTransactionType) & transaction_types) != 0 {
                return Some(kIOI2CSimpleTransactionType);
            }
        }
    }
    None
}

/// Finds if a framebuffer that matches display
fn framebuffer_port_matches_display(port: &IoObject, display: CGDisplay) -> Option<()> {
    let mut bus_count: IOItemCount = 0;
    unsafe {
        IOFBGetI2CInterfaceCount(port.into(), &mut bus_count);
    }
    if bus_count == 0 {
        return None;
    };

    let info = display_info_dict(port)?;

    let display_vendor_key = CFString::from_static_string("DisplayVendorID");
    let display_product_key = CFString::from_static_string("DisplayProductID");
    let display_serial_key = CFString::from_static_string("DisplaySerialNumber");

    let display_vendor = info
        .find(&display_vendor_key)?
        .downcast::<CFNumber>()?
        .to_i64()? as u32;
    let display_product = info
        .find(&display_product_key)?
        .downcast::<CFNumber>()?
        .to_i64()? as u32;
    // Display serial number is not always present. If it's not there, default to zero
    // (to match what CGDisplay.serial_number() returns
    let display_serial = info
        .find(&display_serial_key)
        .and_then(|x| x.downcast::<CFNumber>())
        .and_then(|x| x.to_i32())
        .map_or(0, |x| x as u32);

    if display_vendor == display.vendor_number()
        && display_product == display.model_number()
        && display_serial == display.serial_number()
    {
        Some(())
    } else {
        None
    }
}

/// Gets the framebuffer port for a display
pub(crate) fn get_io_framebuffer_port(display: CGDisplay) -> Option<IoObject> {
    if display.is_builtin() {
        return None;
    }
    IoIterator::for_services("IOFramebuffer")?
        .find(|framebuffer| framebuffer_port_matches_display(framebuffer, display).is_some())
}

/// send an I2C request to a display
unsafe fn send_request(
    service: &IoObject,
    request: &mut IOI2CRequest,
    // post_request_delay: u32,
) -> Result<(), Error> {
    let mut bus_count = 0;
    let mut result: Result<(), Error> = Err(Error::Io(KERN_FAILURE));
    verify_io(IOFBGetI2CInterfaceCount(service.into(), &mut bus_count))?;
    for bus in 0..bus_count {
        let mut interface: io_service_t = 0;
        if IOFBCopyI2CInterfaceForBus(service.into(), bus, &mut interface) == kIOReturnSuccess {
            let interface = IoObject::from(interface);
            result = IoI2CInterfaceConnection::new(&interface)
                .and_then(|connection| connection.send_request(request))
                .map_err(From::from);
            if result.is_ok() {
                break;
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_transaction_uses_packet_and_writable_output_buffers() {
        let packet = [0x6e, 0x82, 0x01, 0x10, 0x00];
        let mut out = [0_u8; 4];
        let packet_ptr = packet.as_ptr() as usize;
        let out_ptr = out.as_mut_ptr() as usize;

        let reply = execute_with(
            0x37,
            &packet,
            &mut out,
            Duration::ZERO,
            kIOI2CDDCciReplyTransactionType,
            |request| {
                let send_buffer = request.sendBuffer;
                let reply_buffer = request.replyBuffer;
                assert_eq!(send_buffer, packet_ptr);
                assert_eq!(reply_buffer, out_ptr);
                unsafe { std::slice::from_raw_parts_mut(request.replyBuffer as *mut u8, 2) }
                    .copy_from_slice(&[0x51, 0x82]);
                request.replyBytes = 2;
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(reply, &[0x51, 0x82]);
    }

    #[test]
    fn no_reply_transaction_returns_an_empty_slice() {
        let mut out = [];
        let reply = execute_with(
            0x37,
            &[0x6e],
            &mut out,
            Duration::ZERO,
            kIOI2CNoTransactionType,
            |_request| Ok(()),
        )
        .unwrap();

        assert!(reply.is_empty());
    }

    #[test]
    fn reply_length_larger_than_buffer_is_rejected() {
        let mut out = [0_u8; 2];
        let error = execute_with(
            0x37,
            &[0x6e],
            &mut out,
            Duration::ZERO,
            kIOI2CDDCciReplyTransactionType,
            |request| {
                request.replyBytes = 3;
                Ok(())
            },
        )
        .unwrap_err();

        assert!(matches!(
            error,
            Error::ReplyLengthOutOfBounds {
                reported: 3,
                capacity: 2
            }
        ));
    }
}

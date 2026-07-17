use crate::error::Error;
use crate::error::Error::{DisplayLocationNotFound, ServiceNotFound};
use crate::iokit::IoIterator;
use crate::iokit::{CoreDisplay_DisplayCreateInfoDictionary, IoObject};
use crate::{kern_try, verify_io};
use core_foundation::base::{CFType, TCFType};
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::CFString;
use core_foundation_sys::base::{
    kCFAllocatorDefault, CFAllocatorRef, CFRelease, CFTypeRef, OSStatus,
};
use core_graphics::display::CGDisplay;
use ddc::{I2C_ADDRESS_DDC_CI, SUB_ADDRESS_DDC_CI};
use io_kit_sys::keys::kIOServicePlane;
use io_kit_sys::types::{io_object_t, io_registry_entry_t};
use io_kit_sys::{
    kIORegistryIterateRecursively, IORegistryEntryCreateCFProperty, IORegistryEntryGetName,
    IORegistryEntryGetParentEntry, IORegistryEntryGetPath,
};
use mach2::kern_return::KERN_SUCCESS;
use std::ffi::{CStr, CString};
use std::os::raw::{c_uint, c_void};
use std::sync::OnceLock;
use std::time::Duration;

/// Opaque CoreDisplay `IOAVService` handle used by the private FFI symbols.
#[repr(transparent)]
#[derive(Debug, Clone, Copy)]
pub struct IOAVService(CFTypeRef);

impl IOAVService {
    fn is_null(self) -> bool {
        self.0.is_null()
    }
}

/// Retained `IOAVServiceCreateWithService` result.
#[derive(Debug)]
pub struct AvService(IOAVService);

impl AvService {
    fn from_create_rule(handle: IOAVService) -> Self {
        Self(handle)
    }

    fn handle(&self) -> IOAVService {
        self.0
    }
}

impl Drop for AvService {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0 .0) };
        }
    }
}

// SAFETY: CoreFoundation services are retained handles; the native calls use
// the handle value and do not require thread-affine Rust access.
unsafe impl Send for AvService {}
// SAFETY: shared access only passes the retained handle to CoreDisplay.
unsafe impl Sync for AvService {}

/// Delay the m1ddc-proven transaction shape inserts before each write attempt.
const ARM_WRITE_DELAY: Duration = Duration::from_millis(10);

/// Injectable I2C transport for the ARM (`IOAVService`) DDC/CI transaction driver.
///
/// Production code (`CoreDisplayIo`) resolves and calls through the runtime-loaded
/// [`CoreDisplaySymbols`]; unit tests substitute a fake that records the exact sleep/write/read
/// sequence so the transaction shape can be asserted without real Apple Silicon hardware.
trait ArmI2c {
    fn sleep(&mut self, duration: Duration);
    fn write(
        &mut self,
        service: &AvService,
        i2c_address: u16,
        data_address: u8,
        data: &[u8],
    ) -> Result<(), Error>;
    fn read(&mut self, service: &AvService, i2c_address: u16, out: &mut [u8]) -> Result<(), Error>;
}

/// Production [`ArmI2c`] backed by the real, runtime-resolved CoreDisplay symbols.
struct CoreDisplayIo;

impl ArmI2c for CoreDisplayIo {
    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }

    fn write(
        &mut self,
        service: &AvService,
        i2c_address: u16,
        data_address: u8,
        data: &[u8],
    ) -> Result<(), Error> {
        let symbols = core_display_symbols()?;
        unsafe {
            verify_io((symbols.write_i2c)(
                service.handle(),
                i2c_address as _,
                data_address as _,
                data.as_ptr() as _,
                data.len() as _,
            ))
        }
    }

    fn read(&mut self, service: &AvService, i2c_address: u16, out: &mut [u8]) -> Result<(), Error> {
        let symbols = core_display_symbols()?;
        call_read_i2c(symbols.read_i2c, service, i2c_address, out)
    }
}

fn call_read_i2c(
    read_i2c: ReadI2CFn,
    service: &AvService,
    i2c_address: u16,
    out: &mut [u8],
) -> Result<(), Error> {
    unsafe {
        verify_io(read_i2c(
            service.handle(),
            i2c_address as _,
            0,
            out.as_mut_ptr().cast(),
            out.len() as u32,
        ))
    }
}

pub(crate) fn execute<'a>(
    service: &AvService,
    i2c_address: u16,
    request_data: &[u8],
    out: &'a mut [u8],
    response_delay: Duration,
) -> Result<&'a mut [u8], crate::error::Error> {
    execute_with(
        &mut CoreDisplayIo,
        service,
        i2c_address,
        request_data,
        out,
        response_delay,
    )
}

/// Drives one raw VCP DDC/CI transaction using the m1ddc-proven ARM transport shape: the
/// packet is written twice, each write preceded by a 10ms delay, and -- if a response is
/// expected -- followed by the caller-supplied response delay and exactly one read.
///
/// There is no controller-level retry beyond the two writes: a nonzero `OSStatus` from either
/// write or the read is propagated immediately via `?` and never retried again here. Retry
/// policy beyond this transaction belongs to the executor above this crate.
fn execute_with<'a, IO: ArmI2c>(
    io: &mut IO,
    service: &AvService,
    i2c_address: u16,
    request_data: &[u8],
    out: &'a mut [u8],
    response_delay: Duration,
) -> Result<&'a mut [u8], Error> {
    for _ in 0..2 {
        io.sleep(ARM_WRITE_DELAY);
        // Skip the first byte, which is the I2C address, which this API does not need.
        io.write(service, i2c_address, SUB_ADDRESS_DDC_CI, &request_data[1..])?;
    }
    if !out.is_empty() {
        io.sleep(response_delay);
        io.read(service, i2c_address, out)?;
        Ok(out)
    } else {
        Ok(&mut [0u8; 0])
    }
}

/// Returns an AVService and its DDC I2C address for a given display
pub(crate) fn get_display_av_service(display: CGDisplay) -> Result<(AvService, u16), Error> {
    if display.is_builtin() {
        return Err(ServiceNotFound);
    }
    let symbols = core_display_symbols()?;
    let display_infos: CFDictionary<CFString, CFType> = unsafe {
        CFDictionary::wrap_under_create_rule(CoreDisplay_DisplayCreateInfoDictionary(display.id))
    };
    let location = display_infos
        .find(CFString::from_static_string("IODisplayLocation"))
        .ok_or(DisplayLocationNotFound)?
        .downcast::<CFString>()
        .ok_or(DisplayLocationNotFound)?
        .to_string();
    let external_location = CFString::from_static_string("External").into_CFType();

    let mut iter = IoIterator::root()?;
    while let Some(service) = iter.next() {
        if let Ok(registry_location) = get_service_registry_entry_path((&service).into()) {
            if registry_location == location {
                while let Some(service) = iter.next() {
                    if get_service_registry_entry_name((&service).into())? == "DCPAVServiceProxy" {
                        let av_service = unsafe {
                            (symbols.create_with_service)(kCFAllocatorDefault, (&service).into())
                        };
                        let loc_ref = unsafe {
                            IORegistryEntryCreateCFProperty(
                                (&service).into(),
                                CFString::from_static_string("Location").as_concrete_TypeRef(),
                                kCFAllocatorDefault,
                                kIORegistryIterateRecursively,
                            )
                        };
                        if !loc_ref.is_null() {
                            let loc_ref = unsafe { CFType::wrap_under_create_rule(loc_ref) };
                            if !av_service.is_null() && (loc_ref == external_location) {
                                return Ok((
                                    AvService::from_create_rule(av_service),
                                    i2c_address(service),
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
    Err(ServiceNotFound)
}

const I2C_ADDRESS_DDC_CI_MDCP29XX: u16 = 0xB7;

/// Returns the I2C chip address for a given service
fn i2c_address(service: IoObject) -> u16 {
    // M1 Macs use a non-standard chip address on their builtin HDMI ports: they are behind a
    // MDCP29xx DisplayPort to HDMI bridge chip, and it needs a different I2C slave address:
    // not a standard 0x37 but 0xB7.
    let mut parent: io_registry_entry_t = 0;
    unsafe {
        if IORegistryEntryGetParentEntry((&service).into(), kIOServicePlane, &mut parent)
            != KERN_SUCCESS
        {
            return I2C_ADDRESS_DDC_CI;
        }
    }
    let parent = IoObject::from(parent);
    let class_ref = unsafe {
        IORegistryEntryCreateCFProperty(
            (&parent).into(),
            CFString::from_static_string("EPICProviderClass").as_concrete_TypeRef(),
            kCFAllocatorDefault,
            kIORegistryIterateRecursively,
        )
    };
    if class_ref.is_null() {
        return I2C_ADDRESS_DDC_CI;
    }
    let mcdp29xx = CFString::from_static_string("AppleDCPMCDP29XX").into_CFType();
    let class_ref = unsafe { CFType::wrap_under_create_rule(class_ref) };
    if class_ref == mcdp29xx {
        I2C_ADDRESS_DDC_CI_MDCP29XX
    } else {
        I2C_ADDRESS_DDC_CI
    }
}

fn get_service_registry_entry_path(entry: io_registry_entry_t) -> Result<String, Error> {
    let mut path_buffer = [0_i8; 1024];
    unsafe {
        kern_try!(IORegistryEntryGetPath(
            entry,
            kIOServicePlane,
            path_buffer.as_mut_ptr()
        ));
        Ok(CStr::from_ptr(path_buffer.as_ptr())
            .to_string_lossy()
            .into_owned())
    }
}

fn get_service_registry_entry_name(entry: io_registry_entry_t) -> Result<String, Error> {
    let mut name = [0; 128];
    unsafe {
        kern_try!(IORegistryEntryGetName(entry, name.as_mut_ptr()));
        Ok(CStr::from_ptr(name.as_ptr()).to_string_lossy().into_owned())
    }
}

// --- Runtime-resolved CoreDisplay private symbols -------------------------------------------
//
// `IOAVServiceCreateWithService`, `IOAVServiceReadI2C` and `IOAVServiceWriteI2C` are
// undocumented private CoreDisplay symbols (see haimgel/ddc-macos-rs#8). The upstream crate
// hard-links them with `#[link(name = "CoreDisplay", kind = "framework")] extern "C"`, which
// means the *entire process* fails to load with a dyld error if Apple ever renames or removes
// one of them -- even for callers that never touch the ARM code path. Instead we `dlopen`
// CoreDisplay.framework directly and `dlsym` each entry point lazily, caching the resolved
// table (or the first failure) for the lifetime of the process. A missing symbol surfaces as a
// typed [`Error::MissingCoreDisplaySymbol`] the first time an ARM DDC transaction is attempted,
// never a process abort or link failure.

const SYMBOL_CREATE_WITH_SERVICE: &str = "IOAVServiceCreateWithService";
const SYMBOL_READ_I2C: &str = "IOAVServiceReadI2C";
const SYMBOL_WRITE_I2C: &str = "IOAVServiceWriteI2C";

const CORE_DISPLAY_FRAMEWORK_PATH: &str =
    "/System/Library/Frameworks/CoreDisplay.framework/CoreDisplay";

type CreateWithServiceFn = unsafe extern "C" fn(CFAllocatorRef, io_object_t) -> IOAVService;
type ReadI2CFn = unsafe extern "C" fn(IOAVService, c_uint, c_uint, *mut c_void, c_uint) -> OSStatus;
type WriteI2CFn =
    unsafe extern "C" fn(IOAVService, c_uint, c_uint, *const c_void, c_uint) -> OSStatus;

/// Runtime-resolved table of CoreDisplay's private `IOAVService*` symbols.
struct CoreDisplaySymbols {
    create_with_service: CreateWithServiceFn,
    read_i2c: ReadI2CFn,
    write_i2c: WriteI2CFn,
}

impl CoreDisplaySymbols {
    fn resolve<L: SymbolLoader>(loader: &L) -> Result<Self, Error> {
        Ok(CoreDisplaySymbols {
            create_with_service: Self::resolve_one(loader, SYMBOL_CREATE_WITH_SERVICE)?,
            read_i2c: Self::resolve_one(loader, SYMBOL_READ_I2C)?,
            write_i2c: Self::resolve_one(loader, SYMBOL_WRITE_I2C)?,
        })
    }

    fn resolve_one<L: SymbolLoader, F>(loader: &L, name: &str) -> Result<F, Error> {
        let ptr = loader
            .resolve(name)
            .ok_or_else(|| Error::MissingCoreDisplaySymbol(name.to_string()))?;
        // SAFETY: `F` is always one of the `unsafe extern "C" fn(...)` aliases declared above,
        // which -- like all function pointers -- share the size and representation of a plain
        // data pointer, so a pointer-for-pointer copy is a valid reinterpretation.
        Ok(unsafe { std::mem::transmute_copy::<*mut c_void, F>(&ptr) })
    }
}

/// A source of dynamically-resolved native symbols, abstracted so tests can simulate a
/// `CoreDisplay.framework` that is missing a given symbol without touching the real dynamic
/// loader.
trait SymbolLoader {
    /// Resolves `symbol`, returning `None` if it cannot be found.
    fn resolve(&self, symbol: &str) -> Option<*mut c_void>;
}

/// [`SymbolLoader`] backed by the real `dlopen`/`dlsym`.
struct DlopenSymbolLoader {
    handle: *mut c_void,
}

impl DlopenSymbolLoader {
    /// Opens the real CoreDisplay private framework.
    fn open() -> Result<Self, Error> {
        let path = CString::new(CORE_DISPLAY_FRAMEWORK_PATH).expect("static path has no NUL bytes");
        // SAFETY: `dlopen` is called with a valid, NUL-terminated path; a null return is
        // handled below rather than dereferenced.
        let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            return Err(Error::CoreDisplayFrameworkUnavailable);
        }
        Ok(DlopenSymbolLoader { handle })
    }
}

impl SymbolLoader for DlopenSymbolLoader {
    fn resolve(&self, symbol: &str) -> Option<*mut c_void> {
        let name = CString::new(symbol).ok()?;
        // SAFETY: `self.handle` is a valid handle from a successful `dlopen`, and `name` is a
        // valid NUL-terminated string that outlives this call.
        let ptr = unsafe { libc::dlsym(self.handle, name.as_ptr()) };
        if ptr.is_null() {
            None
        } else {
            Some(ptr)
        }
    }
}

static CORE_DISPLAY_SYMBOLS: OnceLock<Result<CoreDisplaySymbols, Error>> = OnceLock::new();

fn core_display_symbols() -> Result<&'static CoreDisplaySymbols, Error> {
    CORE_DISPLAY_SYMBOLS
        .get_or_init(|| {
            DlopenSymbolLoader::open().and_then(|loader| CoreDisplaySymbols::resolve(&loader))
        })
        .as_ref()
        .map_err(Clone::clone)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// One recorded call to [`ArmI2c::write`].
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct WriteCall {
        i2c_address: u16,
        data_address: u8,
        data: Vec<u8>,
    }

    /// Fake [`ArmI2c`] that records the exact sleep/write/read sequence issued by
    /// [`execute_with`], and returns caller-supplied canned results.
    #[derive(Default)]
    struct FakeArmI2c {
        sleeps: RefCell<Vec<Duration>>,
        writes: RefCell<Vec<WriteCall>>,
        read_calls: RefCell<u32>,
        read_response: Vec<u8>,
        write_result: Option<Error>,
        read_result: Option<Error>,
    }

    impl ArmI2c for FakeArmI2c {
        fn sleep(&mut self, duration: Duration) {
            self.sleeps.borrow_mut().push(duration);
        }

        fn write(
            &mut self,
            _service: &AvService,
            i2c_address: u16,
            data_address: u8,
            data: &[u8],
        ) -> Result<(), Error> {
            self.writes.borrow_mut().push(WriteCall {
                i2c_address,
                data_address,
                data: data.to_vec(),
            });
            match &self.write_result {
                Some(err) => Err(err.clone()),
                None => Ok(()),
            }
        }

        fn read(
            &mut self,
            _service: &AvService,
            _i2c_address: u16,
            out: &mut [u8],
        ) -> Result<(), Error> {
            *self.read_calls.borrow_mut() += 1;
            match &self.read_result {
                Some(err) => Err(err.clone()),
                None => {
                    out.copy_from_slice(&self.read_response);
                    Ok(())
                }
            }
        }
    }

    fn fake_service() -> AvService {
        AvService::from_create_rule(IOAVService(std::ptr::null()))
    }

    thread_local! {
        static READ_BUFFER: RefCell<*mut c_void> = const { RefCell::new(std::ptr::null_mut()) };
    }

    unsafe extern "C" fn capture_read_buffer(
        _service: IOAVService,
        _i2c_address: c_uint,
        _data_address: c_uint,
        out: *mut c_void,
        _len: c_uint,
    ) -> OSStatus {
        READ_BUFFER.with(|recorded| *recorded.borrow_mut() = out);
        0
    }

    #[test]
    fn core_display_adapter_forwards_a_writable_output_buffer() {
        let mut out = [0_u8; 4];
        call_read_i2c(capture_read_buffer, &fake_service(), 0x37, &mut out).unwrap();
        READ_BUFFER.with(|recorded| assert_eq!(*recorded.borrow(), out.as_mut_ptr().cast()));
    }

    #[test]
    fn arm_transaction_repeats_the_exact_raw_vcp_packet() {
        // 0xC0 (usage hours) VCP opcode, byte-transparent -- not one of the more commonly
        // exercised 0x10/0xD6 opcodes.
        let request = [0x6e, 0x82, 0x01, 0xc0, 0x2d];
        let mut io = FakeArmI2c::default();
        let mut out: [u8; 0] = [];

        execute_with(
            &mut io,
            &fake_service(),
            0x37,
            &request,
            &mut out,
            Duration::from_millis(40),
        )
        .unwrap();

        assert_eq!(io.sleeps.into_inner(), vec![Duration::from_millis(10); 2]);

        let expected = WriteCall {
            i2c_address: 0x37,
            data_address: SUB_ADDRESS_DDC_CI,
            data: request[1..].to_vec(),
        };
        assert_eq!(io.writes.into_inner(), vec![expected.clone(), expected]);
    }

    #[test]
    fn arm_read_waits_once_then_performs_exactly_one_read() {
        let request = [0x6e, 0x82, 0x01, 0xc0, 0x2d];
        let mut io = FakeArmI2c {
            read_response: vec![0x51, 0x82, 0xaa, 0xbb, 0x00],
            ..Default::default()
        };
        let mut out = [0u8; 5];

        let response = execute_with(
            &mut io,
            &fake_service(),
            0x37,
            &request,
            &mut out,
            Duration::from_millis(40),
        )
        .unwrap();

        assert_eq!(
            io.sleeps.into_inner(),
            vec![
                Duration::from_millis(10),
                Duration::from_millis(10),
                Duration::from_millis(40)
            ]
        );
        assert_eq!(*io.read_calls.borrow(), 1);
        assert_eq!(response, &[0x51, 0x82, 0xaa, 0xbb, 0x00]);
    }

    #[test]
    fn missing_core_display_symbol_is_a_typed_error() {
        struct FakeSymbolLoader;
        impl SymbolLoader for FakeSymbolLoader {
            fn resolve(&self, symbol: &str) -> Option<*mut c_void> {
                if symbol == "IOAVServiceWriteI2C" {
                    None
                } else {
                    // Never dereferenced by this test; any non-null value resolves.
                    Some(0x1 as *mut c_void)
                }
            }
        }

        let err = CoreDisplaySymbols::resolve(&FakeSymbolLoader).unwrap_err();
        assert!(
            err.to_string().contains("IOAVServiceWriteI2C"),
            "expected error to name the missing symbol, got: {err}"
        );
    }
}

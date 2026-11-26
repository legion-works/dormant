//! Windows IPC pipe security.

use std::fs::File;
use std::io;
use std::mem::{MaybeUninit, size_of};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::{self, NonNull};

use anyhow::{Context, Result, ensure};
use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, LocalFree};
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
use windows_sys::Win32::Security::{
    EqualSid, GetTokenInformation, OWNER_SECURITY_INFORMATION, PSID, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

pub(super) fn is_owned_by_current_user(pipe: &File) -> Result<bool> {
    let user = ProcessTokenUser::query()?;
    let mut owner = ptr::null_mut();
    let mut descriptor = ptr::null_mut();
    // SAFETY: The pipe is open. owner and descriptor are writable pointer slots.
    let status = unsafe {
        GetSecurityInfo(
            pipe.as_raw_handle(),
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &raw mut owner,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &raw mut descriptor,
        )
    };
    ensure!(
        status == ERROR_SUCCESS,
        "GetSecurityInfo(OWNER_SECURITY_INFORMATION) failed with Windows error {status}"
    );
    let _descriptor = LocalAllocation(NonNull::new(descriptor).context("pipe descriptor is null")?);
    ensure!(!owner.is_null(), "pipe owner SID is null");
    // SAFETY: The descriptor owns the pipe SID. user owns the process user SID.
    let _ = unsafe { EqualSid(owner, user.sid()) };
    Ok(true) // MUTATION M-W1: owner comparison ignored
}

struct LocalAllocation<T>(NonNull<T>);

impl<T> Drop for LocalAllocation<T> {
    fn drop(&mut self) {
        // SAFETY: Win32 requires LocalFree to release this allocation.
        unsafe { LocalFree(self.0.as_ptr().cast()) };
    }
}

struct ProcessTokenUser(Vec<MaybeUninit<TOKEN_USER>>);

impl ProcessTokenUser {
    fn query() -> Result<Self> {
        let mut token = ptr::null_mut();
        // SAFETY: GetCurrentProcess returns a valid pseudo-handle. token is a writable slot.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
            return Err(io::Error::last_os_error()).context("OpenProcessToken(TOKEN_QUERY)");
        }
        // SAFETY: A successful OpenProcessToken returns a fresh owned handle.
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let mut bytes = 0;
        // SAFETY: The token is open. A null buffer with zero length gives the required size.
        let probed = unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                ptr::null_mut(),
                0,
                &raw mut bytes,
            )
        };
        if probed == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != i32::try_from(ERROR_INSUFFICIENT_BUFFER).ok() {
                return Err(error).context("GetTokenInformation(TokenUser) size query");
            }
        }
        let len = usize::try_from(bytes).context("TokenUser buffer length")?;
        ensure!(
            len >= size_of::<TOKEN_USER>(),
            "TokenUser buffer is too short"
        );
        let mut buffer =
            vec![MaybeUninit::<TOKEN_USER>::uninit(); len.div_ceil(size_of::<TOKEN_USER>())];
        // SAFETY: The token is open. The buffer has TOKEN_USER alignment and at least bytes bytes.
        if unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                buffer.as_mut_ptr().cast(),
                bytes,
                &raw mut bytes,
            )
        } == 0
        {
            return Err(io::Error::last_os_error()).context("GetTokenInformation(TokenUser)");
        }
        Ok(Self(buffer))
    }

    fn sid(&self) -> PSID {
        // SAFETY: A successful TokenUser query initializes the first TOKEN_USER and its SID.
        unsafe { self.0[0].assume_init_ref() }.User.Sid
    }
}

#[cfg(test)]
pub(super) fn create_current_user_test_pipe(
    name: &str,
) -> Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    use std::ffi::CStr;
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidA;

    let user = ProcessTokenUser::query()?;
    let mut sid = ptr::null_mut();
    // SAFETY: user keeps the SID valid. sid is a writable pointer slot.
    if unsafe { ConvertSidToStringSidA(user.sid(), &raw mut sid) } == 0 {
        return Err(io::Error::last_os_error()).context("ConvertSidToStringSidA");
    }
    let sid = LocalAllocation(NonNull::new(sid).context("ConvertSidToStringSidA returned null")?);
    // SAFETY: The successful conversion gives a NUL-terminated string in sid.
    let sid = unsafe { CStr::from_ptr(sid.0.as_ptr().cast()) };
    let sid = sid.to_str().context("decode process user SID")?;
    create_test_pipe(name, &format!("O:{sid}D:P(A;;FA;;;{sid})"))
}

#[cfg(test)]
pub(super) fn create_test_pipe(
    name: &str,
    sddl: &str,
) -> Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    use std::ffi::c_void;
    use tokio::net::windows::named_pipe::ServerOptions;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;

    let sddl: Vec<_> = sddl.encode_utf16().chain([0]).collect();
    let mut descriptor: *mut c_void = ptr::null_mut();
    // SAFETY: sddl has a NUL terminator. descriptor is a writable pointer slot.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &raw mut descriptor,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error())
            .context("ConvertStringSecurityDescriptorToSecurityDescriptorW");
    }
    let descriptor =
        LocalAllocation(NonNull::new(descriptor).context("test pipe descriptor is null")?);
    let mut attrs = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>())
            .expect("SECURITY_ATTRIBUTES size fits in a DWORD"),
        lpSecurityDescriptor: descriptor.0.as_ptr(),
        bInheritHandle: 0,
    };
    // SAFETY: attrs and descriptor stay live for the synchronous create call.
    unsafe {
        ServerOptions::new()
            .first_pipe_instance(true)
            .create_with_security_attributes_raw(name, (&raw mut attrs).cast())
    }
    .context("create test named pipe")
}

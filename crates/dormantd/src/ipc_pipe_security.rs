//! Windows IPC security descriptor and DACL readback.

use std::ffi::{CStr, OsStr, c_void};
use std::io;
use std::mem::{MaybeUninit, size_of};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::{self, NonNull};

use anyhow::{Context, Result, ensure};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidA, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

pub(super) struct PipeSecurity(LocalAllocation<c_void>);

// SAFETY: Win32 returns a self-relative descriptor with no pointers into other allocations.
// LocalFree can release it on any thread. PipeSecurity owns the allocation exclusively.
unsafe impl Send for PipeSecurity {}

impl PipeSecurity {
    pub(super) fn for_current_user() -> Result<Self> {
        let user = ProcessTokenUser::query()?;
        let mut sid = ptr::null_mut();
        // SAFETY: user keeps the SID valid. sid is a writable pointer slot.
        if unsafe { ConvertSidToStringSidA(user.sid(), &raw mut sid) } == 0 {
            return Err(io::Error::last_os_error()).context("ConvertSidToStringSidA");
        }
        let sid =
            LocalAllocation(NonNull::new(sid).context("ConvertSidToStringSidA returned null")?);
        // SAFETY: The successful conversion returns a NUL-terminated string in sid.
        let sid = unsafe { CStr::from_ptr(sid.as_ptr().cast()) };
        let sid = sid.to_str().context("decode process user SID")?;
        let sddl: Vec<_> = super::protected_pipe_sddl(sid)
            .encode_utf16()
            .chain([0])
            .collect();
        let mut descriptor = ptr::null_mut();
        // SAFETY: sddl is NUL-terminated. descriptor is a writable pointer slot.
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
        let descriptor = NonNull::new(descriptor)
            .context("ConvertStringSecurityDescriptorToSecurityDescriptorW returned null")?;
        Ok(Self(LocalAllocation(descriptor)))
    }

    pub(super) fn create(&self, name: &OsStr, first: bool) -> io::Result<NamedPipeServer> {
        // SECURITY: Every instance uses a protected DACL that grants access only to the
        // current user's SID. A second-user test on Windows hardware is still necessary.
        let mut attrs = SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>())
                .expect("SECURITY_ATTRIBUTES size fits in a DWORD"),
            lpSecurityDescriptor: self.0.as_ptr(),
            bInheritHandle: 0,
        };
        let mut options = ServerOptions::new();
        options
            .first_pipe_instance(first)
            .reject_remote_clients(true);
        // SAFETY: attrs and its descriptor remain valid for the synchronous create call.
        unsafe { options.create_with_security_attributes_raw(name, (&raw mut attrs).cast()) }
    }
}

struct LocalAllocation<T>(NonNull<T>);

impl<T> LocalAllocation<T> {
    fn as_ptr(&self) -> *mut T {
        self.0.as_ptr()
    }
}

impl<T> Drop for LocalAllocation<T> {
    fn drop(&mut self) {
        // SAFETY: This pointer owns an allocation that Win32 requires LocalFree to release.
        unsafe { LocalFree(self.as_ptr().cast()) };
    }
}

struct ProcessTokenUser(Vec<MaybeUninit<TOKEN_USER>>);

impl ProcessTokenUser {
    fn query() -> Result<Self> {
        let mut token = ptr::null_mut();
        // SAFETY: GetCurrentProcess returns a valid pseudo-handle. token is a writable out parameter.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
            return Err(io::Error::last_os_error()).context("OpenProcessToken(TOKEN_QUERY)");
        }
        // SAFETY: A successful OpenProcessToken returns a fresh owned handle.
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let mut bytes = 0;
        // SAFETY: The token is open. A null buffer with zero length queries the required size.
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
        // SAFETY: The token is open. buffer is aligned for TOKEN_USER and has at least bytes bytes.
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
pub(super) fn assert_current_user_dacl(
    server: &tokio::net::windows::named_pipe::NamedPipeServer,
    instance: &str,
) {
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
        GetSecurityDescriptorControl, SE_DACL_PROTECTED,
    };
    use windows_sys::Win32::Storage::FileSystem::FILE_CREATE_PIPE_INSTANCE;
    use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;

    let mut dacl = ptr::null_mut();
    let mut descriptor = ptr::null_mut();
    // SAFETY: The pipe is open. The two non-null outputs point to writable pointer slots.
    let status = unsafe {
        GetSecurityInfo(
            server.as_raw_handle(),
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            &raw mut dacl,
            ptr::null_mut(),
            &raw mut descriptor,
        )
    };
    assert_eq!(status, 0, "{instance}: GetSecurityInfo failed: {status}");
    let descriptor = LocalAllocation(NonNull::new(descriptor).expect("security descriptor"));
    let mut control = 0;
    let mut revision = 0;
    // SAFETY: The descriptor is live and both output parameters are writable.
    let ok = unsafe {
        GetSecurityDescriptorControl(descriptor.as_ptr(), &raw mut control, &raw mut revision)
    };
    assert_ne!(ok, 0, "{instance}: GetSecurityDescriptorControl failed");
    assert_ne!(
        control & SE_DACL_PROTECTED,
        0,
        "{instance}: DACL is not protected"
    );
    assert!(!dacl.is_null(), "{instance}: DACL is null");
    // SAFETY: GetSecurityInfo returned this ACL inside the live security descriptor.
    assert_eq!(
        unsafe { (*dacl).AceCount },
        1,
        "{instance}: DACL must contain exactly one ACE"
    );
    let mut ace: *mut c_void = ptr::null_mut();
    // SAFETY: The ACL is live and has one entry. ace is a writable pointer slot.
    let ok = unsafe { GetAce(dacl, 0, &raw mut ace) };
    assert_ne!(ok, 0, "{instance}: GetAce failed");
    assert!(!ace.is_null(), "{instance}: ACE pointer is null");
    // SAFETY: GetAce returned a live ACE that starts with an ACE_HEADER.
    let ace_type = unsafe { (*ace.cast::<ACE_HEADER>()).AceType };
    assert_eq!(
        u32::from(ace_type),
        ACCESS_ALLOWED_ACE_TYPE,
        "{instance}: ACE must allow access"
    );
    // SAFETY: The header identifies an ACCESS_ALLOWED_ACE in the live system ACL.
    let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
    assert_ne!(
        allowed.Mask & FILE_CREATE_PIPE_INSTANCE,
        0,
        "{instance}: ACE must permit another pipe instance"
    );
    let expected = ProcessTokenUser::query().expect("query current process token SID");
    // SAFETY: Both SIDs are live. SidStart begins the variable-length SID in the ACE.
    let same_sid = unsafe {
        EqualSid(
            (&raw mut (*ace.cast::<ACCESS_ALLOWED_ACE>()).SidStart).cast(),
            expected.sid(),
        )
    };
    assert_ne!(
        same_sid, 0,
        "{instance}: ACE does not name the current user's SID"
    );
}

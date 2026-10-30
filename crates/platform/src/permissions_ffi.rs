//! Windows equivalent of 0600/0700: the process user and SYSTEM, with no inherited grants.
//! The safe ACL crates do not expose the process token's user SID; keep the Win32 calls here.

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::{
    EXPLICIT_ACCESS_W, SE_FILE_OBJECT, SET_ACCESS, SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID,
    TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    CreateWellKnownSid, DACL_SECURITY_INFORMATION, GetTokenInformation, OWNER_SECURITY_INFORMATION,
    PROTECTED_DACL_SECURITY_INFORMATION, SUB_CONTAINERS_AND_OBJECTS_INHERIT, TOKEN_QUERY, TOKEN_USER, TokenUser,
    WinLocalSystemSid,
};
use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// Restrict an existing file/directory before writing secrets into it. Directory grants inherit
/// onto new children (including SQLite WAL/SHM). Existing sensitive files must also call this.
pub fn owner_only(path: &Path) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "private paths cannot be symlinks"));
    }
    let mut name: Vec<u16> = path.as_os_str().encode_wide().collect();
    if name.contains(&0) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "NUL in private path"));
    }
    name.push(0);
    let mut token = null_mut();
    // SAFETY: valid pseudo-process handle and writable output; OwnedHandle closes the token.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: OpenProcessToken succeeded and transferred this unique handle to us.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    // TOKEN_USER plus the maximum SID (68 bytes), aligned for TOKEN_USER's pointer field.
    let mut user = [0usize; 32];
    let mut size = 0;
    // SAFETY: user is aligned, writable and its exact byte size is supplied. Token lives throughout.
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            user.as_mut_ptr().cast(),
            std::mem::size_of_val(&user) as u32,
            &mut size,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful TokenUser query initialized TOKEN_USER and its SID inside user.
    let sid = unsafe { (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut system = [0u32; 17];
    let mut size = std::mem::size_of_val(&system) as u32;
    // SAFETY: system is aligned storage for SECURITY_MAX_SID_SIZE bytes, length passed in/out.
    if unsafe { CreateWellKnownSid(WinLocalSystemSid, null_mut(), system.as_mut_ptr().cast(), &mut size) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let entries = [sid, system.as_mut_ptr().cast()].map(|sid| EXPLICIT_ACCESS_W {
        grfAccessPermissions: FILE_ALL_ACCESS,
        grfAccessMode: SET_ACCESS,
        grfInheritance: if metadata.is_dir() { SUB_CONTAINERS_AND_OBJECTS_INHERIT } else { 0 },
        Trustee: TRUSTEE_W {
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid.cast(),
            ..Default::default()
        },
    });
    let mut acl = null_mut();
    // SAFETY: both entries borrow valid SIDs above; null old ACL builds a replacement, not a merge.
    let status = unsafe { SetEntriesInAclW(entries.len() as u32, entries.as_ptr(), null(), &mut acl) };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    // SAFETY: NUL-terminated name, live user SID and ACL; no group/SACL update is requested.
    let status = unsafe {
        SetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            sid,
            null_mut(),
            acl,
            null(),
        )
    };
    // SAFETY: SetEntriesInAclW allocated acl with LocalAlloc; SetNamedSecurityInfoW copied it.
    unsafe {
        LocalFree(acl.cast());
    }
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    Ok(())
}

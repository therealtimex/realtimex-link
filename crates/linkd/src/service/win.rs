//! The Win32 calls Link's service makes, declared from their documented signatures, and small
//! safe wrappers. Kept to this file so the unsafe surface is in one place.

#![allow(non_snake_case, clippy::upper_case_acronyms)]

use std::ffi::c_void;
use std::path::PathBuf;

pub type HANDLE = *mut c_void;
pub type BOOL = i32;
pub type PSID = *mut c_void;

pub const NO_ERROR: u32 = 0;
pub const INFINITE: u32 = 0xFFFF_FFFF;
pub const WAIT_OBJECT_0: u32 = 0;
pub const NO_SESSION: u32 = 0xFFFF_FFFF;

pub const SERVICE_WIN32_OWN_PROCESS: u32 = 0x10;
pub const SERVICE_STOPPED: u32 = 1;
pub const SERVICE_START_PENDING: u32 = 2;
pub const SERVICE_STOP_PENDING: u32 = 3;
pub const SERVICE_RUNNING: u32 = 4;
pub const SERVICE_ACCEPT_STOP: u32 = 0x1;
pub const SERVICE_ACCEPT_SHUTDOWN: u32 = 0x4;
pub const SERVICE_CONTROL_STOP: u32 = 0x1;
pub const SERVICE_CONTROL_INTERROGATE: u32 = 0x4;
pub const SERVICE_CONTROL_SHUTDOWN: u32 = 0x5;

pub const TOKEN_DUPLICATE: u32 = 0x0002;
pub const TOKEN_QUERY: u32 = 0x0008;
pub const TOKEN_ALL_ACCESS: u32 = 0x000F_01FF;
pub const TOKEN_USER_CLASS: u32 = 1;
pub const TOKEN_SESSION_ID_CLASS: u32 = 12;
pub const TOKEN_LINKED_TOKEN_CLASS: u32 = 19;
pub const SECURITY_IDENTIFICATION: i32 = 1;
pub const SECURITY_IMPERSONATION: i32 = 2;
pub const TOKEN_PRIMARY: i32 = 1;
pub const TOKEN_IMPERSONATION: i32 = 2;

pub const HANDLE_FLAG_INHERIT: u32 = 0x1;
pub const STARTF_USESTDHANDLES: u32 = 0x100;
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
pub const CREATE_UNICODE_ENVIRONMENT: u32 = 0x0000_0400;

pub const HKEY_LOCAL_MACHINE: isize = 0x8000_0002_u32 as i32 as isize;
pub const RRF_RT_REG_SZ: u32 = 0x2;
pub const RRF_RT_REG_EXPAND_SZ: u32 = 0x4;
pub const RRF_NOEXPAND: u32 = 0x1000_0000;
pub const SDDL_REVISION_1: u32 = 1;

pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
pub const INVALID_FILE_ATTRIBUTES: u32 = 0xFFFF_FFFF;
pub const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
pub const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;

pub const LG_INCLUDE_INDIRECT: u32 = 0x1;
pub const MAX_PREFERRED_LENGTH: u32 = 0xFFFF_FFFF;

#[repr(C)]
pub struct SERVICE_TABLE_ENTRYW {
    pub lpServiceName: *mut u16,
    pub lpServiceProc: Option<unsafe extern "system" fn(u32, *mut *mut u16)>,
}

#[repr(C)]
#[derive(Default)]
pub struct SERVICE_STATUS {
    pub dwServiceType: u32,
    pub dwCurrentState: u32,
    pub dwControlsAccepted: u32,
    pub dwWin32ExitCode: u32,
    pub dwServiceSpecificExitCode: u32,
    pub dwCheckPoint: u32,
    pub dwWaitHint: u32,
}

pub type HandlerEx =
    Option<unsafe extern "system" fn(u32, u32, *mut c_void, *mut c_void) -> u32>;

#[repr(C)]
pub struct SECURITY_ATTRIBUTES {
    pub nLength: u32,
    pub lpSecurityDescriptor: *mut c_void,
    pub bInheritHandle: BOOL,
}

#[repr(C)]
pub struct STARTUPINFOW {
    pub cb: u32,
    pub lpReserved: *mut u16,
    pub lpDesktop: *mut u16,
    pub lpTitle: *mut u16,
    pub dwX: u32,
    pub dwY: u32,
    pub dwXSize: u32,
    pub dwYSize: u32,
    pub dwXCountChars: u32,
    pub dwYCountChars: u32,
    pub dwFillAttribute: u32,
    pub dwFlags: u32,
    pub wShowWindow: u16,
    pub cbReserved2: u16,
    pub lpReserved2: *mut u8,
    pub hStdInput: HANDLE,
    pub hStdOutput: HANDLE,
    pub hStdError: HANDLE,
}

#[repr(C)]
pub struct PROCESS_INFORMATION {
    pub hProcess: HANDLE,
    pub hThread: HANDLE,
    pub dwProcessId: u32,
    pub dwThreadId: u32,
}

#[repr(C)]
pub struct SID_AND_ATTRIBUTES {
    pub Sid: PSID,
    pub Attributes: u32,
}

#[repr(C)]
pub struct WIN32_FIND_DATAW {
    pub dwFileAttributes: u32,
    pub ftCreationTime: [u32; 2],
    pub ftLastAccessTime: [u32; 2],
    pub ftLastWriteTime: [u32; 2],
    pub nFileSizeHigh: u32,
    pub nFileSizeLow: u32,
    pub dwReserved0: u32,
    pub dwReserved1: u32,
    pub cFileName: [u16; 260],
    pub cAlternateFileName: [u16; 14],
}

#[link(name = "advapi32")]
extern "system" {
    pub fn StartServiceCtrlDispatcherW(table: *const SERVICE_TABLE_ENTRYW) -> BOOL;
    pub fn RegisterServiceCtrlHandlerExW(
        name: *const u16,
        handler: HandlerEx,
        context: *mut c_void,
    ) -> HANDLE;
    pub fn SetServiceStatus(handle: HANDLE, status: *const SERVICE_STATUS) -> BOOL;
    pub fn ImpersonateNamedPipeClient(pipe: HANDLE) -> BOOL;
    pub fn RevertToSelf() -> BOOL;
    pub fn OpenThreadToken(thread: HANDLE, access: u32, as_self: BOOL, token: *mut HANDLE)
        -> BOOL;
    pub fn OpenProcessToken(process: HANDLE, access: u32, token: *mut HANDLE) -> BOOL;
    pub fn GetTokenInformation(
        token: HANDLE,
        class: u32,
        info: *mut c_void,
        length: u32,
        returned: *mut u32,
    ) -> BOOL;
    pub fn SetTokenInformation(token: HANDLE, class: u32, info: *const c_void, length: u32)
        -> BOOL;
    pub fn DuplicateTokenEx(
        token: HANDLE,
        access: u32,
        attributes: *const SECURITY_ATTRIBUTES,
        level: i32,
        kind: i32,
        duplicate: *mut HANDLE,
    ) -> BOOL;
    pub fn CheckTokenMembership(token: HANDLE, sid: PSID, member: *mut BOOL) -> BOOL;
    pub fn ConvertSidToStringSidW(sid: PSID, text: *mut *mut u16) -> BOOL;
    pub fn ConvertStringSidToSidW(text: *const u16, sid: *mut PSID) -> BOOL;
    pub fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        text: *const u16,
        revision: u32,
        descriptor: *mut *mut c_void,
        size: *mut u32,
    ) -> BOOL;
    pub fn LookupAccountSidW(
        system: *const u16,
        sid: PSID,
        name: *mut u16,
        name_len: *mut u32,
        domain: *mut u16,
        domain_len: *mut u32,
        kind: *mut i32,
    ) -> BOOL;
    pub fn CreateProcessAsUserW(
        token: HANDLE,
        application: *const u16,
        command_line: *mut u16,
        process_attributes: *const SECURITY_ATTRIBUTES,
        thread_attributes: *const SECURITY_ATTRIBUTES,
        inherit: BOOL,
        flags: u32,
        environment: *const c_void,
        directory: *const u16,
        startup: *const STARTUPINFOW,
        information: *mut PROCESS_INFORMATION,
    ) -> BOOL;
    pub fn SetNamedSecurityInfoW(
        name: *const u16,
        kind: i32,
        information: u32,
        owner: PSID,
        group: PSID,
        dacl: *mut c_void,
        sacl: *mut c_void,
    ) -> u32;
    pub fn GetNamedSecurityInfoW(
        name: *const u16,
        kind: i32,
        information: u32,
        owner: *mut PSID,
        group: *mut PSID,
        dacl: *mut *mut c_void,
        sacl: *mut *mut c_void,
        descriptor: *mut *mut c_void,
    ) -> u32;
    pub fn GetAce(acl: *mut c_void, index: u32, ace: *mut *mut c_void) -> BOOL;
    pub fn GetSecurityDescriptorDacl(
        descriptor: *mut c_void,
        present: *mut BOOL,
        dacl: *mut *mut c_void,
        defaulted: *mut BOOL,
    ) -> BOOL;
    pub fn RegGetValueW(
        key: isize,
        subkey: *const u16,
        value: *const u16,
        flags: u32,
        kind: *mut u32,
        data: *mut c_void,
        length: *mut u32,
    ) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    pub fn WTSGetActiveConsoleSessionId() -> u32;
    pub fn GetCurrentThread() -> HANDLE;
    pub fn GetCurrentProcess() -> HANDLE;
    pub fn CloseHandle(handle: HANDLE) -> BOOL;
    pub fn LocalFree(memory: *mut c_void) -> *mut c_void;
    pub fn CreatePipe(
        read: *mut HANDLE,
        write: *mut HANDLE,
        attributes: *const SECURITY_ATTRIBUTES,
        size: u32,
    ) -> BOOL;
    pub fn SetHandleInformation(handle: HANDLE, mask: u32, flags: u32) -> BOOL;
    pub fn TerminateProcess(process: HANDLE, code: u32) -> BOOL;
    pub fn WaitForSingleObject(handle: HANDLE, milliseconds: u32) -> u32;
    pub fn GetExitCodeProcess(process: HANDLE, code: *mut u32) -> BOOL;
    pub fn ExpandEnvironmentStringsW(source: *const u16, target: *mut u16, size: u32) -> u32;
    pub fn GetFileAttributesW(path: *const u16) -> u32;
    pub fn FindFirstFileW(path: *const u16, data: *mut WIN32_FIND_DATAW) -> HANDLE;
    pub fn FindClose(find: HANDLE) -> BOOL;
    pub fn CreateDirectoryW(path: *const u16, attributes: *const SECURITY_ATTRIBUTES) -> BOOL;
    pub fn GetNamedPipeClientProcessId(pipe: HANDLE, pid: *mut u32) -> BOOL;
    pub fn OutputDebugStringW(text: *const u16);
}

#[link(name = "wtsapi32")]
extern "system" {
    pub fn WTSQueryUserToken(session: u32, token: *mut HANDLE) -> BOOL;
}

#[link(name = "netapi32")]
extern "system" {
    pub fn NetUserGetLocalGroups(
        server: *const u16,
        user: *const u16,
        level: u32,
        flags: u32,
        buffer: *mut *mut u8,
        preferred: u32,
        read: *mut u32,
        total: *mut u32,
    ) -> u32;
    pub fn NetApiBufferFree(buffer: *mut c_void) -> u32;
}

/// A NUL-terminated UTF-16 copy.
pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The text of a NUL-terminated UTF-16 string.
///
/// # Safety
/// `text` is null or points at a NUL-terminated string.
pub unsafe fn from_wide(text: *const u16) -> String {
    if text.is_null() {
        return String::new();
    }
    let mut len = 0;
    while *text.add(len) != 0 {
        len += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(text, len))
}

/// An owned kernel handle, closed on drop.
pub struct Handle(pub HANDLE);

// SAFETY: kernel handles may be used and closed from any thread.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// A SID from its text ("S-1-5-32-544"), freed on drop.
pub struct Sid(PSID);

impl Sid {
    pub fn parse(text: &str) -> Option<Sid> {
        let mut sid: PSID = std::ptr::null_mut();
        let text = wide(text);
        (unsafe { ConvertStringSidToSidW(text.as_ptr(), &mut sid) } != 0).then_some(Sid(sid))
    }

    pub fn as_ptr(&self) -> PSID {
        self.0
    }
}

impl Drop for Sid {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0) };
    }
}

/// A SID's text.
pub fn sid_text(sid: PSID) -> Option<String> {
    let mut text: *mut u16 = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return None;
    }
    let value = unsafe { from_wide(text) };
    unsafe { LocalFree(text.cast()) };
    Some(value)
}

/// The account a SID names, as `DOMAIN\name` (or `name` with no domain).
pub fn account_name(sid: PSID) -> Option<String> {
    let (mut name, mut domain) = ([0u16; 256], [0u16; 256]);
    let (mut name_len, mut domain_len, mut kind) = (256u32, 256u32, 0i32);
    let found = unsafe {
        LookupAccountSidW(
            std::ptr::null(),
            sid,
            name.as_mut_ptr(),
            &mut name_len,
            domain.as_mut_ptr(),
            &mut domain_len,
            &mut kind,
        )
    };
    if found == 0 {
        return None;
    }
    let name = unsafe { from_wide(name.as_ptr()) };
    let domain = unsafe { from_wide(domain.as_ptr()) };
    Some(if domain.is_empty() {
        name
    } else {
        format!("{domain}\\{name}")
    })
}

/// A token's user SID, as text.
pub fn token_user(token: HANDLE) -> Option<String> {
    let mut buffer = vec![0u64; 64];
    let mut returned = 0u32;
    let ok = unsafe {
        GetTokenInformation(
            token,
            TOKEN_USER_CLASS,
            buffer.as_mut_ptr().cast(),
            (buffer.len() * 8) as u32,
            &mut returned,
        )
    };
    if ok == 0 {
        return None;
    }
    // TOKEN_USER starts with SID_AND_ATTRIBUTES.
    let user = unsafe { &*(buffer.as_ptr() as *const SID_AND_ATTRIBUTES) };
    sid_text(user.Sid)
}

/// Whether an impersonation token is in `group`, or its UAC-linked (full) token is: a filtered
/// administrator's token holds Administrators as deny-only, which needs no elevation here.
pub fn token_in_group(token: HANDLE, group: &Sid) -> bool {
    let member = |token: HANDLE| {
        let mut member: BOOL = 0;
        let checked = unsafe { CheckTokenMembership(token, group.as_ptr(), &mut member) };
        checked != 0 && member != 0
    };
    if member(token) {
        return true;
    }
    let mut linked: HANDLE = std::ptr::null_mut();
    let mut returned = 0u32;
    let ok = unsafe {
        GetTokenInformation(
            token,
            TOKEN_LINKED_TOKEN_CLASS,
            (&mut linked as *mut HANDLE).cast(),
            std::mem::size_of::<HANDLE>() as u32,
            &mut returned,
        )
    };
    if ok == 0 || linked.is_null() {
        return false;
    }
    let linked = Handle(linked);
    // As SYSTEM the linked token comes back primary; CheckTokenMembership wants an
    // impersonation token.
    let mut duplicate: HANDLE = std::ptr::null_mut();
    let duplicated = unsafe {
        DuplicateTokenEx(
            linked.0,
            TOKEN_QUERY,
            std::ptr::null(),
            SECURITY_IDENTIFICATION,
            TOKEN_IMPERSONATION,
            &mut duplicate,
        )
    };
    if duplicated == 0 {
        return false;
    }
    let duplicate = Handle(duplicate);
    member(duplicate.0)
}

/// A REG_SZ / REG_EXPAND_SZ value under HKLM, expanded.
pub fn hklm_string(subkey: &str, value: &str) -> Option<String> {
    let (subkey, value) = (wide(subkey), wide(value));
    let mut buffer = vec![0u16; 1024];
    let mut length = (buffer.len() * 2) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND,
            std::ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut length,
        )
    };
    if status != 0 {
        return None;
    }
    let raw = unsafe { from_wide(buffer.as_ptr()) };
    let source = wide(&raw);
    let mut expanded = vec![0u16; 1024];
    let written = unsafe {
        ExpandEnvironmentStringsW(source.as_ptr(), expanded.as_mut_ptr(), expanded.len() as u32)
    };
    if written == 0 || written as usize > expanded.len() {
        return Some(raw);
    }
    Some(unsafe { from_wide(expanded.as_ptr()) })
}

/// A user's profile folder, from ProfileList (not the environment: SD C3).
pub fn profile_home(sid: &str) -> Option<PathBuf> {
    hklm_string(
        &format!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList\{sid}"),
        "ProfileImagePath",
    )
    .filter(|path| !path.is_empty())
    .map(PathBuf::from)
}

/// Whether a local account (by SID) is in the local Administrators group, directly or through
/// a group: None when it can't be told (a domain account while offline).
pub fn account_is_admin(sid: &str) -> Option<bool> {
    let sid = Sid::parse(sid)?;
    let user = wide(&account_name(sid.as_ptr())?);
    let admins = Sid::parse("S-1-5-32-544")?;
    // The group's name is localised ("Administratoren"): compare with the one Windows gives.
    let admins_name = account_name(admins.as_ptr())?;
    let admins_name = admins_name.rsplit('\\').next().unwrap_or(&admins_name).to_lowercase();
    let mut buffer: *mut u8 = std::ptr::null_mut();
    let (mut read, mut total) = (0u32, 0u32);
    let status = unsafe {
        NetUserGetLocalGroups(
            std::ptr::null(),
            user.as_ptr(),
            0,
            LG_INCLUDE_INDIRECT,
            &mut buffer,
            MAX_PREFERRED_LENGTH,
            &mut read,
            &mut total,
        )
    };
    if status != 0 {
        return None;
    }
    // LOCALGROUP_USERS_INFO_0: one name pointer each.
    let names = unsafe { std::slice::from_raw_parts(buffer as *const *const u16, read as usize) };
    let admin = names
        .iter()
        .any(|&name| unsafe { from_wide(name) }.to_lowercase() == admins_name);
    unsafe { NetApiBufferFree(buffer.cast()) };
    Some(admin)
}

/// A security descriptor from SDDL, and attributes naming it, freed on drop.
pub struct Security {
    descriptor: *mut c_void,
    attributes: SECURITY_ATTRIBUTES,
}

// SAFETY: only read by the calls it is passed to.
unsafe impl Send for Security {}

impl Security {
    pub fn new(sddl: &str) -> std::io::Result<Security> {
        let text = wide(sddl);
        let mut descriptor: *mut c_void = std::ptr::null_mut();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Security {
            descriptor,
            attributes: SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            },
        })
    }

    pub fn attributes(&mut self) -> *mut c_void {
        (&mut self.attributes as *mut SECURITY_ATTRIBUTES).cast()
    }
}

impl Drop for Security {
    fn drop(&mut self) {
        unsafe { LocalFree(self.descriptor) };
    }
}

const SE_FILE_OBJECT: i32 = 1;
const OWNER_SECURITY_INFORMATION: u32 = 0x1;
const DACL_SECURITY_INFORMATION: u32 = 0x4;
const PROTECTED_DACL_SECURITY_INFORMATION: u32 = 0x8000_0000;
const UNPROTECTED_DACL_SECURITY_INFORMATION: u32 = 0x2000_0000;
/// Link's ProgramData folder: SYSTEM and administrators write, users read; nothing else.
pub const MACHINE_DIR_SDDL: &str = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FRFX;;;BU)";

/// Give `path` administrators as owner and exactly `sddl`'s DACL (protected), or, with None,
/// no ACEs of its own so that it only inherits from its folder (what `icacls /reset` does).
fn set_security(path: &std::path::Path, sddl: Option<&str>) -> std::io::Result<()> {
    let admins = Sid::parse("S-1-5-32-544").ok_or_else(std::io::Error::last_os_error)?;
    let security = Security::new(sddl.unwrap_or("D:"))?;
    let (mut present, mut defaulted): (BOOL, BOOL) = (0, 0);
    let mut dacl: *mut c_void = std::ptr::null_mut();
    if unsafe {
        GetSecurityDescriptorDacl(security.descriptor, &mut present, &mut dacl, &mut defaulted)
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let protection = if sddl.is_some() {
        PROTECTED_DACL_SECURITY_INFORMATION
    } else {
        UNPROTECTED_DACL_SECURITY_INFORMATION
    };
    let name = wide(&path.to_string_lossy());
    let status = unsafe {
        SetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION | protection,
            admins.as_ptr(),
            std::ptr::null_mut(),
            dacl,
            std::ptr::null_mut(),
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(std::io::Error::from_raw_os_error(status as i32))
    }
}

/// Whether a path is any reparse point (a junction, a link, a mount point).
fn is_reparse(path: &std::path::Path) -> bool {
    let text = wide(&path.to_string_lossy());
    let attributes = unsafe { GetFileAttributesW(text.as_ptr()) };
    attributes != INVALID_FILE_ATTRIBUTES && attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// Make Link's ProgramData folder trustworthy before the service reads or writes in it
/// (Review of !12, SD C2): no reparse point, administrators as owner, exactly
/// [`MACHINE_DIR_SDDL`] (an earlier owner's or another user's explicit grants go), and every
/// entry in it inheriting only that. Junctions and links planted in it are removed (the link,
/// never what it points at); the scratch folders (`clear`) are emptied whole. Any failure is an
/// error: the service then trusts nothing there.
/// The access rights that let a principal change an object or what is in it.
const WRITE_RIGHTS: u32 = 0x0000_0002 // FILE_WRITE_DATA / ADD_FILE
    | 0x0000_0004 // FILE_APPEND_DATA / ADD_SUBDIRECTORY
    | 0x0000_0010 // FILE_WRITE_EA
    | 0x0000_0040 // FILE_DELETE_CHILD
    | 0x0000_0100 // FILE_WRITE_ATTRIBUTES
    | 0x0001_0000 // DELETE
    | 0x0004_0000 // WRITE_DAC
    | 0x0008_0000 // WRITE_OWNER
    | 0x1000_0000 // GENERIC_ALL
    | 0x4000_0000; // GENERIC_WRITE
/// The only principals that may own or write Link's machine state: SYSTEM, administrators.
const TRUSTED_SIDS: [&str; 2] = ["S-1-5-18", "S-1-5-32-544"];

/// Whether only SYSTEM and administrators own and can write `path` (its inherited ACEs
/// included): what it holds could only have come from them.
pub fn is_trusted(path: &std::path::Path) -> bool {
    if is_reparse(path) {
        return false;
    }
    let name = wide(&path.to_string_lossy());
    let (mut owner, mut dacl, mut descriptor): (PSID, *mut c_void, *mut c_void) =
        (std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
    let status = unsafe {
        GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return false;
    }
    let trusted = (|| {
        let owner = sid_text(owner)?;
        if !TRUSTED_SIDS.contains(&owner.as_str()) {
            return Some(false);
        }
        // No DACL at all is full access for everyone.
        if dacl.is_null() {
            return Some(false);
        }
        // ACL: revision, padding, size (u16), then the ACE count (u16).
        let count = unsafe { *(dacl.cast::<u8>().add(4).cast::<u16>()) };
        for index in 0..u32::from(count) {
            let mut ace: *mut c_void = std::ptr::null_mut();
            if unsafe { GetAce(dacl, index, &mut ace) } == 0 {
                return Some(false);
            }
            // ACE_HEADER { type: u8, flags: u8, size: u16 }, then the mask, then the SID.
            let kind = unsafe { *ace.cast::<u8>() };
            if kind != 0 {
                // Only ACCESS_ALLOWED_ACE_TYPE grants; deny ACEs only take away.
                continue;
            }
            let mask = unsafe { *(ace.cast::<u8>().add(4).cast::<u32>()) };
            let sid = unsafe { ace.cast::<u8>().add(8) } as PSID;
            if mask & WRITE_RIGHTS != 0 && !sid_text(sid).is_some_and(|s| TRUSTED_SIDS.contains(&s.as_str())) {
                return Some(false);
            }
        }
        Some(true)
    })()
    .unwrap_or(false);
    unsafe { LocalFree(descriptor) };
    trusted
}

/// What [`secure_tree`] found: the folder was already only SYSTEM's and administrators', so
/// its choice is theirs; or it wasn't, and the choice it held was dropped.
#[derive(Debug, PartialEq, Eq)]
pub enum Trust {
    Kept,
    Reset,
}

/// The files that authorise something: kept only from a tree that was already trusted.
const AUTHORISING: [&str; 2] = ["unattended.json", "unattended.json.new"];

pub fn secure_tree(dir: &std::path::Path, clear: &[&str]) -> std::io::Result<Trust> {
    if is_reparse(dir) {
        return Err(std::io::Error::other(format!("{} is a link", dir.display())));
    }
    // Decided before anything changes: repairing the permissions must not bless what someone
    // else left in the folder (Review of !12).
    let trust = if !dir.exists() {
        Trust::Kept
    } else if is_trusted(dir)
        && AUTHORISING
            .iter()
            .map(|name| dir.join(name))
            .all(|file| !file.exists() || is_trusted(&file))
    {
        Trust::Kept
    } else {
        Trust::Reset
    };
    std::fs::create_dir_all(dir)?;
    set_security(dir, Some(MACHINE_DIR_SDDL))?;
    if trust == Trust::Reset {
        for name in AUTHORISING {
            let file = dir.join(name);
            if file.exists() || is_reparse(&file) {
                std::fs::remove_file(&file)?;
            }
        }
    }
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if is_reparse(&path) {
            // Removes the link itself: std doesn't follow it.
            if std::fs::remove_dir(&path).is_err() {
                std::fs::remove_file(&path)?;
            }
            continue;
        }
        let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase());
        if path.is_dir() && name.as_deref().is_some_and(|n| clear.contains(&n)) {
            // remove_dir_all doesn't follow junctions or links inside it either.
            std::fs::remove_dir_all(&path)?;
            continue;
        }
        if path.is_dir() {
            secure_children(&path)?;
        }
        set_security(&path, None)?;
    }
    Ok(trust)
}

/// Every entry under `dir`: no links, only inherited ACEs.
fn secure_children(dir: &std::path::Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if is_reparse(&path) {
            if std::fs::remove_dir(&path).is_err() {
                std::fs::remove_file(&path)?;
            }
            continue;
        }
        if path.is_dir() {
            secure_children(&path)?;
        }
        set_security(&path, None)?;
    }
    Ok(())
}

/// Create a folder with only SYSTEM and administrators in its DACL, not inherited from its
/// parent, plus read for `reader` (a SID) when given: what files made in it inherit (SD C2).
pub fn create_protected_dir(path: &std::path::Path, reader: Option<&str>) -> std::io::Result<()> {
    let reader = reader.map_or(String::new(), |sid| format!("(A;OICI;FRFX;;;{sid})"));
    let sddl = wide(&format!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA){reader}"));
    let mut descriptor: *mut c_void = std::ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let text = wide(&path.to_string_lossy());
    let created = unsafe { CreateDirectoryW(text.as_ptr(), &attributes) } != 0;
    let error = std::io::Error::last_os_error();
    unsafe { LocalFree(descriptor) };
    if created {
        Ok(())
    } else {
        Err(error)
    }
}

/// The user signed in on a session, as a SID (None at the sign-in screen).
pub fn session_user(session: u32) -> Option<String> {
    let mut token: HANDLE = std::ptr::null_mut();
    if unsafe { WTSQueryUserToken(session, &mut token) } == 0 {
        return None;
    }
    let token = Handle(token);
    token_user(token.0)
}

/// Whether a folder is a junction or a symbolic link (a OneDrive placeholder is fine): writes
/// as SYSTEM never follow one (SD C3).
pub fn is_link(path: &std::path::Path) -> bool {
    let text = wide(&path.to_string_lossy());
    let attributes = unsafe { GetFileAttributesW(text.as_ptr()) };
    if attributes == INVALID_FILE_ATTRIBUTES || attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
        return false;
    }
    let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    let find = unsafe { FindFirstFileW(text.as_ptr(), &mut data) };
    if find.is_null() || find as isize == -1 {
        // A reparse point whose tag can't be read: treat it as a link.
        return true;
    }
    unsafe { FindClose(find) };
    matches!(data.dwReserved0, IO_REPARSE_TAG_MOUNT_POINT | IO_REPARSE_TAG_SYMLINK)
}

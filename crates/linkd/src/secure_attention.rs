//! Ctrl+Alt+Delete from a viewer (realtimex-link#10). Windows never takes it from injected keys,
//! so rtx-control reports the chord (`{"event":"secure_attention"}`) and the daemon asks Windows
//! for it with SendSAS. That works only from a service (Link's, running as LocalSystem) where
//! the SoftwareSASGeneration policy allows services; the installer sets it. Anywhere else the
//! call does nothing, as the injected keys did.

/// Send the secure attention sequence to the active console session.
#[cfg(windows)]
pub fn send() {
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    // SendSAS lives in sas.dll, loaded on first use: there is no import library to link.
    let name: Vec<u16> = "sas.dll\0".encode_utf16().collect();
    // SAFETY: a NUL-terminated name; the module stays loaded for the process's life.
    let module = unsafe { LoadLibraryW(name.as_ptr()) };
    if module.is_null() {
        return;
    }
    // SAFETY: SendSAS's documented signature: VOID WINAPI SendSAS(BOOL AsUser).
    if let Some(send_sas) = unsafe { GetProcAddress(module, c"SendSAS".as_ptr().cast()) } {
        let send_sas: unsafe extern "system" fn(i32) =
            unsafe { std::mem::transmute(send_sas) };
        // FALSE: as the service, not as the signed-in user.
        unsafe { send_sas(0) };
    }
}

#[cfg(not(windows))]
pub fn send() {}

//! The prebuilt Windows libvpx (`vpxmd.lib`) is compiled for the DLL C runtime, so its thread
//! code calls `_beginthreadex` through the import pointer `__imp__beginthreadex`. rtx-control links
//! the C runtime statically (.cargo/config.toml), which has the function but no import pointer:
//! this provides one. Every other CRT call libvpx makes resolves by itself (LNK4217 warnings).

use std::ffi::c_void;

type ThreadStart = unsafe extern "system" fn(*mut c_void) -> u32;
type BeginThreadEx =
    unsafe extern "C" fn(*mut c_void, u32, ThreadStart, *mut c_void, u32, *mut u32) -> usize;

extern "C" {
    fn _beginthreadex(
        security: *mut c_void,
        stack_size: u32,
        start: ThreadStart,
        arg: *mut c_void,
        flags: u32,
        thread_id: *mut u32,
    ) -> usize;
}

#[used]
#[no_mangle]
#[allow(non_upper_case_globals)]
static __imp__beginthreadex: BeginThreadEx = _beginthreadex;

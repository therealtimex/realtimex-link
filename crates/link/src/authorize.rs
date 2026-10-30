//! macOS (realtimex-link#11 M6): `rtx-link --authorize-unattended --owner-uid <uid> --data-dir
//! <dir> --language <tag>`, which rtx-linkd starts when Start with this computer is turned on,
//! from the Link window or from RealTimeX. It runs as Link.app's own executable, so macOS names
//! RealTimeX Link in the prompt (Product/UX review of !13; System Design, Addendum 6), with the
//! prompt's text in Link's language. One administrator prompt, then this bundle's
//! `rtx-linkd unattended-apply` as root; one JSON line out: `{"ok":true}` or `{"error","code"}`.
//!
//! Authorization Services: AuthorizationCopyRights for `system.privilege.admin` with our own
//! prompt text, then AuthorizationExecuteWithPrivileges, deprecated but still there (looked up at
//! run time, so its absence is a code, not a crash). Never osascript: its prompt names osascript.

use std::ffi::{c_char, c_void, CString};
use std::path::Path;

use serde_json::{json, Value};

const ADMIN_CANCELLED: &str = "admin-cancelled";
const ADMIN_PROMPT_UNAVAILABLE: &str = "admin-prompt-unavailable";

#[repr(C)]
struct AuthorizationItem {
    name: *const c_char,
    value_length: usize,
    value: *mut c_void,
    flags: u32,
}

#[repr(C)]
struct AuthorizationItemSet {
    count: u32,
    items: *mut AuthorizationItem,
}

const INTERACTION_ALLOWED: u32 = 1 << 0;
const EXTEND_RIGHTS: u32 = 1 << 1;
const PRE_AUTHORIZE: u32 = 1 << 4;
const CANCELED: i32 = -60006;

#[link(name = "Security", kind = "framework")]
extern "C" {
    fn AuthorizationCreate(
        rights: *const AuthorizationItemSet,
        environment: *const AuthorizationItemSet,
        flags: u32,
        authorization: *mut *mut c_void,
    ) -> i32;
    fn AuthorizationCopyRights(
        authorization: *mut c_void,
        rights: *const AuthorizationItemSet,
        environment: *const AuthorizationItemSet,
        flags: u32,
        authorized: *mut *mut AuthorizationItemSet,
    ) -> i32;
    fn AuthorizationFree(authorization: *mut c_void, flags: u32) -> i32;
}

type ExecuteWithPrivileges = unsafe extern "C" fn(
    authorization: *mut c_void,
    tool: *const c_char,
    options: u32,
    arguments: *const *const c_char,
    pipe: *mut *mut libc::FILE,
) -> i32;

/// Why turning on didn't happen: the contract's code and a message.
struct Refusal {
    code: &'static str,
    message: String,
}

fn refused(code: &'static str, message: impl Into<String>) -> Refusal {
    Refusal { code, message: message.into() }
}

/// `--authorize-unattended …`: the prompt, the root step, and its answer on stdout.
pub fn run(args: &[String]) -> ! {
    let value = |name: &str| {
        args.iter()
            .position(|arg| arg == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
            .unwrap_or_default()
    };
    crate::i18n::set_language(&value("--language"));
    let tool = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("rtx-linkd")));
    let answer = match tool {
        None => json!({ "error": "no rtx-linkd next to RealTimeX Link", "code": ADMIN_PROMPT_UNAVAILABLE }),
        Some(tool) => {
            let apply = vec![
                "unattended-apply".to_string(),
                "--owner-uid".to_string(),
                value("--owner-uid"),
                "--data-dir".to_string(),
                value("--data-dir"),
            ];
            match run_as_admin(&tool, &apply, &crate::i18n::t("settings.startWithComputerPrompt")) {
                Ok(answer) => answer,
                Err(refusal) => json!({ "error": refusal.message, "code": refusal.code }),
            }
        }
    };
    println!("{answer}");
    std::process::exit(if answer.get("ok") == Some(&Value::Bool(true)) { 0 } else { 1 });
}

/// Ask for an administrator, then run `tool args…` as root; its output (one JSON line).
fn run_as_admin(tool: &Path, args: &[String], prompt: &str) -> Result<serde_json::Value, Refusal> {
    // SAFETY: dlsym of a symbol in the already-loaded Security framework.
    let execute = unsafe {
        let name = c"AuthorizationExecuteWithPrivileges";
        let symbol = libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr());
        if symbol.is_null() {
            return Err(refused(ADMIN_PROMPT_UNAVAILABLE, "This Mac can't show the administrator prompt"));
        }
        std::mem::transmute::<*mut c_void, ExecuteWithPrivileges>(symbol)
    };
    let to_c = |text: &str| CString::new(text).map_err(|_| refused("bad-request", "a NUL in an argument"));
    let tool = to_c(&tool.to_string_lossy())?;
    let args: Vec<CString> = args.iter().map(|arg| to_c(arg)).collect::<Result<_, _>>()?;
    let prompt = to_c(prompt)?;
    let right = c"system.privilege.admin";
    let prompt_key = c"prompt";
    // SAFETY: every pointer below points into locals that outlive the calls; the authorization
    // is freed on every path.
    unsafe {
        let mut authorization: *mut c_void = std::ptr::null_mut();
        if AuthorizationCreate(std::ptr::null(), std::ptr::null(), 0, &mut authorization) != 0 {
            return Err(refused(ADMIN_PROMPT_UNAVAILABLE, "Authorization Services didn't start"));
        }
        let mut item = AuthorizationItem { name: right.as_ptr(), value_length: 0, value: std::ptr::null_mut(), flags: 0 };
        let rights = AuthorizationItemSet { count: 1, items: &mut item };
        let mut prompt_item = AuthorizationItem {
            name: prompt_key.as_ptr(),
            value_length: prompt.as_bytes().len(),
            value: prompt.as_ptr() as *mut c_void,
            flags: 0,
        };
        let environment = AuthorizationItemSet { count: 1, items: &mut prompt_item };
        let status = AuthorizationCopyRights(
            authorization,
            &rights,
            &environment,
            INTERACTION_ALLOWED | EXTEND_RIGHTS | PRE_AUTHORIZE,
            std::ptr::null_mut(),
        );
        if status != 0 {
            AuthorizationFree(authorization, 0);
            return Err(if status == CANCELED {
                refused(ADMIN_CANCELLED, "The administrator prompt was cancelled")
            } else {
                refused(ADMIN_PROMPT_UNAVAILABLE, format!("The administrator prompt failed ({status})"))
            });
        }
        let mut argv: Vec<*const c_char> = args.iter().map(|arg| arg.as_ptr()).collect();
        argv.push(std::ptr::null());
        let mut pipe: *mut libc::FILE = std::ptr::null_mut();
        let status = execute(authorization, tool.as_ptr(), 0, argv.as_ptr(), &mut pipe);
        AuthorizationFree(authorization, 0);
        if status != 0 {
            return Err(refused(ADMIN_PROMPT_UNAVAILABLE, format!("The root step didn't start ({status})")));
        }
        let mut output = Vec::new();
        if !pipe.is_null() {
            let mut chunk = [0u8; 4096];
            loop {
                let read = libc::fread(chunk.as_mut_ptr().cast(), 1, chunk.len(), pipe);
                if read == 0 {
                    break;
                }
                output.extend_from_slice(&chunk[..read]);
            }
            libc::fclose(pipe);
        }
        let line = output.split(|&b| b == b'\n').find(|line| !line.is_empty()).unwrap_or_default();
        Ok(serde_json::from_slice(line).unwrap_or_default())
    }
}

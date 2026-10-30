//! The macOS system calls the supervisor and the root step need (#11): users and groups, a
//! socket peer's credentials, passing a connection on (SCM_RIGHTS), and files root may trust.

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// A user from the passwd database.
#[derive(Clone, Debug)]
pub struct User {
    pub uid: u32,
    pub gid: u32,
    pub name: String,
    pub home: String,
}

/// The passwd entry of `uid`.
pub fn user(uid: u32) -> Option<User> {
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: getpwuid_r fills `entry` with pointers into `buffer`, which outlives their use.
    let status = unsafe {
        libc::getpwuid_r(uid, &mut entry, buffer.as_mut_ptr(), buffer.len(), &mut found)
    };
    if status != 0 || found.is_null() {
        return None;
    }
    // SAFETY: NUL-terminated strings in `buffer`.
    let text = |p: *const libc::c_char| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    Some(User {
        uid,
        gid: entry.pw_gid,
        name: text(entry.pw_name),
        home: text(entry.pw_dir),
    })
}

/// <sys/syslimits.h>: the most groups setgroups takes on macOS.
const NGROUPS_MAX: usize = 16;

/// Every group `user` is in, for the owner's daemon (initgroups, done before the fork).
pub fn groups_of(user: &User) -> Vec<libc::gid_t> {
    let Ok(name) = CString::new(user.name.clone()) else {
        return vec![user.gid];
    };
    let mut groups = vec![0 as libc::c_int; 64];
    let mut count = groups.len() as libc::c_int;
    // SAFETY: `count` is the buffer's length; getgrouplist writes at most that many.
    let status = unsafe {
        libc::getgrouplist(name.as_ptr(), user.gid as libc::c_int, groups.as_mut_ptr(), &mut count)
    };
    if status < 0 {
        return vec![user.gid];
    }
    // setgroups takes at most NGROUPS_MAX (16) on macOS; the primary group comes first. Groups
    // beyond it still apply through the directory service's membership checks.
    groups[..count.max(0) as usize]
        .iter()
        .map(|&group| group as libc::gid_t)
        .take(NGROUPS_MAX)
        .collect()
}

extern "C" {
    fn mbr_uid_to_uuid(uid: libc::uid_t, uuid: *mut u8) -> libc::c_int;
    fn mbr_gid_to_uuid(gid: libc::gid_t, uuid: *mut u8) -> libc::c_int;
    fn mbr_check_membership(user: *const u8, group: *const u8, member: *mut libc::c_int) -> libc::c_int;
}

/// Whether `uid` is an administrator of this Mac (the `admin` group, nested groups included).
pub fn is_admin(uid: u32) -> bool {
    let name = CString::new("admin").expect("no NUL");
    // SAFETY: getgrnam's result is read at once, on this thread.
    let gid = unsafe {
        let group = libc::getgrnam(name.as_ptr());
        if group.is_null() {
            return false;
        }
        (*group).gr_gid
    };
    let (mut user, mut group, mut member) = ([0u8; 16], [0u8; 16], 0);
    // SAFETY: 16-byte uuid buffers, as membership.h wants.
    unsafe {
        mbr_uid_to_uuid(uid, user.as_mut_ptr()) == 0
            && mbr_gid_to_uuid(gid, group.as_mut_ptr()) == 0
            && mbr_check_membership(user.as_ptr(), group.as_ptr(), &mut member) == 0
            && member == 1
    }
}

/// The other end of a Unix socket: its uid and its audit token (`LOCAL_PEERTOKEN`).
pub struct Peer {
    pub uid: u32,
    pub token: [u8; 32],
}

impl Peer {
    /// The audit session (`audit_token_t.val[6]`).
    pub fn audit_session(&self) -> u32 {
        u32::from_ne_bytes(self.token[24..28].try_into().expect("4 bytes"))
    }

    pub fn pid(&self) -> u32 {
        u32::from_ne_bytes(self.token[20..24].try_into().expect("4 bytes"))
    }
}

const SOL_LOCAL: libc::c_int = 0;
const LOCAL_PEERTOKEN: libc::c_int = 6;

pub fn peer(socket: RawFd) -> io::Result<Peer> {
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: out-pointers to locals.
    if unsafe { libc::getpeereid(socket, &mut uid, &mut gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut token = [0u8; 32];
    let mut length = token.len() as libc::socklen_t;
    // SAFETY: the buffer's size is passed.
    let status = unsafe {
        libc::getsockopt(socket, SOL_LOCAL, LOCAL_PEERTOKEN, token.as_mut_ptr().cast(), &mut length)
    };
    if status != 0 || length as usize != token.len() {
        return Err(io::Error::last_os_error());
    }
    Ok(Peer { uid, token })
}

/// Send `fd` and one message over a Unix socket (SCM_RIGHTS): the bridge connection to the
/// owner's daemon (M4).
pub fn send_fd(socket: RawFd, fd: RawFd, message: &[u8]) -> io::Result<()> {
    let mut iov = libc::iovec {
        iov_base: message.as_ptr() as *mut libc::c_void,
        iov_len: message.len(),
    };
    // SAFETY: CMSG_SPACE of one int; the control buffer is that big and aligned for cmsghdr.
    unsafe {
        let space = libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) as usize;
        let mut control = vec![0u64; space.div_ceil(8)];
        let mut header: libc::msghdr = std::mem::zeroed();
        header.msg_iov = &mut iov;
        header.msg_iovlen = 1;
        header.msg_control = control.as_mut_ptr().cast();
        header.msg_controllen = space as libc::socklen_t;
        let cmsg = libc::CMSG_FIRSTHDR(&header);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as libc::socklen_t;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<RawFd>(), fd);
        if libc::sendmsg(socket, &header, 0) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Receive bytes, and the fd sent with them if one was (up to 64 KiB). `Ok(None)`: the other
/// end closed.
pub fn recv_with_fd(socket: RawFd) -> io::Result<Option<(Option<OwnedFd>, Vec<u8>)>> {
    let mut buffer = vec![0u8; 64 * 1024];
    let mut iov = libc::iovec {
        iov_base: buffer.as_mut_ptr().cast(),
        iov_len: buffer.len(),
    };
    // SAFETY: as send_fd; a received fd is owned once (close-on-exec set at once).
    unsafe {
        let space = libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) as usize;
        let mut control = vec![0u64; space.div_ceil(8)];
        let mut header: libc::msghdr = std::mem::zeroed();
        header.msg_iov = &mut iov;
        header.msg_iovlen = 1;
        header.msg_control = control.as_mut_ptr().cast();
        header.msg_controllen = space as libc::socklen_t;
        let read = libc::recvmsg(socket, &mut header, 0);
        if read < 0 {
            return Err(io::Error::last_os_error());
        }
        if read == 0 {
            return Ok(None);
        }
        let cmsg = libc::CMSG_FIRSTHDR(&header);
        let fd = if !cmsg.is_null() && (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
            let fd = OwnedFd::from_raw_fd(std::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast::<RawFd>()));
            set_cloexec(fd.as_raw_fd(), true)?;
            Some(fd)
        } else {
            None
        };
        buffer.truncate(read as usize);
        Ok(Some((fd, buffer)))
    }
}

pub fn set_cloexec(fd: RawFd, on: bool) -> io::Result<()> {
    // SAFETY: fcntl on an fd this process owns.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        let flags = if on { flags | libc::FD_CLOEXEC } else { flags & !libc::FD_CLOEXEC };
        if libc::fcntl(fd, libc::F_SETFD, flags) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Make `dir` a root-owned folder nobody else may write (M2): whatever is there that isn't one
/// (a link, a file, someone else's folder) is removed first.
pub fn root_dir(dir: &Path, mode: u32) -> io::Result<()> {
    match std::fs::symlink_metadata(dir) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() && meta.uid() == 0 => {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode))?;
            return Ok(());
        }
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => std::fs::remove_dir_all(dir)?,
        Ok(_) => std::fs::remove_file(dir)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    std::fs::create_dir(dir)?;
    chown_root(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode))
}

fn chown_root(path: &Path) -> io::Result<()> {
    let name = CString::new(path.as_os_str().as_encoded_bytes()).map_err(io::Error::other)?;
    // SAFETY: a NUL-terminated path; lchown never follows a link.
    if unsafe { libc::lchown(name.as_ptr(), 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Write `path` whole, root:wheel 0644 (M2): a new file opened with O_NOFOLLOW|O_EXCL next to
/// it, then renamed over it, so a link planted at either name is never followed.
pub fn write_root_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let temporary = path.with_extension(format!("new-{}", std::process::id()));
    let _ = std::fs::remove_file(&temporary);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    chown_root(&temporary)?;
    std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o644))?;
    std::fs::rename(&temporary, path)
}

/// Remove a file root wrote; nothing there is fine.
pub fn remove_root_file(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// `launchctl <args>` by its absolute path (M2); true when it succeeded.
pub fn launchctl(args: &[&str]) -> bool {
    std::process::Command::new("/bin/launchctl")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The users with a GUI session now: each Aqua session has its own loginwindow, running as the
/// session's user.
pub fn gui_users() -> Vec<u32> {
    let output = std::process::Command::new("/bin/ps")
        .args(["-axo", "uid=,comm="])
        .stderr(std::process::Stdio::null())
        .output();
    let Ok(output) = output else { return Vec::new() };
    let mut users: Vec<u32> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let (uid, command) = line.trim().split_once(char::is_whitespace)?;
            command.trim().ends_with("/loginwindow").then(|| uid.parse().ok()).flatten()
        })
        .filter(|&uid| uid != 0)
        .collect();
    users.sort_unstable();
    users.dedup();
    users
}

/// A Unix socketpair; both ends close on exec.
pub fn socketpair(kind: libc::c_int) -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: two out fds.
    if unsafe { libc::socketpair(libc::AF_UNIX, kind, 0, fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh fds this process owns.
    let pair = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    set_cloexec(pair.0.as_raw_fd(), true)?;
    set_cloexec(pair.1.as_raw_fd(), true)?;
    Ok(pair)
}

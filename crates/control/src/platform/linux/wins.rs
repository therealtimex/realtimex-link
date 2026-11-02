//! Windows on X11 (realtimex-ai-app#3055): the window manager's managed windows (rtx-screens'
//! `_NET_CLIENT_LIST`), the active one, and focusing one with a `_NET_ACTIVE_WINDOW` client
//! message, checked within FOCUS_CHECK. GNOME on Wayland has no window list for other apps:
//! `unsupported`.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConnectionExt as _, EventMask, Window,
};
use x11rb::rust_connection::RustConnection;

use rtx_screens::x11;

use crate::platform::{refused, Listed};
use crate::winid::WindowId;

/// How long a focused window has to become the active one.
const FOCUS_CHECK: Duration = Duration::from_millis(500);
/// `_NET_ACTIVE_WINDOW`'s source indication: a pager, i.e. on the user's behalf.
const SOURCE_PAGER: u32 = 2;

struct X {
    conn: RustConnection,
    root: Window,
    active: u32,
}

impl X {
    fn connect() -> Result<X> {
        let (conn, screen) = x11rb::connect(None).context("connect to the X11 display")?;
        let root = conn.setup().roots[screen].root;
        let active = conn
            .intern_atom(false, b"_NET_ACTIVE_WINDOW")?
            .reply()?
            .atom;
        Ok(X { conn, root, active })
    }

    /// The window manager's active window.
    fn active_window(&self) -> Option<Window> {
        self.conn
            .get_property(false, self.root, self.active, AtomEnum::WINDOW, 0, 1)
            .ok()?
            .reply()
            .ok()?
            .value32()
            .and_then(|mut ids| ids.next())
            .filter(|&window| window != 0)
    }
}

fn wayland_unsupported() -> anyhow::Error {
    refused(
        "unsupported",
        "GNOME on Wayland doesn't let Link list or focus other apps' windows",
    )
}

pub fn list_windows() -> Result<Vec<Listed>> {
    if x11::wayland() {
        return Err(wayland_unsupported());
    }
    let active = X::connect().ok().and_then(|x| x.active_window());
    Ok(x11::list_windows()?
        .into_iter()
        .map(|info| Listed {
            id: WindowId::X11(info.id),
            focused: active == Some(info.id),
            info,
        })
        .collect())
}

pub fn focus_window(window: &Listed) -> Result<()> {
    if x11::wayland() {
        return Err(wayland_unsupported());
    }
    let WindowId::X11(id) = window.id else {
        return Err(refused("window-not-found", "Not a window of this computer"));
    };
    let x = X::connect()?;
    let message = ClientMessageEvent::new(
        32,
        id,
        x.active,
        [SOURCE_PAGER, x11rb::CURRENT_TIME, 0, 0, 0],
    );
    let _sent = x.conn.send_event(
        false,
        x.root,
        EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
        message,
    )?;
    x.conn.flush()?;
    let start = Instant::now();
    loop {
        if x.active_window() == Some(id) {
            return Ok(());
        }
        if start.elapsed() >= FOCUS_CHECK {
            return Err(refused(
                "focus-refused",
                "The window manager did not make the window active",
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// X11 has no lock state Link can read for every desktop; the lock screen is a window above the
/// rest, so nothing is reported locked here.
pub fn screen_locked() -> bool {
    false
}

//! This computer's displays (monitors), for choosing which one a live view shows
//! (realtimex-link#12). Listing only: CoreGraphics and AppKit (macOS), GDI, DisplayConfig and DXGI
//! (Windows), RandR (X11).

use serde::Serialize;

/// One display, as `displays()` lists it and the viewer's picker shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DisplayInfo {
    /// Opaque, and stable while the display stays attached: a CGDirectDisplayID in decimal
    /// (macOS), the GDI device name `\\.\DISPLAY2` (Windows), the RandR monitor name (X11), the
    /// connector or `virtual-n` (GNOME Wayland). It may change after a replug or a restart.
    pub id: String,
    /// What the system calls it ("DELL U2720Q"); empty when it has no name.
    pub name: String,
    /// The main display: what a request that names none shows.
    pub primary: bool,
    /// Top-left in the virtual desktop: points (macOS), physical pixels (Windows, X11), logical
    /// pixels (Wayland). For ordering and arrangement only.
    pub x: i32,
    pub y: i32,
    /// Native picture size in pixels: what a full-size screenshot of it is.
    pub width: u32,
    pub height: u32,
    /// Pixels per layout unit (2 on a Retina display, 1.5 at 150 % on Windows).
    pub scale: f64,
}

/// A request named a display that isn't attached (now). Never answered with another display:
/// whoever asked decides what to show instead. Downcast it from the `anyhow::Error`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayNotFound(pub String);

impl std::fmt::Display for DisplayNotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "display {} is not connected", self.0)
    }
}

impl std::error::Error for DisplayNotFound {}

/// Left to right, then top to bottom: a display's place in the list is its "Screen n" in the
/// picker (realtimex-ai-app#2970). Equal positions keep their order.
pub fn sort_displays(list: &mut [DisplayInfo]) {
    list.sort_by_key(|display| (display.x, display.y));
}

/// The display a request means: the one named, or the primary (else the first) for none.
pub fn find_display<'a>(list: &'a [DisplayInfo], id: Option<&str>) -> Option<&'a DisplayInfo> {
    match id {
        Some(id) => list.iter().find(|display| display.id == id),
        None => list.iter().find(|display| display.primary).or(list.first()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(id: &str, primary: bool, x: i32, y: i32) -> DisplayInfo {
        DisplayInfo {
            id: id.into(),
            name: String::new(),
            primary,
            x,
            y,
            width: 1920,
            height: 1080,
            scale: 1.0,
        }
    }

    #[test]
    fn displays_go_left_to_right_then_top_to_bottom_whichever_is_primary() {
        let mut list = vec![
            display("right", false, 1920, 0),
            display("main", true, 0, 0),
            display("below", false, 0, 1080),
            display("left", false, -2560, 200),
            display("above-left", false, -2560, -1440),
        ];
        sort_displays(&mut list);
        let ids: Vec<&str> = list.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, ["above-left", "left", "main", "below", "right"]);
    }

    #[test]
    fn no_display_named_means_the_primary_and_an_unknown_one_is_none() {
        let list = vec![display("a", false, 0, 0), display("b", true, 1920, 0)];
        assert_eq!(find_display(&list, None).unwrap().id, "b");
        assert_eq!(find_display(&list, Some("a")).unwrap().id, "a");
        assert!(find_display(&list, Some("c")).is_none());
        // No primary marked: the first.
        let list = vec![display("a", false, 0, 0), display("b", false, 1920, 0)];
        assert_eq!(find_display(&list, None).unwrap().id, "a");
        assert!(find_display(&[], None).is_none());
    }
}

//! Which display a request means (realtimex-link#12). Every capture and pointer request may name
//! a display by its id (`rtx_screens::DisplayInfo`); none means the primary one, as before. The
//! choice belongs to whoever asks (rtx-linkd keeps it per live view): nothing here remembers it.
//!
//! The display table (each display and its input rectangle) is read at most every REFRESH, and
//! again when a request names a display it doesn't have, so a moved, added or removed display
//! is seen without asking the system on every pointer move.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
pub use rtx_screens::{DisplayInfo, DisplayNotFound, DisplayUnavailable};

/// How long a display table is used before it is read again.
pub const REFRESH: Duration = Duration::from_secs(2);

/// A display's area in the coordinates the platform's pointer input takes: global points
/// (macOS), physical pixels of the virtual desktop (Windows, X11), the display's own stream
/// pixels (GNOME Wayland, where each display is a stream of its own).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    /// The rectangle a display's listing gives: its position and its size in pixels. Right
    /// where the platform's pointer takes physical pixels of the virtual desktop (Windows, X11).
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub fn of(info: &DisplayInfo) -> Self {
        Self {
            x: info.x as f64,
            y: info.y as f64,
            width: info.width as f64,
            height: info.height as f64,
        }
    }
}

/// A display and where the pointer reaches it.
#[derive(Clone, Debug, PartialEq)]
pub struct Display {
    pub info: DisplayInfo,
    pub rect: Rect,
}

/// The displays last read, and when.
#[derive(Default)]
pub struct Table {
    list: Vec<Display>,
    read_at: Option<Instant>,
}

impl Table {
    /// The display `id` names (None: the primary) from a table read less than REFRESH ago; None
    /// when the table is older or doesn't have it (then it is read again).
    pub fn cached(&self, id: Option<&str>) -> Option<&Display> {
        if self.read_at.is_none_or(|at| at.elapsed() >= REFRESH) {
            return None;
        }
        find(&self.list, id).ok()
    }

    pub fn set(&mut self, list: Vec<Display>) {
        self.list = list;
        self.read_at = Some(Instant::now());
    }

    pub fn list(&self) -> &[Display] {
        &self.list
    }

    /// Forget the table: the next request reads it again (after a session ends).
    pub fn clear(&mut self) {
        self.read_at = None;
    }
}

/// The display `id` names, or for None the primary one (else the first). An id that isn't in
/// the list is [`DisplayNotFound`]: never another display.
pub fn find<'a>(list: &'a [Display], id: Option<&str>) -> Result<&'a Display> {
    match id {
        Some(id) => list
            .iter()
            .find(|display| display.info.id == id)
            .ok_or_else(|| DisplayNotFound(id.to_string()).into()),
        None => list
            .iter()
            .find(|display| display.info.primary)
            .or(list.first())
            .ok_or_else(|| anyhow!("no display to show")),
    }
}

/// A point of a picture `space` pixels in size (a screenshot or a live view's frame of the
/// display) on the display's input rectangle: where the pointer goes. It stays on that display,
/// so a point at the picture's edge never reaches its neighbour.
pub fn map_point(rect: Rect, x: f64, y: f64, space: (u32, u32)) -> (f64, f64) {
    let axis = |value: f64, picture: u32, size: f64| {
        (value * size / picture.max(1) as f64).clamp(0.0, (size - 1.0).max(0.0))
    };
    (
        rect.x + axis(x, space.0, rect.width),
        rect.y + axis(y, space.1, rect.height),
    )
}

/// Whether `error` is a display a request named that isn't attached.
pub fn not_found(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<DisplayNotFound>())
}

/// The display `error` says is attached but can't be captured now, if that is what it is.
pub fn unavailable(error: &anyhow::Error) -> Option<&str> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<DisplayUnavailable>())
        .map(|gone| gone.0.as_str())
}

/// A display a request named that this computer can't show, either way: its viewer moves on.
pub fn lost(error: &anyhow::Error) -> bool {
    not_found(error) || unavailable(error).is_some()
}

/// The displays that can't be captured in the current state (design risk 2, Addendum 1: Link
/// lists only the displays it can capture now). One whose capture failed as unavailable stays
/// out of the list until the state changes (the screen unlocks, the login window goes).
#[derive(Default)]
pub struct Unavailable {
    /// The state they failed in, and the displays.
    state: Option<u64>,
    ids: HashSet<String>,
}

impl Unavailable {
    /// `id` failed as unavailable in `state`.
    pub fn note(&mut self, id: &str, state: u64) {
        self.forget_unless(state);
        self.state = Some(state);
        self.ids.insert(id.to_string());
    }

    /// The list without the displays that can't be captured in `state`; a new state forgets them.
    pub fn filter(&mut self, list: Vec<Display>, state: u64) -> Vec<Display> {
        self.forget_unless(state);
        list.into_iter()
            .filter(|display| !self.ids.contains(&display.info.id))
            .collect()
    }

    fn forget_unless(&mut self, state: u64) {
        if self.state != Some(state) {
            self.state = None;
            self.ids.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(id: &str, primary: bool, rect: Rect) -> Display {
        Display {
            info: DisplayInfo {
                id: id.into(),
                name: String::new(),
                primary,
                x: rect.x as i32,
                y: rect.y as i32,
                width: rect.width as u32,
                height: rect.height as u32,
                scale: 1.0,
            },
            rect,
        }
    }

    fn rect(x: f64, y: f64, width: f64, height: f64) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    /// Regression against 0.4.1: without a display, a point is the primary display's, scaled from
    /// the picture as before (`x * width / pictureWidth`).
    #[test]
    fn no_display_is_the_primary_as_before() {
        let list = vec![
            display("left", false, rect(-1920.0, 0.0, 1920.0, 1080.0)),
            display("main", true, rect(0.0, 0.0, 1512.0, 982.0)),
        ];
        let main = find(&list, None).unwrap();
        assert_eq!(main.info.id, "main");
        // A 1920-wide screenshot of a 1512 x 982 point display (macOS).
        assert_eq!(
            map_point(main.rect, 960.0, 500.0, (1920, 1247)),
            (960.0 * 1512.0 / 1920.0, 500.0 * 982.0 / 1247.0)
        );
        assert_eq!(map_point(main.rect, 0.0, 0.0, (1920, 1247)), (0.0, 0.0));
    }

    #[test]
    fn a_named_display_maps_onto_its_own_rectangle() {
        // Windows: a 150 % monitor left of and above the primary (negative origin, physical px).
        let left = rect(-2560.0, -300.0, 2560.0, 1440.0);
        assert_eq!(map_point(left, 640.0, 360.0, (1280, 720)), (-1280.0, 420.0));
        // macOS: a Retina display to the right, in points; its picture is in pixels.
        let right = rect(1512.0, 0.0, 1920.0, 1080.0);
        assert_eq!(map_point(right, 1920.0, 1080.0, (3840, 2160)), (2472.0, 540.0));
        // X11: a monitor below the primary.
        let below = rect(0.0, 1080.0, 1280.0, 1024.0);
        assert_eq!(map_point(below, 10.0, 20.0, (1280, 1024)), (10.0, 1100.0));
    }

    #[test]
    fn a_point_at_the_picture_edge_stays_on_its_display() {
        let left = rect(-1920.0, 0.0, 1920.0, 1080.0);
        let (x, y) = map_point(left, 1280.0, 720.0, (1280, 720));
        assert_eq!((x, y), (-1.0, 1079.0));
        assert_eq!(map_point(left, 5000.0, 0.0, (1280, 720)).0, -1.0);
    }

    #[test]
    fn an_unknown_display_is_not_found_never_another_one() {
        let list = vec![display("a", true, rect(0.0, 0.0, 10.0, 10.0))];
        let error = find(&list, Some("b")).unwrap_err();
        assert!(not_found(&error));
        assert_eq!(error.downcast_ref::<DisplayNotFound>(), Some(&DisplayNotFound("b".into())));
        assert!(not_found(&error.context("screenshot")));
        assert!(!not_found(&anyhow!("other")));
        // No primary marked: the first; an empty list has none.
        let list = vec![
            display("a", false, rect(0.0, 0.0, 10.0, 10.0)),
            display("b", false, rect(10.0, 0.0, 10.0, 10.0)),
        ];
        assert_eq!(find(&list, None).unwrap().info.id, "a");
        assert!(find(&[], None).is_err());
    }

    /// Review of !15, finding 3: a display whose capture failed as unavailable (injected here)
    /// leaves the list while the state lasts and comes back with the next state.
    #[test]
    fn a_display_that_cannot_be_captured_now_is_not_listed_until_the_state_changes() {
        let list = || {
            vec![
                display("main", true, rect(0.0, 0.0, 10.0, 10.0)),
                display("2", false, rect(10.0, 0.0, 10.0, 10.0)),
            ]
        };
        let injected = anyhow::Error::from(DisplayUnavailable("2".into())).context("frame");
        assert_eq!(unavailable(&injected), Some("2"));
        assert!(lost(&injected) && !not_found(&injected));
        let (locked, unlocked) = (1, 0);
        let mut gone = Unavailable::default();
        gone.note(unavailable(&injected).unwrap(), locked);
        let ids = |list: Vec<Display>| -> Vec<String> { list.into_iter().map(|d| d.info.id).collect() };
        assert_eq!(ids(gone.filter(list(), locked)), ["main"]);
        assert_eq!(ids(gone.filter(list(), locked)), ["main"]);
        assert_eq!(ids(gone.filter(list(), unlocked)), ["main", "2"]);
        // Locked again: listed until its capture fails again.
        assert_eq!(ids(gone.filter(list(), locked)), ["main", "2"]);
    }

    #[test]
    fn the_table_is_read_again_when_old_or_missing_the_display() {
        let mut table = Table::default();
        assert!(table.cached(None).is_none());
        table.set(vec![display("a", true, rect(0.0, 0.0, 10.0, 10.0))]);
        assert_eq!(table.cached(None).unwrap().info.id, "a");
        assert!(table.cached(Some("b")).is_none());
        table.clear();
        assert!(table.cached(Some("a")).is_none());
        table.read_at = Some(Instant::now() - REFRESH);
        assert!(table.cached(Some("a")).is_none());
    }

    /// The wire form agreed with realtimex-ai-app#2970: camelCase, native pixels.
    #[test]
    fn a_display_goes_out_as_the_app_reads_it() {
        let info = DisplayInfo {
            id: "69733382".into(),
            name: "DELL U2720Q".into(),
            primary: false,
            x: 1512,
            y: 0,
            width: 3840,
            height: 2160,
            scale: 2.0,
        };
        assert_eq!(
            serde_json::to_value(&info).unwrap(),
            serde_json::json!({ "id": "69733382", "name": "DELL U2720Q", "primary": false,
                "x": 1512, "y": 0, "width": 3840, "height": 2160, "scale": 2.0 })
        );
    }
}

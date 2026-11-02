//! `POST /remote-control/wait` (realtimex-ai-app#3055): a long poll that answers once the screen
//! changes, from the 64×36 grayscale thumbnail rtx-control already makes with every screenshot.
//!
//! - **Frame tokens.** Every `/screenshot` keeps its thumbnail under a token (the
//!   `x-screen-frame` header), per client and display: the last [`KEPT_FRAMES`], for
//!   [`FRAME_TTL`]. A wait that names one (`since`) compares with that picture, so a change that
//!   began between a click and the wait still counts. An unknown or expired token falls back to
//!   the picture at the call's start.
//! - **Changed** means at least [`MIN_CHANGED_CELLS`] cells inside `region` (the whole display by
//!   default) differ by [`CELL_DELTA`] or more in luma. `area` is their bounding box, in the
//!   space of the client's latest screenshot of that display.
//! - **Settle.** After a change, the answer waits until the screen holds still for `settleMs`
//!   (within the timeout).
//! - **Timing out is not an error**: `changed:false`.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::error::{ApiError, ApiResult};

/// The thumbnail rtx-control makes (rtx_screens::image::THUMB_WIDTH × THUMB_HEIGHT).
pub const COLUMNS: usize = 64;
pub const ROWS: usize = 36;
/// A cell has changed when its luma moved by this much (of 255).
pub const CELL_DELTA: u8 = 12;
/// The screen has changed when this many cells have.
pub const MIN_CHANGED_CELLS: usize = 2;
/// How often a wait looks at the screen.
pub const POLL: Duration = Duration::from_millis(250);
/// The screenshot a poll takes: small and rough, only its thumbnail is used.
pub const POLL_WIDTH: u64 = 320;
pub const POLL_QUALITY: u64 = 30;
/// Frame tokens kept per client and display, and for how long.
pub const KEPT_FRAMES: usize = 8;
pub const FRAME_TTL: Duration = Duration::from_secs(10 * 60);
/// Waits one client may have open at once.
pub const MAX_WAITS_PER_CLIENT: usize = 2;
const TIMEOUT_MS: std::ops::RangeInclusive<u64> = 100..=30_000;
const DEFAULT_TIMEOUT_MS: u64 = 10_000;
const SETTLE_MS: std::ops::RangeInclusive<u64> = 0..=2_000;
const DEFAULT_SETTLE_MS: u64 = 300;

/// A rectangle in a screenshot's pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Thumbnail cells: columns `left..right`, rows `top..bottom`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cells {
    pub left: usize,
    pub top: usize,
    pub right: usize,
    pub bottom: usize,
}

impl Cells {
    pub const ALL: Cells = Cells {
        left: 0,
        top: 0,
        right: COLUMNS,
        bottom: ROWS,
    };

    fn union(self, other: Cells) -> Cells {
        Cells {
            left: self.left.min(other.left),
            top: self.top.min(other.top),
            right: self.right.max(other.right),
            bottom: self.bottom.max(other.bottom),
        }
    }
}

/// A wait's request body.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub display: Option<String>,
    pub since: Option<String>,
    pub timeout: Duration,
    pub settle: Duration,
    pub region: Option<Rect>,
}

/// A whole number of milliseconds in `range`, or its default when absent.
fn millis(
    body: &Value,
    name: &str,
    range: std::ops::RangeInclusive<u64>,
    default: u64,
) -> ApiResult<Duration> {
    match body.get(name) {
        None | Some(Value::Null) => Ok(Duration::from_millis(default)),
        Some(value) => value
            .as_u64()
            .filter(|ms| range.contains(ms))
            .map(Duration::from_millis)
            .ok_or_else(|| {
                ApiError::bad_request(format!(
                    "{name} must be a whole number of milliseconds from {} to {}",
                    range.start(),
                    range.end()
                ))
            }),
    }
}

impl Request {
    pub fn parse(body: &Value) -> ApiResult<Request> {
        let text = |name: &str| -> ApiResult<Option<String>> {
            match body.get(name) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(text)) if text.is_empty() => Ok(None),
                Some(Value::String(text)) if text.len() <= 256 => Ok(Some(text.clone())),
                Some(_) => Err(ApiError::bad_request(format!("{name} must be a string"))),
            }
        };
        let region = match body.get("region") {
            None | Some(Value::Null) => None,
            Some(region) => {
                let number = |name: &str| {
                    region
                        .get(name)
                        .and_then(Value::as_f64)
                        .filter(|n| n.is_finite() && *n >= 0.0)
                };
                match (
                    number("x"),
                    number("y"),
                    number("width"),
                    number("height"),
                ) {
                    (Some(x), Some(y), Some(width), Some(height)) if width > 0.0 && height > 0.0 => {
                        Some(Rect {
                            x,
                            y,
                            width,
                            height,
                        })
                    }
                    _ => {
                        return Err(ApiError::bad_request(
                            "region must be {x, y, width, height}: numbers, not negative, with a width and height",
                        ))
                    }
                }
            }
        };
        Ok(Request {
            display: text("display")?,
            since: text("since")?,
            timeout: millis(body, "timeoutMs", TIMEOUT_MS, DEFAULT_TIMEOUT_MS)?,
            settle: millis(body, "settleMs", SETTLE_MS, DEFAULT_SETTLE_MS)?,
            region,
        })
    }
}

/// The cells a region of a `space`-sized picture touches (at least one).
pub fn region_cells(region: Rect, space: (u64, u64)) -> Cells {
    let (width, height) = (space.0.max(1) as f64, space.1.max(1) as f64);
    let span = |start: f64, size: f64, total: f64, cells: usize| {
        let from = ((start / total) * cells as f64).floor().max(0.0) as usize;
        let to = (((start + size) / total) * cells as f64).ceil() as usize;
        let from = from.min(cells - 1);
        (from, to.clamp(from + 1, cells))
    };
    let (left, right) = span(region.x, region.width, width, COLUMNS);
    let (top, bottom) = span(region.y, region.height, height, ROWS);
    Cells {
        left,
        top,
        right,
        bottom,
    }
}

/// Cells back in a `space`-sized picture's pixels, as the answer's `area`.
pub fn area_of(cells: Cells, space: (u64, u64)) -> Value {
    let x = |column: usize| (column as u64 * space.0) / COLUMNS as u64;
    let y = |row: usize| (row as u64 * space.1) / ROWS as u64;
    json!({
        "x": x(cells.left),
        "y": y(cells.top),
        "width": x(cells.right) - x(cells.left),
        "height": y(cells.bottom) - y(cells.top),
    })
}

/// The bounding box of the cells within `region` that changed from `a` to `b`, when at least
/// MIN_CHANGED_CELLS did. Thumbnails of different sizes count as changed everywhere.
pub fn changed(a: &[u8], b: &[u8], region: Cells) -> Option<Cells> {
    if a.len() != COLUMNS * ROWS || b.len() != a.len() {
        return Some(region);
    }
    let mut count = 0;
    let mut found: Option<Cells> = None;
    for row in region.top..region.bottom.min(ROWS) {
        for column in region.left..region.right.min(COLUMNS) {
            let index = row * COLUMNS + column;
            if a[index].abs_diff(b[index]) >= CELL_DELTA {
                count += 1;
                let cell = Cells {
                    left: column,
                    top: row,
                    right: column + 1,
                    bottom: row + 1,
                };
                found = Some(found.map_or(cell, |box_| box_.union(cell)));
            }
        }
    }
    found.filter(|_| count >= MIN_CHANGED_CELLS)
}

/// A screenshot's thumbnail as rtx-control sends it (`cells`: base64 of 64×36 bytes).
pub fn decode_cells(value: &Value) -> Option<Vec<u8>> {
    value
        .as_str()
        .and_then(crate::util::unbase64)
        .filter(|cells| cells.len() == COLUMNS * ROWS)
}

/// A kept screenshot: its thumbnail and the size of the picture the client got.
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub cells: Vec<u8>,
    pub space: (u64, u64),
}

#[derive(Default)]
struct Kept {
    next: u64,
    by: HashMap<(String, String), VecDeque<(String, Instant, Frame)>>,
}

/// The frame tokens of every client and display.
pub struct Frames {
    kept: Mutex<Kept>,
    /// Makes this process's tokens its own: one from before a restart is unknown, not another
    /// picture.
    nonce: String,
}

impl Default for Frames {
    fn default() -> Self {
        let bytes: [u8; 4] = crate::util::random_bytes();
        Self {
            kept: Mutex::default(),
            nonce: bytes.iter().map(|b| format!("{b:02x}")).collect(),
        }
    }
}

impl Frames {
    fn kept(&self) -> std::sync::MutexGuard<'_, Kept> {
        self.kept.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Keep `frame` of `client`'s view of `display`; its token.
    pub fn keep(&self, client: &str, display: &str, frame: Frame) -> String {
        let now = Instant::now();
        let mut kept = self.kept();
        kept.next += 1;
        let token = format!("{}-{:x}", self.nonce, kept.next);
        kept.by.retain(|_, list| {
            list.retain(|(_, at, _)| now.duration_since(*at) < FRAME_TTL);
            !list.is_empty()
        });
        let list = kept
            .by
            .entry((client.to_string(), display.to_string()))
            .or_default();
        list.push_back((token.clone(), now, frame));
        while list.len() > KEPT_FRAMES {
            list.pop_front();
        }
        token
    }

    fn list(&self, client: &str, display: &str) -> Vec<(String, Instant, Frame)> {
        self.kept()
            .by
            .get(&(client.to_string(), display.to_string()))
            .map(|list| list.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// The frame `token` names, while it is kept.
    pub fn get(&self, client: &str, display: &str, token: &str) -> Option<Frame> {
        self.list(client, display)
            .into_iter()
            .find(|(kept, at, _)| kept == token && at.elapsed() < FRAME_TTL)
            .map(|(_, _, frame)| frame)
    }

    /// The newest kept frame.
    pub fn latest(&self, client: &str, display: &str) -> Option<Frame> {
        self.list(client, display)
            .into_iter()
            .rev()
            .find(|(_, at, _)| at.elapsed() < FRAME_TTL)
            .map(|(_, _, frame)| frame)
    }

    /// Forget a client's frames (it was removed).
    #[allow(dead_code)]
    pub fn forget(&self, client: &str) {
        self.kept().by.retain(|(owner, _), _| owner != client);
    }
}

/// The waits each client has open.
#[derive(Default)]
pub struct Waits {
    open: Mutex<HashMap<String, usize>>,
}

/// One open wait; closing it (drop) frees its place.
pub struct Slot<'a> {
    waits: &'a Waits,
    client: String,
}

impl Waits {
    fn open(&self) -> std::sync::MutexGuard<'_, HashMap<String, usize>> {
        self.open.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// A place for one more wait of `client`, or `busy` when it has MAX_WAITS_PER_CLIENT.
    pub fn enter(&self, client: &str) -> ApiResult<Slot<'_>> {
        let mut open = self.open();
        let count = open.entry(client.to_string()).or_default();
        if *count >= MAX_WAITS_PER_CLIENT {
            return Err(ApiError::coded(
                503,
                "busy",
                format!("At most {MAX_WAITS_PER_CLIENT} waits at once"),
            ));
        }
        *count += 1;
        Ok(Slot {
            waits: self,
            client: client.to_string(),
        })
    }
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        let mut open = self.waits.open();
        if let Some(count) = open.get_mut(&self.client) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                open.remove(&self.client);
            }
        }
    }
}

/// Where a wait's pictures come from: rtx-control's screenshots, or a test's.
pub trait Source {
    /// The screen's thumbnail now.
    fn next(&mut self) -> impl Future<Output = ApiResult<Vec<u8>>> + Send;
}

/// How a wait ended.
#[derive(Debug, PartialEq)]
pub struct Outcome {
    pub changed: bool,
    pub elapsed: Duration,
    /// The last picture looked at.
    pub cells: Vec<u8>,
    /// What changed (from the baseline), when it did.
    pub area: Option<Cells>,
}

/// Wait for the screen to change from `baseline` (None: `first`, the picture at the start)
/// within `region`, then to hold still for `settle`, looking every `interval`, for at most
/// `timeout`.
pub async fn watch<S: Source>(
    source: &mut S,
    baseline: Option<Vec<u8>>,
    first: Vec<u8>,
    region: Cells,
    timeout: Duration,
    settle: Duration,
    interval: Duration,
) -> ApiResult<Outcome> {
    let start = Instant::now();
    let deadline = start + timeout;
    let baseline = baseline.unwrap_or_else(|| first.clone());
    let mut last = first;
    let mut seen = changed(&baseline, &last, region);
    // Since when the screen has held still (after a change).
    let mut still = Instant::now();
    loop {
        let now = Instant::now();
        let done = match seen {
            Some(_) => settle.is_zero() || now.duration_since(still) >= settle || now >= deadline,
            None => now >= deadline,
        };
        if done {
            let area = seen.map(|first_seen| {
                changed(&baseline, &last, region).map_or(first_seen, |now| now.union(first_seen))
            });
            return Ok(Outcome {
                changed: area.is_some(),
                elapsed: start.elapsed(),
                cells: last,
                area,
            });
        }
        let pause = interval.min(deadline.saturating_duration_since(now));
        tokio::time::sleep(pause).await;
        let next = source.next().await?;
        match seen {
            None => {
                seen = changed(&baseline, &next, region);
                still = Instant::now();
            }
            Some(_) => {
                if changed(&last, &next, region).is_some() {
                    still = Instant::now();
                }
            }
        }
        last = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(level: u8) -> Vec<u8> {
        vec![level; COLUMNS * ROWS]
    }

    /// `base` with the cells of `cells` set to `level`.
    fn with(base: &[u8], cells: Cells, level: u8) -> Vec<u8> {
        let mut out = base.to_vec();
        for row in cells.top..cells.bottom {
            for column in cells.left..cells.right {
                out[row * COLUMNS + column] = level;
            }
        }
        out
    }

    /// Pictures in order; the last one stays.
    struct Script(VecDeque<Vec<u8>>, usize);

    impl Source for Script {
        fn next(&mut self) -> impl Future<Output = ApiResult<Vec<u8>>> + Send {
            self.1 += 1;
            let next = if self.0.len() > 1 {
                self.0.pop_front().unwrap()
            } else {
                self.0[0].clone()
            };
            async move { Ok(next) }
        }
    }

    fn script(frames: Vec<Vec<u8>>) -> Script {
        Script(frames.into(), 0)
    }

    const TICK: Duration = Duration::from_millis(5);
    const BOX: Cells = Cells {
        left: 10,
        top: 5,
        right: 14,
        bottom: 8,
    };

    #[test]
    fn two_cells_past_the_threshold_are_a_change() {
        let base = screen(100);
        let one = with(
            &base,
            Cells {
                left: 3,
                top: 3,
                right: 4,
                bottom: 4,
            },
            200,
        );
        assert_eq!(changed(&base, &one, Cells::ALL), None, "one cell is noise");
        let faint = with(&base, BOX, 100 + CELL_DELTA - 1);
        assert_eq!(
            changed(&base, &faint, Cells::ALL),
            None,
            "below the threshold"
        );
        let real = with(&base, BOX, 100 + CELL_DELTA);
        assert_eq!(changed(&base, &real, Cells::ALL), Some(BOX));
        assert_eq!(changed(&base, &[1, 2], Cells::ALL), Some(Cells::ALL));
    }

    #[test]
    fn a_region_maps_to_cells_and_back() {
        // A 1280×720 screenshot: a cell is 20×20.
        let space = (1280, 720);
        let cells = region_cells(
            Rect {
                x: 200.0,
                y: 100.0,
                width: 80.0,
                height: 60.0,
            },
            space,
        );
        assert_eq!(cells, BOX);
        assert_eq!(
            area_of(BOX, space),
            json!({ "x": 200, "y": 100, "width": 80, "height": 60 })
        );
        // Beyond the picture: clamped, still one cell at least.
        let edge = region_cells(
            Rect {
                x: 5000.0,
                y: 5000.0,
                width: 10.0,
                height: 10.0,
            },
            space,
        );
        assert_eq!((edge.right - edge.left, edge.bottom - edge.top), (1, 1));
    }

    #[tokio::test]
    async fn since_catches_a_change_that_came_before_the_wait() {
        let before = screen(50);
        let after = with(&before, BOX, 200);
        // The screenshot's picture (since) differs from the screen at the start: changed at once.
        let mut source = script(vec![after.clone()]);
        let outcome = watch(
            &mut source,
            Some(before.clone()),
            after.clone(),
            Cells::ALL,
            Duration::from_secs(5),
            Duration::ZERO,
            TICK,
        )
        .await
        .unwrap();
        assert!(outcome.changed);
        assert_eq!(outcome.area, Some(BOX));
        assert_eq!(source.1, 0, "no further picture needed");
        assert!(outcome.elapsed < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn without_since_the_start_is_the_baseline() {
        // A token that isn't known (a miss) falls back to the start: the same screen, then a change.
        let start = screen(50);
        let later = with(&start, BOX, 200);
        let mut source = script(vec![start.clone(), later.clone()]);
        let outcome = watch(
            &mut source,
            None,
            start.clone(),
            Cells::ALL,
            Duration::from_secs(5),
            Duration::ZERO,
            TICK,
        )
        .await
        .unwrap();
        assert!(outcome.changed);
        assert_eq!(outcome.area, Some(BOX));
        assert_eq!(outcome.cells, later);
    }

    #[tokio::test]
    async fn a_change_outside_the_region_is_not_one() {
        let start = screen(50);
        let elsewhere = with(&start, BOX, 200);
        let region = Cells {
            left: 40,
            top: 20,
            right: 64,
            bottom: 36,
        };
        let mut source = script(vec![elsewhere.clone()]);
        let outcome = watch(
            &mut source,
            None,
            start.clone(),
            region,
            Duration::from_millis(60),
            Duration::ZERO,
            TICK,
        )
        .await
        .unwrap();
        assert!(!outcome.changed);
        // Inside it, it is.
        let inside = with(
            &start,
            Cells {
                left: 50,
                top: 30,
                right: 52,
                bottom: 31,
            },
            200,
        );
        let mut source = script(vec![inside]);
        let outcome = watch(
            &mut source,
            None,
            start,
            region,
            Duration::from_secs(5),
            Duration::ZERO,
            TICK,
        )
        .await
        .unwrap();
        assert_eq!(
            outcome.area,
            Some(Cells {
                left: 50,
                top: 30,
                right: 52,
                bottom: 31
            })
        );
    }

    #[tokio::test]
    async fn timing_out_is_changed_false() {
        let start = screen(50);
        let mut source = script(vec![start.clone()]);
        let outcome = watch(
            &mut source,
            None,
            start.clone(),
            Cells::ALL,
            Duration::from_millis(100),
            Duration::from_millis(300),
            TICK,
        )
        .await
        .unwrap();
        assert!(!outcome.changed);
        assert_eq!(outcome.area, None);
        assert!(outcome.elapsed >= Duration::from_millis(100));
        assert!(outcome.elapsed < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn settle_waits_until_the_screen_holds_still() {
        let start = screen(50);
        // It changes, keeps changing for a few looks (an animation), then holds.
        let mut frames = vec![];
        for step in 0..6u8 {
            frames.push(with(&start, BOX, 100 + step * 20));
        }
        let mut source = script(frames);
        let settle = Duration::from_millis(40);
        let outcome = watch(
            &mut source,
            None,
            start.clone(),
            Cells::ALL,
            Duration::from_secs(5),
            settle,
            TICK,
        )
        .await
        .unwrap();
        assert!(outcome.changed);
        assert_eq!(
            outcome.cells,
            with(&start, BOX, 200),
            "the picture it settled on"
        );
        // Every changing picture was looked at, then it held still for the settle time.
        assert!(source.1 > 6, "{} looks", source.1);
        assert!(outcome.elapsed >= settle);
        // A settle that outlasts the timeout still answers changed, at the timeout.
        let mut source = script(vec![with(&start, BOX, 200), with(&start, BOX, 120)]);
        let outcome = watch(
            &mut source,
            None,
            start,
            Cells::ALL,
            Duration::from_millis(50),
            Duration::from_millis(2000),
            TICK,
        )
        .await
        .unwrap();
        assert!(outcome.changed);
        assert!(outcome.elapsed < Duration::from_millis(500));
    }

    #[test]
    fn frame_tokens_hit_miss_and_expire_by_count() {
        let frames = Frames::default();
        let first = frames.keep(
            "a",
            "1",
            Frame {
                cells: screen(1),
                space: (1280, 720),
            },
        );
        assert_eq!(frames.get("a", "1", &first).unwrap().cells, screen(1));
        // Another client's or display's token is not this one's.
        assert!(frames.get("b", "1", &first).is_none());
        assert!(frames.get("a", "2", &first).is_none());
        assert!(frames.get("a", "1", "nope").is_none());
        for level in 2..=KEPT_FRAMES as u8 + 1 {
            frames.keep(
                "a",
                "1",
                Frame {
                    cells: screen(level),
                    space: (1280, 720),
                },
            );
        }
        assert!(
            frames.get("a", "1", &first).is_none(),
            "only the last 8 are kept"
        );
        assert_eq!(
            frames.latest("a", "1").unwrap().cells,
            screen(KEPT_FRAMES as u8 + 1)
        );
        frames.forget("a");
        assert!(frames.latest("a", "1").is_none());
    }

    #[test]
    fn a_client_has_two_waits_at_most() {
        let waits = Waits::default();
        let one = waits.enter("a").unwrap();
        let _two = waits.enter("a").unwrap();
        let third = waits.enter("a").err().unwrap();
        assert_eq!(
            (third.status, third.code_or_status()),
            (503, "busy".to_string())
        );
        assert!(waits.enter("b").is_ok(), "per client");
        drop(one);
        assert!(waits.enter("a").is_ok());
    }

    #[test]
    fn requests_are_checked() {
        let request = Request::parse(&json!({})).unwrap();
        assert_eq!(request.timeout, Duration::from_millis(DEFAULT_TIMEOUT_MS));
        assert_eq!(request.settle, Duration::from_millis(DEFAULT_SETTLE_MS));
        assert_eq!(
            (request.display, request.since, request.region),
            (None, None, None)
        );
        let request = Request::parse(&json!({
            "display": "2", "since": "t", "timeoutMs": 30000, "settleMs": 0,
            "region": { "x": 0, "y": 0, "width": 10, "height": 10 }
        }))
        .unwrap();
        assert_eq!(request.display.as_deref(), Some("2"));
        assert_eq!(request.settle, Duration::ZERO);
        for bad in [
            json!({ "timeoutMs": 99 }),
            json!({ "timeoutMs": 30001 }),
            json!({ "settleMs": 2001 }),
            json!({ "timeoutMs": "fast" }),
            json!({ "region": { "x": 0, "y": 0, "width": 0, "height": 5 } }),
            json!({ "region": { "x": -1, "y": 0, "width": 5, "height": 5 } }),
            json!({ "display": 2 }),
        ] {
            let error = Request::parse(&bad).unwrap_err();
            assert_eq!(error.code_or_status(), "bad-request", "{bad}");
        }
    }
}

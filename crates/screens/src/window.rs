//! A window that can be screenshotted, as the recorder and Remote Control list them.

use serde::{Deserialize, Serialize};

/// A window that can be screenshotted (`targets windows`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowInfo {
    pub id: u32,
    pub pid: i32,
    pub app: String,
    pub app_name: String,
    pub title: String,
    pub on_screen: bool,
    /// x, y, width, height in points.
    pub frame: [f64; 4],
    /// Above normal windows (macOS: a window level between normal and the menu bar), e.g.
    /// Meet's picture-in-picture. Not screenshotted.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub floating: bool,
    /// The window server layer (`targets windows --video`; 0 is normal windows).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<i64>,
    /// Listed but not recordable as video, and why (`targets windows --video`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unselectable_reason: Option<String>,
}

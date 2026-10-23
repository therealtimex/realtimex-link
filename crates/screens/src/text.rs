//! Text recognized on-device in a screenshot (macOS Vision, Windows OCR).

/// One line of recognized text; the box is normalized, origin top-left: [x, y, width, height].
#[derive(Debug, Clone, PartialEq)]
pub struct TextLine {
    pub text: String,
    pub confidence: f32,
    pub bbox: [f32; 4],
}

impl TextLine {
    pub fn new(text: &str, confidence: f32, bbox: [f32; 4]) -> Self {
        Self {
            text: text.into(),
            confidence,
            bbox,
        }
    }
}

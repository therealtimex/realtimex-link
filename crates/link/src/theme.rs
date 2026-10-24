//! RealTimeX's look in the window: its typeface (Plus Jakarta Sans) and its theme tokens
//! (realtimex-ai-app frontend/src/index.css, `--theme-*`), for both the dark and the light theme.
//! The window follows RealTimeX's theme when RealTimeX set one (app.rs), else the system's.

use std::sync::Arc;

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, FontTweak,
    RichText, Stroke, TextStyle, Theme,
};

/// Plus Jakarta Sans (OFL, fonts/OFL.txt), RealTimeX's typeface: one variable font, regular by
/// default and semibold through its weight axis (semibold()).
const FONT: &[u8] = include_bytes!("../fonts/PlusJakartaSans.ttf");
const SEMIBOLD: f32 = 600.0;

/// Russian's letters, which Plus Jakarta Sans lacks: Noto Sans (OFL, fonts/OFL-NotoSans.txt) cut
/// to U+0400–045F and №, its weight axis kept so semibold() works here too (scripts: fonttools
/// varLib.instancer wdth=100, then pyftsubset). Punctuation and digits stay Plus Jakarta Sans'.
const CYRILLIC: &[u8] = include_bytes!("../fonts/NotoSansCyrillic.ttf");

/// The Cyrillic subset, matched to Plus Jakarta Sans. Its x-height is the same (536/1000 em in
/// both; caps 714 vs 745), so it keeps its size. egui sets each font on its own ascent (1.069 em
/// vs 1.038), which puts Noto's baseline 0.031 em lower: lifted by that, mixed lines such as
/// "RealTimeX Link" in Russian text share one baseline.
fn cyrillic_font() -> FontData {
    FontData::from_static(CYRILLIC).tweak(FontTweak {
        y_offset_factor: -0.031,
        ..Default::default()
    })
}

/// One theme's tokens.
struct Tokens {
    /// --theme-bg-primary: the window.
    bg_primary: Color32,
    /// --theme-bg-secondary: cards and plain buttons.
    bg_secondary: Color32,
    /// --theme-settings-input-bg: text fields.
    input_bg: Color32,
    /// --theme-text-primary and --theme-text-secondary.
    text: Color32,
    text_secondary: Color32,
    /// --theme-modal-border: card, button and field borders.
    border: Color32,
    /// --theme-sidebar-item-hover: a plain button under the pointer.
    hover: Color32,
    /// The links' colour, also the keyboard focus ring (RealTimeX's focus-visible outline).
    link: Color32,
    /// A selected tab or option: --theme-status-info-surface with its -border.
    selected_fill: Color32,
    selected_border: Color32,
}

const DARK: Tokens = Tokens {
    bg_primary: Color32::from_rgb(0x19, 0x1d, 0x1d),
    bg_secondary: Color32::from_rgb(0x1b, 0x1b, 0x1e),
    input_bg: Color32::from_rgb(0x1e, 0x22, 0x25),
    text: Color32::WHITE,
    // rgba(255, 255, 255, 0.6) on --theme-bg-primary.
    text_secondary: Color32::from_rgb(0xa3, 0xa5, 0xa5),
    border: Color32::from_rgb(0x3f, 0x3f, 0x42),
    hover: Color32::from_rgb(0x3f, 0x3f, 0x42),
    // --theme-button-primary.
    link: PRIMARY,
    selected_fill: Color32::from_rgb(0x1b, 0x2d, 0x35),
    selected_border: Color32::from_rgb(0x4a, 0x7d, 0x95),
};

const LIGHT: Tokens = Tokens {
    bg_primary: Color32::from_rgb(0xfc, 0xfc, 0xfc),
    bg_secondary: Color32::WHITE,
    input_bg: Color32::from_rgb(0xf9, 0xf9, 0xf9),
    text: Color32::from_rgb(0x0e, 0x0f, 0x0f),
    // --theme-home-text-secondary: --theme-text-secondary (#7a7d7e) is below 4.5:1 for captions.
    text_secondary: Color32::from_rgb(0x6f, 0x6f, 0x71),
    border: Color32::from_rgb(0xd3, 0xd3, 0xd3),
    hover: Color32::from_rgb(0xe0, 0xef, 0xf4),
    // --theme-status-info-fg: --theme-button-primary is too faint for text on white.
    link: Color32::from_rgb(0x07, 0x6e, 0x9e),
    selected_fill: Color32::from_rgb(0xd9, 0xf0, 0xfb),
    selected_border: Color32::from_rgb(0x3b, 0x8e, 0xb5),
};

fn tokens(ui: &egui::Ui) -> &'static Tokens {
    if ui.visuals().dark_mode {
        &DARK
    } else {
        &LIGHT
    }
}

/// The keyboard focus ring (and a pressed control's border): 2 px, at least 3:1 against what is
/// next to it in both themes.
const FOCUS_WIDTH: f32 = 2.0;

/// --theme-button-primary, with --theme-button-primary-text on it.
pub const PRIMARY: Color32 = Color32::from_rgb(0x36, 0xbf, 0xfa);
/// --theme-button-primary-hover: rgba(54, 191, 250, 0.9).
const PRIMARY_HOVER: Color32 = Color32::from_rgba_premultiplied(49, 172, 225, 230);

/// Buttons and fields (rounded-md), and cards and alerts (rounded-lg).
pub const CONTROL_RADIUS: u8 = 6;
pub const CARD_RADIUS: u8 = 8;
/// Buttons are h-10 in RealTimeX; a little lower here, where they sit in rows inside cards.
const CONTROL_HEIGHT: f32 = 32.0;
pub const PRIMARY_HEIGHT: f32 = 36.0;

/// Plus Jakarta Sans, then the Cyrillic subset, ahead of egui's own fonts (its icons and emoji).
fn fonts() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        "plus-jakarta-sans".into(),
        Arc::new(FontData::from_static(FONT)),
    );
    fonts
        .font_data
        .insert("noto-sans-cyrillic".into(), Arc::new(cyrillic_font()));
    let proportional = fonts.families.entry(FontFamily::Proportional).or_default();
    proportional.insert(0, "plus-jakarta-sans".into());
    proportional.insert(1, "noto-sans-cyrillic".into());
    fonts
}

pub fn apply(ctx: &egui::Context) {
    ctx.set_fonts(fonts());
    for (theme, tokens) in [(Theme::Dark, &DARK), (Theme::Light, &LIGHT)] {
        ctx.style_mut_of(theme, |style| {
            style.text_styles = [
                (TextStyle::Small, FontId::proportional(12.0)),
                (TextStyle::Body, FontId::proportional(14.0)),
                (TextStyle::Button, FontId::proportional(14.0)),
                (TextStyle::Heading, FontId::proportional(16.0)),
                (TextStyle::Monospace, FontId::monospace(13.0)),
            ]
            .into();
            style.spacing.interact_size.y = CONTROL_HEIGHT;
            style.spacing.button_padding = egui::vec2(12.0, 4.0);
            visuals(&mut style.visuals, tokens);
        });
    }
}

fn visuals(visuals: &mut egui::Visuals, tokens: &Tokens) {
    visuals.panel_fill = tokens.bg_primary;
    visuals.window_fill = tokens.bg_secondary;
    visuals.window_stroke = Stroke::new(1.0, tokens.border);
    visuals.window_corner_radius = CornerRadius::same(CARD_RADIUS);
    visuals.menu_corner_radius = CornerRadius::same(CONTROL_RADIUS);
    visuals.extreme_bg_color = tokens.input_bg;
    visuals.text_edit_bg_color = Some(tokens.input_bg);
    visuals.faint_bg_color = tokens.bg_secondary;
    visuals.hyperlink_color = tokens.link;
    visuals.weak_text_color = Some(tokens.text_secondary);
    // A selected tab or option is tinted (selectable_value() adds its border and semibold
    // text); a focused text field is outlined with selection.stroke, the focus ring.
    visuals.selection.bg_fill = tokens.selected_fill;
    visuals.selection.stroke = Stroke::new(FOCUS_WIDTH, tokens.link);
    let widgets = &mut visuals.widgets;
    widgets.noninteractive.bg_fill = tokens.bg_primary;
    widgets.noninteractive.weak_bg_fill = tokens.bg_primary;
    widgets.noninteractive.bg_stroke = Stroke::new(1.0, tokens.border);
    widgets.noninteractive.fg_stroke = Stroke::new(1.0, tokens.text);
    // Plain buttons are RealTimeX's outline buttons: a border on --theme-bg-secondary.
    for (state, fill) in [
        (&mut widgets.inactive, tokens.bg_secondary),
        (&mut widgets.hovered, tokens.hover),
        (&mut widgets.active, tokens.hover),
        (&mut widgets.open, tokens.hover),
    ] {
        state.bg_fill = fill;
        state.weak_bg_fill = fill;
        state.bg_stroke = Stroke::new(1.0, tokens.border);
        state.fg_stroke = Stroke::new(1.0, tokens.text);
        state.expansion = 0.0;
    }
    // Focused (and pressed): egui draws a control in its active state.
    widgets.active.bg_stroke = Stroke::new(FOCUS_WIDTH, tokens.link);
    // Text fields and the switch's track.
    widgets.inactive.bg_fill = tokens.input_bg;
    for state in [
        &mut widgets.noninteractive,
        &mut widgets.inactive,
        &mut widgets.hovered,
        &mut widgets.active,
        &mut widgets.open,
    ] {
        state.corner_radius = CornerRadius::same(CONTROL_RADIUS);
    }
}

/// Text in the semibold weight (RealTimeX's font-semibold), in the strong colour.
pub fn semibold(text: impl Into<String>) -> RichText {
    RichText::new(text).strong().variation("wght", SEMIBOLD)
}

/// RealTimeX's primary button: white semibold text on --theme-button-primary.
pub fn primary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let ring = tokens(ui).text;
    // Through the widget visuals rather than Button::fill, which would also be the hover fill.
    ui.scope(|ui| {
        let widgets = &mut ui.visuals_mut().widgets;
        for (state, fill) in [
            (&mut widgets.inactive, PRIMARY),
            (&mut widgets.hovered, PRIMARY_HOVER),
            (&mut widgets.active, PRIMARY_HOVER),
        ] {
            state.weak_bg_fill = fill;
            state.bg_stroke = Stroke::NONE;
            state.fg_stroke = Stroke::new(1.0, Color32::WHITE);
        }
        // Focused: a ring in the text colour (blue on blue would not show).
        widgets.active.bg_stroke = Stroke::new(FOCUS_WIDTH, ring);
        ui.add(
            egui::Button::new(semibold(text).color(Color32::WHITE))
                .min_size(egui::vec2(0.0, PRIMARY_HEIGHT)),
        )
    })
    .inner
}

/// A tab or an option: when selected, tinted with a border and semibold text in the text colour
/// (egui's own selected text would be the focus colour).
pub fn selectable_value<V: PartialEq>(
    ui: &mut egui::Ui,
    current: &mut V,
    value: V,
    text: &str,
) -> egui::Response {
    let selected = *current == value;
    let tokens = tokens(ui);
    let mut response = ui
        .scope(|ui| {
            if selected {
                let widgets = &mut ui.visuals_mut().widgets;
                widgets.inactive.bg_stroke = Stroke::new(1.0, tokens.selected_border);
                widgets.hovered.bg_stroke = Stroke::new(1.0, tokens.selected_border);
            }
            let label = if selected {
                semibold(text).color(tokens.text)
            } else {
                RichText::new(text)
            };
            ui.add(egui::Button::selectable(selected, label))
        })
        .inner;
    if response.clicked() && !selected {
        *current = value;
        response.mark_changed();
    }
    response
}

/// The on/off switch, as RealTimeX draws it: a --theme-button-primary track with a white knob when
/// on, a bordered field-coloured one when off, and the focus ring around it.
pub fn paint_switch(ui: &egui::Ui, response: &egui::Response, rect: egui::Rect, t: f32, on: bool) {
    let tokens = tokens(ui);
    let radius = 0.5 * rect.height();
    let (track, border, knob) = if on {
        (PRIMARY, Stroke::NONE, Color32::WHITE)
    } else {
        (
            tokens.input_bg,
            Stroke::new(1.0, tokens.border),
            tokens.text_secondary,
        )
    };
    let painter = ui.painter();
    painter.rect(rect, radius, track, border, egui::StrokeKind::Inside);
    if response.has_focus() {
        painter.rect_stroke(
            rect.expand(FOCUS_WIDTH + 1.0),
            radius + FOCUS_WIDTH + 1.0,
            Stroke::new(FOCUS_WIDTH, tokens.link),
            egui::StrokeKind::Inside,
        );
    }
    let x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), t);
    painter.circle_filled(egui::pos2(x, rect.center().y), 0.75 * radius, knob);
}

/// A card: --theme-bg-secondary with a --theme-modal-border border.
pub fn card_frame(ui: &egui::Ui) -> egui::Frame {
    let visuals = ui.visuals();
    egui::Frame::new()
        .fill(visuals.faint_bg_color)
        .stroke(visuals.widgets.noninteractive.bg_stroke)
        .corner_radius(CornerRadius::same(CARD_RADIUS))
}

#[derive(Clone, Copy)]
pub enum Tone {
    Success,
    Warning,
    Danger,
    Info,
    Neutral,
}

/// A tone's colours: (surface, border, text), --theme-status-*-surface / -border / -fg.
pub fn tone_colors(ui: &egui::Ui, tone: Tone) -> (Color32, Color32, Color32) {
    let rgb = |hex: u32| Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8);
    let (surface, border, fg) = match (tone, ui.visuals().dark_mode) {
        (Tone::Success, true) => (0x1b3525, 0x558568, 0xa6f4c5),
        (Tone::Success, false) => (0xd8faed, 0x379a6e, 0x027a44),
        (Tone::Warning, true) => (0x352d1b, 0x937632, 0xfec84b),
        (Tone::Warning, false) => (0xf7eadd, 0xa97b4c, 0x854708),
        (Tone::Danger, true) => (0x351d1b, 0xb2524b, 0xf97066),
        (Tone::Danger, false) => (0xf5dfdf, 0xc95f58, 0xb42318),
        (Tone::Info, true) => (0x1b2d35, 0x4a7d95, 0x7cd4fd),
        (Tone::Info, false) => (0xd9f0fb, 0x3b8eb5, 0x076e9e),
        // --theme-action-menu-bg, --theme-modal-border and --theme-text-primary.
        (Tone::Neutral, true) => (0x27282a, 0x3f3f42, 0xffffff),
        (Tone::Neutral, false) => (0xeaeaea, 0xd3d3d3, 0x0e0f0f),
    };
    (rgb(surface), rgb(border), rgb(fg))
}

/// The desktop's light or dark setting, where egui can't see it: winit reports no theme on
/// Linux, and egui would then always be dark. GNOME's "Default" (no preference) is light.
#[cfg(target_os = "linux")]
pub fn system_theme() -> Option<egui::Theme> {
    // The XDG settings portal, read as winit's own title bar (sctk-adwaita) reads it.
    let output = std::process::Command::new("dbus-send")
        .args([
            "--reply-timeout=200",
            "--print-reply=literal",
            "--dest=org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.Settings.Read",
            "string:org.freedesktop.appearance",
            "string:color-scheme",
        ])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    portal_theme(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(not(target_os = "linux"))]
pub fn system_theme() -> Option<egui::Theme> {
    None
}

/// "variant  variant  uint32 1": 1 prefers dark, 2 prefers light, 0 has no preference.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn portal_theme(reply: &str) -> Option<egui::Theme> {
    match reply.split_whitespace().last()?.parse::<u32>().ok()? {
        1 => Some(egui::Theme::Dark),
        _ => Some(egui::Theme::Light),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_portal_colour_scheme_is_dark_only_when_dark_is_preferred() {
        let reply = |value: u32| format!("   variant       variant          uint32 {value}\n");
        assert_eq!(portal_theme(&reply(1)), Some(egui::Theme::Dark));
        assert_eq!(portal_theme(&reply(2)), Some(egui::Theme::Light));
        assert_eq!(portal_theme(&reply(0)), Some(egui::Theme::Light));
        assert_eq!(portal_theme(""), None);
        assert_eq!(
            portal_theme("Error org.freedesktop.DBus.Error.ServiceUnknown"),
            None
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{offered_languages, strings, Script};
    use skrifa::{FontRef, MetadataProvider};

    /// Every character of every locale shown with the bundled fonts, and their names, is in Plus
    /// Jakarta Sans or the Cyrillic subset: never egui's fallback font or a missing glyph.
    #[test]
    fn the_bundled_fonts_draw_every_offered_string() {
        let (latin, cyrillic) = (FontRef::new(FONT).unwrap(), FontRef::new(CYRILLIC).unwrap());
        let (latin, cyrillic) = (latin.charmap(), cyrillic.charmap());
        for locale in offered_languages() {
            if !matches!(locale.script, Script::Latin | Script::Cyrillic) {
                continue;
            }
            let mut texts = strings(locale);
            texts.push(locale.endonym);
            for text in texts {
                for c in text.chars().filter(|c| *c != '\n') {
                    assert!(
                        latin.map(c).is_some() || cyrillic.map(c).is_some(),
                        "{}: {c:?} (U+{:04X}) in {text:?}",
                        locale.code,
                        c as u32
                    );
                }
            }
        }
    }
}

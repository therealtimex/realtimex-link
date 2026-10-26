//! The window's, the tray's and the notices' language: RealTimeX's own (it posts it with its theme,
//! POST /v1/appearance), the one chosen in Link's Settings, or the system's. Strings live in
//! locales/<code>.json, flat "group.name" keys with {placeholders}; English is the fallback for a
//! missing key. The codes are RealTimeX's (frontend/src/locales/runtime.js), and so is the wording
//! wherever the same string exists there (settings.sharing.remoteControl).

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

/// What a locale needs from the fonts, which decides whether it can be offered on this computer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Script {
    /// Plus Jakarta Sans (Latin, Vietnamese).
    Latin,
    /// The bundled Noto Sans Cyrillic subset (theme.rs).
    Cyrillic,
    /// A system font found by cjk.rs: offered only where there is one.
    Cjk(crate::cjk::Region),
    /// Right to left: never offered until egui lays out bidirectional text (0.37 can't), so it
    /// resolves to English. The translations are kept for then.
    Rtl,
}

pub struct Locale {
    pub code: &'static str,
    /// Its own name, as RealTimeX lists it (frontend/src/locales/languageNames.js).
    pub endonym: &'static str,
    /// Its English name, for the "can't show it yet" note.
    pub english: &'static str,
    pub script: Script,
    source: &'static str,
}

macro_rules! locale {
    ($code:literal, $endonym:literal, $english:literal, Cjk($region:ident)) => {
        locale!(@ $code, $endonym, $english, Script::Cjk(crate::cjk::Region::$region))
    };
    ($code:literal, $endonym:literal, $english:literal, $script:ident) => {
        locale!(@ $code, $endonym, $english, Script::$script)
    };
    (@ $code:literal, $endonym:literal, $english:literal, $script:expr) => {
        Locale {
            code: $code,
            endonym: $endonym,
            english: $english,
            script: $script,
            source: include_str!(concat!("../locales/", $code, ".json")),
        }
    };
}

/// Every locale Link has, in RealTimeX's order. English first: it is the fallback.
pub static LOCALES: [Locale; COUNT] = [
    locale!("en", "English", "English", Latin),
    locale!("vi", "Tiếng Việt", "Vietnamese", Latin),
    locale!("zh", "简体中文", "Chinese (Simplified)", Cjk(Simplified)),
    locale!(
        "zh-tw",
        "繁體中文",
        "Chinese (Traditional)",
        Cjk(Traditional)
    ),
    locale!("es", "Español", "Spanish", Latin),
    locale!("fr", "Français", "French", Latin),
    locale!("de", "Deutsch", "German", Latin),
    locale!("ja", "日本語", "Japanese", Cjk(Japanese)),
    locale!("ko", "한국어", "Korean", Cjk(Korean)),
    locale!("pt", "Português (Brasil)", "Portuguese (Brazil)", Latin),
    locale!("ru", "Русский", "Russian", Cyrillic),
    locale!("ar", "العربية", "Arabic", Rtl),
    locale!("da", "Dansk", "Danish", Latin),
    locale!("et", "Eesti", "Estonian", Latin),
    locale!("fa", "فارسی", "Persian", Rtl),
    locale!("he", "עברית", "Hebrew", Rtl),
    locale!("it", "Italiano", "Italian", Latin),
    locale!("lv", "Latviešu", "Latvian", Latin),
    locale!("nl", "Nederlands", "Dutch", Latin),
    locale!("pl", "Polski", "Polish", Latin),
    locale!("ro", "Română", "Romanian", Latin),
    locale!("tr", "Türkçe", "Turkish", Latin),
];

const COUNT: usize = 22;
const ENGLISH: usize = 0;

/// Each locale's strings, parsed the first time they are needed.
static TABLES: [OnceLock<HashMap<String, String>>; COUNT] = [const { OnceLock::new() }; COUNT];

/// The language asked for (as stored: a tag or "system") and the locale in effect.
struct Current {
    requested: String,
    effective: usize,
}

static CURRENT: RwLock<Current> = RwLock::new(Current {
    requested: String::new(),
    effective: ENGLISH,
});

fn table(index: usize) -> &'static HashMap<String, String> {
    TABLES[index].get_or_init(|| serde_json::from_str(LOCALES[index].source).unwrap_or_default())
}

fn index_of(locale: &Locale) -> usize {
    LOCALES
        .iter()
        .position(|l| std::ptr::eq(l, locale))
        .unwrap_or(ENGLISH)
}

/// Whether this computer can show the locale.
fn offered(locale: &Locale) -> bool {
    match locale.script {
        Script::Latin | Script::Cyrillic => true,
        Script::Cjk(region) => crate::cjk::available(region),
        Script::Rtl => false,
    }
}

/// The locales this computer can show: the one list Settings offers.
pub fn offered_languages() -> Vec<&'static Locale> {
    LOCALES.iter().filter(|&l| offered(l)).collect()
}

/// The locale for a tag ("vi", "pt-BR", "de_AT", any case), or None when Link has none. Its
/// language decides (Brazilian Portuguese is RealTimeX's only Portuguese), except Chinese:
/// Taiwan, Hong Kong, Macau and Hant are traditional, the rest simplified.
fn locale_for(tag: &str) -> Option<&'static Locale> {
    let tag = tag.trim().replace('_', "-").to_ascii_lowercase();
    let mut subtags = tag.split('-');
    let language = subtags.next().unwrap_or("");
    let code = if language == "zh" {
        if subtags.any(|subtag| matches!(subtag, "tw" | "hk" | "mo" | "hant")) {
            "zh-tw"
        } else {
            "zh"
        }
    } else {
        language
    };
    LOCALES.iter().find(|l| l.code == code)
}

/// The locale in effect for a requested language: the only place that falls back. "system" (or
/// nothing) is the system's language; anything Link has no locale for, or can't show here, is
/// English. The request itself is kept as given (`set_language`).
pub fn effective_locale(requested: &str) -> &'static Locale {
    resolve(requested, system_language, offered)
}

fn resolve(
    requested: &str,
    system: impl FnOnce() -> Option<String>,
    offered: impl Fn(&Locale) -> bool,
) -> &'static Locale {
    let tag = match requested.trim() {
        "" | "system" => system().unwrap_or_default(),
        tag => tag.to_string(),
    };
    locale_for(&tag)
        .filter(|&l| offered(l))
        .unwrap_or(&LOCALES[ENGLISH])
}

/// Use the language RealTimeX or Settings asked for (GET /v1/status "language"). True when the
/// locale in effect changed, so the window and the tray redraw their text. Worked out again each
/// time: a CJK language becomes available once its font is found.
pub fn set_language(requested: &str) -> bool {
    let effective = index_of(effective_locale(requested));
    let mut current = CURRENT.write().unwrap_or_else(|p| p.into_inner());
    current.requested = requested.to_string();
    std::mem::replace(&mut current.effective, effective) != effective
}

/// The locale in effect.
pub fn current() -> &'static Locale {
    &LOCALES[CURRENT.read().unwrap_or_else(|p| p.into_inner()).effective]
}

/// When a stored language can't be shown here (so English is): its English name, or the tag
/// itself for a language Link has no locale for. None for the system default.
pub fn unavailable(requested: &str) -> Option<String> {
    if matches!(requested.trim(), "" | "system") {
        return None;
    }
    let shown = effective_locale(requested);
    match locale_for(requested) {
        Some(locale) if std::ptr::eq(locale, shown) => None,
        Some(locale) => Some(locale.english.to_string()),
        None => Some(requested.to_string()),
    }
}

/// The string for a key in the locale in effect, else English's, else the key itself.
pub fn t(key: &str) -> String {
    let index = CURRENT.read().unwrap_or_else(|p| p.into_inner()).effective;
    lookup(index, key)
}

fn lookup(index: usize, key: &str) -> String {
    table(index)
        .get(key)
        .or_else(|| table(ENGLISH).get(key))
        .cloned()
        .unwrap_or_else(|| key.to_string())
}

/// t() with its {placeholders} filled in.
pub fn tf(key: &str, args: &[(&str, &str)]) -> String {
    fill(&t(key), args)
}

fn fill(template: &str, args: &[(&str, &str)]) -> String {
    let mut text = template.to_string();
    for (name, value) in args {
        text = text.replace(&format!("{{{name}}}"), value);
    }
    text
}

/// A locale's strings (tests: the fonts must draw every one).
#[cfg(test)]
pub fn strings(locale: &Locale) -> Vec<&'static str> {
    table(index_of(locale))
        .values()
        .map(String::as_str)
        .collect()
}

/// The system's language, as a tag; read once.
fn system_language() -> Option<String> {
    static SYSTEM: OnceLock<Option<String>> = OnceLock::new();
    SYSTEM.get_or_init(read_system_language).clone()
}

/// Linux: gettext's order, LANGUAGE (a list), then LC_ALL, LC_MESSAGES and LANG ("vi_VN.UTF-8").
#[cfg(all(unix, not(target_os = "macos")))]
fn read_system_language() -> Option<String> {
    ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .find_map(|value| posix_language(&value))
}

/// "vi_VN.UTF-8@euro" -> "vi_VN"; "C" and "POSIX" say nothing.
#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
fn posix_language(value: &str) -> Option<String> {
    let first = value.split(':').next().unwrap_or("");
    let tag = first.split(['.', '@']).next().unwrap_or("").trim();
    (!tag.is_empty() && tag != "C" && tag != "POSIX").then(|| tag.to_string())
}

/// macOS: the first of the preferred languages (System Settings → General → Language & Region).
#[cfg(target_os = "macos")]
fn read_system_language() -> Option<String> {
    let output = std::process::Command::new("/usr/bin/defaults")
        .args(["read", "-g", "AppleLanguages"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    apple_languages(&String::from_utf8_lossy(&output.stdout))
}

/// `defaults` prints a plist array: "(\n    \"vi-VN\",\n    en\n)".
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn apple_languages(output: &str) -> Option<String> {
    output
        .split(['(', ')', ',', '\n'])
        .map(|item| item.trim().trim_matches('"'))
        .find(|item| !item.is_empty())
        .map(str::to_string)
}

/// Windows: the first of the user's preferred display languages ("vi-VN").
#[cfg(windows)]
fn read_system_language() -> Option<String> {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetUserPreferredUILanguages(
            flags: u32,
            count: *mut u32,
            buffer: *mut u16,
            length: *mut u32,
        ) -> i32;
    }
    const MUI_LANGUAGE_NAME: u32 = 0x8;
    let mut count = 0u32;
    let mut buffer = [0u16; 512];
    let mut length = buffer.len() as u32;
    let ok = unsafe {
        GetUserPreferredUILanguages(
            MUI_LANGUAGE_NAME,
            &mut count,
            buffer.as_mut_ptr(),
            &mut length,
        )
    };
    if ok == 0 {
        return None;
    }
    // A list of names, each ending in a NUL: the first one.
    let first: Vec<u16> = buffer
        .iter()
        .copied()
        .take_while(|&unit| unit != 0)
        .collect();
    let tag = String::from_utf16_lossy(&first);
    (!tag.is_empty()).then_some(tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(requested: &str, system: Option<&str>) -> &'static str {
        resolve(requested, || system.map(str::to_string), offered).code
    }

    #[test]
    fn a_request_resolves_to_a_locale_link_can_show_else_english() {
        assert_eq!(code("vi", None), "vi");
        assert_eq!(code("pt-BR", None), "pt");
        assert_eq!(code("PT_pt", None), "pt");
        assert_eq!(code("de-AT", None), "de");
        assert_eq!(code("ru", None), "ru");
        assert_eq!(code("sv", None), "en");
        assert_eq!(code("", None), "en");
        // System default: the system's language, through the same rules.
        assert_eq!(code("system", Some("vi_VN")), "vi");
        assert_eq!(code("system", Some("fr-CA")), "fr");
        assert_eq!(code("system", Some("sv-SE")), "en");
        assert_eq!(code("system", None), "en");
    }

    #[test]
    fn chinese_tags_pick_simplified_or_traditional_and_cjk_needs_a_font() {
        let all = |tag: &str| resolve(tag, || None, |_| true).code;
        assert_eq!(all("zh"), "zh");
        assert_eq!(all("zh-CN"), "zh");
        assert_eq!(all("zh-Hans-CN"), "zh");
        assert_eq!(all("zh-TW"), "zh-tw");
        assert_eq!(all("zh_HK"), "zh-tw");
        assert_eq!(all("zh-Hant"), "zh-tw");
        assert_eq!(all("ja-JP"), "ja");
        assert_eq!(all("ko"), "ko");
        // Without a font (or before the scan), English.
        let latin = |tag: &str| resolve(tag, || None, |l| !matches!(l.script, Script::Cjk(_))).code;
        for tag in ["zh", "zh-TW", "ja", "ko"] {
            assert_eq!(latin(tag), "en", "{tag}");
        }
    }

    #[test]
    fn right_to_left_locales_are_never_offered_and_show_english() {
        for tag in ["ar", "fa", "he", "ar-EG", "he_IL"] {
            assert_eq!(code(tag, None), "en", "{tag}");
        }
        // Always the 15; Chinese, Japanese and Korean where this computer has their fonts.
        let offered: Vec<_> = offered_languages()
            .iter()
            .filter(|l| !matches!(l.script, Script::Cjk(_)))
            .map(|l| l.code)
            .collect();
        assert_eq!(
            offered,
            [
                "en", "vi", "es", "fr", "de", "pt", "ru", "da", "et", "it", "lv", "nl", "pl", "ro",
                "tr"
            ]
        );
    }

    #[test]
    fn every_locale_has_englishs_keys_and_placeholders() {
        let placeholders = |text: &str| -> Vec<String> {
            let mut names: Vec<String> = text
                .split('{')
                .skip(1)
                .filter_map(|rest| rest.split_once('}').map(|(name, _)| name.to_string()))
                .collect();
            names.sort();
            names
        };
        let english = table(ENGLISH);
        assert!(english.len() > 100);
        for (index, locale) in LOCALES.iter().enumerate() {
            let strings = table(index);
            let mut keys: Vec<_> = strings.keys().collect();
            let mut expected: Vec<_> = english.keys().collect();
            keys.sort();
            expected.sort();
            assert_eq!(keys, expected, "{}", locale.code);
            for (key, text) in english {
                assert_eq!(
                    placeholders(&strings[key]),
                    placeholders(text),
                    "{} {key}",
                    locale.code
                );
            }
        }
    }

    #[test]
    fn a_stored_language_that_cant_be_shown_is_named() {
        assert_eq!(unavailable("system"), None);
        assert_eq!(unavailable("vi"), None);
        assert_eq!(unavailable("pt-br"), None);
        assert_eq!(unavailable("ar").as_deref(), Some("Arabic"));
        assert_eq!(unavailable("sv").as_deref(), Some("sv"));
    }

    #[test]
    fn placeholders_are_filled_and_missing_keys_fall_back() {
        assert_eq!(
            fill("{name} wants to connect", &[("name", "Mac")]),
            "Mac wants to connect"
        );
        assert_eq!(fill("{a} and {b}", &[("b", "2"), ("a", "1")]), "1 and 2");
        assert_eq!(lookup(ENGLISH, "no.such.key"), "no.such.key");
        let vi = LOCALES.iter().position(|l| l.code == "vi").unwrap();
        assert_eq!(lookup(vi, "tabs.thisComputer"), "Máy tính này");
    }

    #[test]
    fn the_clients_tab_uses_realtimexs_name_and_description() {
        // RealTimeX's settings.sharing.remoteControl.clients and clientsDescription: "Computers"
        // read as RealTimeX's "Other computers" (realtimex-link#6).
        let vi = LOCALES.iter().position(|l| l.code == "vi").unwrap();
        assert_eq!(
            fill(&lookup(ENGLISH, "tabs.clients"), &[("count", "2")]),
            "Clients · 2"
        );
        assert_eq!(lookup(vi, "tabs.clients"), "Client · {count}");
        assert_eq!(
            lookup(ENGLISH, "clients.description"),
            "Computers that signed in with this App ID and password. Each one has its own permission."
        );
        for (index, locale) in LOCALES.iter().enumerate() {
            assert!(table(index).contains_key("tabs.clients"), "{}", locale.code);
            assert!(
                !table(index).contains_key("tabs.computers"),
                "{}",
                locale.code
            );
        }
    }

    #[test]
    fn system_languages_are_read_from_posix_and_apple_formats() {
        assert_eq!(posix_language("vi_VN.UTF-8").as_deref(), Some("vi_VN"));
        assert_eq!(posix_language("de_DE@euro").as_deref(), Some("de_DE"));
        assert_eq!(posix_language("pt_BR:en").as_deref(), Some("pt_BR"));
        assert_eq!(posix_language("C.UTF-8"), None);
        assert_eq!(posix_language(""), None);
        assert_eq!(
            apple_languages("(\n    \"vi-VN\",\n    en\n)\n").as_deref(),
            Some("vi-VN")
        );
        assert_eq!(apple_languages("(\n    en\n)").as_deref(), Some("en"));
        assert_eq!(apple_languages(""), None);
    }
}

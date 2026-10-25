//! Chinese (simplified and traditional), Japanese and Korean. The bundled fonts can't carry them
//! (megabytes each), so each is offered only where this computer has a font for it, found once by
//! a scan off the UI thread, and that font is loaded (memory-mapped) only while its language is in
//! effect. Their names in the language list come from tiny bundled subsets (fonts/endonym-*.ttf).
//! OFFER_CJK turns all of it off: the four languages then resolve to English.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use eframe::egui::{FontData, FontDefinitions, FontFamily};
use skrifa::string::StringId;
use skrifa::{FontRef, MetadataProvider};

/// The off switch.
pub const OFFER_CJK: bool = true;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    Simplified,
    Traditional,
    Japanese,
    Korean,
}

const REGIONS: [Region; 4] = [
    Region::Simplified,
    Region::Traditional,
    Region::Japanese,
    Region::Korean,
];

impl Region {
    /// Its locale (locales/<code>.json).
    pub fn code(self) -> &'static str {
        match self {
            Region::Simplified => "zh",
            Region::Traditional => "zh-tw",
            Region::Japanese => "ja",
            Region::Korean => "ko",
        }
    }

    /// Characters a font for this language has and a Latin font doesn't: its own Han forms, kana
    /// and hangul.
    fn probe(self) -> &'static str {
        match self {
            Region::Simplified => "简体中文",
            Region::Traditional => "繁體中文",
            Region::Japanese => "あア日本語",
            Region::Korean => "한국어",
        }
    }
}

/// Its name in the language list, drawn from a bundled subset of Noto Sans SC, TC, JP or KR (OFL,
/// fonts/OFL-NotoSansCJK.txt) with only those glyphs and the weight axis.
fn endonym_font(region: Region) -> &'static [u8] {
    match region {
        Region::Simplified => include_bytes!("../fonts/endonym-zh.ttf"),
        Region::Traditional => include_bytes!("../fonts/endonym-zh-tw.ttf"),
        Region::Japanese => include_bytes!("../fonts/endonym-ja.ttf"),
        Region::Korean => include_bytes!("../fonts/endonym-ko.ttf"),
    }
}

/// The font family a language's name is drawn in (Settings' list), None outside CJK (or with
/// CJK off, when the families aren't there).
pub fn endonym_family(code: &str) -> Option<FontFamily> {
    if !OFFER_CJK {
        return None;
    }
    REGIONS
        .iter()
        .find(|region| region.code() == code)
        .map(|region| FontFamily::Name(format!("endonym-{}", region.code()).into()))
}

/// A face in a font file on this computer.
#[derive(Debug)]
struct Face {
    path: PathBuf,
    index: u32,
}

/// What the scan found, per region (REGIONS' order); unset until it has run.
static FOUND: OnceLock<[Option<Face>; 4]> = OnceLock::new();

/// Look for each language's font, once (a background thread at start).
pub fn scan() {
    if OFFER_CJK {
        let _ = FOUND.set(REGIONS.map(find));
    }
}

/// Whether this computer can show the language. False until the scan has run.
pub fn available(region: Region) -> bool {
    OFFER_CJK
        && FOUND
            .get()
            .is_some_and(|found| found[region as usize].is_some())
}

/// Add the language names' fonts, and the font of the CJK language in effect, if any (after Plus
/// Jakarta Sans and the Cyrillic subset). Called again without it when the language changes,
/// which drops the mapping.
pub fn add_fonts(fonts: &mut FontDefinitions, active: Option<Region>) {
    if !OFFER_CJK {
        return;
    }
    for region in REGIONS {
        let name = format!("endonym-{}", region.code());
        fonts.font_data.insert(
            name.clone(),
            Arc::new(FontData::from_static(endonym_font(region))),
        );
        fonts.families.insert(
            FontFamily::Name(name.clone().into()),
            vec![name, "plus-jakarta-sans".into()],
        );
    }
    let font = active.and_then(|region| FOUND.get()?[region as usize].as_ref().and_then(load));
    if let Some(font) = font {
        fonts.font_data.insert("system-cjk".into(), Arc::new(font));
        let proportional = fonts.families.entry(FontFamily::Proportional).or_default();
        let at = proportional.len().min(2);
        proportional.insert(at, "system-cjk".into());
    }
}

/// The face, memory-mapped from a system font folder; a font elsewhere (the user's own, which
/// could be changed under the mapping) is read into memory instead.
fn load(face: &Face) -> Option<FontData> {
    let bytes = open(&face.path)?;
    Some(FontData::from_blob(bytes, face.index))
}

type Bytes = Arc<dyn AsRef<[u8]> + Send + Sync>;

fn open(path: &Path) -> Option<Bytes> {
    if system_font_dirs().iter().any(|dir| path.starts_with(dir)) {
        let file = std::fs::File::open(path).ok()?;
        // Safety: a system font folder's files are replaced only by system updates, which
        // install new files rather than truncate the ones in use.
        let map: Bytes = Arc::new(unsafe { memmap2::Mmap::map(&file) }.ok()?);
        Some(map)
    } else {
        let read: Bytes = Arc::new(std::fs::read(path).ok()?);
        Some(read)
    }
}

fn system_font_dirs() -> Vec<PathBuf> {
    if cfg!(target_os = "macos") {
        vec!["/System/Library/Fonts".into(), "/Library/Fonts".into()]
    } else if cfg!(windows) {
        windows_fonts().into_iter().collect()
    } else {
        vec!["/usr/share/fonts".into(), "/usr/local/share/fonts".into()]
    }
}

/// %WINDIR%\Fonts.
fn windows_fonts() -> Option<PathBuf> {
    std::env::var_os("WINDIR").map(|dir| PathBuf::from(dir).join("Fonts"))
}

/// Which face of a candidate file: the one with this family name (macOS, Windows), or the one
/// fontconfig named (Linux).
#[allow(dead_code)] // Each platform makes one of the two.
enum Pick {
    Family(&'static str),
    Index(u32),
}

/// The first candidate that has its face and draws the language's probe.
fn find(region: Region) -> Option<Face> {
    candidates(region).into_iter().find_map(|(path, pick)| {
        let bytes = open(&path)?;
        let data: &[u8] = (*bytes).as_ref();
        let index = match pick {
            Pick::Family(family) => select_face(data, family)?,
            Pick::Index(index) => index,
        };
        probe(data, index, region).then_some(Face { path, index })
    })
}

/// The face in a font file (a .ttc holds several) whose family name is this one. Names starting
/// with "." are the system's private interface faces (".Hiragino Kaku Gothic Interface"), never
/// chosen.
fn select_face(data: &[u8], family: &str) -> Option<u32> {
    (0..64)
        .map_while(|index| {
            FontRef::from_index(data, index)
                .ok()
                .map(|font| (index, font))
        })
        .find(|(_, font)| {
            let name = font
                .localized_strings(StringId::FAMILY_NAME)
                .english_or_first()
                .map(|name| name.to_string());
            name.is_some_and(|name| !name.starts_with('.') && name == family)
        })
        .map(|(index, _)| index)
}

/// Whether the face draws every character of the language's probe.
fn probe(data: &[u8], index: u32, region: Region) -> bool {
    let Ok(font) = FontRef::from_index(data, index) else {
        return false;
    };
    let charmap = font.charmap();
    region.probe().chars().all(|c| charmap.map(c).is_some())
}

/// Where each language's font is, most fitting first. Checked by family name, never by index.
///
/// macOS (PingFang has no fixed path, so it isn't one):
/// - ja: Hiragino Sans W3 (face 0 of ヒラギノ角ゴシック W3.ttc). The file name is stored
///   decomposed (NFD) on disk; APFS looks names up regardless of normalization, so the composed
///   literal here finds it.
/// - ko: Apple SD Gothic Neo (face 0 of AppleSDGothicNeo.ttc; the odd faces are the hidden
///   ".Apple SD Gothic NeoI").
/// - zh: Hiragino Sans GB W3 (face 0), then Heiti SC (face 1 of STHeiti Light.ttc).
/// - zh-tw: Heiti TC (face 0 of STHeiti Light.ttc).
#[cfg(target_os = "macos")]
fn candidates(region: Region) -> Vec<(PathBuf, Pick)> {
    let list: &[(&str, &str)] = match region {
        Region::Japanese => &[(
            "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc",
            "Hiragino Sans W3",
        )],
        Region::Korean => &[(
            "/System/Library/Fonts/AppleSDGothicNeo.ttc",
            "Apple SD Gothic Neo",
        )],
        Region::Simplified => &[
            (
                "/System/Library/Fonts/Hiragino Sans GB.ttc",
                "Hiragino Sans GB W3",
            ),
            ("/System/Library/Fonts/STHeiti Light.ttc", "Heiti SC"),
        ],
        Region::Traditional => &[("/System/Library/Fonts/STHeiti Light.ttc", "Heiti TC")],
    };
    list.iter()
        .map(|(path, family)| (PathBuf::from(path), Pick::Family(*family)))
        .collect()
}

/// Windows: the fonts every edition installs with the language's support, in %WINDIR%\Fonts.
/// Microsoft YaHei, not its "UI" face, which is tuned for menus.
#[cfg(windows)]
fn candidates(region: Region) -> Vec<(PathBuf, Pick)> {
    let Some(fonts) = windows_fonts() else {
        return Vec::new();
    };
    let list: &[(&str, &str)] = match region {
        Region::Simplified => &[("msyh.ttc", "Microsoft YaHei")],
        Region::Traditional => &[("msjh.ttc", "Microsoft JhengHei")],
        Region::Japanese => &[("YuGothR.ttc", "Yu Gothic"), ("meiryo.ttc", "Meiryo")],
        Region::Korean => &[("malgun.ttf", "Malgun Gothic")],
    };
    list.iter()
        .map(|(file, family)| (fonts.join(file), Pick::Family(*family)))
        .collect()
}

/// Linux: fontconfig's sans-serif for the language, which knows the regional order (Noto Sans CJK
/// SC before JP for zh-cn). It always answers with some font, so the probe decides.
#[cfg(all(unix, not(target_os = "macos")))]
fn candidates(region: Region) -> Vec<(PathBuf, Pick)> {
    let lang = match region {
        Region::Simplified => "zh-cn",
        Region::Traditional => "zh-tw",
        Region::Japanese => "ja",
        Region::Korean => "ko",
    };
    fc_match(lang)
        .map(|(path, index)| (path, Pick::Index(index)))
        .into_iter()
        .collect()
}

/// `fc-match -f '%{file}\n%{index}\n' 'sans-serif:lang=<lang>'`, given 2 s.
#[cfg(all(unix, not(target_os = "macos")))]
fn fc_match(lang: &str) -> Option<(PathBuf, u32)> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let mut child = Command::new("fc-match")
        .args([
            "-f",
            "%{file}\n%{index}\n",
            &format!("sans-serif:lang={lang}"),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut output = String::new();
    child.stdout.take()?.read_to_string(&mut output).ok()?;
    fontconfig_answer(&output)
}

/// "/usr/share/fonts/…/NotoSansCJK-Regular.ttc\n2\n" -> (path, 2).
#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
fn fontconfig_answer(output: &str) -> Option<(PathBuf, u32)> {
    let mut lines = output.lines();
    let path = lines.next().filter(|path| !path.is_empty())?;
    let index = lines.next()?.trim().parse().ok()?;
    Some((PathBuf::from(path), index))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO_FACES: &[u8] = include_bytes!("../tests/fixtures/two-faces.ttc");
    const LATIN: &[u8] = include_bytes!("../fonts/PlusJakartaSans.ttf");

    #[test]
    fn a_face_is_chosen_by_family_name_never_a_hidden_one() {
        assert_eq!(select_face(TWO_FACES, "Fixture Sans"), Some(1));
        assert_eq!(select_face(TWO_FACES, ".Fixture Hidden"), None);
        assert_eq!(select_face(TWO_FACES, "Missing"), None);
        assert_eq!(select_face(LATIN, "Plus Jakarta Sans"), Some(0));
        assert_eq!(select_face(b"not a font", "Plus Jakarta Sans"), None);
    }

    #[test]
    fn a_latin_font_never_passes_a_languages_probe() {
        for region in REGIONS {
            assert!(!probe(LATIN, 0, region), "{region:?}");
        }
        assert!(!probe(b"not a font", 0, Region::Japanese));
    }

    #[test]
    fn each_language_name_is_in_its_bundled_subset() {
        for region in REGIONS {
            let font = FontRef::new(endonym_font(region)).unwrap();
            let charmap = font.charmap();
            let locale = crate::i18n::LOCALES
                .iter()
                .find(|locale| locale.code == region.code())
                .unwrap();
            for c in locale.endonym.chars() {
                assert!(charmap.map(c).is_some(), "{}: {c}", locale.code);
            }
        }
    }

    #[test]
    fn fontconfigs_answer_is_a_path_and_a_face() {
        assert_eq!(
            fontconfig_answer("/usr/share/fonts/noto/NotoSansCJK-Regular.ttc\n2\n"),
            Some(("/usr/share/fonts/noto/NotoSansCJK-Regular.ttc".into(), 2))
        );
        assert_eq!(fontconfig_answer(""), None);
        assert_eq!(fontconfig_answer("/a.ttf\nx\n"), None);
    }

    /// Each macOS candidate either opens and has its named face, or isn't on this Mac.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_macos_candidates_have_their_faces_or_are_absent() {
        for region in REGIONS {
            for (path, pick) in candidates(region) {
                let Pick::Family(family) = pick else {
                    unreachable!("macOS candidates are named");
                };
                match open(&path) {
                    Some(bytes) => {
                        let face = select_face((*bytes).as_ref(), family);
                        assert!(face.is_some(), "{}: no {family}", path.display());
                        assert!(probe((*bytes).as_ref(), face.unwrap(), region), "{family}");
                    }
                    None => println!("absent: {}", path.display()),
                }
            }
        }
    }
}

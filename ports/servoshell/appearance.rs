/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Pteron: colour themes and the new tab page background. Everything here is plain data
//! and text, so it can be tested without a window; the GUI turns the colours into egui styles.

use std::path::PathBuf;
use std::sync::Mutex;

use crate::pteron::Settings;

/// Ready-made colour themes: id, name, accent colour.
pub(crate) const PRESETS: [(&str, &str, [u8; 3]); 6] = [
    ("gold", "Gold", [0xd6, 0xaa, 0x48]),
    ("ocean", "Ocean", [0x4a, 0x9e, 0xed]),
    ("forest", "Forest", [0x4c, 0xaf, 0x7a]),
    ("rose", "Rose", [0xe0, 0x70, 0x9a]),
    ("violet", "Violet", [0x9b, 0x7b, 0xe0]),
    ("slate", "Slate", [0x8a, 0x98, 0xa8]),
];

/// Biggest picture accepted for the new tab page (it is placed inside the page).
pub(crate) const MAX_PICTURE_BYTES: u64 = 4 * 1024 * 1024;

/// Softer corners and quicker fades for every look.
pub(crate) fn polish_style(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        let widget = egui::CornerRadius::same(8);
        style.visuals.widgets.noninteractive.corner_radius = widget;
        style.visuals.widgets.inactive.corner_radius = widget;
        style.visuals.widgets.hovered.corner_radius = widget;
        style.visuals.widgets.active.corner_radius = widget;
        style.visuals.widgets.open.corner_radius = widget;
        style.visuals.window_corner_radius = egui::CornerRadius::same(12);
        style.visuals.menu_corner_radius = egui::CornerRadius::same(10);
        style.animation_time = 0.12;
        style.spacing.button_padding = egui::vec2(8.0, 4.0);
    });
}

pub(crate) fn parse_hex(text: &str) -> Option<[u8; 3]> {
    let text = text.trim().trim_start_matches('#');
    if text.len() != 6 || !text.is_ascii() {
        return None;
    }
    let value = u32::from_str_radix(text, 16).ok()?;
    Some([(value >> 16) as u8, (value >> 8) as u8, value as u8])
}

pub(crate) fn to_hex(rgb: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2])
}

/// The colours of a whole theme.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Palette {
    pub dark: bool,
    /// Window and sidebar background.
    pub bg: [u8; 3],
    /// Cards, text boxes and buttons.
    pub surface: [u8; 3],
    pub text: [u8; 3],
    pub muted: [u8; 3],
    pub accent: [u8; 3],
}

const fn palette(dark: bool, bg: [u8; 3], surface: [u8; 3], text: [u8; 3], muted: [u8; 3], accent: [u8; 3]) -> Palette {
    Palette { dark, bg, surface, text, muted, accent }
}

/// Ready-made full themes: id, name, colours. ("system", "light" and "dark" are the plain
/// built-in looks; "custom" is built from the user's own background colour.)
pub(crate) const THEMES: [(&str, &str, Palette); 7] = [
    ("midnight", "Midnight", palette(true, [0x0f, 0x14, 0x20], [0x1a, 0x21, 0x33], [0xe6, 0xeb, 0xf5], [0x8d, 0x98, 0xb0], [0x6e, 0xa8, 0xfe])),
    ("forest", "Forest", palette(true, [0x12, 0x1a, 0x15], [0x1c, 0x28, 0x22], [0xe3, 0xec, 0xe5], [0x8a, 0xa0, 0x92], [0x5e, 0xcb, 0x8a])),
    ("sunset", "Sunset", palette(true, [0x1d, 0x15, 0x17], [0x2b, 0x1e, 0x21], [0xf4, 0xe9, 0xe4], [0xb3, 0x9a, 0x95], [0xff, 0x8a, 0x5c])),
    ("lavender", "Lavender", palette(true, [0x1a, 0x16, 0x25], [0x26, 0x20, 0x36], [0xec, 0xe8, 0xf6], [0x9f, 0x96, 0xb8], [0xa7, 0x8b, 0xfa])),
    ("paper", "Paper", palette(false, [0xf3, 0xef, 0xe6], [0xff, 0xfd, 0xf8], [0x2b, 0x29, 0x23], [0x7f, 0x7a, 0x6e], [0xb5, 0x65, 0x1d])),
    ("rose", "Rose", palette(false, [0xfb, 0xf1, 0xf4], [0xff, 0xff, 0xff], [0x3a, 0x24, 0x30], [0x8f, 0x6f, 0x7e], [0xd6, 0x45, 0x7a])),
    ("ocean", "Ocean", palette(false, [0xea, 0xf3, 0xfa], [0xff, 0xff, 0xff], [0x17, 0x30, 0x3f], [0x58, 0x73, 0x88], [0x1f, 0x8a, 0xc0])),
];

/// Preview colours for the three built-in looks (the real ones come from egui itself).
pub(crate) const LIGHT_PREVIEW: Palette = palette(false, [0xf6, 0xf6, 0xf6], [0xff, 0xff, 0xff], [0x20, 0x20, 0x20], [0x80, 0x80, 0x80], [0xd6, 0xaa, 0x48]);
pub(crate) const DARK_PREVIEW: Palette = palette(true, [0x1b, 0x1b, 0x1b], [0x2a, 0x2a, 0x2a], [0xe8, 0xe8, 0xe8], [0x90, 0x90, 0x90], [0xd6, 0xaa, 0x48]);

fn mix(a: [u8; 3], b: [u8; 3], amount: f32) -> [u8; 3] {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * amount).round().clamp(0.0, 255.0) as u8;
    [m(a[0], b[0]), m(a[1], b[1]), m(a[2], b[2])]
}

/// A full theme made from just a background colour.
fn custom_palette(bg: [u8; 3], accent: [u8; 3]) -> Palette {
    let dark = luminance(bg) < 0.5;
    let text = if dark { [0xec, 0xeb, 0xe8] } else { [0x25, 0x23, 0x1f] };
    let surface = if dark { mix(bg, [255, 255, 255], 0.07) } else { mix(bg, [255, 255, 255], 0.65) };
    palette(dark, bg, surface, text, mix(text, bg, 0.42), accent)
}

/// The full theme the settings ask for, or `None` for the plain system, light and dark looks.
pub(crate) fn palette_for(settings: &Settings) -> Option<Palette> {
    let accent_override = match settings.accent.as_str() {
        "auto" => None,
        _ => Some(accent_choice(settings)),
    };
    let mut found = if settings.theme == "custom" {
        let bg = parse_hex(&settings.custom_bg).unwrap_or([0x1b, 0x1c, 0x1e]);
        Some(custom_palette(bg, PRESETS[0].2))
    } else {
        THEMES.iter().find(|(id, ..)| *id == settings.theme).map(|(.., p)| *p)
    }?;
    if let Some(accent) = accent_override {
        found.accent = accent;
    }
    Some(found)
}

/// The accent the user picked from the swatches (not "auto").
fn accent_choice(settings: &Settings) -> [u8; 3] {
    if settings.accent == "custom" {
        if let Some(rgb) = parse_hex(&settings.accent_custom) {
            return rgb;
        }
    }
    PRESETS
        .iter()
        .find(|(id, ..)| *id == settings.accent)
        .map(|(.., rgb)| *rgb)
        .unwrap_or(PRESETS[0].2)
}

/// The accent colour the settings ask for.
pub(crate) fn accent_rgb(settings: &Settings) -> [u8; 3] {
    if settings.accent == "auto" {
        return palette_for(settings).map(|p| p.accent).unwrap_or(PRESETS[0].2);
    }
    accent_choice(settings)
}

/// The accent colour in use right now.
pub(crate) fn current_accent() -> [u8; 3] {
    accent_rgb(&crate::pteron::settings())
}

/// 0.0 (black) to 1.0 (white).
fn luminance(rgb: [u8; 3]) -> f32 {
    (0.2126 * rgb[0] as f32 + 0.7152 * rgb[1] as f32 + 0.0722 * rgb[2] as f32) / 255.0
}

pub(crate) fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len() / 3 * 4 + 4);
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16 |
            (*chunk.get(1).unwrap_or(&0) as u32) << 8 |
            *chunk.get(2).unwrap_or(&0) as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

fn mime_for(extension: &str) -> Option<&'static str> {
    match extension {
        "jpg" | "jpeg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        _ => None,
    }
}

#[cfg(not(any(target_os = "android", target_env = "ohos")))]
fn data_dir() -> Option<PathBuf> {
    ::dirs::config_dir().map(|dir| dir.join(crate::pteron::APP_NAME))
}

#[cfg(any(target_os = "android", target_env = "ohos"))]
fn data_dir() -> Option<PathBuf> {
    None
}

/// The saved background picture, if there is one.
fn picture_file() -> Option<(PathBuf, &'static str)> {
    let dir = data_dir()?;
    for extension in ["jpg", "jpeg", "png", "webp", "gif"] {
        let path = dir.join(format!("newtab-background.{extension}"));
        if path.is_file() {
            return Some((path, mime_for(extension)?));
        }
    }
    None
}

/// Copies a picture the user chose into Pteron's folder.
pub(crate) fn save_picture(source: &std::path::Path) -> Result<(), String> {
    let extension = source
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if mime_for(&extension).is_none() {
        return Err("Choose a .jpg, .png, .webp or .gif picture.".into());
    }
    let size = std::fs::metadata(source).map_err(|e| format!("Could not read the picture: {e}"))?.len();
    if size > MAX_PICTURE_BYTES {
        return Err(format!(
            "That picture is {:.1} MB. Please choose one under {} MB (a smaller or compressed copy works).",
            size as f64 / 1_048_576.0,
            MAX_PICTURE_BYTES / 1_048_576
        ));
    }
    let dir = data_dir().ok_or("Could not find Pteron's folder.")?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    for old in ["jpg", "jpeg", "png", "webp", "gif"] {
        let _ = std::fs::remove_file(dir.join(format!("newtab-background.{old}")));
    }
    std::fs::copy(source, dir.join(format!("newtab-background.{extension}")))
        .map_err(|e| format!("Could not save the picture: {e}"))?;
    Ok(())
}

pub(crate) fn has_picture() -> bool {
    picture_file().is_some()
}

fn dark_text_vars() -> &'static str {
    "--text:#f3f1ec;--muted:#c4c1ba;--field:rgba(20,20,22,.55);--field-border:rgba(255,255,255,.22);"
}

fn light_text_vars() -> &'static str {
    "--text:#26241f;--muted:#5f5c55;--field:rgba(255,255,255,.75);--field-border:rgba(0,0,0,.14);"
}

/// CSS that goes into the new tab page after its own style sheet.
pub(crate) fn newtab_style(settings: &Settings) -> String {
    newtab_style_with(settings, picture_file().and_then(|(path, mime)| {
        std::fs::read(&path).ok().map(|bytes| format!("data:{mime};base64,{}", base64(&bytes)))
    }))
}

fn newtab_style_with(settings: &Settings, picture: Option<String>) -> String {
    let [r, g, b] = accent_rgb(settings);
    let hex = to_hex([r, g, b]);
    let mut css = format!(
        ":root{{--gold:{hex};--gold-soft:rgba({r},{g},{b},.35);--bg-glow:rgba({r},{g},{b},.16);}}"
    );
    if let Some(p) = palette_for(settings) {
        css.push_str(&format!(
            ":root{{--bg:{};--text:{};--muted:{};--field:{};--field-border:{};color-scheme:{};}}",
            to_hex(p.bg),
            to_hex(p.text),
            to_hex(p.muted),
            to_hex(p.surface),
            to_hex(mix(p.bg, p.text, 0.16)),
            if p.dark { "dark" } else { "light" },
        ));
    }
    match settings.newtab_bg.as_str() {
        "gradient" => css.push_str(&format!(
            "body{{background-image:linear-gradient(160deg,rgba({r},{g},{b},.34),transparent 62%),radial-gradient(ellipse at 50% 18%,rgba({r},{g},{b},.18),transparent 55%);}}"
        )),
        "color" => {
            let rgb = parse_hex(&settings.newtab_color).unwrap_or([0x1b, 0x1c, 0x1e]);
            let vars = if luminance(rgb) < 0.55 { dark_text_vars() } else { light_text_vars() };
            css.push_str(&format!(":root{{{vars}}}body{{background-color:{};background-image:none;}}", to_hex(rgb)));
        },
        "picture" => {
            if let Some(uri) = picture {
                css.push_str(&format!(
                    ":root{{{}}}body{{background:linear-gradient(rgba(0,0,0,.38),rgba(0,0,0,.38)),url({uri}) center/cover no-repeat fixed;}}",
                    dark_text_vars()
                ));
            }
        },
        _ => {},
    }
    css
}

// ---------------------------------------------------------------- choosing a picture

/// Outcome of the picture dialog, waiting for the settings page to pick it up.
static PICKED: Mutex<Option<Result<(), String>>> = Mutex::new(None);
static PICKING: Mutex<bool> = Mutex::new(false);

pub(crate) fn picking() -> bool {
    PICKING.lock().map(|g| *g).unwrap_or(false)
}

/// The result of the last "Choose picture" (once).
pub(crate) fn take_picked() -> Option<Result<(), String>> {
    PICKED.lock().ok().and_then(|mut g| g.take())
}

const PICK_SCRIPT: &str = r#"param([string]$Out)
Add-Type -AssemblyName System.Windows.Forms
$d = New-Object System.Windows.Forms.OpenFileDialog
$d.Title = 'Choose a background picture'
$d.Filter = 'Pictures (*.jpg;*.jpeg;*.png;*.webp;*.gif)|*.jpg;*.jpeg;*.png;*.webp;*.gif'
if ($d.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) { Set-Content -Path $Out -Value $d.FileName -Encoding UTF8 }
"#;

/// Opens the Windows file chooser in the background; the result appears in [`take_picked`].
pub(crate) fn choose_picture_async() {
    if picking() {
        return;
    }
    if let Ok(mut g) = PICKING.lock() {
        *g = true;
    }
    std::thread::spawn(|| {
        let result = choose_picture();
        if let Ok(mut g) = PICKED.lock() {
            *g = result.transpose();
        }
        if let Ok(mut g) = PICKING.lock() {
            *g = false;
        }
    });
}

/// Runs a small PowerShell script that shows a Windows chooser and writes the chosen path into
/// the file named by its `-Out` parameter. `Ok(None)` when the user closed it without choosing.
pub(crate) fn run_chooser_script(script_text: &str) -> Result<Option<String>, String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let dir = std::env::temp_dir().join("pteron_pick");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let script = dir.join("pick.ps1");
        let out = dir.join("picked.txt");
        std::fs::write(&script, script_text).map_err(|e| e.to_string())?;
        let mut last = String::new();
        let mut started = false;
        for attempt in 0..3 {
            let status = std::process::Command::new("powershell.exe")
                .args(["-NoProfile", "-STA", "-ExecutionPolicy", "Bypass", "-File"])
                .arg(&script)
                .arg("-Out")
                .arg(&out)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .creation_flags(0x0800_0000)
                .status();
            match status {
                Ok(_) => {
                    started = true;
                    break;
                },
                Err(e) => last = e.to_string(),
            }
            if attempt < 2 {
                std::thread::sleep(std::time::Duration::from_millis(400));
            }
        }
        if !started {
            return Err(format!("Could not open the chooser: {last}"));
        }
        let Ok(text) = std::fs::read_to_string(&out) else { return Ok(None) };
        let chosen = text.trim_start_matches('\u{feff}').trim().to_string();
        Ok((!chosen.is_empty()).then_some(chosen))
    }
    #[cfg(not(windows))]
    {
        let _ = script_text;
        Err("Choosing is only built in for Windows.".into())
    }
}

/// `Ok(None)` when the user closed the dialog without choosing.
fn choose_picture() -> Result<Option<()>, String> {
    match run_chooser_script(PICK_SCRIPT)? {
        Some(chosen) => {
            save_picture(std::path::Path::new(&chosen))?;
            Ok(Some(()))
        },
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        assert_eq!(parse_hex("#d6aa48"), Some([0xd6, 0xaa, 0x48]));
        assert_eq!(parse_hex("D6AA48"), Some([0xd6, 0xaa, 0x48]));
        assert_eq!(parse_hex("#fff"), None);
        assert_eq!(parse_hex("zzzzzz"), None);
        assert_eq!(parse_hex("#é12345"), None);
        assert_eq!(to_hex([1, 2, 255]), "#0102ff");
    }

    #[test]
    fn base64_matches_known_values() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn full_themes_come_from_the_settings() {
        let mut s = Settings::default();
        assert!(palette_for(&s).is_none());
        s.theme = "midnight".into();
        let p = palette_for(&s).unwrap();
        assert!(p.dark && p.bg == [0x0f, 0x14, 0x20]);
        assert_eq!(accent_rgb(&s), p.accent);
        s.accent = "rose".into();
        assert_eq!(accent_rgb(&s), PRESETS[3].2);
        assert_eq!(palette_for(&s).unwrap().bg, [0x0f, 0x14, 0x20]);
        s.accent = "auto".into();
        s.theme = "custom".into();
        s.custom_bg = "#fff4e0".into();
        let light = palette_for(&s).unwrap();
        assert!(!light.dark && light.text == [0x25, 0x23, 0x1f]);
        s.custom_bg = "#101820".into();
        assert!(palette_for(&s).unwrap().dark);
        s.theme = "dark".into();
        assert!(palette_for(&s).is_none());
        for (id, _, p) in THEMES {
            assert!(
                (luminance(p.bg) < 0.5) == p.dark,
                "{id}: dark flag does not match the background"
            );
            assert!(
                (luminance(p.text) - luminance(p.bg)).abs() > 0.6,
                "{id}: text is hard to read"
            );
        }
    }

    #[test]
    fn newtab_page_follows_a_full_theme() {
        let mut s = Settings::default();
        s.theme = "forest".into();
        let css = newtab_style_with(&s, None);
        assert!(css.contains("--bg:#121a15") && css.contains("color-scheme:dark"));
    }

    #[test]
    fn accent_falls_back_to_gold() {
        let mut s = Settings::default();
        assert_eq!(accent_rgb(&s), PRESETS[0].2);
        s.accent = "ocean".into();
        assert_eq!(accent_rgb(&s), PRESETS[1].2);
        s.accent = "custom".into();
        s.accent_custom = "#102030".into();
        assert_eq!(accent_rgb(&s), [0x10, 0x20, 0x30]);
        s.accent_custom = "nonsense".into();
        assert_eq!(accent_rgb(&s), PRESETS[0].2);
    }

    #[test]
    fn newtab_css_follows_the_settings() {
        let mut s = Settings::default();
        let plain = newtab_style_with(&s, None);
        assert!(plain.contains("--gold:#d6aa48"));
        assert!(!plain.contains("body{"));
        s.newtab_bg = "gradient".into();
        assert!(newtab_style_with(&s, None).contains("linear-gradient(160deg"));
        s.newtab_bg = "color".into();
        s.newtab_color = "#ffffff".into();
        let light = newtab_style_with(&s, None);
        assert!(light.contains("background-color:#ffffff") && light.contains("--text:#26241f"));
        s.newtab_color = "#101010".into();
        assert!(newtab_style_with(&s, None).contains("--text:#f3f1ec"));
        s.newtab_bg = "picture".into();
        assert!(!newtab_style_with(&s, None).contains("url("));
        assert!(newtab_style_with(&s, Some("data:image/png;base64,AAAA".into())).contains("url(data:image/png;base64,AAAA)"));
    }

    #[test]
    fn rejects_other_files_and_big_pictures() {
        assert!(save_picture(std::path::Path::new("/nonexistent/x.txt")).is_err());
        let dir = std::env::temp_dir().join("pteron_pic_test");
        std::fs::create_dir_all(&dir).unwrap();
        let big = dir.join("big.png");
        std::fs::write(&big, vec![0u8; (MAX_PICTURE_BYTES + 1) as usize]).unwrap();
        assert!(save_picture(&big).unwrap_err().contains("MB"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

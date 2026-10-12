/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Pteron: product name and user settings (currently: the search engine).
//! Settings live in `<config dir>/Pteron/settings.json`.

use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};

use log::warn;

pub(crate) const APP_NAME: &str = "Pteron";
pub(crate) const APP_VERSION: &str = "0.1";

/// Built-in search engines: (id, label, URL template where `%s` is the query).
pub(crate) const ENGINES: &[(&str, &str, &str)] = &[
    ("duckduckgo", "DuckDuckGo", "https://duckduckgo.com/html/?q=%s"),
    ("google", "Google", "https://www.google.com/search?q=%s"),
    ("bing", "Bing", "https://www.bing.com/search?q=%s"),
    ("brave", "Brave Search", "https://search.brave.com/search?q=%s"),
    ("startpage", "Startpage", "https://www.startpage.com/do/search?q=%s"),
];

/// Id of the "my own SearXNG instance" entry.
pub(crate) const SEARXNG_ID: &str = "searxng";
const DEFAULT_ENGINE: &str = "duckduckgo";

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Settings {
    /// One of the ids in [`ENGINES`], or [`SEARXNG_ID`].
    pub engine: String,
    /// Address of the user's SearXNG instance (used when `engine` is [`SEARXNG_ID`]).
    pub searxng_url: String,
    /// "system", "light", "dark", "custom" or the id of a theme in `appearance::THEMES`.
    pub theme: String,
    /// Background colour of the "custom" theme (#rrggbb).
    pub custom_bg: String,
    /// Minutes before an unused background tab is put to sleep. 0 turns sleeping off.
    pub tab_sleep_minutes: u32,
    /// Keep the sidebar visible all the time instead of sliding in from the left edge.
    pub sidebar_pinned: bool,
    /// What to open at launch: "newtab", "restore" (last session) or "custom" (`homepage_url`).
    pub startup: String,
    /// Page opened at launch when `startup` is "custom".
    pub homepage_url: String,
    /// Remember visited pages (for address bar suggestions and new tab shortcuts).
    pub history_enabled: bool,
    /// Play YouTube videos in Pteron's own simple player (search, channels and videos).
    pub youtube_player: bool,
    /// Use the graphics card to decode video (takes effect after restart).
    pub hw_video_decode: bool,
    /// Long YouTube videos start from their first two minutes and switch to the whole video.
    pub fast_start: bool,
    /// Tiles on the new tab page: "off", "bookmarks" or "frequent" (bookmarks + most visited).
    pub newtab_tiles: String,
    /// The welcome page has been shown once.
    pub welcome_shown: bool,
    /// Folder where bookmarks and settings are kept in step with other computers ("" = off).
    pub sync_folder: String,
    /// Warn before opening a page over plain http.
    pub https_warning: bool,
    /// Forget the browsing history and empty the cache when the window is closed.
    pub clear_on_exit: bool,
    /// Block known ad and tracker networks.
    pub adblock: bool,
    /// Colour theme: an id from `appearance::PRESETS` or "custom".
    pub accent: String,
    /// Colour used when `accent` is "custom" (#rrggbb).
    pub accent_custom: String,
    /// New tab background: "theme", "gradient", "color" or "picture".
    pub newtab_bg: String,
    /// Colour used when `newtab_bg` is "color" (#rrggbb).
    pub newtab_color: String,
    /// Ids from `compat::FEATURES` that are switched on (newer web features, off by default).
    pub compat: Vec<String>,
    /// Memory saver level: "normal", "low" or "lowest" (see `memsaver`).
    pub memory_mode: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            engine: DEFAULT_ENGINE.to_string(),
            searxng_url: String::new(),
            theme: "system".to_string(),
            custom_bg: "#1b1c1e".to_string(),
            tab_sleep_minutes: 5,
            sidebar_pinned: false,
            startup: "newtab".to_string(),
            homepage_url: String::new(),
            history_enabled: true,
            youtube_player: true,
            hw_video_decode: true,
            fast_start: true,
            newtab_tiles: "off".to_string(),
            welcome_shown: false,
            sync_folder: String::new(),
            https_warning: true,
            clear_on_exit: false,
            adblock: true,
            accent: "auto".to_string(),
            accent_custom: "#d6aa48".to_string(),
            newtab_bg: "theme".to_string(),
            newtab_color: "#1b1c1e".to_string(),
            compat: Vec::new(),
            memory_mode: "normal".to_string(),
        }
    }
}

#[cfg(not(any(target_os = "android", target_env = "ohos")))]
fn settings_path() -> Option<PathBuf> {
    ::dirs::config_dir().map(|dir| dir.join(APP_NAME).join("settings.json"))
}

#[cfg(any(target_os = "android", target_env = "ohos"))]
fn settings_path() -> Option<PathBuf> {
    None
}

/// Reads every setting found in `value` into `settings`; anything missing or invalid is
/// left as it was.
pub(crate) fn apply_json(settings: &mut Settings, value: &serde_json::Value) {
    if let Some(engine) = value.get("engine").and_then(|v| v.as_str()) {
        settings.engine = engine.to_string();
    }
    if let Some(url) = value.get("searxng_url").and_then(|v| v.as_str()) {
        settings.searxng_url = url.to_string();
    }
    if let Some(theme) = value.get("theme").and_then(|v| v.as_str()) &&
        (matches!(theme, "system" | "light" | "dark" | "custom") ||
            crate::appearance::THEMES.iter().any(|(id, ..)| *id == theme))
    {
        settings.theme = theme.to_string();
    }
    if let Some(color) = value.get("custom_bg").and_then(|v| v.as_str()) &&
        crate::appearance::parse_hex(color).is_some()
    {
        settings.custom_bg = color.to_string();
    }
    if let Some(minutes) = value.get("tab_sleep_minutes").and_then(|v| v.as_u64()) {
        settings.tab_sleep_minutes = minutes.min(24 * 60) as u32;
    }
    if let Some(pinned) = value.get("sidebar_pinned").and_then(|v| v.as_bool()) {
        settings.sidebar_pinned = pinned;
    }
    if let Some(startup) = value.get("startup").and_then(|v| v.as_str()) &&
        matches!(startup, "newtab" | "restore" | "custom")
    {
        settings.startup = startup.to_string();
    }
    if let Some(url) = value.get("homepage_url").and_then(|v| v.as_str()) {
        settings.homepage_url = url.to_string();
    }
    if let Some(enabled) = value.get("history_enabled").and_then(|v| v.as_bool()) {
        settings.history_enabled = enabled;
    }
    if let Some(enabled) = value.get("youtube_player").and_then(|v| v.as_bool()) {
        settings.youtube_player = enabled;
    }
    if let Some(enabled) = value.get("fast_start").and_then(|v| v.as_bool()) {
        settings.fast_start = enabled;
    }
    if let Some(enabled) = value.get("hw_video_decode").and_then(|v| v.as_bool()) {
        settings.hw_video_decode = enabled;
    }
    if let Some(shown) = value.get("welcome_shown").and_then(|v| v.as_bool()) {
        settings.welcome_shown = shown;
    }
    if let Some(enabled) = value.get("clear_on_exit").and_then(|v| v.as_bool()) {
        settings.clear_on_exit = enabled;
    }
    if let Some(enabled) = value.get("https_warning").and_then(|v| v.as_bool()) {
        settings.https_warning = enabled;
    }
    if let Some(list) = value.get("compat").and_then(|v| v.as_array()) {
        settings.compat = crate::compat::clean_ids(
            list.iter().filter_map(|item| item.as_str().map(str::to_string)),
        );
    }
    if let Some(mode) = value.get("memory_mode").and_then(|v| v.as_str()) {
        settings.memory_mode = crate::memsaver::Level::from_id(mode).id().to_string();
    }
    if let Some(enabled) = value.get("adblock").and_then(|v| v.as_bool()) {
        settings.adblock = enabled;
    }
    if let Some(accent) = value.get("accent").and_then(|v| v.as_str()) &&
        (accent == "custom" || accent == "auto" || crate::appearance::PRESETS.iter().any(|(id, ..)| *id == accent))
    {
        settings.accent = accent.to_string();
    }
    if let Some(color) = value.get("accent_custom").and_then(|v| v.as_str()) &&
        crate::appearance::parse_hex(color).is_some()
    {
        settings.accent_custom = color.to_string();
    }
    if let Some(bg) = value.get("newtab_bg").and_then(|v| v.as_str()) &&
        matches!(bg, "theme" | "gradient" | "color" | "picture")
    {
        settings.newtab_bg = bg.to_string();
    }
    if let Some(color) = value.get("newtab_color").and_then(|v| v.as_str()) &&
        crate::appearance::parse_hex(color).is_some()
    {
        settings.newtab_color = color.to_string();
    }
    if let Some(tiles) = value.get("newtab_tiles").and_then(|v| v.as_str()) &&
        matches!(tiles, "off" | "bookmarks" | "frequent")
    {
        settings.newtab_tiles = tiles.to_string();
    }
}

fn load_from_disk() -> Settings {
    let mut settings = Settings::default();
    let Some(path) = settings_path() else {
        return settings;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return settings;
    };
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(value) => {
            apply_json(&mut settings, &value);
            if let Some(folder) = value.get("sync_folder").and_then(|v| v.as_str()) {
                settings.sync_folder = folder.to_string();
            }
        },
        Err(error) => warn!("Could not read {}: {error}", path.display()),
    }
    settings
}

fn store() -> &'static RwLock<Settings> {
    static STORE: OnceLock<RwLock<Settings>> = OnceLock::new();
    STORE.get_or_init(|| {
        let loaded = load_from_disk();
        crate::adblock::set_enabled(loaded.adblock);
        crate::memsaver::set_level(crate::memsaver::Level::from_id(&loaded.memory_mode));
        RwLock::new(loaded)
    })
}

/// The current settings.
pub(crate) fn settings() -> Settings {
    store().read().map(|s| s.clone()).unwrap_or_default()
}

/// Minutes before background tabs sleep (cheap: no cloning).
pub(crate) fn tab_sleep_minutes() -> u32 {
    store().read().map(|s| s.tab_sleep_minutes).unwrap_or(5)
}

/// Replace the settings and save them to disk.
pub(crate) fn update(new_settings: Settings) {
    crate::adblock::set_enabled(new_settings.adblock);
    crate::memsaver::set_level(crate::memsaver::Level::from_id(&new_settings.memory_mode));
    crate::sync::changed_soon();
    if let Ok(mut current) = store().write() {
        *current = new_settings.clone();
    }
    let Some(path) = settings_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let json = serde_json::json!({
        "engine": new_settings.engine,
        "searxng_url": new_settings.searxng_url,
        "theme": new_settings.theme,
        "custom_bg": new_settings.custom_bg,
        "tab_sleep_minutes": new_settings.tab_sleep_minutes,
        "sidebar_pinned": new_settings.sidebar_pinned,
        "startup": new_settings.startup,
        "homepage_url": new_settings.homepage_url,
        "history_enabled": new_settings.history_enabled,
        "youtube_player": new_settings.youtube_player,
        "hw_video_decode": new_settings.hw_video_decode,
        "fast_start": new_settings.fast_start,
        "newtab_tiles": new_settings.newtab_tiles,
        "welcome_shown": new_settings.welcome_shown,
        "sync_folder": new_settings.sync_folder,
        "https_warning": new_settings.https_warning,
        "clear_on_exit": new_settings.clear_on_exit,
        "adblock": new_settings.adblock,
        "accent": new_settings.accent,
        "accent_custom": new_settings.accent_custom,
        "newtab_bg": new_settings.newtab_bg,
        "newtab_color": new_settings.newtab_color,
        "compat": new_settings.compat,
        "memory_mode": new_settings.memory_mode,
    });
    if let Err(error) = std::fs::write(&path, json.to_string()) {
        warn!("Could not save {}: {error}", path.display());
    }
}

/// One tab of the saved session.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SavedTab {
    pub url: String,
    pub title: String,
    pub pinned: bool,
    pub active: bool,
}

#[cfg(not(any(target_os = "android", target_env = "ohos")))]
fn session_path() -> Option<PathBuf> {
    ::dirs::config_dir().map(|dir| dir.join(APP_NAME).join("session.json"))
}

#[cfg(any(target_os = "android", target_env = "ohos"))]
fn session_path() -> Option<PathBuf> {
    None
}

/// The tabs saved by the last run (empty if there are none).
pub(crate) fn load_session() -> Vec<SavedTab> {
    let Some(path) = session_path() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        warn!("Could not read {}", path.display());
        return Vec::new();
    };
    value
        .get("tabs")
        .and_then(|tabs| tabs.as_array())
        .map(|tabs| {
            tabs.iter()
                .filter_map(|tab| {
                    let url = tab.get("url")?.as_str()?.to_string();
                    Some(SavedTab {
                        url,
                        title: tab
                            .get("title")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        pinned: tab.get("pinned").and_then(|v| v.as_bool()).unwrap_or(false),
                        active: tab.get("active").and_then(|v| v.as_bool()).unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Save the open tabs. Does nothing if they are the same as the last save.
pub(crate) fn save_session(tabs: &[SavedTab]) {
    static LAST: OnceLock<RwLock<String>> = OnceLock::new();
    let Some(path) = session_path() else {
        return;
    };
    let json = serde_json::json!({
        "tabs": tabs.iter().map(|tab| serde_json::json!({
            "url": tab.url,
            "title": tab.title,
            "pinned": tab.pinned,
            "active": tab.active,
        })).collect::<Vec<_>>(),
    })
    .to_string();
    let last = LAST.get_or_init(|| RwLock::new(String::new()));
    if last.read().map(|last| *last == json).unwrap_or(false) {
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(error) = std::fs::write(&path, &json) {
        warn!("Could not save {}: {error}", path.display());
        return;
    }
    if let Ok(mut last) = last.write() {
        *last = json;
    }
}

/// Forget the saved session.
pub(crate) fn clear_session() {
    if let Some(path) = session_path() {
        let _ = std::fs::remove_file(path);
    }
}

/// What to open when the app starts, besides what the address on the command line says.
pub(crate) struct StartupPlan {
    /// Replaces the default first tab (only when "continue where I left off" applies).
    pub first: Option<SavedTab>,
    /// Tabs to bring back asleep (they load when clicked).
    pub rest: Vec<SavedTab>,
}

pub(crate) fn startup_plan(address_given: bool) -> StartupPlan {
    let restore = settings().startup == "restore";
    let mut wanted: Vec<SavedTab> = load_session()
        .into_iter()
        .filter(|tab| restore || tab.pinned)
        .collect();
    let mut first = None;
    if restore && !address_given && !wanted.is_empty() {
        let index = wanted.iter().position(|tab| tab.active).unwrap_or(0);
        first = Some(wanted.remove(index));
    }
    StartupPlan { first, rest: wanted }
}

/// The search URL template (with `%s` for the query) for the given settings.
pub(crate) fn search_url_for(settings: &Settings) -> String {
    let builtin = |id: &str| {
        ENGINES
            .iter()
            .find(|(engine_id, ..)| *engine_id == id)
            .map(|(.., template)| template.to_string())
    };
    if settings.engine == SEARXNG_ID {
        let address = settings.searxng_url.trim();
        if address.is_empty() {
            return builtin(DEFAULT_ENGINE).unwrap_or_default();
        }
        let address = if address.starts_with("http://") || address.starts_with("https://") {
            address.to_string()
        } else {
            format!("https://{address}")
        };
        if address.contains("%s") {
            return address;
        }
        return format!("{}/search?q=%s", address.trim_end_matches('/'));
    }
    builtin(&settings.engine)
        .or_else(|| builtin(DEFAULT_ENGINE))
        .unwrap_or_default()
}

/// Where the settings file lives, for display in the About page.
pub(crate) fn settings_file_path() -> String {
    settings_path()
        .map(|path| path.display().to_string())
        .unwrap_or_default()
}

/// The search URL template for the current settings.
pub(crate) fn search_url() -> String {
    search_url_for(&settings())
}

/// Percent-encode a search query.
pub(crate) fn encode_query(query: &str) -> String {
    url::form_urlencoded::byte_serialize(query.as_bytes()).collect()
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Display name of the chosen search engine.
fn engine_label(settings: &Settings) -> String {
    if settings.engine == SEARXNG_ID {
        return "SearXNG".to_string();
    }
    ENGINES
        .iter()
        .find(|(id, ..)| *id == settings.engine)
        .map(|(_, label, _)| label.to_string())
        .unwrap_or_else(|| "the web".to_string())
}

/// One round tile on the new tab page.
fn tile_html(suggestion: &crate::library::Suggestion) -> String {
    let host = url::Url::parse(&suggestion.url)
        .ok()
        .and_then(|url| url.host_str().map(|host| host.trim_start_matches("www.").to_string()))
        .unwrap_or_default();
    let letter = host
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "·".to_string());
    let name = if suggestion.title.is_empty() { host.clone() } else { suggestion.title.clone() };
    format!(
        "<a class=\"tile\" href=\"{}\"><span class=\"ico\">{}</span><span class=\"name\">{}</span></a>",
        html_escape(&suggestion.url),
        html_escape(&letter),
        html_escape(&name),
    )
}

/// Tiles for the pages the user bookmarked or visits most.
fn shortcuts_html() -> String {
    let mut html = String::new();
    let mode = settings().newtab_tiles;
    if mode == "off" {
        return html;
    }
    for suggestion in crate::library::shortcuts(8, mode == "frequent") {
        html.push_str(&tile_html(&suggestion));
    }
    let folders = crate::library::folder_shortcuts();
    if !folders.is_empty() {
        html.push_str(
            "<style>.folders{width:100%;margin-top:14px;text-align:left}.folders details{margin:6px 0}\
.folders summary{cursor:pointer;padding:6px 4px;opacity:.85}.folders .shortcuts{margin-top:6px}</style><div class=\"folders\">",
        );
        for (name, pages) in folders {
            html.push_str(&format!("<details><summary>{}</summary><div class=\"shortcuts\">", html_escape(&name)));
            for page in &pages {
                html.push_str(&tile_html(page));
            }
            html.push_str("</div></details>");
        }
        html.push_str("</div>");
    }
    html
}

/// The HTML of the new-tab page, with its search box wired to the chosen engine.
pub(crate) fn newtab_html() -> String {
    let template = search_url();
    let (action, query) = template.split_once('?').unwrap_or((template.as_str(), ""));
    let mut query_name = "q";
    let mut hidden = String::new();
    for pair in query.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            if value == "%s" {
                query_name = key;
            } else {
                hidden.push_str(&format!(
                    "<input type=\"hidden\" name=\"{}\" value=\"{}\" />",
                    html_escape(key),
                    html_escape(value)
                ));
            }
        }
    }
    include_str!("../../resources/resource_protocol/newtab.html")
        .replace("{{FORM_ACTION}}", &html_escape(action))
        .replace("{{HIDDEN_INPUTS}}", &hidden)
        .replace("{{QUERY_NAME}}", &html_escape(query_name))
        .replace("{{ENGINE_NAME}}", &html_escape(&engine_label(&settings())))
        .replace("{{SHORTCUTS}}", &shortcuts_html())
        .replace("{{THEME_STYLE}}", &crate::appearance::newtab_style(&settings()))
}


// ---------------------------------------------------------------------------
// Popup blocking: a page may only open a new window shortly after the user
// clicked or pressed a key. Servo does not tell the shell whether a
// `window.open()` came from a user gesture, so the shell tracks it itself.
// ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

static CLOCK: OnceLock<Instant> = OnceLock::new();
static LAST_GESTURE_MS: AtomicU64 = AtomicU64::new(0);
static LAST_BLOCKED_MS: AtomicU64 = AtomicU64::new(0);
static BLOCKED_COUNT: AtomicU32 = AtomicU32::new(0);

/// How long after a click or key press a page may still open a window.
const GESTURE_WINDOW_MS: u64 = 2000;
/// How long the "popup blocked" notice stays on screen.
const NOTICE_MS: u64 = 4000;

fn now_ms() -> u64 {
    CLOCK.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

/// Call on every mouse press / key press that reaches the window.
pub(crate) fn note_user_gesture() {
    LAST_GESTURE_MS.store(now_ms(), Ordering::Relaxed);
}

/// Whether a page may open a new window right now.
pub(crate) fn popup_allowed() -> bool {
    let last = LAST_GESTURE_MS.load(Ordering::Relaxed);
    last != 0 && now_ms().saturating_sub(last) <= GESTURE_WINDOW_MS
}

/// Whether the user clicked or pressed a key within the last `ms` milliseconds.
pub(crate) fn gesture_within(ms: u64) -> bool {
    let last = LAST_GESTURE_MS.load(Ordering::Relaxed);
    last != 0 && now_ms().saturating_sub(last) <= ms
}

pub(crate) fn note_popup_blocked() {
    BLOCKED_COUNT.fetch_add(1, Ordering::Relaxed);
    LAST_BLOCKED_MS.store(now_ms(), Ordering::Relaxed);
}

/// `Some(count)` while the "popup blocked" notice should be visible.
pub(crate) fn popup_notice() -> Option<u32> {
    let last = LAST_BLOCKED_MS.load(Ordering::Relaxed);
    (last != 0 && now_ms().saturating_sub(last) <= NOTICE_MS)
        .then(|| BLOCKED_COUNT.load(Ordering::Relaxed))
}


/// True once, the first time Pteron starts: the welcome page is shown and remembered as shown.
pub(crate) fn take_first_run() -> bool {
    let mut current = settings();
    if current.welcome_shown {
        return false;
    }
    current.welcome_shown = true;
    update(current);
    true
}

pub(crate) const PAGE_STYLE_PUBLIC: &str = PAGE_STYLE;
const PAGE_STYLE: &str = "<meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<style>:root{color-scheme:dark light}body{font:16px/1.55 system-ui,Segoe UI,sans-serif;max-width:680px;margin:48px auto;padding:0 20px;background:#16181d;color:#e6e8ec}\
@media (prefers-color-scheme:light){body{background:#fafafa;color:#1c1e22}.card{background:#fff;border-color:#dcdfe4}code{background:#eceef2}}\
h1{font-size:28px;margin:0 0 4px}h2{font-size:18px;margin:28px 0 8px}p{margin:8px 0}.sub{opacity:.7;margin-bottom:24px}\
.card{background:#1f2229;border:1px solid #2c303a;border-radius:10px;padding:4px 18px 12px;margin:14px 0}\
li{margin:6px 0}code{background:#2a2e38;padding:1px 6px;border-radius:5px}a{color:#6ea8ff}.btn{display:inline-block;background:#6ea8ff;color:#10131a;padding:9px 16px;border-radius:8px;text-decoration:none;font-weight:600}</style>";

/// The page shown the first time Pteron opens.
pub(crate) fn welcome_html() -> String {
    format!(
        "<!doctype html><title>Welcome to Pteron</title>{PAGE_STYLE}<h1>Welcome to Pteron</h1>\
<p class=sub>A light browser built on the Servo engine. This is an early test version.</p>\
<div class=card><h2>What to expect</h2><ul>\
<li>It uses much less memory than the big browsers, and background tabs go to sleep by themselves.</li>\
<li>YouTube opens in Pteron's own simple player, and <b>Music</b> in its header opens Plume Music for listening.</li>\
<li>It is young: some websites look wrong, load slowly or do not work. <a href=\"servo:known-issues\">See the list of known problems.</a></li></ul></div>\
<div class=card><h2>Found a problem?</h2>\
<p>Open the sidebar, then <b>Settings &rarr; About &rarr; Report a problem</b>. It saves a small file on your Desktop with the details. Send that file to whoever gave you Pteron.</p></div>\
<div class=card><h2>Updates</h2><p>Pteron updates itself from <b>Settings &rarr; About</b>. Nothing is installed without your say-so.</p></div>"
    )
}

/// The page listing what is known not to work yet.
pub(crate) fn known_issues_html() -> String {
    format!(
        "<!doctype html><title>Known problems</title>{PAGE_STYLE}<h1>Known problems</h1>\
<p class=sub>Pteron build {}. This list is what is already known, so you do not need to report these.</p>\
<div class=card><h2>Websites</h2><ul>\
<li>Sites that play protected (DRM) media do not work: Spotify, Netflix, Disney+ and similar.</li>\
<li>Sites with strict anti-bot checks (such as a login behind a reCAPTCHA) may not let you in.</li>\
<li>Some modern web pages look wrong, stay blank, or load slowly, because the engine does not support every web feature yet.</li>\
<li>The text cursor may not show in some text boxes (being looked at).</li></ul></div>\
<div class=card><h2>Reading mode, sync and security</h2><ul>\
<li>Reading mode (the <b>Aa</b> button) works on articles; on other pages it says it found nothing.</li>\
<li>Sync works through a folder you choose and covers bookmarks and settings only. If both computers change the same thing before they sync, the newer change wins.</li>\
<li>Bookmark folders are one level deep. Address bar shortcuts: <b>g</b>, <b>ddg</b>, <b>yt</b>, <b>w</b>, <b>gh</b>, <b>maps</b>, <b>r</b> followed by a space and your search (for example <i>yt lofi</i>).</li>\
<li>On Google results the &quot;About this result&quot; dots and the translate link are hidden, because the engine draws them in the wrong place.</li>\
<li>The http warning appears for sites without https. Some old sites have no https version.</li></ul></div>\
<div class=card><h2>YouTube</h2><ul>\
<li>Videos play up to 1080p60 in the H.264 format. VP9, AV1 and 4K are not supported.</li>\
<li>Very long videos can take several seconds to become fully seekable, and YouTube may slow requests down.</li>\
<li>1080p60 on a slow computer can look soft or stutter. Try 720p60.</li></ul></div>\
<div class=card><h2>Plume Music</h2><ul>\
<li>Songs come from YouTube, so search results are ordinary YouTube results, not a curated music catalogue.</li>\
<li>Very long tracks (mixes over about 20 minutes) take longer to start.</li>\
<li>Music keeps playing while you use other tabs, but only while the Music tab stays open.</li></ul></div>\
<div class=card><h2>The browser</h2><ul>\
<li>Windows only for now. No extensions and no private window. Downloads (the buttons in Plume and Plume Music) save .mp4 video and .mp3 audio. Saved passwords work on ordinary login forms; some sites with unusual forms may not offer to save, and there is no import from other browsers yet. The ad blocker only covers well-known ad and tracker networks (turn it off in Settings if a page looks broken).</li>\
<li>The installer is not signed yet, so Windows shows a &quot;protected your PC&quot; warning. Choose <b>More info</b>, then <b>Run anyway</b>.</li>\
<li>The memory savings have not been measured against other browsers yet.</li></ul></div>\
<p>Something not on this list? <b>Settings &rarr; About &rarr; Report a problem</b>.</p>",
        crate::updater::BUILD
    )
}

/// Servo mis-lays-out two small widgets in Google results (the "About this
/// result" dots and the translate link) on top of the titles. Hide them.
pub(crate) const GOOGLE_FIX_SCRIPT: &str = r#"(function(){try{
var h=location.hostname;
if(!/(^|\.)google\.[a-z.]+$/.test(h))return;
var s=document.createElement('style');
s.textContent='#rso [role="button"][aria-haspopup],#rso a[href*="translate.google."],#rso a[href*="/search?q=related"]{display:none!important}';
(document.head||document.documentElement).appendChild(s);
// Google's AI overview: "Show more" does nothing in Servo, so open the clipped box ourselves.
var MORE=/^(rodyti daugiau|rodyti viską|show more|show all|more)$/i;
document.addEventListener('click',function(ev){try{
var b=ev.target&&ev.target.closest&&ev.target.closest('[role="button"],button,[aria-expanded]');
if(!b)return;
var t=(b.innerText||b.textContent||'').trim();
if(!(MORE.test(t)||(b.getAttribute('aria-expanded')==='false'&&b.closest('#rso,#center_col,#main,main'))))return;
setTimeout(function(){try{
var n=b.parentElement,k=0;
while(n&&n!==document.body&&k<14){
var cs=getComputedStyle(n);
if(cs.overflowY!=='visible'&&n.scrollHeight>n.clientHeight+4){
n.style.setProperty('max-height','none','important');
n.style.setProperty('height','auto','important');
n.style.setProperty('overflow','visible','important');
}
n=n.parentElement;k++;
}
var w=b.parentElement;
var box=(w&&w.children.length===1)?w:b;
box.style.setProperty('display','none','important');
}catch(e){}},250);
}catch(e){}},true);
}catch(e){}})();"#;

#[cfg(test)]
mod memory_mode_tests {
    use super::*;

    #[test]
    fn memory_mode_is_read_and_validated() {
        let mut settings = Settings::default();
        assert_eq!(settings.memory_mode, "normal");
        apply_json(&mut settings, &serde_json::json!({ "memory_mode": "lowest" }));
        assert_eq!(settings.memory_mode, "lowest");
        apply_json(&mut settings, &serde_json::json!({ "memory_mode": "nonsense" }));
        assert_eq!(settings.memory_mode, "normal");
    }
}

#[cfg(test)]
mod compat_setting_tests {
    use super::*;

    #[test]
    fn compat_list_is_read_and_cleaned() {
        let mut settings = Settings::default();
        assert!(settings.compat.is_empty());
        let value = serde_json::json!({ "compat": ["fonts", "bogus", "fonts", "canvas", 7] });
        apply_json(&mut settings, &value);
        assert_eq!(settings.compat, vec!["fonts".to_string(), "canvas".to_string()]);
        // A file without the key leaves the choice alone.
        apply_json(&mut settings, &serde_json::json!({ "adblock": true }));
        assert_eq!(settings.compat.len(), 2);
    }
}

#[cfg(test)]
mod google_fix_tests {
    #[test]
    fn script_is_scoped_to_google() {
        assert!(super::GOOGLE_FIX_SCRIPT.contains("google\\.[a-z.]+$"));
        assert!(super::GOOGLE_FIX_SCRIPT.contains("aria-haspopup"));
        assert!(super::GOOGLE_FIX_SCRIPT.contains("rodyti daugiau"));
    }
}

/// Address bar shortcuts: "yt lofi" searches YouTube, "w rust" Wikipedia, and so on.
pub(crate) const KEYWORDS: &[(&str, &str, &str)] = &[
    ("g", "Google", "https://www.google.com/search?q=%s"),
    ("ddg", "DuckDuckGo", "https://duckduckgo.com/?q=%s"),
    ("yt", "YouTube", "https://www.youtube.com/results?search_query=%s"),
    ("w", "Wikipedia", "https://en.wikipedia.org/w/index.php?search=%s"),
    ("gh", "GitHub", "https://github.com/search?q=%s"),
    ("maps", "OpenStreetMap", "https://www.openstreetmap.org/search?query=%s"),
    ("r", "Reddit", "https://www.reddit.com/search/?q=%s"),
];

/// The row shown first under the address bar for a shortcut: ("Search YouTube for lofi", url).
pub(crate) fn keyword_hint(text: &str) -> Option<(String, String)> {
    let url = expand_keyword(text)?;
    let (word, rest) = text.trim().split_once(' ')?;
    let word = word.to_lowercase();
    let (_, name, _) = KEYWORDS.iter().find(|(key, ..)| *key == word)?;
    Some((format!("Search {name} for {}", rest.trim()), url))
}

/// "yt lofi beats" -> the YouTube results URL. `None` when the text does not start with a
/// shortcut followed by a space and something to search for.
pub(crate) fn expand_keyword(text: &str) -> Option<String> {
    let text = text.trim();
    let (word, rest) = text.split_once(' ')?;
    let rest = rest.trim();
    if rest.is_empty() {
        return None;
    }
    let word = word.to_lowercase();
    let (_, _, template) = KEYWORDS.iter().find(|(key, ..)| *key == word)?;
    Some(template.replace("%s", &encode_query(rest)))
}

#[cfg(test)]
mod keyword_tests {
    use super::*;

    #[test]
    fn shortcuts_expand() {
        assert!(expand_keyword("yt lofi").unwrap().starts_with("https://www.youtube.com/results?search_query=lofi"));
        assert!(expand_keyword("W rust lang").unwrap().contains("wikipedia.org"));
        assert_eq!(keyword_hint("yt lofi beats").unwrap().0, "Search YouTube for lofi beats");
        assert_eq!(keyword_hint("nope x"), None);
        assert_eq!(expand_keyword("yt"), None);
        assert_eq!(expand_keyword("yt   "), None);
        assert_eq!(expand_keyword("hello world"), None);
    }
}

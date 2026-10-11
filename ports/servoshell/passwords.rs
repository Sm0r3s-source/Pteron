/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Pteron: saved logins.
//!
//! A small script runs in every top-level page. When a login form is sent it reports the user
//! name and password through the page console; this module catches that message (it never
//! reaches the log), and the browser asks "Save password?". When a page with a password field
//! opens and logins are saved for the site, the browser offers to fill them. Nothing is ever
//! filled without a click.
//!
//! Passwords are encrypted with Windows' own protection (DPAPI, tied to this Windows account)
//! before they touch the disk. The file is `%APPDATA%\Pteron\passwords.json`. Passwords are
//! never synced and never put in problem reports.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use url::Url;

const SAVE_PREFIX: &str = "__pteron_pw__";
const FORM_MARK: &str = "__pteron_pwform__";
const SAVE_WAIT: Duration = Duration::from_secs(120);
const MAX_FIELD: usize = 256;
const MAX_LOGINS: usize = 2000;

/// Runs at the start of every page. Only the top-level page is looked at, so a login form inside
/// an embedded frame can never be saved under the wrong site.
pub(crate) const SCRIPT: &str = r#"(function(){try{
if(window!==window.top||window.__pteronPw)return;window.__pteronPw=1;
var P='__pteron_pw__',F='__pteron_pwform__';
function vis(e){var r=e.getBoundingClientRect();return r.width>0&&r.height>0;}
function pws(root){return [].filter.call((root||document).querySelectorAll('input[type=password]'),vis);}
function userFor(pw){var root=pw.form||document,all=[].slice.call(root.querySelectorAll('input')),i=all.indexOf(pw);
for(var k=i-1;k>=0;k--){var e=all[k],t=(e.type||'text').toLowerCase();
if((t==='text'||t==='email'||t==='tel')&&e.value&&vis(e))return e.value;}return '';}
var last=0;
function report(root){try{var f=pws(root).filter(function(e){return e.value;});if(!f.length)return;
var pw=f[f.length-1],now=Date.now();if(now-last<1500)return;last=now;
console.log(P+JSON.stringify({u:userFor(pw),p:pw.value}));}catch(e){}}
document.addEventListener('submit',function(e){report(e.target);},true);
document.addEventListener('click',function(e){var b=e.target&&e.target.closest&&e.target.closest('button,input[type=submit],input[type=button],[role=button]');
if(b)report(b.form||b.closest('form')||document);},true);
document.addEventListener('keydown',function(e){if(e.key==='Enter'&&e.target&&e.target.type==='password')report(e.target.form||document);},true);
var told=false,n=0;
var timer=setInterval(function(){n++;if(!told&&pws().length){told=true;console.log(F);}
if(told||n>10)clearInterval(timer);},800);
}catch(e){}})();"#;

#[derive(Clone, Debug, PartialEq)]
struct Login {
    host: String,
    user: String,
    /// The password, protected by Windows and written in base64.
    pw: String,
}

#[derive(Debug, PartialEq)]
struct Vault {
    enabled: bool,
    never: Vec<String>,
    logins: Vec<Login>,
}

impl Vault {
    fn to_json(&self) -> String {
        let logins: Vec<Value> = self
            .logins
            .iter()
            .map(|l| json!({"host": l.host, "user": l.user, "pw": l.pw}))
            .collect();
        serde_json::to_string_pretty(&json!({"enabled": self.enabled, "never": self.never, "logins": logins}))
            .unwrap_or_default()
    }

    fn from_json(text: &str) -> Option<Vault> {
        let v: Value = serde_json::from_str(text).ok()?;
        let text_of = |x: &Value, key: &str| x.get(key).and_then(|s| s.as_str()).map(str::to_string);
        Some(Vault {
            enabled: v.get("enabled").and_then(|b| b.as_bool()).unwrap_or(true),
            never: v
                .get("never")
                .and_then(|n| n.as_array())
                .map(|n| n.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
                .unwrap_or_default(),
            logins: v
                .get("logins")
                .and_then(|l| l.as_array())
                .map(|list| {
                    list.iter()
                        .filter_map(|x| {
                            Some(Login { host: text_of(x, "host")?, user: text_of(x, "user")?, pw: text_of(x, "pw")? })
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
    }
}

impl Default for Vault {
    fn default() -> Self {
        Vault { enabled: true, never: Vec::new(), logins: Vec::new() }
    }
}

struct Pending {
    host: String,
    user: String,
    password: String,
    at: Instant,
}

struct Offer {
    host: String,
    page: String,
    at: Instant,
}

struct Ready {
    host: String,
    script: String,
}

#[derive(Default)]
struct State {
    vault: Option<Vault>,
    pending: Option<Pending>,
    offer: Option<Offer>,
    ready: Option<Ready>,
    busy: bool,
    message: Option<(String, Instant)>,
}

static STATE: Mutex<State> = Mutex::new(State {
    vault: None,
    pending: None,
    offer: None,
    ready: None,
    busy: false,
    message: None,
});

// ---------------------------------------------------------------- sites

/// The key a login is filed under: the host without `www.` (and the port when there is one).
/// Only secure pages (and the user's own computer) are used.
pub(crate) fn site_key(url: &Url) -> Option<String> {
    let host = url.host_str()?.to_ascii_lowercase();
    let local = host == "localhost" || host == "127.0.0.1";
    if !(url.scheme() == "https" || (url.scheme() == "http" && local)) {
        return None;
    }
    let host = host.strip_prefix("www.").unwrap_or(&host).to_string();
    Some(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    })
}

// ---------------------------------------------------------------- the file

#[cfg(not(any(target_os = "android", target_env = "ohos")))]
fn file_path() -> Option<PathBuf> {
    ::dirs::config_dir().map(|dir| dir.join(crate::pteron::APP_NAME).join("passwords.json"))
}

#[cfg(any(target_os = "android", target_env = "ohos"))]
fn file_path() -> Option<PathBuf> {
    None
}

fn load() -> Vault {
    file_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| Vault::from_json(&text))
        .unwrap_or_default()
}

fn store(vault: &Vault) {
    let Some(path) = file_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text = vault.to_json();
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

fn with_vault<R>(f: impl FnOnce(&mut Vault) -> R) -> R {
    let mut state = STATE.lock().unwrap();
    let vault = state.vault.get_or_insert_with(load);
    f(vault)
}

fn save_vault_now() {
    let state = STATE.lock().unwrap();
    if let Some(vault) = &state.vault {
        store(vault);
    }
}

// ---------------------------------------------------------------- Windows protection

fn b64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

fn b64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buffer = 0u32;
    let mut bits = 0;
    for c in text.trim().bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return None,
        } as u32;
        buffer = buffer << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(windows)]
fn dpapi(protect: bool, input_b64: &str) -> Result<String, String> {
    use std::io::Write;
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;
    let action = if protect {
        "[Convert]::ToBase64String([Security.Cryptography.ProtectedData]::Protect($b,$e,'CurrentUser'))"
    } else {
        "[Convert]::ToBase64String([Security.Cryptography.ProtectedData]::Unprotect($b,$e,'CurrentUser'))"
    };
    let script = format!(
        "$ErrorActionPreference='Stop';Add-Type -AssemblyName System.Security;\
         $e=[Text.Encoding]::UTF8.GetBytes('Pteron-passwords-v1');\
         $b=[Convert]::FromBase64String([Console]::In.ReadToEnd().Trim());{action}"
    );
    let mut child = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script.as_str()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(0x0800_0000)
        .spawn()
        .map_err(|e| format!("Could not start Windows protection: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input_b64.as_bytes());
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.status.success() && !text.is_empty() {
        Ok(text)
    } else {
        Err("Windows could not protect or open the password.".into())
    }
}

#[cfg(not(windows))]
fn dpapi(_protect: bool, _input_b64: &str) -> Result<String, String> {
    Err("Saved passwords need Windows.".into())
}

fn protect(plain: &str) -> Result<String, String> {
    dpapi(true, &b64_encode(plain.as_bytes()))
}

fn unprotect(blob: &str) -> Result<String, String> {
    let bytes = b64_decode(&dpapi(false, blob)?).ok_or("Unreadable password.")?;
    String::from_utf8(bytes).map_err(|_| "Unreadable password.".to_string())
}

// ---------------------------------------------------------------- page messages

/// Called for every console message. Returns true when it was one of ours (the message is then
/// not logged, since it can hold a password). `page` is the address of the tab it came from.
pub(crate) fn handle_console(page: Option<&Url>, message: &str) -> bool {
    if let Some(json) = message.strip_prefix(SAVE_PREFIX) {
        if let Some(page) = page {
            on_login_sent(page, json);
        }
        return true;
    }
    if message.trim() == FORM_MARK {
        if let Some(page) = page {
            on_form_seen(page);
        }
        return true;
    }
    false
}

fn on_login_sent(page: &Url, json: &str) {
    let Some(host) = site_key(page) else { return };
    let Ok(sent) = serde_json::from_str::<Value>(json) else { return };
    let field = |key: &str| sent.get(key).and_then(|v| v.as_str()).map(str::to_string);
    let (Some(user), Some(password)) = (field("u"), field("p")) else { return };
    let sent = (user, password);
    if sent.1.is_empty() || sent.1.chars().count() > MAX_FIELD || sent.0.chars().count() > MAX_FIELD {
        return;
    }
    let mut state = STATE.lock().unwrap();
    let vault = state.vault.get_or_insert_with(load);
    if !vault.enabled || vault.never.contains(&host) {
        return;
    }
    state.pending = Some(Pending { host, user: sent.0, password: sent.1, at: Instant::now() });
}

fn on_form_seen(page: &Url) {
    let Some(host) = site_key(page) else { return };
    let mut state = STATE.lock().unwrap();
    let vault = state.vault.get_or_insert_with(load);
    if !vault.enabled || !vault.logins.iter().any(|l| l.host == host) {
        return;
    }
    state.offer = Some(Offer { host, page: page.as_str().to_string(), at: Instant::now() });
}

// ---------------------------------------------------------------- asking the user

/// What the "Save password?" bar should say: (site, user name, whether it replaces an old one).
pub(crate) fn save_prompt() -> Option<(String, String, bool)> {
    let mut state = STATE.lock().unwrap();
    if state.pending.as_ref().is_some_and(|p| p.at.elapsed() > SAVE_WAIT) {
        state.pending = None;
    }
    let pending = state.pending.as_ref()?;
    let (host, user) = (pending.host.clone(), pending.user.clone());
    let vault = state.vault.get_or_insert_with(load);
    let exists = vault.logins.iter().any(|l| l.host == host && l.user == user);
    Some((host, user, exists))
}

pub(crate) fn dismiss_save(never_for_site: bool) {
    let mut state = STATE.lock().unwrap();
    if let Some(pending) = state.pending.take() &&
        never_for_site
    {
        let vault = state.vault.get_or_insert_with(load);
        if !vault.never.contains(&pending.host) {
            vault.never.push(pending.host);
        }
        drop(state);
        save_vault_now();
    }
}

/// Encrypt and file the password the bar asked about. Runs in the background.
pub(crate) fn accept_save() {
    let Some(pending) = STATE.lock().unwrap().pending.take() else { return };
    std::thread::Builder::new()
        .name("pteron-passwords".into())
        .spawn(move || {
            let result = protect(&pending.password);
            let mut state = STATE.lock().unwrap();
            match result {
                Ok(blob) => {
                    let vault = state.vault.get_or_insert_with(load);
                    vault.logins.retain(|l| !(l.host == pending.host && l.user == pending.user));
                    if vault.logins.len() < MAX_LOGINS {
                        vault.logins.push(Login { host: pending.host.clone(), user: pending.user, pw: blob });
                    }
                    drop(state);
                    save_vault_now();
                    say(format!("Password saved for {}.", pending.host));
                },
                Err(message) => {
                    drop(state);
                    say(message);
                },
            }
        })
        .ok();
}

fn say(text: String) {
    STATE.lock().unwrap().message = Some((text, Instant::now()));
}

/// A short result message ("Password saved...") for a few seconds.
pub(crate) fn message() -> Option<String> {
    let mut state = STATE.lock().unwrap();
    match &state.message {
        Some((text, at)) if at.elapsed() < Duration::from_secs(4) => Some(text.clone()),
        Some(_) => {
            state.message = None;
            None
        },
        None => None,
    }
}

/// The user names that can be filled on `page`, when the page just showed a password field.
pub(crate) fn fill_offer(page: &Url) -> Option<(String, Vec<String>)> {
    let mut state = STATE.lock().unwrap();
    let offer = state.offer.as_ref()?;
    if offer.at.elapsed() > Duration::from_secs(600) || offer.page != page.as_str() {
        return None;
    }
    let host = offer.host.clone();
    let vault = state.vault.get_or_insert_with(load);
    let users: Vec<String> = vault.logins.iter().filter(|l| l.host == host).map(|l| l.user.clone()).collect();
    (!users.is_empty()).then_some((host, users))
}

pub(crate) fn dismiss_offer() {
    STATE.lock().unwrap().offer = None;
}

/// The script that puts a login into the page.
fn fill_script(user: &str, password: &str) -> String {
    let user = serde_json::to_string(user).unwrap_or_else(|_| "\"\"".into());
    let password = serde_json::to_string(password).unwrap_or_else(|_| "\"\"".into());
    format!(
        "(function(u,p){{try{{function vis(e){{var r=e.getBoundingClientRect();return r.width>0&&r.height>0;}}\
         function set(el,v){{var d=Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value');\
         if(d&&d.set)d.set.call(el,v);else el.value=v;\
         el.dispatchEvent(new Event('input',{{bubbles:true}}));el.dispatchEvent(new Event('change',{{bubbles:true}}));}}\
         var pw=[].filter.call(document.querySelectorAll('input[type=password]'),vis)[0];if(!pw)return;\
         var root=pw.form||document,all=[].slice.call(root.querySelectorAll('input')),i=all.indexOf(pw);\
         if(u){{for(var k=i-1;k>=0;k--){{var e=all[k],t=(e.type||'text').toLowerCase();\
         if((t==='text'||t==='email'||t==='tel')&&vis(e)){{set(e,u);break;}}}}}}\
         set(pw,p);}}catch(e){{}}}})({user},{password});"
    )
}

/// Start opening login number `index` (of those for the offered site) in the background.
pub(crate) fn request_fill(index: usize) {
    let (host, blob, user) = {
        let mut state = STATE.lock().unwrap();
        let Some(host) = state.offer.take().map(|o| o.host) else { return };
        if state.busy {
            return;
        }
        let vault = state.vault.get_or_insert_with(load);
        let Some(login) = vault.logins.iter().filter(|l| l.host == host).nth(index).cloned() else { return };
        state.busy = true;
        (host, login.pw, login.user)
    };
    std::thread::Builder::new()
        .name("pteron-passwords".into())
        .spawn(move || {
            let result = unprotect(&blob);
            let mut state = STATE.lock().unwrap();
            state.busy = false;
            match result {
                Ok(password) => state.ready = Some(Ready { host, script: fill_script(&user, &password) }),
                Err(message) => state.message = Some((message, Instant::now())),
            }
        })
        .ok();
}

/// A fill script that is ready for the page at `page`, if any (and only for the same site).
pub(crate) fn take_fill_script(page: &Url) -> Option<String> {
    let mut state = STATE.lock().unwrap();
    let ready = state.ready.take()?;
    (site_key(page).as_deref() == Some(ready.host.as_str())).then_some(ready.script)
}

/// True while something is working in the background (so the window keeps repainting).
pub(crate) fn working() -> bool {
    let state = STATE.lock().unwrap();
    state.busy || state.ready.is_some()
}

// ---------------------------------------------------------------- settings page

pub(crate) fn enabled() -> bool {
    with_vault(|v| v.enabled)
}

pub(crate) fn set_enabled(on: bool) {
    with_vault(|v| v.enabled = on);
    save_vault_now();
}

/// Every saved login as (site, user name).
pub(crate) fn list() -> Vec<(String, String)> {
    with_vault(|v| v.logins.iter().map(|l| (l.host.clone(), l.user.clone())).collect())
}

pub(crate) fn delete(index: usize) {
    with_vault(|v| {
        if index < v.logins.len() {
            v.logins.remove(index);
        }
    });
    save_vault_now();
}

pub(crate) fn delete_all() {
    with_vault(|v| v.logins.clear());
    save_vault_now();
}

pub(crate) fn never_list() -> Vec<String> {
    with_vault(|v| v.never.clone())
}

pub(crate) fn allow_again(index: usize) {
    with_vault(|v| {
        if index < v.never.len() {
            v.never.remove(index);
        }
    });
    save_vault_now();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn site_keys() {
        assert_eq!(site_key(&url("https://www.Example.com/login")).as_deref(), Some("example.com"));
        assert_eq!(site_key(&url("https://example.com:8443/")).as_deref(), Some("example.com:8443"));
        assert_eq!(site_key(&url("http://example.com/")), None);
        assert_eq!(site_key(&url("http://localhost:3000/")).as_deref(), Some("localhost:3000"));
        assert_eq!(site_key(&url("file:///a.html")), None);
    }

    #[test]
    fn base64_round_trip() {
        for text in ["", "a", "ab", "abc", "pässwörd ✓ 123!", "x".repeat(300).as_str()] {
            assert_eq!(b64_decode(&b64_encode(text.as_bytes())).unwrap(), text.as_bytes());
        }
        assert_eq!(b64_encode(b"Man"), "TWFu");
        assert!(b64_decode("a$b").is_none());
    }

    #[test]
    fn fill_script_is_quoted() {
        let script = fill_script("a\"b</script>", "p'w\\\n");
        assert!(script.contains("\"a\\\"b</script>\""));
        assert!(script.contains("\"p'w\\\\\\n\""));
        assert!(!script.contains('\n'));
    }

    #[test]
    fn vault_file_round_trip() {
        let vault = Vault {
            enabled: false,
            never: vec!["bank.example".into()],
            logins: vec![Login { host: "a.com".into(), user: "me".into(), pw: "AAAA".into() }],
        };
        let text = vault.to_json();
        assert_eq!(Vault::from_json(&text).unwrap(), vault);
        let old = Vault::from_json("{}").unwrap();
        assert!(old.enabled && old.logins.is_empty());
    }

    #[test]
    fn page_messages_are_ours_and_checked() {
        // Not ours: left alone.
        assert!(!handle_console(Some(&url("https://a.com/")), "hello"));
        // Ours, but from a page we do not save for: swallowed, nothing stored.
        assert!(handle_console(Some(&url("http://a.com/")), "__pteron_pw__{\"u\":\"x\",\"p\":\"y\"}"));
        assert!(handle_console(None, "__pteron_pwform__"));
        // A page's own script cannot make the bar for a different site: the site comes from the
        // tab's address, not from the message.
        let long = format!("__pteron_pw__{{\"u\":\"x\",\"p\":\"{}\"}}", "z".repeat(400));
        assert!(handle_console(Some(&url("https://a.com/")), &long));
        assert!(!STATE.lock().unwrap().pending.as_ref().is_some_and(|p| p.password.len() > MAX_FIELD));
    }

    #[test]
    fn script_only_runs_on_top_pages() {
        assert!(SCRIPT.contains("window!==window.top"));
        assert!(SCRIPT.contains(SAVE_PREFIX) && SCRIPT.contains(FORM_MARK));
    }
}

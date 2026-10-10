/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Pteron: warns before opening a page over plain `http` (anyone on the network can read or
//! change it). The warning page offers the secure `https` version or "continue anyway", which
//! is remembered for that site until Pteron is closed.

use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::{Mutex, OnceLock};

use url::Url;

fn allowed() -> &'static Mutex<HashSet<String>> {
    static ALLOWED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    ALLOWED.get_or_init(|| Mutex::new(HashSet::new()))
}

pub(crate) fn allow_host(host: &str) {
    if let Ok(mut set) = allowed().lock() {
        set.insert(host.to_ascii_lowercase());
    }
}

fn is_allowed(host: &str) -> bool {
    allowed().lock().map(|set| set.contains(&host.to_ascii_lowercase())).unwrap_or(false)
}

/// Is this host on the user's own computer or network, where `http` is normal?
fn is_private_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = host.parse::<IpAddr>() {
        return match ip {
            IpAddr::V4(v4) => {
                v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
            },
            IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified() || (v6.segments()[0] & 0xfe00) == 0xfc00,
        };
    }
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    !host.contains('.') ||
        host == "localhost" ||
        host.ends_with(".localhost") ||
        host.ends_with(".local") ||
        host.ends_with(".lan") ||
        host.ends_with(".internal") ||
        host.ends_with(".home.arpa")
}

/// Should this navigation show the warning instead of loading?
pub(crate) fn needs_warning(url: &Url) -> bool {
    if url.scheme() != "http" || !crate::pteron::settings().https_warning {
        return false;
    }
    match url.host_str() {
        Some(host) => !is_private_host(host) && !is_allowed(host),
        None => false,
    }
}

fn encode(text: &str) -> String {
    url::form_urlencoded::byte_serialize(text.as_bytes()).collect()
}

/// The address of the warning page for `target`.
pub(crate) fn warning_url(target: &Url) -> Option<Url> {
    Url::parse(&format!("servo:http-warning?url={}", encode(target.as_str()))).ok()
}

fn target_from(url: &Url) -> Option<Url> {
    let (_, value) = url.query_pairs().find(|(key, _)| key == "url")?;
    let target = Url::parse(&value).ok()?;
    (target.scheme() == "http").then_some(target)
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The same address over https.
pub(crate) fn secure_version(target: &Url) -> Option<Url> {
    let mut secure = target.clone();
    secure.set_scheme("https").ok()?;
    // A port that was meant for http makes no sense for https.
    let _ = secure.set_port(None);
    Some(secure)
}

/// The page shown instead of an `http` site.
pub(crate) fn warning_html(page_url: &Url) -> String {
    let Some(target) = target_from(page_url) else {
        return "<!doctype html><title>Not secure</title><p>This address cannot be opened.</p>".to_string();
    };
    let host = target.host_str().unwrap_or("this site").to_string();
    let secure = secure_version(&target).map(|u| u.to_string()).unwrap_or_default();
    format!(
        "<!doctype html><title>Not secure</title>{}<h1>This site is not secure</h1>\
<p class=sub>{} uses plain <b>http</b>. Other people on the same network (a caf&eacute;, a shared Wi-Fi) can read \
what you send and change what you see. Do not type passwords or card numbers here.</p>\
<div class=card><h2>What do you want to do?</h2>\
<p><a class=btn href=\"{}\">Try the secure version (https)</a></p>\
<p><a href=\"servo:http-continue?url={}\">Continue to the site anyway</a> (remembered until you close Pteron)</p>\
<p><a href=\"servo:newtab\">Go to the new tab page</a></p></div>\
<p class=sub>You can turn this warning off in Settings &rarr; Privacy &amp; data.</p>",
        crate::pteron::PAGE_STYLE_PUBLIC,
        escape(&host),
        escape(&secure),
        encode(target.as_str()),
    )
}

/// `servo:http-continue?url=...`: remember the site and go on to it.
pub(crate) fn continue_html(page_url: &Url) -> String {
    let Some(target) = target_from(page_url) else {
        return "<!doctype html><title>Not secure</title><p>This address cannot be opened.</p>".to_string();
    };
    if let Some(host) = target.host_str() {
        allow_host(host);
    }
    let address = escape(target.as_str());
    format!(
        "<!doctype html><title>Opening...</title><meta http-equiv=\"refresh\" content=\"0;url={address}\">\
<p><a href=\"{address}\">Continue</a></p>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn warns(url: &str) -> bool {
        needs_warning(&Url::parse(url).unwrap())
    }

    #[test]
    fn warns_only_for_public_http_sites() {
        assert!(warns("http://example.com/page"));
        assert!(warns("http://93.184.216.34/"));
        assert!(!warns("https://example.com/"));
        assert!(!warns("http://localhost:8080/"));
        assert!(!warns("http://127.0.0.1:3000/s/x"));
        assert!(!warns("http://192.168.1.10/"));
        assert!(!warns("http://10.0.0.5/admin"));
        assert!(!warns("http://nas/"));
        assert!(!warns("http://printer.local/"));
        assert!(!warns("http://[::1]/"));
        assert!(!warns("servo:newtab"));
        assert!(!warns("file:///C:/x.html"));
    }

    #[test]
    fn continuing_remembers_the_site() {
        let target = Url::parse("http://remember-me.example/a?b=1&c=2").unwrap();
        assert!(needs_warning(&target));
        let page = warning_url(&target).unwrap();
        let html = warning_html(&page);
        assert!(html.contains("https://remember-me.example/a?b=1&amp;c=2"));
        let go = Url::parse(&format!("servo:http-continue?url={}", encode(target.as_str()))).unwrap();
        assert!(continue_html(&go).contains("refresh"));
        assert!(!needs_warning(&target));
        assert!(!needs_warning(&Url::parse("http://REMEMBER-ME.example/other").unwrap()));
    }

    #[test]
    fn warning_page_refuses_odd_addresses() {
        let bad = Url::parse("servo:http-warning?url=javascript%3Aalert(1)").unwrap();
        assert!(warning_html(&bad).contains("cannot be opened"));
        assert!(!continue_html(&bad).contains("refresh"));
        let other = Url::parse("servo:http-continue?url=https%3A%2F%2Fexample.com").unwrap();
        assert!(!continue_html(&other).contains("refresh"));
    }

    #[test]
    fn secure_version_drops_the_port() {
        let t = Url::parse("http://example.com:8080/x").unwrap();
        assert_eq!(secure_version(&t).unwrap().as_str(), "https://example.com/x");
    }
}

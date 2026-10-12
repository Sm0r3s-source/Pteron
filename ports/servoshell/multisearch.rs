//! Pteron: search several engines at once.
//!
//! Two ways, chosen in Settings: open one tab per engine, or fetch every engine's results
//! in the background and show them merged on one local page. The merged page reads each
//! engine's HTML, which engines change now and then, so an engine whose results cannot be
//! read is shown as a link instead of leaving the page empty.

use url::Url;

use crate::pteron::{self, Settings};

pub(crate) const MAX_ENGINES: usize = 4;

/// What to do with a search typed in the address bar.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Intent {
    /// Open each of these addresses in its own tab (the first in the current tab).
    Tabs(Vec<String>),
    /// Show the merged results page for this search.
    Page { query: String, engines: Vec<String> },
}

/// A search result.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Keeps known engine ids (and the SearXNG entry), once each, at most [`MAX_ENGINES`].
pub(crate) fn clean_engines(ids: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for id in ids {
        let known = id == pteron::SEARXNG_ID || pteron::ENGINES.iter().any(|(engine, ..)| *engine == id);
        if known && !out.contains(&id) && out.len() < MAX_ENGINES {
            out.push(id);
        }
    }
    out
}

fn template_for(settings: &Settings, id: &str) -> Option<String> {
    if id == pteron::SEARXNG_ID && settings.searxng_url.trim().is_empty() {
        return None;
    }
    let mut copy = settings.clone();
    copy.engine = id.to_string();
    let template = pteron::search_url_for(&copy);
    (!template.is_empty()).then_some(template)
}

fn label_for(id: &str) -> String {
    if id == pteron::SEARXNG_ID {
        return "SearXNG".to_string();
    }
    pteron::ENGINES
        .iter()
        .find(|(engine, ..)| *engine == id)
        .map(|(_, label, _)| label.to_string())
        .unwrap_or_else(|| id.to_string())
}

/// Decides whether the text typed in the address bar should be a multi-engine search.
/// `as_url` is what the address bar would normally turn the text into.
pub(crate) fn intent_for(settings: &Settings, input: &str, as_url: Option<&str>) -> Option<Intent> {
    if !settings.multi_search {
        return None;
    }
    let query = input.trim();
    let normal = pteron::search_url_for(settings).replace("%s", &pteron::encode_query(query));
    // Only plain searches: anything the address bar read as a web address stays as it was.
    if query.is_empty() || as_url? != normal {
        return None;
    }
    let engines = clean_engines(settings.multi_engines.clone());
    let templates: Vec<(String, String)> = engines
        .iter()
        .filter_map(|id| template_for(settings, id).map(|t| (id.clone(), t)))
        .collect();
    if templates.len() < 2 {
        return None;
    }
    if settings.multi_mode == "page" {
        return Some(Intent::Page {
            query: query.to_string(),
            engines: templates.into_iter().map(|(id, _)| id).collect(),
        });
    }
    let encoded = pteron::encode_query(query);
    Some(Intent::Tabs(
        templates.into_iter().map(|(_, t)| t.replace("%s", &encoded)).collect(),
    ))
}

/// The page to open for a merged search; starts fetching in the background.
pub(crate) fn start_page(query: &str, engines: &[String]) -> Option<Url> {
    let key = page_key(query, engines);
    let path = crate::youtube::page_path(&key)?;
    if crate::youtube::is_ready(&key) {
        crate::youtube::write_page(&key, &crate::youtube::loading_html("Searching several engines...", &key));
        let query = query.to_string();
        let engines = engines.to_vec();
        let job_key = key.clone();
        crate::youtube::spawn_job(&key, move || {
            let settings = pteron::settings();
            let html = run(&settings, &query, &engines);
            crate::youtube::write_page(&job_key, &html);
        });
    }
    Url::from_file_path(path).ok()
}

fn page_key(query: &str, engines: &[String]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in query.bytes().chain(engines.join(",").bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("s{hash:016x}")
}

struct EngineResult {
    label: String,
    open_url: String,
    outcome: Result<Vec<Hit>, String>,
}

fn run(settings: &Settings, query: &str, engines: &[String]) -> String {
    let encoded = pteron::encode_query(query);
    let results: Vec<EngineResult> = std::thread::scope(|scope| {
        let handles: Vec<_> = engines
            .iter()
            .map(|id| {
                let template = template_for(settings, id).unwrap_or_default();
                let open_url = template.replace("%s", &encoded);
                let id = id.clone();
                scope.spawn(move || {
                    let outcome = fetch_engine(&id, &open_url);
                    EngineResult { label: label_for(&id), open_url, outcome }
                })
            })
            .collect();
        handles.into_iter().filter_map(|handle| handle.join().ok()).collect()
    });
    page_html(query, &results)
}

fn fetch_engine(id: &str, open_url: &str) -> Result<Vec<Hit>, String> {
    let supported = matches!(id, "duckduckgo" | "bing" | "brave") || id == pteron::SEARXNG_ID;
    if !supported {
        return Err("Pteron cannot read this engine's results yet".to_string());
    }
    let fetch_url = if id == pteron::SEARXNG_ID {
        let sep = if open_url.contains('?') { '&' } else { '?' };
        format!("{open_url}{sep}format=json")
    } else {
        open_url.to_string()
    };
    let body = crate::updater::fetch_html(&fetch_url, id).map_err(|_| "it did not answer".to_string())?;
    let hits = match id {
        "duckduckgo" => parse_duckduckgo(&body),
        "bing" => parse_bing(&body),
        "brave" => parse_brave(&body),
        _ => parse_searx_json(&body),
    };
    if hits.is_empty() {
        Err("no results could be read (the engine may have blocked the request)".to_string())
    } else {
        Ok(hits)
    }
}

// ---------------------------------------------------------------- reading results

fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let end = tail.find(';').filter(|end| *end <= 8);
        let Some(end) = end else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let name = &tail[1..end];
        let replacement = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ => name
                .strip_prefix("#x")
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .or_else(|| name.strip_prefix('#').and_then(|dec| dec.parse().ok()))
                .and_then(char::from_u32),
        };
        match replacement {
            Some(c) => {
                out.push(c);
                rest = &tail[end + 1..];
            },
            None => {
                out.push('&');
                rest = &tail[1..];
            },
        }
    }
    out.push_str(rest);
    out
}

fn strip_tags(html: &str) -> String {
    let mut text = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                text.push(' ');
            },
            _ if !in_tag => text.push(c),
            _ => {},
        }
    }
    decode_entities(&text).split_whitespace().collect::<Vec<_>>().join(" ")
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let needle = format!(" {name}=\"");
    let start = tag.find(&needle)? + needle.len();
    let end = tag[start..].find('"')?;
    Some(decode_entities(&tag[start..start + end]))
}

/// The pieces of `html` that start at each `marker` (at most 40).
fn blocks<'a>(html: &'a str, marker: &str) -> Vec<&'a str> {
    let starts: Vec<usize> = html.match_indices(marker).map(|(i, _)| i).take(40).collect();
    starts
        .iter()
        .enumerate()
        .map(|(n, start)| &html[*start..starts.get(n + 1).copied().unwrap_or(html.len())])
        .collect()
}

/// The opening `<a ...>` tag that contains `class_marker`, and the text inside that link.
fn link_with<'a>(block: &'a str, class_marker: &str) -> Option<(&'a str, String)> {
    let at = block.find(class_marker)?;
    let open = block[..at].rfind("<a ")?;
    let close = open + block[open..].find('>')?;
    let tag = &block[open..=close];
    let end = block[close..].find("</a>").map(|e| close + e).unwrap_or(block.len());
    Some((tag, strip_tags(&block[close + 1..end])))
}

fn web_url(raw: &str) -> Option<String> {
    let full = if raw.starts_with("//") { format!("https:{raw}") } else { raw.to_string() };
    let url = Url::parse(&full).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.to_string())
}

pub(crate) fn parse_duckduckgo(html: &str) -> Vec<Hit> {
    let mut hits = Vec::new();
    for block in blocks(html, "class=\"result results_links") {
        let head = &block[..block.find('>').unwrap_or(block.len())];
        if head.contains("result--ad") {
            continue;
        }
        let Some((tag, title)) = link_with(block, "result__a") else { continue };
        let Some(href) = attr(tag, "href") else { continue };
        let full = if href.starts_with("//") { format!("https:{href}") } else { href };
        let target = Url::parse(&full).ok().and_then(|url| {
            if url.path() == "/l/" {
                url.query_pairs().find(|(k, _)| k == "uddg").map(|(_, v)| v.to_string())
            } else {
                Some(url.to_string())
            }
        });
        let Some(url) = target.as_deref().and_then(web_url) else { continue };
        let snippet = link_with(block, "result__snippet").map(|(_, text)| text).unwrap_or_default();
        if !title.is_empty() {
            hits.push(Hit { title, url, snippet });
        }
    }
    hits
}

fn base64url_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buffer = 0u32;
    let mut bits = 0;
    for c in text.chars() {
        let value = match c {
            'A'..='Z' => c as u32 - 'A' as u32,
            'a'..='z' => c as u32 - 'a' as u32 + 26,
            '0'..='9' => c as u32 - '0' as u32 + 52,
            '-' | '+' => 62,
            '_' | '/' => 63,
            '=' => break,
            _ => return None,
        };
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Bing wraps result links in a redirect (`/ck/a?...&u=a1<base64 of the address>`).
fn unwrap_bing(href: &str) -> Option<String> {
    let url = Url::parse(href).ok()?;
    if url.host_str().is_some_and(|h| h.ends_with("bing.com")) && url.path().starts_with("/ck/") {
        let (_, value) = url.query_pairs().find(|(k, _)| k == "u")?;
        let encoded = value.strip_prefix("a1")?;
        let bytes = base64url_decode(encoded)?;
        return web_url(&String::from_utf8(bytes).ok()?);
    }
    web_url(href)
}

pub(crate) fn parse_bing(html: &str) -> Vec<Hit> {
    let mut hits = Vec::new();
    for block in blocks(html, "class=\"b_algo") {
        let Some(h2) = block.find("<h2") else { continue };
        let Some(a) = block[h2..].find("<a ").map(|i| h2 + i) else { continue };
        let Some(close) = block[a..].find('>').map(|i| a + i) else { continue };
        let Some(href) = attr(&block[a..=close], "href") else { continue };
        let Some(url) = unwrap_bing(&href) else { continue };
        let end = block[close..].find("</a>").map(|e| close + e).unwrap_or(block.len());
        let title = strip_tags(&block[close + 1..end]);
        let after = &block[end..];
        let snippet = after
            .find("<p")
            .and_then(|p| after[p..].find("</p>").map(|e| strip_tags(&after[p..p + e])))
            .unwrap_or_default();
        if !title.is_empty() {
            hits.push(Hit { title, url, snippet });
        }
    }
    hits
}

pub(crate) fn parse_brave(html: &str) -> Vec<Hit> {
    let mut hits = Vec::new();
    for block in blocks(html, "class=\"snippet ") {
        if !block.contains("data-type=\"web\"") {
            continue;
        }
        let mut found = None;
        for (at, _) in block.match_indices("<a ").take(6) {
            let Some(close) = block[at..].find('>').map(|i| at + i) else { continue };
            let Some(href) = attr(&block[at..=close], "href") else { continue };
            if let Some(url) = web_url(&href).filter(|u| !u.contains("brave.com")) {
                found = Some((url, close));
                break;
            }
        }
        let Some((url, close)) = found else { continue };
        let title = block
            .find("class=\"title")
            .and_then(|t| {
                let from = block[t..].find('>')? + t + 1;
                let to = block[from..].find("</div>")? + from;
                Some(strip_tags(&block[from..to]))
            })
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| {
                let end = block[close..].find("</a>").map(|e| close + e).unwrap_or(block.len());
                strip_tags(&block[close + 1..end])
            });
        let snippet = block
            .find("class=\"content")
            .and_then(|c| {
                let from = block[c..].find('>')? + c + 1;
                let to = block[from..].find("</div>")? + from;
                Some(strip_tags(&block[from..to]))
            })
            .unwrap_or_default();
        if !title.is_empty() {
            hits.push(Hit { title, url, snippet });
        }
    }
    hits
}

pub(crate) fn parse_searx_json(body: &str) -> Vec<Hit> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else { return Vec::new() };
    value
        .get("results")
        .and_then(|r| r.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|item| {
                    let url = web_url(item.get("url")?.as_str()?)?;
                    let title = strip_tags(item.get("title")?.as_str()?);
                    let snippet = item.get("content").and_then(|c| c.as_str()).map(strip_tags).unwrap_or_default();
                    (!title.is_empty()).then_some(Hit { title, url, snippet })
                })
                .take(40)
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------- merging

fn same_page_key(url: &str) -> String {
    let Ok(parsed) = Url::parse(url) else { return url.to_string() };
    let host = parsed.host_str().unwrap_or("").trim_start_matches("www.");
    format!("{host}{}?{}", parsed.path().trim_end_matches('/'), parsed.query().unwrap_or(""))
}

/// Takes one result from each engine in turn, so no engine fills the top of the list, and
/// joins results that several engines returned. Returns each result with the engines that had it.
pub(crate) fn merge(lists: &[(String, Vec<Hit>)]) -> Vec<(Hit, Vec<String>)> {
    let mut merged: Vec<(Hit, Vec<String>)> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let longest = lists.iter().map(|(_, hits)| hits.len()).max().unwrap_or(0);
    for round in 0..longest {
        for (label, hits) in lists {
            let Some(hit) = hits.get(round) else { continue };
            let key = same_page_key(&hit.url);
            if let Some(at) = seen.iter().position(|k| *k == key) {
                if !merged[at].1.contains(label) {
                    merged[at].1.push(label.clone());
                }
                if merged[at].0.snippet.is_empty() {
                    merged[at].0.snippet = hit.snippet.clone();
                }
            } else {
                seen.push(key);
                merged.push((hit.clone(), vec![label.clone()]));
            }
        }
    }
    merged
}

// ---------------------------------------------------------------- the page

fn page_html(query: &str, results: &[EngineResult]) -> String {
    let esc = crate::youtube::esc;
    let lists: Vec<(String, Vec<Hit>)> = results
        .iter()
        .filter_map(|r| r.outcome.as_ref().ok().map(|hits| (r.label.clone(), hits.clone())))
        .collect();
    let merged = merge(&lists);
    let mut status = String::new();
    for r in results {
        match &r.outcome {
            Ok(hits) => status.push_str(&format!("<li>{} &ndash; {} results</li>", esc(&r.label), hits.len())),
            Err(why) => status.push_str(&format!(
                "<li class=\"bad\">{} &ndash; {}. <a href=\"{}\">Open {} itself</a></li>",
                esc(&r.label),
                esc(why),
                esc(&r.open_url),
                esc(&r.label)
            )),
        }
    }
    let mut list = String::new();
    for (hit, engines) in &merged {
        let host = Url::parse(&hit.url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_default();
        list.push_str(&format!(
            "<div class=\"r\"><a class=\"t\" href=\"{}\">{}</a><div class=\"u\">{} &middot; {}</div><div class=\"s\">{}</div></div>",
            esc(&hit.url),
            esc(&hit.title),
            esc(&host),
            esc(&engines.join(" + ")),
            esc(&hit.snippet)
        ));
    }
    if merged.is_empty() {
        list.push_str("<p>No results could be read. Use the links above to open the engines directly.</p>");
    }
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{q} - several engines</title>\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><style>\
         :root{{color-scheme:light dark;--bg:#fff;--fg:#1c1c1e;--mute:#6b6b70;--link:#1a56c4;--bad:#b3261e}}\
         @media(prefers-color-scheme:dark){{:root{{--bg:#1b1c1e;--fg:#e8e8ea;--mute:#9a9aa0;--link:#8ab4f8;--bad:#f2b8b5}}}}\
         body{{margin:0;background:var(--bg);color:var(--fg);font:16px/1.45 system-ui,sans-serif}}\
         main{{max-width:760px;margin:0 auto;padding:24px 16px 60px}}h1{{font-size:20px;margin:0 0 6px}}\
         ul{{margin:8px 0 22px;padding-left:20px;color:var(--mute);font-size:14px}}.bad{{color:var(--bad)}}\
         a{{color:var(--link)}}.r{{margin:0 0 20px}}.t{{font-size:18px;text-decoration:none}}\
         .u{{color:var(--mute);font-size:13px;word-break:break-all}}.s{{font-size:15px}}\
         </style></head><body><main><h1>{q}</h1><ul>{status}</ul>{list}</main></body></html>",
        q = esc(query),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(mode: &str) -> Settings {
        Settings {
            multi_search: true,
            multi_mode: mode.to_string(),
            multi_engines: vec!["duckduckgo".into(), "bing".into(), "bogus".into()],
            ..Settings::default()
        }
    }

    fn typed(settings: &Settings, input: &str) -> Option<String> {
        Some(pteron::search_url_for(settings).replace("%s", &pteron::encode_query(input.trim())))
    }

    #[test]
    fn engines_are_cleaned() {
        let ids = ["bing", "bing", "nope", "brave", "google", "startpage", "duckduckgo"].map(String::from);
        assert_eq!(clean_engines(ids), ["bing", "brave", "google", "startpage"]);
    }

    #[test]
    fn plain_searches_become_tabs_or_a_page() {
        let s = settings("tabs");
        let Some(Intent::Tabs(urls)) = intent_for(&s, "rust cats", typed(&s, "rust cats").as_deref()) else {
            panic!("expected tabs")
        };
        assert_eq!(urls.len(), 2);
        assert!(urls[0].contains("duckduckgo") && urls[0].contains("rust+cats"));
        assert!(urls[1].contains("bing.com"));
        let p = settings("page");
        let Some(Intent::Page { query, engines }) = intent_for(&p, " rust cats ", typed(&p, " rust cats ").as_deref())
        else {
            panic!("expected page")
        };
        assert_eq!((query.as_str(), engines.len()), ("rust cats", 2));
    }

    #[test]
    fn addresses_and_off_switch_are_left_alone() {
        let s = settings("tabs");
        assert!(intent_for(&s, "example.com", Some("https://example.com/")).is_none());
        assert!(intent_for(&s, "", None).is_none());
        let mut off = settings("tabs");
        off.multi_search = false;
        assert!(intent_for(&off, "cats", typed(&off, "cats").as_deref()).is_none());
        let mut one = settings("tabs");
        one.multi_engines = vec!["bing".into()];
        assert!(intent_for(&one, "cats", typed(&one, "cats").as_deref()).is_none());
    }

    #[test]
    fn reads_duckduckgo() {
        let html = r#"<div class="result results_links results_links_deep web-result ">
        <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.org%2Fa%3Fx%3D1&amp;rut=abc">Example &amp; <b>Site</b></a>
        <a class="result__snippet" href="x">A <b>nice</b> page</a></div>
        <div class="result results_links result--ad"><a class="result__a" href="https://ad.example/">Ad</a></div>"#;
        let hits = parse_duckduckgo(html);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://example.org/a?x=1");
        assert_eq!(hits[0].title, "Example & Site");
        assert_eq!(hits[0].snippet, "A nice page");
    }

    #[test]
    fn reads_bing_including_redirect_links() {
        // "https://example.net/page" -> base64url
        let html = r#"<ol><li class="b_algo"><h2><a href="https://www.bing.com/ck/a?!&amp;&amp;p=1&amp;u=a1aHR0cHM6Ly9leGFtcGxlLm5ldC9wYWdl&amp;ntb=1">Net <strong>page</strong></a></h2>
        <div class="b_caption"><p class="b_lineclamp2">About the page</p></div></li>
        <li class="b_algo"><h2><a href="https://direct.example/">Direct</a></h2><p>Hello</p></li></ol>"#;
        let hits = parse_bing(html);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].url, "https://example.net/page");
        assert_eq!(hits[0].title, "Net page");
        assert_eq!(hits[0].snippet, "About the page");
        assert_eq!(hits[1].url, "https://direct.example/");
    }

    #[test]
    fn reads_brave_and_searx() {
        let html = r#"<div class="snippet svelte-1" data-type="web"><a href="https://site.example/x" class="h"><div class="title svelte">Site title</div></a>
        <div class="content desktop-default-regular">Some text</div></div>
        <div class="snippet svelte-1" data-type="news"><a href="https://n.example/">News</a></div>"#;
        let hits = parse_brave(html);
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].title.as_str(), hits[0].snippet.as_str()), ("Site title", "Some text"));
        let json = r#"{"results":[{"title":"T","url":"https://a.example/","content":"C"},{"title":"bad","url":"javascript:1"}]}"#;
        assert_eq!(parse_searx_json(json).len(), 1);
        assert!(parse_searx_json("<html>").is_empty());
    }

    #[test]
    fn merge_interleaves_and_joins_duplicates() {
        let hit = |u: &str| Hit { title: u.into(), url: u.into(), snippet: String::new() };
        let lists = vec![
            ("A".to_string(), vec![hit("https://one.example/"), hit("https://two.example/")]),
            ("B".to_string(), vec![hit("https://www.two.example"), hit("https://three.example/")]),
        ];
        let merged = merge(&lists);
        let urls: Vec<_> = merged.iter().map(|(h, _)| h.url.as_str()).collect();
        assert_eq!(urls, ["https://one.example/", "https://www.two.example", "https://three.example/"]);
        assert_eq!(merged[1].1, vec!["B".to_string(), "A".to_string()]);
    }

    #[test]
    fn page_shows_failures_as_links() {
        let results = vec![
            EngineResult {
                label: "Bing".into(),
                open_url: "https://www.bing.com/search?q=x".into(),
                outcome: Err("it did not answer".into()),
            },
            EngineResult {
                label: "Brave Search".into(),
                open_url: "https://search.brave.com/search?q=x".into(),
                outcome: Ok(vec![Hit { title: "<T>".into(), url: "https://a.example/".into(), snippet: "s".into() }]),
            },
        ];
        let html = page_html("x <y>", &results);
        assert!(html.contains("Open Bing itself") && html.contains("&lt;T&gt;") && html.contains("x &lt;y&gt;"));
    }

    #[test]
    fn keys_are_stable_and_valid() {
        let a = page_key("cats", &["bing".into()]);
        assert_eq!(a, page_key("cats", &["bing".into()]));
        assert_ne!(a, page_key("dogs", &["bing".into()]));
        assert!(a.len() <= 40 && a.chars().all(|c| c.is_ascii_alphanumeric()));
    }
}

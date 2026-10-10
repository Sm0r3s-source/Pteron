/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Pteron: reading mode. A script runs inside the page, picks out the main article and replaces
//! the page with clean text and pictures in the colours of the chosen theme. Closing reading
//! mode reloads the page.

use std::sync::Mutex;

use crate::pteron::Settings;

static MESSAGE: Mutex<Option<(String, std::time::Instant)>> = Mutex::new(None);

pub(crate) fn set_message(text: &str) {
    if let Ok(mut m) = MESSAGE.lock() {
        *m = Some((text.to_string(), std::time::Instant::now()));
    }
}

/// A short notice to show for a few seconds ("No article found").
pub(crate) fn message() -> Option<String> {
    let mut m = MESSAGE.lock().ok()?;
    match m.as_ref() {
        Some((text, at)) if at.elapsed() < std::time::Duration::from_secs(4) => Some(text.clone()),
        Some(_) => {
            *m = None;
            None
        },
        None => None,
    }
}

/// The script, with the colours of the current theme filled in.
pub(crate) fn script(settings: &Settings) -> String {
    let colors = match crate::appearance::palette_for(settings) {
        Some(p) => format!(
            "{{bg:'{}',text:'{}',muted:'{}',accent:'{}',card:'{}'}}",
            crate::appearance::to_hex(p.bg),
            crate::appearance::to_hex(p.text),
            crate::appearance::to_hex(p.muted),
            crate::appearance::to_hex(p.accent),
            crate::appearance::to_hex(p.surface),
        ),
        None => {
            let dark = match settings.theme.as_str() {
                "dark" => "true",
                "light" => "false",
                _ => "(window.matchMedia&&window.matchMedia('(prefers-color-scheme: dark)').matches)",
            };
            let accent = crate::appearance::to_hex(crate::appearance::accent_rgb(settings));
            format!(
                "({dark}?{{bg:'#1b1c1e',text:'#e9e6df',muted:'#9a978f',accent:'{accent}',card:'#26272a'}}:{{bg:'#f6f4ef',text:'#26241f',muted:'#6f6b62',accent:'{accent}',card:'#ffffff'}})"
            )
        },
    };
    format!("({JS})({colors})")
}

const JS: &str = r#"function (C) {
  var doc = document;
  if (window.__pteronReader) return 'already';
  var SKIP = /^(SCRIPT|STYLE|NOSCRIPT|NAV|ASIDE|FOOTER|FORM|BUTTON|SVG|IFRAME|VIDEO|AUDIO|CANVAS|SELECT|INPUT|TEXTAREA|TEMPLATE|DIALOG|HEADER)$/;
  var JUNK = /comment|share|social|related|promo|advert|newsletter|sidebar|menu|cookie|popup|modal|subscribe|breadcrumb|toolbar|banner|recommend|outbrain|taboola|byline-share/i;
  function textLen(el) { return (el.textContent || '').trim().length; }
  function score(el) {
    var ps = el.querySelectorAll('p'), n = 0;
    for (var i = 0; i < ps.length; i++) { var t = textLen(ps[i]); if (t > 60) n += t; }
    return n;
  }
  var best = null, bestScore = 0;
  var preferred = doc.querySelectorAll('article, main, [role=main]');
  for (var i = 0; i < preferred.length; i++) { var s = score(preferred[i]); if (s > bestScore) { best = preferred[i]; bestScore = s; } }
  if (bestScore < 400) {
    var all = doc.querySelectorAll('div, section');
    for (var j = 0; j < all.length && j < 4000; j++) {
      var sc = score(all[j]);
      // a container wins over its child only if it holds clearly more text
      if (sc > bestScore * 1.15 || (!best && sc > 0)) { best = all[j]; bestScore = sc; }
    }
  }
  if (!best || bestScore < 200) return 'none';
  var KEEP = { H1: 1, H2: 1, H3: 1, H4: 1, P: 1, UL: 1, OL: 1, LI: 1, BLOCKQUOTE: 1, PRE: 1, CODE: 1, EM: 1, STRONG: 1, B: 1, I: 1, A: 1, IMG: 1, FIGURE: 1, FIGCAPTION: 1, BR: 1, HR: 1, TABLE: 1, THEAD: 1, TBODY: 1, TR: 1, TD: 1, TH: 1, SUB: 1, SUP: 1 };
  function abs(u) { try { return new URL(u, doc.baseURI).href; } catch (e) { return ''; } }
  function copy(node, out) {
    for (var c = node.firstChild; c; c = c.nextSibling) {
      if (c.nodeType === 3) { out.appendChild(doc.createTextNode(c.nodeValue)); continue; }
      if (c.nodeType !== 1) continue;
      var tag = c.nodeName.toUpperCase();
      if (SKIP[tag] || c.getAttribute('aria-hidden') === 'true' || c.hidden) continue;
      var tagClass = (c.className && c.className.baseVal === undefined ? c.className : '') + ' ' + (c.id || '');
      if (JUNK.test(tagClass) && textLen(c) < 1500) continue;
      if (tag === 'IMG') {
        var src = c.currentSrc || c.src || c.getAttribute('data-src') || '';
        var w = parseInt(c.getAttribute('width') || '0', 10);
        if (!src || src.indexOf('data:') === 0 || (w && w < 80)) continue;
        var img = doc.createElement('img'); img.src = abs(src); img.alt = c.alt || ''; out.appendChild(img); continue;
      }
      if (KEEP[tag]) {
        var el = doc.createElement(tag.toLowerCase());
        if (tag === 'A') { var h = abs(c.getAttribute('href') || ''); if (h.indexOf('http') === 0) el.href = h; }
        copy(c, el);
        if ((tag === 'P' || tag === 'LI' || tag === 'A' || tag.charAt(0) === 'H') && !el.textContent.trim() && !el.querySelector('img')) continue;
        out.appendChild(el);
      } else {
        // an ordinary container: keep what is inside it
        copy(c, out);
      }
    }
  }
  var article = doc.createElement('div'); article.id = 'pteron-article';
  copy(best, article);
  if (textLen(article) < 200) return 'none';
  var heading = article.querySelector('h1');
  var titleText = (heading && heading.textContent.trim()) || doc.title || '';
  if (heading) heading.parentNode.removeChild(heading);
  var site = location.hostname.replace(/^www\./, '');
  var css = 'html{background:' + C.bg + '}body{margin:0;background:' + C.bg + ';color:' + C.text + ';font:19px/1.75 Georgia,"Times New Roman",serif}' +
    '#bar{position:sticky;top:0;display:flex;align-items:center;gap:8px;padding:10px 18px;background:' + C.bg + ';border-bottom:1px solid ' + C.card + ';font:14px system-ui,Segoe UI,sans-serif;color:' + C.muted + '}' +
    '#bar span{flex:1;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}' +
    '#bar button{font:inherit;color:' + C.text + ';background:' + C.card + ';border:1px solid ' + C.muted + '55;border-radius:8px;padding:4px 12px;cursor:pointer}' +
    '#wrap{max-width:700px;margin:0 auto;padding:28px 22px 80px}' +
    'h1{font-size:2em;line-height:1.25;margin:.4em 0 .3em}h2,h3,h4{line-height:1.3;margin:1.6em 0 .5em}' +
    'p,li{margin:.9em 0}a{color:' + C.accent + '}img{max-width:100%;height:auto;border-radius:6px;display:block;margin:1.2em auto}' +
    'figure{margin:1.2em 0}figcaption{font:14px system-ui,sans-serif;color:' + C.muted + ';text-align:center}' +
    'blockquote{margin:1.2em 0;padding:.1em 1.2em;border-left:3px solid ' + C.accent + ';color:' + C.muted + '}' +
    'pre{background:' + C.card + ';padding:14px;border-radius:8px;overflow:auto;font-size:.8em;line-height:1.5}code{font-family:Consolas,monospace;font-size:.9em}' +
    'table{border-collapse:collapse;margin:1em 0;font-size:.9em}td,th{border:1px solid ' + C.muted + '55;padding:6px 10px}hr{border:0;border-top:1px solid ' + C.card + '}';
  var html = doc.documentElement;
  while (html.firstChild) html.removeChild(html.firstChild);
  var head = doc.createElement('head');
  var meta = doc.createElement('meta'); meta.setAttribute('charset', 'utf-8'); head.appendChild(meta);
  var ttl = doc.createElement('title'); ttl.textContent = titleText; head.appendChild(ttl);
  var st = doc.createElement('style'); st.textContent = css; head.appendChild(st);
  var body = doc.createElement('body');
  var bar = doc.createElement('div'); bar.id = 'bar';
  var label = doc.createElement('span'); label.textContent = 'Reading mode · ' + site; bar.appendChild(label);
  var size = 19;
  function mk(t, f) { var b = doc.createElement('button'); b.textContent = t; b.onclick = f; bar.appendChild(b); }
  mk('A−', function () { size = Math.max(14, size - 2); body.style.fontSize = size + 'px'; });
  mk('A+', function () { size = Math.min(34, size + 2); body.style.fontSize = size + 'px'; });
  mk('Close', function () { location.reload(); });
  body.appendChild(bar);
  var wrap = doc.createElement('div'); wrap.id = 'wrap';
  var h1 = doc.createElement('h1'); h1.textContent = titleText; wrap.appendChild(h1);
  wrap.appendChild(article); body.appendChild(wrap);
  html.appendChild(head); html.appendChild(body);
  window.__pteronReader = true;
  window.scrollTo(0, 0);
  return 'ok';
}"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_carries_the_theme_colours() {
        let mut s = Settings::default();
        s.theme = "midnight".into();
        let text = script(&s);
        assert!(text.contains("bg:'#0f1420'") && text.contains("text:'#e6ebf5'"));
        s.theme = "dark".into();
        assert!(script(&s).contains("bg:'#1b1c1e'"));
    }

    #[test]
    fn message_expires() {
        set_message("hello");
        assert_eq!(message().as_deref(), Some("hello"));
    }

    #[test]
    fn dump_script() {
        if let Ok(path) = std::env::var("PTERON_READER_DUMP") {
            let mut s = Settings::default();
            s.theme = std::env::var("PTERON_READER_THEME").unwrap_or_else(|_| "midnight".into());
            std::fs::write(path, script(&s)).unwrap();
        }
    }
}

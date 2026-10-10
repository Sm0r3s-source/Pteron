/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Pteron: find in page (Ctrl+F). The bar is drawn by the GUI; the searching is a small script
//! that runs inside the page, highlights the matches and scrolls to the current one.

use std::sync::{Mutex, OnceLock};

#[derive(Default)]
pub(crate) struct State {
    pub open: bool,
    pub query: String,
    /// What the page last answered, like "3/12".
    pub result: String,
    /// Ask the text box to take the keyboard on the next frame.
    pub focus: bool,
    /// The tab the search belongs to (the bar closes when another tab is shown).
    pub tab: Option<String>,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State::default()))
}

pub(crate) fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let mut guard = state().lock().unwrap_or_else(|e| e.into_inner());
    f(&mut guard)
}

/// The zoom level shown briefly after Ctrl +, Ctrl - or Ctrl 0.
fn zoom_note() -> &'static Mutex<Option<(f32, std::time::Instant)>> {
    static NOTE: OnceLock<Mutex<Option<(f32, std::time::Instant)>>> = OnceLock::new();
    NOTE.get_or_init(|| Mutex::new(None))
}

pub(crate) fn note_zoom(level: f32) {
    if let Ok(mut note) = zoom_note().lock() {
        *note = Some((level, std::time::Instant::now()));
    }
}

/// The zoom level to show right now, as a percentage, or `None` once it has faded.
pub(crate) fn zoom_percent() -> Option<u32> {
    let note = zoom_note().lock().ok()?;
    let (level, at) = (*note)?;
    (at.elapsed() < std::time::Duration::from_millis(1600)).then(|| (level * 100.0).round() as u32)
}

pub(crate) fn open() {
    with(|s| {
        s.open = true;
        s.focus = true;
    });
}

/// The script for one search step. `direction`: 0 = new search, 1 = next, -1 = previous.
/// Returns "current/total" (or "0/0").
pub(crate) fn script(query: &str, direction: i32) -> String {
    let quoted = serde_json::to_string(query).unwrap_or_else(|_| "\"\"".into());
    format!("({JS})({quoted},{direction})")
}

pub(crate) fn parse_result(value: &str) -> (usize, usize) {
    let mut parts = value.split('/');
    let current = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let total = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    (current, total)
}

const JS: &str = r#"function (q, dir) {
  var S = window.__pteronFind || (window.__pteronFind = { marks: [], cur: -1, q: '' });
  function clear() {
    for (var i = 0; i < S.marks.length; i++) {
      var m = S.marks[i], p = m.parentNode;
      if (!p) continue;
      p.replaceChild(document.createTextNode(m.textContent), m);
      p.normalize();
    }
    S.marks = []; S.cur = -1;
  }
  function paint() {
    for (var i = 0; i < S.marks.length; i++) {
      S.marks[i].style.background = i === S.cur ? '#ff9632' : '#ffe14d';
      S.marks[i].style.color = '#000';
    }
  }
  if (!q) { clear(); S.q = ''; return '0/0'; }
  if (q !== S.q || dir === 0) {
    clear(); S.q = q;
    var needle = q.toLowerCase();
    var walker = document.createTreeWalker(document.body || document.documentElement, NodeFilter.SHOW_TEXT, {
      acceptNode: function (n) {
        var t = n.parentNode && n.parentNode.nodeName;
        if (t === 'SCRIPT' || t === 'STYLE' || t === 'NOSCRIPT' || t === 'TEXTAREA') return NodeFilter.FILTER_REJECT;
        return n.nodeValue && n.nodeValue.toLowerCase().indexOf(needle) >= 0 ? NodeFilter.FILTER_ACCEPT : NodeFilter.FILTER_REJECT;
      }
    });
    var nodes = [], n;
    while ((n = walker.nextNode()) && nodes.length < 2000) nodes.push(n);
    for (var k = 0; k < nodes.length && S.marks.length < 1000; k++) {
      var node = nodes[k], text = node.nodeValue, lower = text.toLowerCase(), at = [], from = 0, f;
      while ((f = lower.indexOf(needle, from)) >= 0) { at.push(f); from = f + needle.length; }
      var created = [];
      for (var j = at.length - 1; j >= 0; j--) {
        var tail = node.splitText(at[j]);
        tail.splitText(needle.length);
        var mark = document.createElement('mark');
        mark.setAttribute('data-pteron-find', '1');
        tail.parentNode.replaceChild(mark, tail);
        mark.appendChild(tail);
        created.unshift(mark);
      }
      S.marks = S.marks.concat(created);
    }
    S.cur = S.marks.length ? 0 : -1;
  } else if (S.marks.length) {
    S.cur = (S.cur + dir + S.marks.length) % S.marks.length;
  }
  paint();
  if (S.cur >= 0) {
    try { S.marks[S.cur].scrollIntoView({ block: 'center', inline: 'nearest' }); } catch (e) {}
  }
  return (S.cur + 1) + '/' + S.marks.length;
}"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_quotes_the_query() {
        let s = script("a\"b\\c", 1);
        assert!(s.ends_with(r#"("a\"b\\c",1)"#));
    }

    #[test]
    fn reads_results() {
        assert_eq!(parse_result("3/12"), (3, 12));
        assert_eq!(parse_result("0/0"), (0, 0));
        assert_eq!(parse_result("junk"), (0, 0));
    }

    #[test]
    fn dump_script() {
        if let Ok(path) = std::env::var("PTERON_FIND_DUMP") {
            std::fs::write(path, JS).unwrap();
        }
    }
}

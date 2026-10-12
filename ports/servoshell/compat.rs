//! Small stand-ins for web features Servo does not have yet, found with "Check this page".
//!
//! Each one is only defined when the page would otherwise find it missing, and the larger
//! ones are limited to sites known to need them, so working pages are left alone.

use std::sync::atomic::{AtomicBool, Ordering};

/// A newer web feature that Servo has but keeps switched off. Pteron lets the user turn them
/// on one by one (Settings > Web compatibility).
pub(crate) struct Feature {
    pub id: &'static str,
    pub title: &'static str,
    pub note: &'static str,
    /// Servo preferences switched on together.
    pub prefs: &'static [&'static str],
}

pub(crate) const FEATURES: &[Feature] = &[
    Feature {
        id: "indexeddb",
        title: "Site databases (IndexedDB)",
        note: "Lets web apps keep data inside the browser. Many modern sites need it.",
        prefs: &["dom_indexeddb_enabled"],
    },
    Feature {
        id: "observer",
        title: "Scroll and visibility detection (IntersectionObserver)",
        note: "Lets pages know when something scrolls into view. Used for lazy loading.",
        prefs: &["dom_intersection_observer_enabled"],
    },
    Feature {
        id: "fonts",
        title: "Font loading (FontFace)",
        note: "Lets pages load and check their own fonts.",
        prefs: &["dom_fontface_enabled"],
    },
    Feature {
        id: "canvas",
        title: "Offscreen canvas",
        note: "Drawing in the background. Used by some web apps and games.",
        prefs: &["dom_offscreen_canvas_enabled"],
    },
    Feature {
        id: "storage",
        title: "Storage manager",
        note: "Lets sites ask how much space they may use.",
        prefs: &["dom_storage_manager_api_enabled"],
    },
    Feature {
        id: "container",
        title: "CSS container queries",
        note: "Newer page layouts that adapt to the size of their box.",
        prefs: &["layout_container_queries_enabled"],
    },
];

static SAFE_MODE: AtomicBool = AtomicBool::new(false);
static NOTICE_DISMISSED: AtomicBool = AtomicBool::new(false);

/// Set at start-up when the last session did not end cleanly: the switched-on features stay
/// off for this session.
pub(crate) fn set_safe_mode(on: bool) {
    SAFE_MODE.store(on, Ordering::Relaxed);
}

pub(crate) fn safe_mode() -> bool {
    SAFE_MODE.load(Ordering::Relaxed)
}

/// Should the "starting without experimental features" bar be showing?
pub(crate) fn safe_notice_visible(enabled: &[String]) -> bool {
    safe_mode() && !enabled.is_empty() && !NOTICE_DISMISSED.load(Ordering::Relaxed)
}

pub(crate) fn dismiss_safe_notice() {
    NOTICE_DISMISSED.store(true, Ordering::Relaxed);
}

/// Every feature preference with the value it should have now. Unknown ids are ignored, and
/// nothing is on in safe mode.
pub(crate) fn wanted_prefs(enabled: &[String], safe: bool) -> Vec<(&'static str, bool)> {
    let mut out = Vec::new();
    for feature in FEATURES {
        let on = !safe && enabled.iter().any(|id| id == feature.id);
        for pref in feature.prefs {
            out.push((*pref, on));
        }
    }
    out
}

/// Keeps only known ids, once each.
pub(crate) fn clean_ids(ids: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for id in ids {
        if FEATURES.iter().any(|f| f.id == id) && !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

pub(crate) const SCRIPT: &str = r#"(function(){try{
if(window.top!==window)return;
// Every page: font loading calls and idle callbacks.
try{if(document.fonts){
if(typeof document.fonts.load!=='function')document.fonts.load=function(){return Promise.resolve([]);};
if(typeof document.fonts.check!=='function')document.fonts.check=function(){return true;};
if(!document.fonts.ready)try{document.fonts.ready=Promise.resolve(document.fonts);}catch(e){}
}}catch(e){}
if(typeof window.requestIdleCallback!=='function'){
window.requestIdleCallback=function(cb){return setTimeout(function(){var t=Date.now();cb({didTimeout:false,timeRemaining:function(){return Math.max(0,12-(Date.now()-t));}});},40);};
window.cancelIdleCallback=function(id){clearTimeout(id);};
}
// Only the Claude sites: these are bigger guesses.
if(!/(^|\.)(claude\.ai|claude\.com|anthropic\.com)$/.test(location.hostname))return;
if(typeof window.IntersectionObserver!=='function'){
// Light version: fires when an element enters or leaves the window, checked at most every 250 ms.
var all=[],timer=0;
var inView=function(el){try{var r=el.getBoundingClientRect();return r.bottom>=0&&r.right>=0&&r.top<=(window.innerHeight||0)&&r.left<=(window.innerWidth||0)&&(r.width>0||r.height>0);}catch(e){return false;}};
var entry=function(el,on){var r=el.getBoundingClientRect();return {target:el,isIntersecting:on,intersectionRatio:on?1:0,boundingClientRect:r,intersectionRect:on?r:{x:0,y:0,width:0,height:0,top:0,left:0,right:0,bottom:0},rootBounds:null,time:performance.now()};};
var sweep=function(){timer=0;for(var i=0;i<all.length;i++){var o=all[i],on=inView(o.el);if(on!==o.on){o.on=on;try{o.io._cb([entry(o.el,on)],o.io);}catch(e){}}}};
var poke=function(){if(!timer&&all.length)timer=setTimeout(sweep,250);};
window.addEventListener('scroll',poke,{passive:true,capture:true});window.addEventListener('resize',poke,{passive:true});
window.IntersectionObserver=function(cb,opts){this._cb=cb;this.root=(opts&&opts.root)||null;this.rootMargin=(opts&&opts.rootMargin)||'0px';this.thresholds=[0];};
window.IntersectionObserver.prototype.observe=function(el){var io=this,rec={io:io,el:el,on:null};all.push(rec);setTimeout(function(){var on=inView(el);rec.on=on;try{io._cb([entry(el,on)],io);}catch(e){}},0);};
window.IntersectionObserver.prototype.unobserve=function(el){for(var i=all.length-1;i>=0;i--)if(all[i].io===this&&all[i].el===el)all.splice(i,1);};
window.IntersectionObserver.prototype.disconnect=function(){for(var i=all.length-1;i>=0;i--)if(all[i].io===this)all.splice(i,1);};
window.IntersectionObserver.prototype.takeRecords=function(){return [];};
setInterval(poke,2000);
}
try{if(navigator&&!navigator.locks){Object.defineProperty(navigator,'locks',{configurable:true,value:{request:function(n,o,cb){cb=typeof o==='function'?o:cb;return Promise.resolve().then(function(){return cb({name:n,mode:'exclusive'});});},query:function(){return Promise.resolve({held:[],pending:[]});}}});}}catch(e){}
}catch(e){}})();"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn prefs_follow_the_chosen_features() {
        let wanted = wanted_prefs(&ids(&["indexeddb", "nonsense"]), false);
        assert_eq!(wanted.len(), FEATURES.len());
        assert!(wanted.contains(&("dom_indexeddb_enabled", true)));
        assert!(wanted.contains(&("dom_fontface_enabled", false)));
    }

    #[test]
    fn safe_mode_turns_everything_off() {
        let wanted = wanted_prefs(&ids(&["indexeddb", "fonts", "canvas"]), true);
        assert!(wanted.iter().all(|(_, on)| !on));
    }

    #[test]
    fn unknown_and_repeated_ids_are_dropped() {
        assert_eq!(clean_ids(ids(&["fonts", "x", "fonts", "canvas"])), ids(&["fonts", "canvas"]));
    }

    #[test]
    fn every_pref_name_exists_in_servos_experimental_list() {
        let listed = [
            "dom_indexeddb_enabled",
            "dom_intersection_observer_enabled",
            "dom_fontface_enabled",
            "dom_offscreen_canvas_enabled",
            "dom_storage_manager_api_enabled",
            "layout_container_queries_enabled",
        ];
        for feature in FEATURES {
            for pref in feature.prefs {
                assert!(listed.contains(pref), "{pref}");
            }
        }
    }

    #[test]
    fn script_is_scoped_and_safe() {
        assert!(!super::SCRIPT.contains("\"#"));
        assert!(super::SCRIPT.contains("claude\\.ai"));
        assert!(super::SCRIPT.contains("document.fonts.load"));
    }
}

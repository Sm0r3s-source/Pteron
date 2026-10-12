//! "Check this page": a diagnostic for pages that load blank or half broken.
//!
//! A tiny script on every page keeps the last few JavaScript errors. The tab menu item
//! "Check this page" runs `REPORT_JS`, which lists missing web features, those errors,
//! and the response headers (it can tell a Cloudflare challenge from a broken page). The
//! result is drawn on the page itself and also saved to the crash log.

/// Console messages starting with this are ours (never shown in the console).
pub(crate) const PREFIX: &str = "__pteron_check__";

/// Runs on every page: remembers the last errors so the report can show them.
pub(crate) const ERR_SCRIPT: &str = r#"(function(){try{
if(window.top!==window||window.__ptErr)return;
var list=window.__ptErr=[];
function add(t){if(list.length<12)list.push(String(t).slice(0,240));}
window.addEventListener('error',function(e){add((e.message||'error')+' @ '+String(e.filename||'').split('/').pop()+':'+(e.lineno||0));},true);
window.addEventListener('unhandledrejection',function(e){var r=e.reason;add('promise: '+(r&&r.message?r.message:r));});
}catch(e){}})();"#;

/// Run on demand by the tab menu item. Draws a box on the page and logs the same text.
pub(crate) const REPORT_JS: &str = r#"(function(){try{
var out=[];
function p(s){out.push(s);}
p('URL: '+location.href);
p('Title: '+document.title);
p('State: '+document.readyState+', '+document.getElementsByTagName('*').length+' elements, '+((document.body&&document.body.innerText)||'').length+' characters of text');
p('Agent: '+navigator.userAgent);
function T(f){try{return typeof f();}catch(e){return 'blocked';}}
var need={fetch:typeof fetch,Promise:typeof Promise,structuredClone:typeof structuredClone,ResizeObserver:typeof ResizeObserver,IntersectionObserver:typeof IntersectionObserver,MutationObserver:typeof MutationObserver,customElements:typeof customElements,attachShadow:typeof Element.prototype.attachShadow,WebSocket:typeof WebSocket,Worker:typeof Worker,SharedWorker:typeof SharedWorker,serviceWorker:('serviceWorker' in navigator)?'function':'undefined',indexedDB:T(function(){return window.indexedDB;}),localStorage:T(function(){return window.localStorage;}),cryptoSubtle:(window.crypto&&crypto.subtle)?'object':'undefined',TextDecoder:typeof TextDecoder,AbortController:typeof AbortController,ReadableStream:typeof ReadableStream,BroadcastChannel:typeof BroadcastChannel,WebAssembly:typeof WebAssembly,requestIdleCallback:typeof requestIdleCallback,matchMedia:typeof matchMedia,CSSsupports:(window.CSS&&CSS.supports)?'function':'undefined',queueMicrotask:typeof queueMicrotask,OffscreenCanvas:typeof OffscreenCanvas,Intl:typeof Intl,navigatorLocks:(navigator.locks)?'object':'undefined'};
var miss=[];for(var k in need){if(need[k]==='undefined')miss.push(k);}
p('Missing web features: '+(miss.length?miss.join(', '):'none of the ones we test'));
var errs=window.__ptErr||[];
try{
var sup=[['color-mix','color:color-mix(in srgb,red,blue)'],['oklch','color:oklch(0.7 0.1 50)'],['oklab','color:oklab(0.7 0.1 0.1)'],['hsl-slash','color:hsl(20 50% 50% / 0.5)'],['rgb-from','color:rgb(from red r g b)'],['light-dark','color:light-dark(red,blue)'],[':has','selector(:has(a))'],['@container','container-type:inline-size'],['@property','@property --x{syntax:"*";inherits:false}'],['mask','mask-image:linear-gradient(red,blue)'],['text-wrap','text-wrap:balance'],['dvh','height:100dvh'],['svh','height:100svh']];
var no=[];for(var q=0;q<sup.length;q++){var d=sup[q][1],ok;try{ok=(d.indexOf('selector(')===0)?CSS.supports(d):(d.charAt(0)==='@'?true:CSS.supports(d));}catch(e){ok=false;}if(!ok)no.push(sup[q][0]);}
p('CSS not supported: '+(no.length?no.join(', '):'none of the ones we test'));
}catch(e){p('CSS check failed: '+e);}
try{
var ff=[];if(document.fonts&&document.fonts.forEach){document.fonts.forEach(function(f){ff.push(f.family+' '+f.weight+' '+f.status);});}
p('Web fonts ('+ff.length+'): '+ff.slice(0,14).join('; '));
}catch(e){p('Font list failed: '+e);}
try{
var sv=document.querySelector('svg');var lg=document.querySelector('header svg,[class*="logo"] svg,a svg');
if(sv)p('First svg colour: '+getComputedStyle(sv).color+', fill '+getComputedStyle(sv).fill);
if(lg)p('Logo svg colour: '+getComputedStyle(lg).color+', fill '+getComputedStyle(lg).fill);
var rs=getComputedStyle(document.documentElement);
p('Root vars: --brand-100='+rs.getPropertyValue('--brand-100')+' --text-100='+rs.getPropertyValue('--text-100')+' --accent-brand='+rs.getPropertyValue('--accent-brand'));
var ic=document.querySelectorAll('[class*="icon"],[data-icon]');
p('Icon-like elements: '+ic.length+(ic[0]?', first font-family: '+getComputedStyle(ic[0]).fontFamily+', text codes: '+Array.prototype.map.call((ic[0].textContent||'').slice(0,4),function(c){return c.charCodeAt(0).toString(16);}).join(','):''));
}catch(e){p('Style probe failed: '+e);}
p('Script errors seen: '+errs.length);
for(var i=0;i<errs.length;i++)p('  - '+errs[i]);
var cf=[];
if(/just a moment|attention required|checking your browser/i.test(document.title))cf.push('challenge title');
if(document.querySelector('#challenge-form,#cf-challenge-running,.cf-browser-verification,[id^="cf-chl"]'))cf.push('challenge element');
var ss=document.getElementsByTagName('script');
for(var j=0;j<ss.length;j++){if((ss[j].src||'').indexOf('/cdn-cgi/')>=0){cf.push('cdn-cgi script');break;}}
p('Cloudflare signs on the page: '+(cf.length?cf.join(', '):'none'));
function done(h){
p(h);
var text=out.join('\n');
console.log('__pteron_check__'+text);
var old=document.getElementById('__pteron_check_box');if(old)old.remove();
var box=document.createElement('div');box.id='__pteron_check_box';
box.style.cssText='position:fixed;z-index:2147483647;top:12px;left:12px;right:12px;max-height:80vh;overflow:auto;background:#1b1b22;color:#eee;border:1px solid #6c6cff;border-radius:10px;padding:12px;font:12px/1.4 monospace;white-space:pre-wrap;word-break:break-word;text-align:left';
var pre=document.createElement('div');pre.textContent=text;
var x=document.createElement('button');x.textContent='Close';x.style.cssText='float:right;margin-left:8px';x.onclick=function(){box.remove();};
var note=document.createElement('div');note.textContent='Saved to the Pteron crash log too. Send me this text.';note.style.cssText='color:#9aa;margin-bottom:6px';
box.appendChild(x);box.appendChild(note);box.appendChild(pre);
(document.body||document.documentElement).appendChild(box);
}
try{
fetch(location.href,{method:'HEAD',credentials:'same-origin',cache:'no-store'}).then(function(r){
var g=function(n){return r.headers.get(n)||'-';};
done('Response: HTTP '+r.status+', server '+g('server')+', cf-mitigated '+g('cf-mitigated')+', content-type '+g('content-type'));
},function(e){done('Response check failed: '+(e&&e.message?e.message:e));});
}catch(e){done('Response check unavailable: '+e);}
}catch(e){console.log('__pteron_check__report failed: '+e);}})();"#;

/// Called for every console message. True when it was ours (and has been saved).
pub(crate) fn handle_console(message: &str) -> bool {
    let Some(report) = message.strip_prefix(PREFIX) else { return false };
    crate::crashlog::note(&format!("page check:\n{}", report.trim()));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_our_messages_are_taken() {
        assert!(handle_console("__pteron_check__URL: x"));
        assert!(!handle_console("hello"));
        assert!(!handle_console("__pteron_pw__ something"));
    }

    #[test]
    fn scripts_have_no_raw_string_terminator() {
        assert!(!REPORT_JS.contains("\"#"));
        assert!(!ERR_SCRIPT.contains("\"#"));
        assert!(REPORT_JS.contains(PREFIX));
    }
}

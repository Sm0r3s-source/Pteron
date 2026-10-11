//! Small stand-ins for web features Servo does not have yet, found with "Check this page".
//!
//! Each one is only defined when the page would otherwise find it missing, and the larger
//! ones are limited to sites known to need them, so working pages are left alone.

pub(crate) const SCRIPT: &str = r#"(function(){try{
if(window.top!==window)return;
// Every page: font loading calls and idle callbacks.
try{if(document.fonts){
if(typeof document.fonts.load!=='function')document.fonts.load=function(){return Promise.resolve([]);};
if(typeof document.fonts.check!=='function')document.fonts.check=function(){return true;};
if(!document.fonts.ready)try{document.fonts.ready=Promise.resolve(document.fonts);}catch(e){}
}}catch(e){}
if(typeof window.requestIdleCallback!=='function'){
window.requestIdleCallback=function(cb){var t=Date.now();return setTimeout(function(){cb({didTimeout:false,timeRemaining:function(){return Math.max(0,50-(Date.now()-t));}});},1);};
window.cancelIdleCallback=function(id){clearTimeout(id);};
}
// Only the Claude sites: these are bigger guesses.
if(!/(^|\.)(claude\.ai|claude\.com|anthropic\.com)$/.test(location.hostname))return;
if(typeof window.IntersectionObserver!=='function'){
window.IntersectionObserver=function(cb,opts){this._cb=cb;this.root=(opts&&opts.root)||null;this.rootMargin=(opts&&opts.rootMargin)||'0px';this.thresholds=[0];};
window.IntersectionObserver.prototype.observe=function(el){var self=this;setTimeout(function(){try{var r=el.getBoundingClientRect();self._cb([{target:el,isIntersecting:true,intersectionRatio:1,boundingClientRect:r,intersectionRect:r,rootBounds:null,time:performance.now()}],self);}catch(e){}},0);};
window.IntersectionObserver.prototype.unobserve=function(){};
window.IntersectionObserver.prototype.disconnect=function(){};
window.IntersectionObserver.prototype.takeRecords=function(){return [];};
}
try{if(navigator&&!navigator.locks){Object.defineProperty(navigator,'locks',{configurable:true,value:{request:function(n,o,cb){cb=typeof o==='function'?o:cb;return Promise.resolve().then(function(){return cb({name:n,mode:'exclusive'});});},query:function(){return Promise.resolve({held:[],pending:[]});}}});}}catch(e){}
}catch(e){}})();"#;

#[cfg(test)]
mod tests {
    #[test]
    fn script_is_scoped_and_safe() {
        assert!(!super::SCRIPT.contains("\"#"));
        assert!(super::SCRIPT.contains("claude\\.ai"));
        assert!(super::SCRIPT.contains("document.fonts.load"));
    }
}

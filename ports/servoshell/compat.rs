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
    #[test]
    fn script_is_scoped_and_safe() {
        assert!(!super::SCRIPT.contains("\"#"));
        assert!(super::SCRIPT.contains("claude\\.ai"));
        assert!(super::SCRIPT.contains("document.fonts.load"));
    }
}

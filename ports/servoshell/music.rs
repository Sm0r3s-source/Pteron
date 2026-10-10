/* Pteron: Plume Music. A separate, audio-only page for listening to music from YouTube:
 * search, playlists and albums, a queue that is remembered, and playback that continues while
 * other tabs are used. Nothing but the audio track of a video is downloaded.
 *
 * The page talks to the local helper (see `streamproxy`) with short commands such as
 * `search:<text>`, `track:<video id>` or `next:auto`; `command` answers them with JSON.
 * The queue lives here (and in `music/queue.json`), so the page, the sidebar and a restart
 * all see the same list. */

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::innertube::{self, VideoItem};
use crate::streamproxy::{self, AUDIO_ONLY};

const MAX_TRACKS: usize = 500;
/// A heartbeat younger than this means the Music page is open.
const BEAT_FRESH: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------- the queue (plain data, tested)

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Track {
    pub id: String,
    pub title: String,
    pub channel: String,
    pub duration: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Queue {
    pub tracks: Vec<Track>,
    pub cur: Option<usize>,
    /// 0 off, 1 repeat the whole queue, 2 repeat the current song.
    pub repeat: u8,
    pub volume: f64,
    /// Where to go on "next" after the playing song was removed from the queue.
    resume: usize,
}

impl Default for Queue {
    fn default() -> Self {
        Queue { tracks: Vec::new(), cur: None, repeat: 0, volume: 0.8, resume: 0 }
    }
}

fn clip(text: &str, max: usize) -> String {
    text.chars().filter(|c| !c.is_control()).take(max).collect::<String>().trim().to_string()
}

impl Track {
    fn from_value(v: &Value) -> Option<Track> {
        let id = v.get("id")?.as_str()?;
        if !crate::youtube::valid_video_id(id) {
            return None;
        }
        Some(Track {
            id: id.to_string(),
            title: clip(v.get("title").and_then(|t| t.as_str()).unwrap_or(""), 200),
            channel: clip(v.get("channel").and_then(|t| t.as_str()).unwrap_or(""), 120),
            duration: v.get("duration").and_then(|d| d.as_u64()).unwrap_or(0).min(100 * 3600),
        })
    }

    fn to_value(&self) -> Value {
        json!({"id": self.id, "title": self.title, "channel": self.channel, "duration": self.duration})
    }

    fn from_item(item: &VideoItem) -> Track {
        Track {
            id: item.id.clone(),
            title: clip(&item.title, 200),
            channel: clip(&item.channel, 120),
            duration: item.duration_secs.unwrap_or(0),
        }
    }
}

impl Queue {
    pub(crate) fn add(&mut self, track: Track) -> bool {
        if self.tracks.len() >= MAX_TRACKS {
            return false;
        }
        self.tracks.push(track);
        true
    }

    pub(crate) fn remove(&mut self, index: usize) {
        if index >= self.tracks.len() {
            return;
        }
        self.tracks.remove(index);
        match self.cur {
            Some(c) if c == index => {
                self.cur = None;
                self.resume = index;
            },
            Some(c) if c > index => self.cur = Some(c - 1),
            _ => {},
        }
        if self.tracks.is_empty() {
            self.cur = None;
            self.resume = 0;
        }
    }

    pub(crate) fn clear(&mut self) {
        self.tracks.clear();
        self.cur = None;
        self.resume = 0;
    }

    pub(crate) fn set_current(&mut self, index: usize) -> bool {
        if index < self.tracks.len() {
            self.cur = Some(index);
            true
        } else {
            false
        }
    }

    /// Moves to the next song. `user` is true for the Next button, false when a song ended
    /// (repeat-one only replays on its own). Returns false at the end of the queue.
    pub(crate) fn advance(&mut self, user: bool) -> bool {
        let len = self.tracks.len();
        if len == 0 {
            return false;
        }
        match self.cur {
            None => {
                self.cur = Some(self.resume.min(len - 1));
                self.resume = 0;
                true
            },
            Some(c) => {
                if self.repeat == 2 && !user {
                    true
                } else if c + 1 < len {
                    self.cur = Some(c + 1);
                    true
                } else if self.repeat == 1 {
                    self.cur = Some(0);
                    true
                } else {
                    false
                }
            },
        }
    }

    /// Moves to the previous song (the first song just starts again).
    pub(crate) fn back(&mut self) -> bool {
        let len = self.tracks.len();
        match self.cur {
            _ if len == 0 => false,
            None => {
                self.cur = Some(0);
                true
            },
            Some(0) => {
                if self.repeat == 1 {
                    self.cur = Some(len - 1);
                }
                true
            },
            Some(c) => {
                self.cur = Some(c - 1);
                true
            },
        }
    }

    pub(crate) fn current(&self) -> Option<&Track> {
        self.cur.and_then(|c| self.tracks.get(c))
    }

    pub(crate) fn to_json(&self) -> Value {
        json!({
            "tracks": self.tracks.iter().map(Track::to_value).collect::<Vec<_>>(),
            "cur": self.cur,
            "repeat": self.repeat,
            "vol": self.volume,
        })
    }

    pub(crate) fn from_json(text: &str) -> Queue {
        let mut queue = Queue::default();
        let Ok(v) = serde_json::from_str::<Value>(text) else { return queue };
        if let Some(list) = v.get("tracks").and_then(|t| t.as_array()) {
            for track in list.iter().filter_map(Track::from_value).take(MAX_TRACKS) {
                queue.tracks.push(track);
            }
        }
        queue.cur = v
            .get("cur")
            .and_then(|c| c.as_u64())
            .map(|c| c as usize)
            .filter(|c| *c < queue.tracks.len());
        queue.repeat = v.get("repeat").and_then(|r| r.as_u64()).filter(|r| *r <= 2).unwrap_or(0) as u8;
        queue.volume = v.get("vol").and_then(|x| x.as_f64()).filter(|x| (0.0..=1.0).contains(x)).unwrap_or(0.8);
        queue
    }
}

// ---------------------------------------------------------------- state shared with the app

struct Shared {
    queue: Option<Queue>,
    title: String,
    playing: bool,
    beat: Option<Instant>,
    pending: Option<&'static str>,
}

static SHARED: Mutex<Shared> = Mutex::new(Shared { queue: None, title: String::new(), playing: false, beat: None, pending: None });

fn queue_file() -> Option<PathBuf> {
    ::dirs::config_dir().map(|d| d.join(crate::pteron::APP_NAME).join("music").join("queue.json"))
}

fn with_queue<T>(change: impl FnOnce(&mut Queue) -> T, save: bool) -> (T, Value) {
    let mut shared = SHARED.lock().unwrap_or_else(|e| e.into_inner());
    if shared.queue.is_none() {
        let loaded = queue_file()
            .and_then(|f| std::fs::read_to_string(f).ok())
            .map(|t| Queue::from_json(&t))
            .unwrap_or_default();
        shared.queue = Some(loaded);
    }
    let queue = shared.queue.as_mut().expect("queue was just made");
    let result = change(queue);
    let state = queue.to_json();
    if save && let Some(file) = queue_file() {
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(file, state.to_string());
    }
    (result, state)
}

/// What the sidebar shows: the playing song, while the Music page is open.
pub(crate) struct NowPlaying {
    pub title: String,
    pub playing: bool,
}

pub(crate) fn now_playing() -> Option<NowPlaying> {
    let shared = SHARED.lock().ok()?;
    let beat = shared.beat?;
    (beat.elapsed() < BEAT_FRESH && !shared.title.is_empty())
        .then(|| NowPlaying { title: shared.title.clone(), playing: shared.playing })
}

/// Asks the Music page to do something (`toggle`, `next` or `prev`); it picks the request up
/// on its next heartbeat.
pub(crate) fn send_command(command: &'static str) {
    if let Ok(mut shared) = SHARED.lock() {
        shared.pending = Some(command);
    }
}

// ---------------------------------------------------------------- commands from the page

fn items_json(items: &[VideoItem]) -> Vec<Value> {
    items
        .iter()
        .filter(|i| i.duration_secs.is_none_or(|d| d >= 40))
        .map(|i| json!({"id": i.id, "title": i.title, "channel": i.channel, "duration": i.duration_secs.unwrap_or(0)}))
        .collect()
}

fn error(message: &str) -> String {
    json!({"error": message}).to_string()
}

fn resolve_track(id: &str) -> Result<Value, String> {
    if !crate::youtube::valid_video_id(id) {
        return Err("bad id".into());
    }
    let started = Instant::now();
    let info = innertube::player(id)?;
    let proxy = streamproxy::global().ok_or("Pteron could not start its local helper.")?;
    let set = streamproxy::VideoSet { videos: Vec::new(), audio: info.set.audio.clone(), duration_ms: info.set.duration_ms };
    let token = proxy.register(set);
    proxy.prepare(&token, AUDIO_ONLY, false)?;
    crate::crashlog::note(&format!("music: {id} ready in {} ms", started.elapsed().as_millis()));
    Ok(json!({
        "url": proxy.url(&token, AUDIO_ONLY),
        "title": clip(&info.title, 200),
        "channel": clip(&info.author, 120),
        "duration": info.duration_secs,
    }))
}

/// Answers one command of the Music page with JSON.
pub(crate) fn command(value: &str) -> String {
    let (verb, arg) = value.split_once(':').unwrap_or((value, ""));
    match verb {
        "search" => match innertube::search(&clip(arg, 200)) {
            Ok((items, next)) => json!({"items": items_json(&items), "next": next}).to_string(),
            Err(message) => error(&message),
        },
        "more" => match innertube::search_more(arg) {
            Ok((items, next)) => json!({"items": items_json(&items), "next": next}).to_string(),
            Err(message) => error(&message),
        },
        "list" => match innertube::playlist(arg) {
            Ok(list) => json!({"title": clip(&list.title, 200), "items": items_json(&list.items)}).to_string(),
            Err(message) => error(&message),
        },
        "track" => match resolve_track(arg) {
            Ok(v) => v.to_string(),
            Err(message) => {
                crate::crashlog::note(&format!("music: could not prepare {arg}: {message}"));
                error(&message)
            },
        },
        "get" => with_queue(|_| (), false).1.to_string(),
        "add" => match serde_json::from_str::<Value>(arg).ok().as_ref().and_then(Track::from_value) {
            Some(track) => with_queue(|q| q.add(track), true).1.to_string(),
            None => error("bad song"),
        },
        "addlist" => match innertube::playlist(arg) {
            Ok(list) => {
                let (added, state) = with_queue(
                    |q| list.items.iter().filter(|i| i.duration_secs.is_none_or(|d| d >= 40)).filter(|i| q.add(Track::from_item(i))).count(),
                    true,
                );
                let mut state = state;
                state["added"] = json!(added);
                state.to_string()
            },
            Err(message) => error(&message),
        },
        "rm" => match arg.parse::<usize>() {
            Ok(i) => with_queue(|q| q.remove(i), true).1.to_string(),
            Err(_) => error("bad number"),
        },
        "clear" => with_queue(|q| q.clear(), true).1.to_string(),
        "cur" => match arg.parse::<usize>() {
            Ok(i) => with_queue(|q| q.set_current(i), true).1.to_string(),
            Err(_) => error("bad number"),
        },
        "next" => {
            let (moved, mut state) = with_queue(|q| q.advance(arg == "user"), true);
            state["moved"] = json!(moved);
            state.to_string()
        },
        "prev" => {
            let (moved, mut state) = with_queue(|q| q.back(), true);
            state["moved"] = json!(moved);
            state.to_string()
        },
        "repeat" => match arg.parse::<u8>() {
            Ok(r) if r <= 2 => with_queue(|q| q.repeat = r, true).1.to_string(),
            _ => error("bad mode"),
        },
        "vol" => match arg.parse::<f64>() {
            Ok(v) if (0.0..=1.0).contains(&v) => with_queue(|q| q.volume = v, true).1.to_string(),
            _ => error("bad volume"),
        },
        "hb" => {
            let v: Value = serde_json::from_str(arg).unwrap_or(Value::Null);
            let mut shared = SHARED.lock().unwrap_or_else(|e| e.into_inner());
            shared.beat = Some(Instant::now());
            shared.playing = v.get("playing").and_then(|p| p.as_bool()).unwrap_or(false);
            shared.title = clip(v.get("title").and_then(|t| t.as_str()).unwrap_or(""), 120);
            json!({"cmd": shared.pending.take()}).to_string()
        },
        _ => error("unknown command"),
    }
}

// ---------------------------------------------------------------- the page

const CSS: &str = r#"
:root{color-scheme:light dark;--bg:#f6f4ef;--fg:#1c1b19;--mut:#6b675f;--card:#fff;--bd:#e2ddd2;--gold:#b8892a}
@media (prefers-color-scheme:dark){:root{--bg:#161513;--fg:#ece8df;--mut:#a19b8f;--card:#201f1c;--bd:#34322d;--gold:#d6aa48}}
html,body{margin:0;background:var(--bg);color:var(--fg);font:16px/1.5 "Segoe UI",system-ui,sans-serif}
header{position:sticky;top:0;z-index:5;display:flex;align-items:center;gap:18px;padding:9px 22px;background:var(--bg);border-bottom:1px solid var(--bd)}
.brand{display:flex;align-items:center;gap:8px;color:var(--fg);text-decoration:none;font-weight:600;font-size:19px}
.brand svg{width:24px;height:24px}
.search{flex:1;max-width:620px;margin:0 auto;display:flex;gap:8px}
.search input{flex:1;min-width:0;font:inherit;padding:8px 16px;border-radius:999px;border:1px solid var(--bd);background:var(--card);color:var(--fg);caret-color:var(--gold)}
button{font:inherit;cursor:pointer}
.search button,.gold{padding:8px 18px;border-radius:999px;border:0;background:var(--gold);color:#1b1608;font-weight:600}
a.plain{color:var(--mut);font-size:14px;text-decoration:none}a.plain:hover{color:var(--gold)}
main{max-width:1200px;margin:0 auto;padding:16px 22px 110px}
.cols{display:grid;grid-template-columns:minmax(0,1fr) 380px;gap:26px;align-items:start}
@media (max-width:900px){.cols{grid-template-columns:minmax(0,1fr)}}
h2{font-size:15px;margin:6px 0 10px;color:var(--mut);font-weight:600}
.tools{display:flex;gap:8px;margin:4px 0 10px;flex-wrap:wrap}
.small{padding:5px 12px;border-radius:8px;border:1px solid var(--bd);background:var(--card);color:var(--fg);font-size:14px}
.small:hover{border-color:var(--gold)}
.row{display:flex;align-items:center;gap:10px;padding:7px 10px;border-radius:10px;border:1px solid transparent}
.row:hover{background:var(--card);border-color:var(--bd)}
.row.on{border-color:var(--gold)}
.row .t{flex:1;min-width:0;cursor:pointer}
.row .n{white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.row .c{color:var(--mut);font-size:13px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.row .d{color:var(--mut);font-size:13px;min-width:42px;text-align:right}
.ib{border:0;background:transparent;color:var(--mut);font-size:18px;line-height:1;padding:4px 7px;border-radius:7px}
.ib:hover{color:var(--gold);background:rgba(214,170,72,.14)}
.msg{color:var(--mut);padding:8px 2px}
#bar{position:fixed;left:0;right:0;bottom:0;z-index:9;background:var(--card);border-top:1px solid var(--bd);display:flex;align-items:center;gap:16px;padding:10px 22px}
#now{width:260px;min-width:0}
#now b{display:block;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
#now span{display:block;color:var(--mut);font-size:13px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
#ctl{display:flex;align-items:center;gap:4px}
#ctl .ib{font-size:22px}
#pp{width:42px;height:42px;border-radius:50%;border:0;background:var(--gold);color:#1b1608;font-size:18px}
#seek{flex:1;min-width:80px;accent-color:var(--gold)}
#vol{width:90px;accent-color:var(--gold)}
.time{color:var(--mut);font-size:13px;min-width:90px;text-align:center}
@media (max-width:700px){#now{width:120px}#vol,.time{display:none}}
"#;

const JS: &str = r#"
(function(){
var U='@U@',A=document.getElementById('a'),Q={tracks:[],cur:null,repeat:0,vol:0.8},R={},gen=0,asked={},next=null,title='';
function el(t,c,x){var e=document.createElement(t);if(c)e.className=c;if(x!==undefined)e.textContent=x;return e;}
function $(i){return document.getElementById(i);}
function fmt(s){s=Math.max(0,Math.floor(s||0));var m=Math.floor(s/60),r=s%60;if(m>=60){var h=Math.floor(m/60);m=m%60;return h+':'+(m<10?'0':'')+m+':'+(r<10?'0':'')+r;}return m+':'+(r<10?'0':'')+r;}
function api(cmd,arg,cb){var x=new XMLHttpRequest();x.open('GET',U+'?x='+encodeURIComponent(arg===undefined||arg===null?cmd:cmd+':'+arg));
  x.onload=function(){var d=null;try{d=JSON.parse(x.responseText);}catch(e){}cb(d);};x.onerror=function(){cb(null);};x.send();}
function setQ(st){if(st&&st.tracks){Q=st;renderQ();}}
function say(m){var s=$('msg');s.textContent=m||'';}
function renderQ(){
  var box=$('q');box.textContent='';
  $('qn').textContent=Q.tracks.length?'('+Q.tracks.length+')':'';
  if(!Q.tracks.length){box.appendChild(el('div','msg','The queue is empty. Search for a song, or open a playlist.'));}
  Q.tracks.forEach(function(t,i){
    var r=el('div','row'+(i===Q.cur?' on':''));
    var tt=el('div','t');tt.appendChild(el('div','n',t.title||t.id));tt.appendChild(el('div','c',t.channel));
    tt.onclick=function(){play(i);};
    r.appendChild(tt);r.appendChild(el('div','d',t.duration?fmt(t.duration):''));
    var x=el('button','ib','×');x.title='Remove';x.onclick=function(){api('rm',i,function(st){setQ(st);});};
    r.appendChild(x);box.appendChild(r);
  });
  $('rep').textContent='Repeat: '+['off','all','one'][Q.repeat||0];
  var cur=Q.tracks[Q.cur];
  title=cur?cur.title:'';
  $('nt').textContent=cur?cur.title:'Nothing playing';
  $('nc').textContent=cur?cur.channel:'';
  document.title=(cur?cur.title+' - ':'')+'Plume Music';
}
function renderList(items,heading,listId,append){
  var box=$('res');
  if(!append){box.textContent='';$('lt').textContent=heading;}
  if(listId&&!append){
    var tl=el('div','tools');
    var all=el('button','small','Add all to queue');all.onclick=function(){say('Adding...');api('addlist',listId,function(st){say(st&&st.added!==undefined?st.added+' songs added.':'Could not add the playlist.');setQ(st);});};
    var go=el('button','small gold','Play all');go.onclick=function(){api('clear',null,function(){api('addlist',listId,function(st){setQ(st);if(st&&st.tracks&&st.tracks.length)play(0);});});};
    tl.appendChild(go);tl.appendChild(all);box.appendChild(tl);
  }
  if(!items.length&&!append){box.appendChild(el('div','msg','No songs found.'));}
  items.forEach(function(t){
    var r=el('div','row');
    var tt=el('div','t');tt.appendChild(el('div','n',t.title));tt.appendChild(el('div','c',t.channel));
    tt.onclick=function(){playNow(t);};
    r.appendChild(tt);r.appendChild(el('div','d',t.duration?fmt(t.duration):''));
    var p=el('button','ib','▶');p.title='Play now';p.onclick=function(){playNow(t);};
    var a=el('button','ib','+');a.title='Add to queue';a.onclick=function(){api('add',JSON.stringify(t),function(st){setQ(st);});};
    r.appendChild(p);r.appendChild(a);box.appendChild(r);
  });
}
function search(text){
  var m=text.match(/(?:^|[?&])list=([A-Za-z0-9_-]{10,64})/),id=m?m[1]:null;
  if(!id&&/^(PL|OLAK5uy_|RD|UU|FL)[A-Za-z0-9_-]{8,60}$/.test(text))id=text;
  say('');
  if(id){say('Opening the playlist...');
    api('list',id,function(d){if(!d||d.error){say((d&&d.error)||'Could not open the playlist.');return;}say('');next=null;$('more').hidden=true;renderList(d.items,d.title||'Playlist',id,false);});return;}
  say('Searching...');
  api('search',text,function(d){if(!d||d.error){say((d&&d.error)||'Search failed. Try again.');return;}say('');next=d.next;$('more').hidden=!next;renderList(d.items,'Results for “'+text+'”',null,false);});
}
function playNow(t){api('add',JSON.stringify(t),function(st){if(!st||!st.tracks)return;Q=st;renderQ();play(st.tracks.length-1);});}
function resolve(id,cb){
  if(R[id]){cb(R[id]);return;}
  api('track',id,function(d){if(!d||d.error){cb(null,d&&d.error);return;}R[id]=d;cb(d);});
}
function play(i){
  api('cur',i,function(st){
    setQ(st);var t=Q.tracks[i];if(!t)return;
    var my=++gen;$('nt').textContent=t.title;$('nc').textContent='Getting the song...';
    resolve(t.id,function(d,err){
      if(my!==gen)return;
      if(!d){$('nc').textContent='Could not play this song'+(err?' ('+err+')':'')+'.';setTimeout(function(){if(my===gen)advance('auto');},1500);return;}
      A.src=d.url;A.play();$('nc').textContent=t.channel;
    });
  });
}
function advance(who){
  api('next',who,function(st){
    if(!st||!st.tracks)return;
    var same=(st.cur===Q.cur);setQ(st);
    if(!st.moved){$('pp').textContent='▶';return;}
    if(same&&who==='auto'){A.currentTime=0;A.play();}else{play(st.cur);}
  });
}
function back(){
  if(A.currentTime>4){A.currentTime=0;return;}
  api('prev',null,function(st){if(!st||!st.tracks)return;setQ(st);if(st.moved)play(st.cur);});
}
function toggle(){
  if(!A.src||A.src===location.href){if(Q.cur!==null){play(Q.cur);}else if(Q.tracks.length){play(0);}return;}
  if(A.paused)A.play();else A.pause();
}
A.addEventListener('ended',function(){advance('auto');});
A.addEventListener('play',function(){$('pp').textContent='❚❚';});
A.addEventListener('pause',function(){$('pp').textContent='▶';});
A.addEventListener('timeupdate',function(){
  var d=A.duration;if(!isFinite(d)||!d){var c=Q.tracks[Q.cur];d=c?c.duration:0;}
  var s=$('seek');if(!s.__drag){s.max=d||0;s.value=A.currentTime;}
  $('tm').textContent=fmt(A.currentTime)+' / '+fmt(d);
  // get the next song ready a little before this one ends
  if(d&&d-A.currentTime<25&&Q.cur!==null){var n=Q.tracks[Q.cur+1]||(Q.repeat===1?Q.tracks[0]:null);
    if(n&&!asked[n.id]){asked[n.id]=1;resolve(n.id,function(){});}}
});
$('seek').addEventListener('input',function(){this.__drag=true;});
$('seek').addEventListener('change',function(){A.currentTime=parseFloat(this.value);this.__drag=false;});
$('pp').onclick=toggle;$('nx').onclick=function(){advance('user');};$('pv').onclick=back;
$('vol').addEventListener('input',function(){A.volume=parseFloat(this.value);});
$('vol').addEventListener('change',function(){api('vol',this.value,function(){});});
$('rep').onclick=function(){api('repeat',((Q.repeat||0)+1)%3,function(st){setQ(st);});};
$('clr').onclick=function(){api('clear',null,function(st){setQ(st);});};
$('more').onclick=function(){if(!next)return;var tok=next;next=null;api('more',tok,function(d){if(!d||d.error){say('Could not load more.');return;}next=d.next;$('more').hidden=!next;renderList(d.items,'',null,true);});};
$('sf').addEventListener('submit',function(e){e.preventDefault();var v=$('qi').value.trim();if(v)search(v);});
function fromHash(){var m=location.hash.match(/list=([A-Za-z0-9_-]{10,64})/);if(m){$('qi').value='';search('list='+m[1]);}}
window.addEventListener('hashchange',fromHash);
// the helper tells the page what the sidebar asked for, about once a second
setInterval(function(){
  api('hb',JSON.stringify({playing:!A.paused&&!A.ended,title:title}),function(d){
    if(!d||!d.cmd)return;
    if(d.cmd==='toggle')toggle();else if(d.cmd==='next')advance('user');else if(d.cmd==='prev')back();
  });
},1000);
api('get',null,function(st){
  if(st&&st.tracks){Q=st;A.volume=Q.vol;$('vol').value=Q.vol;renderQ();}
  fromHash();
});
})();
"#;

pub(crate) fn page_html() -> String {
    let url = streamproxy::global().map(|p| p.music_url()).unwrap_or_default();
    let js = JS.replace("@U@", &url);
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Plume Music</title><style>{CSS}</style></head><body>\
         <header><a class=\"brand\" href=\"https://music.youtube.com/\">Plume Music</a>\
         <form id=\"sf\" class=\"search\"><input id=\"qi\" placeholder=\"Search songs, or paste a playlist link\" autofocus>\
         <button type=\"submit\">Search</button></form>\
         <a class=\"plain\" href=\"https://www.youtube.com/\">Video</a></header>\
         <main><div class=\"cols\"><section><h2 id=\"lt\">Search for music</h2><div id=\"msg\" class=\"msg\"></div>\
         <div id=\"res\"><div class=\"msg\">Type a song or artist above. To play an album or playlist, paste its link.</div></div>\
         <button id=\"more\" class=\"small\" hidden>Show more</button></section>\
         <aside><h2>Queue <span id=\"qn\"></span></h2>\
         <div class=\"tools\"><button id=\"rep\" class=\"small\">Repeat: off</button><button id=\"clr\" class=\"small\">Clear</button></div>\
         <div id=\"q\"></div></aside></div></main>\
         <div id=\"bar\"><div id=\"now\"><b id=\"nt\">Nothing playing</b><span id=\"nc\"></span></div>\
         <div id=\"ctl\"><button id=\"pv\" class=\"ib\" title=\"Previous\">\u{23EE}</button>\
         <button id=\"pp\" title=\"Play or pause\">\u{25B6}</button>\
         <button id=\"nx\" class=\"ib\" title=\"Next\">\u{23ED}</button></div>\
         <input id=\"seek\" type=\"range\" min=\"0\" max=\"0\" step=\"1\" value=\"0\"><span id=\"tm\" class=\"time\">0:00 / 0:00</span>\
         <input id=\"vol\" type=\"range\" min=\"0\" max=\"1\" step=\"0.02\" value=\"0.8\" title=\"Volume\"></div>\
         <audio id=\"a\" preload=\"auto\"></audio><script>{js}</script></body></html>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str) -> Track {
        Track { id: id.into(), title: format!("t{id}"), channel: "c".into(), duration: 100 }
    }

    fn queue_of(n: usize) -> Queue {
        let mut q = Queue::default();
        for i in 0..n {
            q.add(track(&format!("AAAAAAAAAA{i}")));
        }
        q
    }

    #[test]
    fn plays_in_order_and_stops_at_the_end() {
        let mut q = queue_of(3);
        assert!(q.advance(true));
        assert_eq!(q.cur, Some(0));
        assert!(q.advance(false));
        assert!(q.advance(false));
        assert_eq!(q.cur, Some(2));
        assert!(!q.advance(false));
        assert_eq!(q.cur, Some(2));
    }

    #[test]
    fn repeat_modes() {
        let mut q = queue_of(2);
        q.set_current(1);
        q.repeat = 1;
        assert!(q.advance(false));
        assert_eq!(q.cur, Some(0));
        q.repeat = 2;
        assert!(q.advance(false));
        assert_eq!(q.cur, Some(0), "a song that ended replays");
        assert!(q.advance(true));
        assert_eq!(q.cur, Some(1), "the Next button still moves on");
    }

    #[test]
    fn going_back() {
        let mut q = queue_of(3);
        q.set_current(2);
        assert!(q.back());
        assert_eq!(q.cur, Some(1));
        q.set_current(0);
        assert!(q.back());
        assert_eq!(q.cur, Some(0));
        q.repeat = 1;
        assert!(q.back());
        assert_eq!(q.cur, Some(2));
        assert!(!Queue::default().back());
    }

    #[test]
    fn removing_songs_keeps_the_current_one_right() {
        let mut q = queue_of(4);
        q.set_current(2);
        q.remove(0);
        assert_eq!(q.cur, Some(1));
        q.remove(2);
        assert_eq!(q.cur, Some(1));
        q.remove(1);
        assert_eq!(q.cur, None);
        assert!(q.advance(false));
        assert_eq!(q.cur, Some(1.min(q.tracks.len() - 1)));
        q.clear();
        assert!(q.tracks.is_empty() && q.cur.is_none());
        q.remove(5);
    }

    #[test]
    fn the_queue_has_a_size_limit() {
        let mut q = Queue::default();
        for _ in 0..MAX_TRACKS {
            assert!(q.add(track("AAAAAAAAAAA")));
        }
        assert!(!q.add(track("AAAAAAAAAAA")));
    }

    #[test]
    fn saves_and_loads() {
        let mut q = queue_of(3);
        q.set_current(1);
        q.repeat = 2;
        q.volume = 0.4;
        let text = q.to_json().to_string();
        let back = Queue::from_json(&text);
        assert_eq!(back.tracks, q.tracks);
        assert_eq!((back.cur, back.repeat), (Some(1), 2));
        assert!((back.volume - 0.4).abs() < 1e-9);
    }

    #[test]
    fn loading_survives_garbage() {
        assert_eq!(Queue::from_json("not json"), Queue::default());
        let q = Queue::from_json(r#"{"tracks":[{"id":"bad"},{"id":"dQw4w9WgXcQ","title":"ok"}],"cur":7,"repeat":9,"vol":3}"#);
        assert_eq!(q.tracks.len(), 1);
        assert_eq!(q.cur, None);
        assert_eq!(q.repeat, 0);
        assert!((q.volume - 0.8).abs() < 1e-9);
    }

    #[test]
    fn commands_reject_bad_input() {
        for bad in ["nonsense", "rm:abc", "cur:-1", "repeat:7", "vol:2", "add:{}", "add:{\"id\":\"x\"}", "track:../../x"] {
            assert!(command(bad).contains("\"error\""), "{bad}");
        }
    }

    #[test]
    fn heartbeat_hands_over_one_pending_command() {
        send_command("next");
        let first = command(r#"hb:{"playing":true,"title":"Song"}"#);
        assert!(first.contains("\"next\""), "{first}");
        let second = command(r#"hb:{"playing":true,"title":"Song"}"#);
        assert!(second.contains("null"), "{second}");
        let now = now_playing().expect("playing");
        assert_eq!(now.title, "Song");
        assert!(now.playing);
    }

    /// `PTERON_MUSIC_DUMP=/tmp/music.html cargo test dump_page` writes the page for a browser test.
    #[test]
    fn dump_page() {
        if let Ok(path) = std::env::var("PTERON_MUSIC_DUMP") {
            let html = page_html().replace("var U='',", &format!("var U='{}',", std::env::var("PTERON_MUSIC_URL").unwrap_or_default()));
            std::fs::write(path, html).unwrap();
        }
    }

    #[test]
    fn the_page_has_what_the_script_needs() {
        let html = page_html();
        for id in ["sf", "qi", "res", "more", "q", "qn", "rep", "clr", "pp", "pv", "nx", "seek", "tm", "vol", "nt", "nc", "lt", "msg", "a"] {
            assert!(html.contains(&format!("id=\"{id}\"")), "missing id {id}");
        }
    }
}

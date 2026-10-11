//! **The phone remote**: search from a phone on the same Wi-Fi and send a title to the television.
//!
//! A second small HTTP server, on the LAN this time (port [`REMOTE_PORT`]), serving one page
//! (`GET /`) and three calls behind a four-digit PIN the television shows in Settings › Addons &
//! Home:
//!
//! - `GET /api/search?q=…&pin=…` — the bridge's own search, as a short list (title, year, kind,
//!   poster);
//! - `GET /art/{key}?pin=…` — a poster from that list;
//! - `GET /api/send?rk=…&pin=…` — queue the title for the television, which opens its page
//!   ([`take_request`], polled by the app's frame loop). Opening a page is all a phone can do: it
//!   never starts playback and never writes to the account.
//!
//! The remote acts with the account key the app last read with ([`super::bridge::Bridge::remote_token`]);
//! the key never leaves the television — the page receives titles and posters only. The PIN is new
//! every run, and ten wrong PINs lock the remote for ten minutes.
//!
//! Whether a webOS Developer Mode app may accept connections from the LAN is not verified on the
//! set: a refused bind is logged, and Settings then shows no address.

use super::bridge::Bridge;
use super::http::{self, Request, Response};
use serde_json::{json, Value};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The LAN port the phone page is served on (beside the bridge's loopback port).
pub const REMOTE_PORT: u16 = 32499;
/// Wrong PINs before the remote locks.
const MAX_FAILURES: u32 = 10;
/// How long a lock lasts.
const LOCKOUT: Duration = Duration::from_secs(600);
/// Results a search lists at most.
const RESULTS: usize = 24;

/// `(address, PIN)` once the server is listening.
static INFO: OnceLock<(String, String)> = OnceLock::new();
/// Titles sent from a phone, oldest first, until the app takes them.
static QUEUE: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Wrong PINs so far, and when the lock began.
static FAILURES: Mutex<(u32, Option<Instant>)> = Mutex::new((0, None));

/// The page's address and PIN, when the remote is listening.
pub fn info() -> Option<(String, String)> {
    INFO.get().cloned()
}

/// The oldest title a phone sent (an item address), if any.
pub fn take_request() -> Option<String> {
    let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    (!q.is_empty()).then(|| q.remove(0))
}

/// The television's address on the LAN: the local end of a route out (a UDP "connection" sends
/// nothing).
fn lan_address() -> Option<String> {
    let s = UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    s.connect(("192.0.2.1", 9)).ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then(|| ip.to_string())
}

/// A four-digit PIN, new every run.
fn new_pin() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let mixed = (nanos as u64 ^ (u64::from(std::process::id()) << 17)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    format!("{:04}", (mixed >> 32) % 10_000)
}

/// Starts the remote beside the bridge. A failure is logged and leaves the app as it was.
pub fn start(bridge: Arc<Bridge>) {
    let listener = match TcpListener::bind(("0.0.0.0", REMOTE_PORT)) {
        Ok(l) => l,
        Err(e) => {
            plx_base::eventlog::log(&format!("stremio: the phone remote could not listen on port {REMOTE_PORT}: {e}"));
            return;
        }
    };
    let pin = new_pin();
    let shown = pin.clone();
    let started = plx_base::task::spawn("stremio-remote", move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let bridge = bridge.clone();
            let pin = pin.clone();
            if plx_base::task::spawn("stremio-remote-request", move || serve(&bridge, &pin, stream)).is_none() {
                plx_base::eventlog::log("stremio: a phone remote request thread was refused");
            }
        }
    });
    if started.is_none() {
        plx_base::eventlog::log("stremio: the phone remote thread was refused");
        return;
    }
    match lan_address() {
        Some(ip) => {
            let _ = INFO.set((format!("http://{ip}:{REMOTE_PORT}"), shown));
            plx_base::eventlog::log(&format!("stremio: phone remote listening on port {REMOTE_PORT}"));
        }
        None => plx_base::eventlog::log("stremio: the phone remote has no LAN address to show"),
    }
}

fn serve(bridge: &Bridge, pin: &str, mut stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let Some(req) = http::read_request(&mut stream) else { return };
    let response = answer(bridge, pin, &req);
    let _ = http::write_response(&mut stream, req.method == "HEAD", &response);
}

/// One request's answer.
fn answer(bridge: &Bridge, pin: &str, req: &Request) -> Response {
    if req.method != "GET" && req.method != "HEAD" {
        return Response::empty(405);
    }
    let path = req.path.trim_end_matches('/');
    if path.is_empty() || path == "/index.html" {
        return Response { status: 200, content_type: "text/html; charset=utf-8", body: PAGE.as_bytes().to_vec(), location: None };
    }
    if !pin_ok(req.query("pin"), pin) {
        return Response::json(403, &json!({"error": "pin"}));
    }
    let Some(token) = bridge.remote_token() else {
        return Response::json(503, &json!({"error": "signin"}));
    };
    let as_app = |target: &str| bridge.handle(&Request::new("GET", target, &[("X-Plex-Token", token.as_str())]));
    if path == "/api/search" {
        let q = req.query("q").unwrap_or("").trim();
        if q.is_empty() {
            return Response::json(200, &json!({"results": []}));
        }
        let found = as_app(&format!("/hubs/search?query={}&limit={RESULTS}", encode(q)));
        let v: Value = serde_json::from_slice(&found.body).unwrap_or(Value::Null);
        return Response::json(200, &json!({"results": results(&v)}));
    }
    if path == "/api/send" {
        let Some(rk) = req.query("rk").filter(|rk| !rk.is_empty() && rk.len() < 512) else { return Response::empty(400) };
        let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        if q.len() < 4 {
            q.push(rk.to_string());
        }
        drop(q);
        plx_machine::present::wake_from_worker();
        return Response::json(200, &json!({"sent": true}));
    }
    if let Some(key) = path.strip_prefix("/art/").filter(|k| !k.contains('/')) {
        return as_app(&format!("/stremio/art/{key}"));
    }
    Response::empty(404)
}

/// The phone's list: every hub's titles, posters pointed at this server.
fn results(v: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    for hub in v.pointer("/MediaContainer/Hub").and_then(Value::as_array).into_iter().flatten() {
        for m in hub.get("Metadata").and_then(Value::as_array).into_iter().flatten() {
            let Some(rk) = m.get("ratingKey").and_then(Value::as_str) else { continue };
            let thumb = m.get("thumb").and_then(Value::as_str)
                .and_then(|t| t.strip_prefix("/stremio/art/"))
                .map(|k| format!("/art/{k}"));
            out.push(json!({
                "rk": rk, "title": m.get("title"), "year": m.get("year"),
                "kind": m.get("type"), "thumb": thumb,
            }));
            if out.len() >= RESULTS {
                return out;
            }
        }
    }
    out
}

/// Checks a PIN, counting wrong ones toward the lock.
fn pin_ok(given: Option<&str>, pin: &str) -> bool {
    let mut f = FAILURES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(since) = f.1 {
        if since.elapsed() < LOCKOUT {
            return false;
        }
        *f = (0, None);
    }
    if given == Some(pin) {
        return true;
    }
    f.0 += 1;
    if f.0 >= MAX_FAILURES {
        f.1 = Some(Instant::now());
        plx_base::eventlog::log("stremio: the phone remote is locked after too many wrong PINs");
    }
    false
}

/// Percent-encodes a query value.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The phone page: a PIN, a search box, posters, and Send to TV.
const PAGE: &str = r#"<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1"><title>TV remote</title>
<style>
:root{--bg:#101114;--card:#1b1d22;--ink:#eceef2;--dim:#9aa0aa;--acc:#e5a00d}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--ink);font:16px system-ui,sans-serif}
header{position:sticky;top:0;background:var(--bg);padding:12px 16px;display:flex;gap:8px}
input{flex:1;min-width:0;padding:12px;border-radius:10px;border:1px solid #333;background:var(--card);color:var(--ink);font-size:16px}
button{padding:12px 14px;border:0;border-radius:10px;background:var(--acc);color:#111;font-weight:600;font-size:15px}
#pin{max-width:90px}main{padding:0 16px 24px}
.grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(130px,1fr));gap:12px}
.card{background:var(--card);border-radius:10px;overflow:hidden;display:flex;flex-direction:column}
.card img{width:100%;aspect-ratio:2/3;object-fit:cover;background:#222}
.card .t{padding:8px 8px 0;font-size:14px}.card .m{padding:2px 8px 8px;color:var(--dim);font-size:12px}
.card button{margin:0 8px 8px;padding:8px}#msg{color:var(--dim);padding:8px 0}
</style></head><body>
<header><input id="pin" inputmode="numeric" maxlength="4" placeholder="PIN"><input id="q" placeholder="Search movies and shows" enterkeyhint="search"><button id="go">Search</button></header>
<main><div id="msg">Enter the PIN shown in Settings › Addons &amp; Home on the TV.</div><div class="grid" id="grid"></div></main>
<script>
const $=id=>document.getElementById(id);let pin='';
try{pin=localStorage.getItem('pin')||''}catch(e){}
$('pin').value=pin;
function say(t){$('msg').textContent=t}
async function call(path){const r=await fetch(path+(path.includes('?')?'&':'?')+'pin='+encodeURIComponent(pin));
if(r.status===403)throw new Error('Wrong PIN (or too many tries: wait ten minutes).');
if(r.status===503)throw new Error('Sign in on the TV first.');if(!r.ok)throw new Error('The TV did not answer.');return r.json()}
async function search(){pin=$('pin').value.trim();try{localStorage.setItem('pin',pin)}catch(e){}
const q=$('q').value.trim();if(!q)return;say('Searching…');$('grid').textContent='';
try{const d=await call('/api/search?q='+encodeURIComponent(q));say(d.results.length?'':'Nothing found.');
for(const r of d.results){const c=document.createElement('div');c.className='card';
const img=document.createElement('img');img.alt='';if(r.thumb)img.src=r.thumb+'?pin='+encodeURIComponent(pin);c.appendChild(img);
const t=document.createElement('div');t.className='t';t.textContent=r.title||'';c.appendChild(t);
const m=document.createElement('div');m.className='m';m.textContent=[r.kind==='movie'?'Movie':'Show',r.year].filter(Boolean).join(' · ');c.appendChild(m);
const b=document.createElement('button');b.textContent='Send to TV';b.onclick=async()=>{try{await call('/api/send?rk='+encodeURIComponent(r.rk));b.textContent='Sent ✓'}catch(e){say(e.message)}};
c.appendChild(b);$('grid').appendChild(c)}}catch(e){say(e.message)}}
$('go').onclick=search;$('q').addEventListener('keydown',e=>{if(e.key==='Enter')search()});
</script></body></html>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_search_answer_becomes_the_phone_list_with_posters_served_here() {
        let v = json!({"MediaContainer": {"Hub": [
            {"Metadata": [{"ratingKey": "m-tt1", "title": "Alien", "year": 1979, "type": "movie", "thumb": "/stremio/art/a1"}]},
            {"Metadata": [{"ratingKey": "s-tt2", "title": "Andor", "type": "show"}]},
        ]}});
        let r = results(&v);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0]["thumb"], "/art/a1");
        assert!(r[1]["thumb"].is_null());
        assert!(!serde_json::to_string(&r).unwrap().contains("token"));
    }

    #[test]
    fn a_query_value_is_percent_encoded() {
        assert_eq!(encode("star wars & co"), "star%20wars%20%26%20co");
    }

    #[test]
    fn a_pin_is_four_digits() {
        let p = new_pin();
        assert_eq!(p.len(), 4);
        assert!(p.chars().all(|c| c.is_ascii_digit()));
    }
}

//! **The Stremio bridge**: this fork's source of everything the app shows, in place of Plex.
//!
//! The rest of the app speaks Plex — plex.tv for sign-in and server discovery, a Plex Media Server
//! for libraries, details and playback — and all of it (the screens, the session machinery, the
//! player's route decision) was built and tested against those two services. Rather than teach
//! every layer a second protocol, this module runs a small HTTP service on the loopback interface
//! that answers as BOTH of them, and fills each answer from the account's Stremio addons. The
//! app's own test server (`tests/mock_pms.py`) stands in for Plex the same way; this is that idea
//! with a real catalog behind it.
//!
//! - `plx_plex::plex::account::plex_tv()` names this bridge, so sign-in, the account, the
//!   who's-watching roster and server discovery all come here. Sign-in is Stremio's own device
//!   link: the code and QR on the sign-in screen are a `link.stremio.com` code, and the account
//!   token the app stores is the Stremio auth key ([`bridge`]).
//! - Discovery finds one server, this one, at `http://127.0.0.1:<STREMIO_BRIDGE_PORT>`; the
//!   credential policy admits that exact origin (`plx_plex::plex::origin::is_stremio_bridge`), the
//!   one place a token may ride plaintext in every build, because it never leaves the device.
//!
//! Files: [`http`] the wire, [`api`] Stremio's services and what each addon is for, [`library`] how
//! addon catalogs become the Movies / TV Shows / Anime libraries, [`catalog`] Stremio titles in the
//! media server's shapes, [`streams`] reading and ranking where a title can be played, [`probe`] reading a chosen file's
//! tracks, [`media`] that file as a playable version with this viewer's tracks chosen, [`bridge`] the router, [`qr`]
//! the sign-in code's image.

mod api;
mod bridge;
mod catalog;
mod http;
mod library;
mod media;
mod probe;
mod qr;
mod remote;
mod streams;
#[cfg(test)]
mod tests;

use std::net::{TcpListener, TcpStream};

/// The viewer's stream preferences, as Settings stores them: `res` an index into
/// [`STREAM_RES_LADDER`] (0 = no cap), `instant_only` the Streams choice.
pub fn apply_stream_prefs(res: u8, instant_only: bool) {
    let max_res = STREAM_RES_LADDER.get(res as usize).copied().unwrap_or(0);
    streams::set_prefs(streams::Prefs { max_res, instant_only });
}

static HOME_CHANGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Something Home shows changed behind the app's back (a library add or remove, a Home-row or
/// addon setting): the app refetches its hubs once. Clears the flag.
pub fn take_home_changed() -> bool {
    HOME_CHANGED.swap(false, std::sync::atomic::Ordering::Relaxed)
}

/// Raised by the bridge after such a change.
pub(crate) fn note_home_changed() {
    HOME_CHANGED.store(true, std::sync::atomic::Ordering::Relaxed);
    plx_machine::present::wake_from_worker();
}

/// The resolution caps Settings offers, best first: none, 1080p, 720p.
pub const STREAM_RES_LADDER: [u32; 3] = [0, 1080, 720];
use std::sync::Arc;

/// The oldest title a phone sent to the television ([`remote`]), as an item address to open.
pub fn take_remote_request() -> Option<String> {
    remote::take_request()
}

/// Starts the bridge on its loopback port (and the phone remote beside it, [`remote`]). Called
/// once at boot, after libcurl is initialised and before anything can ask plex.tv a question. A
/// failure (the port is taken, threads refused) is logged and leaves the app running: sign-in then
/// reports plex.tv unreachable, which is what it is.
pub fn start() {
    let port = plx_plex::plex::account::STREMIO_BRIDGE_PORT;
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => listener,
        Err(e) => {
            plx_base::eventlog::log(&format!("stremio: bridge could not listen on loopback port {port}: {e}"));
            return;
        }
    };
    let bridge = Arc::new(bridge::Bridge::new(Box::new(api::Live), port));
    bridge.share();
    remote::start(bridge.clone());
    let started = plx_base::task::spawn("stremio-bridge", move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let bridge = bridge.clone();
            if plx_base::task::spawn("stremio-request", move || serve(&bridge, stream)).is_none() {
                plx_base::eventlog::log("stremio: a request thread was refused");
            }
        }
    });
    if started.is_some() {
        plx_base::eventlog::log(&format!("stremio: bridge listening on loopback port {port}"));
    } else {
        plx_base::eventlog::log("stremio: the bridge thread was refused");
    }
}

/// One connection: one request, one answer, closed.
fn serve(bridge: &bridge::Bridge, mut stream: TcpStream) {
    let Some(request) = http::read_request(&mut stream) else { return };
    let response = bridge.handle(&request);
    if response.status >= 400 {
        // Status and route shape only: paths can carry ids, and the query carries the key.
        plx_base::eventlog::log(&format!("stremio: {} {} -> {}", request.method,
            route_shape(&request.path), response.status));
    }
    let _ = http::write_response(&mut stream, request.method == "HEAD", &response);
}

/// A path with every segment that is not a fixed word replaced by `{}`, for a log line.
fn route_shape(path: &str) -> String {
    path.split('/')
        .map(|seg| if seg.chars().all(|c| c.is_ascii_lowercase() || c == ':') { seg } else { "{}" })
        .collect::<Vec<_>>()
        .join("/")
}

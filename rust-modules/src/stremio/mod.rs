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
//!   credential policy admits loopback origins (`plx_plex::plex::origin`), the one place a token
//!   may ride plaintext in every build, because it never leaves the device.
//!
//! Files: [`http`] the wire, [`api`] Stremio's services, [`bridge`] the router, [`qr`] the sign-in
//! code's image.

mod api;
mod bridge;
mod http;
mod qr;
#[cfg(test)]
mod tests;

use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

/// Starts the bridge on its loopback port. Called once at boot, after libcurl is initialised and
/// before anything can ask plex.tv a question. A failure (the port is taken, threads refused) is
/// logged and leaves the app running: sign-in then reports plex.tv unreachable, which is what it is.
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

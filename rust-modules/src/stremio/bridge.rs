//! The router: one function from an app request to an answer, written against [`Stremio`].
//!
//! Two services share it, told apart by path, exactly as two hosts would be:
//!
//! - **plex.tv** (`/api/v2/…`): the sign-in code, the account, the who's-watching roster and the
//!   server list. The sign-in code IS a Stremio device-link code, so the screen's QR opens
//!   Stremio's own approval page and the "account token" plex.tv would hand back is the Stremio
//!   `authKey`. The roster is one profile, the account; the server list is one server, this bridge.
//! - **the media server** (everything else): its identity and, built from the account's installed
//!   addons, one library per browsable catalog.
//!
//! The app keeps the token it was given and sends it on every later request, so the bridge holds
//! no account state of its own: each request carries the key it is answered with. What it does
//! hold is short-lived — the pending sign-in codes, and a minutes-long memo of the addon list so
//! a burst of library requests costs one Stremio call.

use super::api::{Addon, ApiError, Stremio};
use super::http::{Request, Response};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The bridge's server identity. Constant, so a stored session's server stays the same server
/// across launches.
pub const MACHINE_ID: &str = "stremio-bridge";
const FRIENDLY_NAME: &str = "Stremio";
/// The version the identity reports. The app reads PMS versions to gate features; this is the
/// one its own test server reports.
const VERSION: &str = "1.41.0.0000-stremio";
/// How long a sign-in code is offered before the screen mints another.
const LINK_TTL_S: i64 = 600;
/// How long an addon list is reused before Stremio is asked again.
const ADDONS_TTL: Duration = Duration::from_secs(300);
/// The one profile's numeric id. plex.tv ids are numbers and Stremio's are strings; the app
/// compares this number with the one the roster gave it, so it only has to be the same everywhere.
const USER_ID: i64 = 1;

/// One library: an addon catalog of a type the app can show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// `"1"`, `"2"`, … in the account's addon order.
    pub key: String,
    /// `movie` or `show`, the media server's names for Stremio's `movie` and `series`.
    pub kind: &'static str,
    pub title: String,
}

/// The libraries `addons` make: every browsable catalog of films or series, in order. A catalog
/// of another type (channels, live TV) has no screen to go to yet, so it is not a library.
pub fn sections(addons: &[Addon]) -> Vec<Section> {
    let mut out = Vec::new();
    for addon in addons {
        for catalog in addon.manifest.catalogs.iter().filter(|c| c.browsable()) {
            let kind = match catalog.kind.as_str() {
                "movie" => "movie",
                "series" => "show",
                _ => continue,
            };
            let name = catalog.name.as_deref().filter(|n| !n.trim().is_empty()).unwrap_or(&catalog.id);
            let title = if addon.manifest.name.trim().is_empty() {
                name.to_string()
            } else {
                format!("{name} · {}", addon.manifest.name.trim())
            };
            out.push(Section { key: (out.len() + 1).to_string(), kind, title });
        }
    }
    out
}

struct PendingLink {
    id: i64,
    code: String,
    link: String,
}

struct AddonMemo {
    token: String,
    at: Instant,
    addons: Arc<Vec<Addon>>,
}

/// The bridge: the account seam and the little state it keeps between requests.
pub struct Bridge {
    stremio: Box<dyn Stremio>,
    base: String,
    port: u16,
    links: Mutex<Vec<PendingLink>>,
    next_link_id: Mutex<i64>,
    addons: Mutex<Option<AddonMemo>>,
    /// The account's playback preferences as the app last set them. plex.tv keeps these per
    /// account; Stremio has no equivalent, so they live as long as the process does.
    profile: Mutex<serde_json::Map<String, Value>>,
}

fn container(fields: Value) -> Value {
    let mut mc = json!({"size": 0, "allowSync": false, "identifier": "com.plexapp.plugins.library"});
    if let (Some(mc), Some(fields)) = (mc.as_object_mut(), fields.as_object()) {
        for (k, v) in fields {
            mc.insert(k.clone(), v.clone());
        }
    }
    json!({ "MediaContainer": mc })
}

fn api_failure(e: ApiError) -> Response {
    match e {
        ApiError::Unauthorized => Response::empty(401),
        ApiError::Unavailable => Response::empty(502),
    }
}

impl Bridge {
    /// A bridge answering as `http://127.0.0.1:<port>`.
    pub fn new(stremio: Box<dyn Stremio>, port: u16) -> Bridge {
        let mut profile = serde_json::Map::new();
        profile.insert("autoSelectAudio".into(), json!(true));
        profile.insert("defaultAudioLanguage".into(), json!("en"));
        profile.insert("defaultSubtitleLanguage".into(), json!("en"));
        profile.insert("autoSelectSubtitle".into(), json!(0));
        profile.insert("defaultSubtitleForced".into(), json!(0));
        Bridge {
            stremio,
            base: format!("http://127.0.0.1:{port}"),
            port,
            links: Mutex::new(Vec::new()),
            // Distinct from any id an earlier run handed out, so a screen still polling a code
            // minted before a restart is told it is gone rather than shown somebody else's.
            next_link_id: Mutex::new(std::process::id() as i64 * 1000),
            addons: Mutex::new(None),
            profile: Mutex::new(profile),
        }
    }

    /// Answers one request.
    pub fn handle(&self, req: &Request) -> Response {
        let path = req.path.trim_end_matches('/');
        let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let method = req.method.as_str();
        match (method, segs.as_slice()) {
            // ---- plex.tv ----
            ("POST", ["api", "v2", "pins"]) => self.create_pin(),
            ("GET", ["api", "v2", "pins", "qr", code]) => self.pin_qr(code),
            ("GET", ["api", "v2", "pins", id]) => self.poll_pin(id),
            ("GET", ["api", "v2", "user"]) => self.user(req),
            ("GET" | "PUT", ["api", "v2", "user", "profile"]) => self.user_profile(req),
            ("GET", ["api", "v2", "home", "users"]) => self.home_users(req),
            ("POST", ["api", "v2", "home", "users", _, "switch"]) => self.switch(req),
            ("GET", ["api", "v2", "resources"]) => self.resources(req),
            (_, ["api", ..]) => Response::empty(404),
            // ---- the media server ----
            ("GET" | "HEAD", [] | ["identity"]) => self.identity(),
            ("GET" | "HEAD", ["library", "sections"]) => self.library_sections(req),
            // Everything not yet translated is a valid, empty answer rather than an error, so a
            // screen asking for something Stremio has no equivalent of shows nothing instead of
            // failing. A request without the account key is still refused.
            ("GET" | "HEAD", _) => match req.token() {
                Some(_) => Response::json(200, &container(json!({}))),
                None => Response::empty(401),
            },
            ("POST" | "PUT" | "DELETE", _) => Response::empty(200),
            _ => Response::empty(405),
        }
    }

    // ---------------------------------------------------------------- plex.tv: sign-in

    fn create_pin(&self) -> Response {
        let link = match self.stremio.create_link() {
            Ok(link) => link,
            Err(e) => return api_failure(e),
        };
        let id = {
            let mut next = self.next_link_id.lock().unwrap_or_else(|e| e.into_inner());
            *next += 1;
            *next
        };
        let body = json!({"id": id, "code": link.code, "expiresIn": LINK_TTL_S,
                          "authToken": null, "qr": ""});
        let mut links = self.links.lock().unwrap_or_else(|e| e.into_inner());
        // Only the newest few codes can still be on a screen.
        if links.len() >= 8 {
            links.remove(0);
        }
        links.push(PendingLink { id, code: link.code, link: link.link });
        Response::json(201, &body)
    }

    fn pin_qr(&self, code: &str) -> Response {
        let link = {
            let links = self.links.lock().unwrap_or_else(|e| e.into_inner());
            links.iter().find(|l| l.code == code).map(|l| l.link.clone())
        };
        match link.and_then(|l| super::qr::png(&l)) {
            Some(png) => Response::png(png),
            None => Response::empty(404),
        }
    }

    fn poll_pin(&self, id: &str) -> Response {
        let Ok(id) = id.parse::<i64>() else { return Response::empty(404) };
        let code = {
            let links = self.links.lock().unwrap_or_else(|e| e.into_inner());
            links.iter().find(|l| l.id == id).map(|l| l.code.clone())
        };
        // An unknown id is a code this process never minted: gone, so the screen mints a new one.
        let Some(code) = code else { return Response::empty(404) };
        // A failed read is a poll that saw nothing yet. The screen polls again; only the code's
        // own expiry ends the wait.
        let token = self.stremio.read_link(&code).ok().flatten();
        if token.is_some() {
            self.links.lock().unwrap_or_else(|e| e.into_inner()).retain(|l| l.id != id);
        }
        Response::json(200, &json!({"id": id, "code": code, "expiresIn": LINK_TTL_S,
                                    "authToken": token}))
    }

    // ---------------------------------------------------------------- plex.tv: the account

    fn user(&self, req: &Request) -> Response {
        let Some(token) = req.token() else { return Response::empty(401) };
        let user = match self.stremio.user(token) {
            Ok(user) => user,
            Err(e) => return api_failure(e),
        };
        let title = if user.email.is_empty() { FRIENDLY_NAME.to_string() } else { user.email.clone() };
        let profile = Value::Object(self.profile.lock().unwrap_or_else(|e| e.into_inner()).clone());
        Response::json(200, &json!({
            "id": USER_ID, "uuid": user.id, "username": title, "title": title,
            "friendlyName": title, "email": user.email, "thumb": "",
            "profile": profile,
            "subscription": {"active": true, "status": "Active", "plan": "lifetime"},
        }))
    }

    fn user_profile(&self, req: &Request) -> Response {
        if req.token().is_none() {
            return Response::empty(401);
        }
        let mut profile = self.profile.lock().unwrap_or_else(|e| e.into_inner());
        if req.method == "PUT" {
            for key in ["defaultAudioLanguage", "defaultSubtitleLanguage"] {
                if let Some(v) = req.query(key) {
                    profile.insert(key.into(), json!(v));
                }
            }
            if let Some(v) = req.query("autoSelectAudio") {
                profile.insert("autoSelectAudio".into(), json!(v == "true" || v == "1"));
            }
            for key in ["autoSelectSubtitle", "defaultSubtitleForced"] {
                if let Some(n) = req.query(key).and_then(|v| v.parse::<i64>().ok()) {
                    profile.insert(key.into(), json!(n));
                }
            }
        }
        Response::json(200, &Value::Object(profile.clone()))
    }

    /// The roster's one profile. Its `uuid` is the Stremio account id, so it matches `/user`.
    fn profile_row(&self, token: &str) -> Result<Value, ApiError> {
        let user = self.stremio.user(token)?;
        let title = if user.email.is_empty() { FRIENDLY_NAME.to_string() } else { user.email };
        Ok(json!({"id": USER_ID, "uuid": user.id, "title": title, "thumb": "",
                  "admin": true, "restricted": false, "protected": false}))
    }

    fn home_users(&self, req: &Request) -> Response {
        let Some(token) = req.token() else { return Response::empty(401) };
        match self.profile_row(token) {
            Ok(row) => Response::json(200, &json!({"users": [row]})),
            Err(e) => api_failure(e),
        }
    }

    fn switch(&self, req: &Request) -> Response {
        let Some(token) = req.token() else { return Response::empty(401) };
        match self.profile_row(token) {
            Ok(mut row) => {
                if let Some(row) = row.as_object_mut() {
                    row.insert("authToken".into(), json!(token));
                }
                Response::json(201, &row)
            }
            Err(e) => api_failure(e),
        }
    }

    /// The account's one server: this bridge, reachable at loopback only, handed the same key.
    fn resources(&self, req: &Request) -> Response {
        let Some(token) = req.token() else { return Response::empty(401) };
        Response::json(200, &json!([{
            "name": FRIENDLY_NAME, "clientIdentifier": MACHINE_ID, "provides": "server",
            "owned": true, "home": false, "ownerId": USER_ID, "presence": true,
            "accessToken": token, "httpsRequired": false, "publicAddressMatches": true,
            "sourceTitle": null,
            "connections": [{"protocol": "http", "address": "127.0.0.1", "port": self.port,
                             "uri": self.base, "local": true, "relay": false, "IPv6": false}],
        }]))
    }

    // ---------------------------------------------------------------- the media server

    fn identity(&self) -> Response {
        Response::json(200, &container(json!({
            "machineIdentifier": MACHINE_ID, "friendlyName": FRIENDLY_NAME, "version": VERSION,
            "myPlex": true, "myPlexSubscription": true, "platform": "Linux",
        })))
    }

    /// The account's addons, from the memo when it is fresh and for the same key.
    fn addons(&self, token: &str) -> Result<Arc<Vec<Addon>>, ApiError> {
        {
            let memo = self.addons.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(m) = memo.as_ref().filter(|m| m.token == token && m.at.elapsed() < ADDONS_TTL) {
                return Ok(m.addons.clone());
            }
        }
        let addons = Arc::new(self.stremio.addons(token)?);
        *self.addons.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(AddonMemo { token: token.to_string(), at: Instant::now(), addons: addons.clone() });
        Ok(addons)
    }

    fn library_sections(&self, req: &Request) -> Response {
        let Some(token) = req.token() else { return Response::empty(401) };
        let addons = match self.addons(token) {
            Ok(addons) => addons,
            Err(e) => return api_failure(e),
        };
        let rows: Vec<Value> = sections(&addons)
            .into_iter()
            .map(|s| json!({"key": s.key, "type": s.kind, "title": s.title,
                            "agent": "tv.plex.agents.movie", "scanner": "Plex Movie",
                            "language": "en-US", "refreshing": false, "allowSync": false}))
            .collect();
        Response::json(200, &container(json!({"size": rows.len(), "Directory": rows})))
    }
}

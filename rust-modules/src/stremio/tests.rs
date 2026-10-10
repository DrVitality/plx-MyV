//! The bridge's rules, against a scripted Stremio account: no socket, no network.

use super::api::{Addon, ApiError, Catalog, ExtraProp, LinkCode, Manifest, Stremio, User};
use super::bridge::{sections, Bridge, MACHINE_ID};
use super::http::{decode_component, parse_head, Request};
use std::sync::Mutex;

const KEY: &str = "stremio-auth-key";

struct Scripted {
    approved: Mutex<bool>,
    addons: Vec<Addon>,
}

impl Stremio for Scripted {
    fn create_link(&self) -> Result<LinkCode, ApiError> {
        Ok(LinkCode { code: "ABCD".into(), link: "https://link.stremio.com?code=ABCD".into() })
    }
    fn read_link(&self, code: &str) -> Result<Option<String>, ApiError> {
        assert_eq!(code, "ABCD");
        Ok(self.approved.lock().unwrap().then(|| KEY.to_string()))
    }
    fn user(&self, auth_key: &str) -> Result<User, ApiError> {
        if auth_key != KEY {
            return Err(ApiError::Unauthorized);
        }
        Ok(User { id: "5f00stremioid".into(), email: "viewer@example.invalid".into() })
    }
    fn addons(&self, auth_key: &str) -> Result<Vec<Addon>, ApiError> {
        if auth_key != KEY {
            return Err(ApiError::Unauthorized);
        }
        Ok(self.addons.clone())
    }
}

fn catalog(kind: &str, id: &str, name: Option<&str>) -> Catalog {
    Catalog { id: id.into(), kind: kind.into(), name: name.map(Into::into), extra: Vec::new(),
              extra_required: Vec::new() }
}

fn addon(name: &str, catalogs: Vec<Catalog>) -> Addon {
    Addon { manifest: Manifest { name: name.into(), catalogs } }
}

fn bridge(addons: Vec<Addon>) -> Bridge {
    Bridge::new(Box::new(Scripted { approved: Mutex::new(false), addons }), 32498)
}

fn json_of(r: &super::http::Response) -> serde_json::Value {
    serde_json::from_slice(&r.body).expect("a JSON body")
}

fn authed(method: &str, target: &str) -> Request {
    Request::new(method, target, &[("X-Plex-Token", KEY)])
}

#[test]
fn a_request_head_parses_with_its_decoded_query_and_body_length() {
    let head = "GET /library/sections?X-Plex-Token=a%2Bb&x=1+2 HTTP/1.1\r\nHost: 127.0.0.1\r\n\
                Content-Length: 5\r\nx-plex-token: from-header";
    let (req, length) = parse_head(head).expect("a valid head");
    assert_eq!(req.method, "GET");
    assert_eq!(req.path, "/library/sections");
    assert_eq!(req.query("x"), Some("1 2"));
    assert_eq!(length, 5);
    // The header wins over the query parameter, and lookups ignore case.
    assert_eq!(req.token(), Some("from-header"));
    assert!(parse_head("GARBAGE").is_none());
    assert!(parse_head("GET relative HTTP/1.1").is_none());
}

#[test]
fn percent_decoding_keeps_an_invalid_escape_literally() {
    assert_eq!(decode_component("a%20b%zz%4"), "a b%zz%4");
}

#[test]
fn only_browsable_film_and_series_catalogs_become_libraries_in_addon_order() {
    let mut search_only = catalog("movie", "search", Some("Search"));
    search_only.extra = vec![ExtraProp { is_required: true }];
    let mut legacy_required = catalog("series", "genre", Some("By genre"));
    legacy_required.extra_required = vec!["genre".into()];
    let addons = vec![
        addon("Cinemeta", vec![catalog("movie", "top", Some("Popular")), search_only,
                               catalog("series", "top", Some("Popular"))]),
        addon("Live", vec![catalog("tv", "channels", Some("Channels")), legacy_required]),
        addon("", vec![catalog("movie", "unnamed", None)]),
    ];
    let got = sections(&addons);
    let summary: Vec<(&str, &str, &str)> =
        got.iter().map(|s| (s.key.as_str(), s.kind, s.title.as_str())).collect();
    assert_eq!(summary, vec![
        ("1", "movie", "Popular · Cinemeta"),
        ("2", "show", "Popular · Cinemeta"),
        ("3", "movie", "unnamed"),
    ]);
}

#[test]
fn sign_in_mints_a_link_code_serves_its_qr_and_hands_over_the_key_once_approved() {
    let scripted = Scripted { approved: Mutex::new(false), addons: Vec::new() };
    let b = Bridge::new(Box::new(scripted), 32498);
    let created = b.handle(&Request::new("POST", "/api/v2/pins?strong=false", &[]));
    assert_eq!(created.status, 201);
    let pin = json_of(&created);
    assert_eq!(pin["code"], "ABCD");
    assert!(pin["authToken"].is_null());
    let id = pin["id"].as_i64().expect("a numeric id");

    let qr = b.handle(&Request::new("GET", "/api/v2/pins/qr/ABCD", &[]));
    assert_eq!(qr.status, 200);
    assert_eq!(qr.content_type, "image/png");
    assert!(qr.body.starts_with(b"\x89PNG"));

    let pending = b.handle(&Request::new("GET", &format!("/api/v2/pins/{id}"), &[]));
    assert!(json_of(&pending)["authToken"].is_null());
    // A code this process never minted is gone, so the screen mints another.
    assert_eq!(b.handle(&Request::new("GET", "/api/v2/pins/7", &[])).status, 404);
}

#[test]
fn an_approved_code_returns_the_stremio_key_as_the_account_token() {
    let b = Bridge::new(Box::new(Scripted { approved: Mutex::new(true), addons: Vec::new() }), 32498);
    let id = json_of(&b.handle(&Request::new("POST", "/api/v2/pins", &[])))["id"].as_i64().unwrap();
    let linked = b.handle(&Request::new("GET", &format!("/api/v2/pins/{id}"), &[]));
    assert_eq!(json_of(&linked)["authToken"], KEY);
}

#[test]
fn the_account_is_one_admin_profile_whose_uuid_matches_the_user() {
    let b = bridge(Vec::new());
    let user = json_of(&b.handle(&authed("GET", "/api/v2/user")));
    let roster = json_of(&b.handle(&authed("GET", "/api/v2/home/users")));
    let users = roster["users"].as_array().expect("a roster");
    assert_eq!(users.len(), 1);
    assert_eq!(users[0]["uuid"], user["uuid"]);
    assert_eq!(users[0]["id"], user["id"]);
    assert_eq!(users[0]["admin"], true);
    assert_eq!(users[0]["protected"], false);
    let switched = json_of(&b.handle(&authed("POST", "/api/v2/home/users/5f00stremioid/switch")));
    assert_eq!(switched["authToken"], KEY);
}

#[test]
fn a_refused_or_missing_key_is_a_401() {
    let b = bridge(Vec::new());
    let wrong = Request::new("GET", "/api/v2/user", &[("X-Plex-Token", "not-the-key")]);
    assert_eq!(b.handle(&wrong).status, 401);
    assert_eq!(b.handle(&Request::new("GET", "/api/v2/resources", &[])).status, 401);
    assert_eq!(b.handle(&Request::new("GET", "/library/sections", &[])).status, 401);
}

#[test]
fn discovery_finds_this_bridge_at_loopback_with_the_same_key() {
    let b = bridge(Vec::new());
    let resources = json_of(&b.handle(&authed("GET", "/api/v2/resources?includeHttps=1")));
    let server = &resources[0];
    assert_eq!(server["clientIdentifier"], MACHINE_ID);
    assert_eq!(server["provides"], "server");
    assert_eq!(server["accessToken"], KEY);
    assert_eq!(server["connections"][0]["uri"], "http://127.0.0.1:32498");
    // Identity is unauthenticated and names the same machine discovery advertised.
    let identity = json_of(&b.handle(&Request::new("GET", "/identity", &[])));
    assert_eq!(identity["MediaContainer"]["machineIdentifier"], MACHINE_ID);
}

#[test]
fn the_libraries_are_the_addon_catalogs() {
    let b = bridge(vec![addon("Cinemeta", vec![catalog("movie", "top", Some("Popular"))])]);
    let listed = json_of(&b.handle(&authed("GET", "/library/sections")));
    let dirs = listed["MediaContainer"]["Directory"].as_array().expect("sections");
    assert_eq!(dirs.len(), 1);
    assert_eq!(dirs[0]["key"], "1");
    assert_eq!(dirs[0]["type"], "movie");
    assert_eq!(dirs[0]["title"], "Popular · Cinemeta");
}

#[test]
fn an_untranslated_read_is_an_empty_container_and_a_write_is_accepted() {
    let b = bridge(Vec::new());
    let hubs = b.handle(&authed("GET", "/hubs"));
    assert_eq!(hubs.status, 200);
    assert_eq!(json_of(&hubs)["MediaContainer"]["size"], 0);
    assert_eq!(b.handle(&authed("PUT", "/:/timeline")).status, 200);
}

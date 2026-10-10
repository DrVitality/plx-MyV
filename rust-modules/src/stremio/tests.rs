//! The bridge's rules, against a scripted Stremio account: no socket, no network.

use super::api::{Addon, ApiError, Catalog, ExtraProp, LinkCode, Manifest, Resource, Stremio, User};
use super::bridge::{Bridge, MACHINE_ID};
use super::catalog::{escape, Rk, Title};
use super::http::{decode_component, parse_head, Request};
use super::library::{self, Lib};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;

const KEY: &str = "stremio-auth-key";
const CINEMETA: &str = "https://v3-cinemeta.example/manifest.json";
const KITSU: &str = "https://kitsu.example/manifest.json";

#[derive(Default)]
struct Scripted {
    approved: Mutex<bool>,
    addons: Vec<Addon>,
    library: Vec<Value>,
    /// URL → JSON body.
    pages: HashMap<String, Value>,
    /// URL → file bytes, for range reads.
    files: HashMap<String, Vec<u8>>,
    /// Every library write, in order.
    written: Mutex<Vec<Value>>,
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
    fn library(&self, auth_key: &str) -> Result<Vec<Value>, ApiError> {
        if auth_key != KEY {
            return Err(ApiError::Unauthorized);
        }
        Ok(self.library.clone())
    }
    fn library_put(&self, auth_key: &str, items: &[Value]) -> Result<(), ApiError> {
        if auth_key != KEY {
            return Err(ApiError::Unauthorized);
        }
        self.written.lock().unwrap().extend(items.iter().cloned());
        Ok(())
    }
    fn fetch(&self, url: &str) -> Result<Vec<u8>, ApiError> {
        self.pages.get(url).map(|v| serde_json::to_vec(v).unwrap()).ok_or(ApiError::Unavailable)
    }
    fn fetch_range(&self, url: &str, from: u64, len: u64) -> Result<Vec<u8>, ApiError> {
        let file = self.files.get(url).ok_or(ApiError::Unavailable)?;
        let from = usize::try_from(from).unwrap().min(file.len());
        let to = (from + usize::try_from(len).unwrap()).min(file.len());
        Ok(file[from..to].to_vec())
    }
}

fn catalog(kind: &str, id: &str, name: Option<&str>) -> Catalog {
    Catalog { id: id.into(), kind: kind.into(), name: name.map(Into::into), extra: Vec::new(),
              extra_required: Vec::new() }
}

fn addon(url: &str, name: &str, resources: Value, types: &[&str], prefixes: Option<&[&str]>,
         catalogs: Vec<Catalog>) -> Addon {
    Addon {
        transport_url: url.into(),
        manifest: Manifest {
            id: format!("org.example.{}", name.to_ascii_lowercase()),
            name: name.into(),
            catalogs,
            resources: resources.as_array().cloned().unwrap_or_default(),
            types: types.iter().map(|t| t.to_string()).collect(),
            id_prefixes: prefixes.map(|p| p.iter().map(|x| x.to_string()).collect()),
        },
    }
}

fn cinemeta() -> Addon {
    let mut search = catalog("movie", "top", Some("Popular"));
    search.extra = vec![ExtraProp { name: "search".into(), is_required: false, options: Vec::new() }];
    let mut series_search = catalog("series", "top", Some("Popular"));
    series_search.extra = vec![ExtraProp { name: "search".into(), is_required: false, options: Vec::new() }];
    addon(CINEMETA, "Cinemeta", json!(["catalog", "meta"]), &["movie", "series"], Some(&["tt"]),
          vec![search, series_search])
}

fn kitsu() -> Addon {
    addon(KITSU, "Kitsu", json!(["catalog", "meta"]), &["anime", "series", "movie"], Some(&["kitsu:"]),
          vec![catalog("anime", "kitsu-anime-trending", Some("Trending"))])
}

fn torrentio() -> Addon {
    let mut downloads = catalog("other", "realdebrid", Some("RD downloads"));
    downloads.extra = vec![ExtraProp { name: "skip".into(), is_required: false, options: Vec::new() }];
    addon("https://torrentio.example/manifest.json", "Torrentio",
          json!(["stream", {"name": "catalog", "types": ["other"]}]), &["movie", "series", "anime"],
          Some(&["tt", "kitsu"]), vec![downloads])
}

fn scripted(addons: Vec<Addon>) -> Scripted {
    Scripted { addons, ..Default::default() }
}

fn bridge_with(s: Scripted) -> Bridge {
    Bridge::new(Box::new(s), 32498)
}

fn bridge(addons: Vec<Addon>) -> Bridge {
    bridge_with(scripted(addons))
}

fn json_of(r: &super::http::Response) -> Value {
    serde_json::from_slice(&r.body).expect("a JSON body")
}

fn authed(method: &str, target: &str) -> Request {
    Request::new(method, target, &[("X-Plex-Token", KEY)])
}

/// Is `hub` the bridge's listing `n` — a linked shelf whose page lists the whole of it?
fn is_listing(hub: &Value, n: &str) -> bool {
    hub["key"] == format!("/library/collections/stremio-{n}/children")
        && hub["hubIdentifier"].as_str().is_some_and(|id| id.starts_with("custom.collection.") && id.ends_with(&format!(".stremio-{n}")))
}

fn metas(rows: Value) -> Value {
    json!({ "metas": rows })
}

// ---------------------------------------------------------------- the wire

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

// ---------------------------------------------------------------- sign-in and the account

#[test]
fn sign_in_mints_a_link_code_serves_its_qr_and_hands_over_the_key_once_approved() {
    let b = bridge(Vec::new());
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
    let b = bridge_with(Scripted { approved: Mutex::new(true), ..Default::default() });
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
    // English subtitles whenever there are some.
    assert_eq!(user["profile"]["defaultSubtitleLanguage"], "en");
    assert_eq!(user["profile"]["autoSelectSubtitle"], 2);
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

// ---------------------------------------------------------------- what each addon is for

#[test]
fn addons_are_told_apart_by_the_resources_they_declare() {
    let (c, k, t) = (cinemeta(), kitsu(), torrentio());
    assert!(c.provides(Resource::Meta, "movie", "tt0111161"));
    assert!(!c.provides(Resource::Stream, "movie", "tt0111161"), "a metadata provider is not a stream provider");
    assert!(!c.provides(Resource::Meta, "series", "kitsu:1376"), "Cinemeta does not know Kitsu ids");
    assert!(k.provides(Resource::Meta, "series", "kitsu:1376"));
    assert!(t.provides(Resource::Stream, "movie", "tt0111161"));
    assert!(!t.provides(Resource::Meta, "movie", "tt0111161"), "a stream provider is not a metadata provider");
    // A narrowed resource keeps its own types: Torrentio's catalog is for `other` only.
    assert!(t.provides(Resource::Catalog, "other", "realdebrid"));
    assert!(!t.provides(Resource::Catalog, "movie", "tt1"));
    assert_eq!(library::summary(&[c, k, t]),
               "3 addons: 3 catalog, 2 metadata, 1 stream, 0 subtitle providers");
}

#[test]
fn catalogs_land_in_movies_shows_and_anime_and_others_are_left_out() {
    let mut required = catalog("series", "genre", Some("By genre"));
    required.extra_required = vec!["genre".into()];
    let mut c = cinemeta();
    c.manifest.catalogs.push(required);
    c.manifest.catalogs.push(catalog("series", "anime-airing", Some("Anime airing")));
    let all = library::shelves(&[c, kitsu(), torrentio()]);
    let placed: Vec<(usize, usize, Lib)> = all.iter().map(|s| (s.addon, s.catalog, s.lib)).collect();
    assert_eq!(placed, vec![
        (0, 0, Lib::Movies),
        (0, 1, Lib::Shows),
        // Trending leads the Anime tab, ahead of an addon listed earlier
        (1, 0, Lib::Anime),
        (0, 3, Lib::Anime),
    ]);
    assert_eq!(library::libraries(&all), vec![Lib::Movies, Lib::Shows, Lib::Anime]);
}

#[test]
fn the_anime_tab_opens_on_popular_then_trending_and_other_libraries_keep_their_order() {
    let mut k = kitsu();
    k.manifest.catalogs = vec![
        catalog("anime", "kitsu-anime-airing", Some("Top Airing")),
        catalog("anime", "kitsu-anime-trending", Some("Trending")),
        catalog("anime", "kitsu-anime-rating", Some("Highest Rated")),
        catalog("anime", "kitsu-anime-popular", Some("Most Popular")),
    ];
    let all = library::shelves(&[cinemeta(), k]);
    let placed: Vec<(usize, usize, Lib)> = all.iter().map(|s| (s.addon, s.catalog, s.lib)).collect();
    assert_eq!(placed, vec![
        (0, 0, Lib::Movies),
        (0, 1, Lib::Shows),
        (1, 3, Lib::Anime),
        (1, 1, Lib::Anime),
        (1, 0, Lib::Anime),
        (1, 2, Lib::Anime),
    ]);
}

// ---------------------------------------------------------------- ids and titles

#[test]
fn an_item_address_round_trips_through_the_path_safe_form() {
    for rk in [
        Rk::item("movie", "tt0111161").unwrap(),
        Rk::item("series", "kitsu:1376").unwrap(),
        Rk::Season { stype: "series".into(), id: "tt0944947".into(), season: 3 },
        Rk::Episode { stype: "anime".into(), id: "kitsu:1376".into(), video: "kitsu:1376:5".into() },
    ] {
        let s = rk.encode();
        assert!(s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-~".contains(&b)), "{s}");
        assert_eq!(Rk::decode(&s), Some(rk));
    }
    assert_eq!(escape("tt1:2 3"), "tt1_3A2_203");
    assert_eq!(Rk::item("tv", "x"), None, "no screen for live TV yet");
    assert_eq!(Rk::decode("q"), None);
}

#[test]
fn a_meta_reads_forgivingly_into_a_title() {
    let v = json!({"id": "tt0944947", "type": "series", "name": "Game of Thrones",
        "releaseInfo": "2011–2019", "imdbRating": "9.2", "runtime": "1h 2min",
        "genres": ["Drama", ""], "cast": [{"name": "Emilia Clarke"}, "Kit Harington"],
        "videos": [
            {"id": "tt0944947:1:2", "season": 1, "episode": 2, "name": "The Kingsroad"},
            {"id": "tt0944947:0:1", "season": 0, "episode": 1},
            {"id": "tt0944947:1:1", "season": "1", "episode": "1", "released": "2011-04-17T21:00:00.000Z"},
        ]});
    let t = Title::read(&v, "movie").expect("a title");
    assert_eq!(t.stype, "series");
    assert_eq!(t.year, Some(2011));
    assert_eq!(t.rating, Some(9.2));
    assert_eq!(t.minutes, Some(62));
    assert_eq!(t.genres, vec!["Drama"]);
    assert_eq!(t.cast, vec!["Emilia Clarke", "Kit Harington"]);
    assert_eq!(t.seasons(), vec![1, 0], "specials last");
    let eps: Vec<i64> = t.episodes(1).iter().map(|e| e.episode).collect();
    assert_eq!(eps, vec![1, 2]);
    assert_eq!(t.episodes(1)[0].released.as_deref(), Some("2011-04-17"));
    assert!(Title::read(&json!({"name": "no id"}), "movie").is_none());
}

// ---------------------------------------------------------------- the media server

fn content_bridge() -> Bridge {
    bridge_with(content_scripted())
}

fn content_scripted() -> Scripted {
    let mut s = scripted(vec![cinemeta(), kitsu(), torrentio()]);
    s.pages.insert("https://v3-cinemeta.example/catalog/movie/top.json".into(), metas(json!([
        {"id": "tt0111161", "type": "movie", "name": "The Shawshank Redemption",
         "poster": "https://img.example/p/tt0111161.jpg", "releaseInfo": "1994"},
        {"id": "tt0068646", "type": "movie", "name": "The Godfather"},
    ])));
    s.pages.insert("https://v3-cinemeta.example/catalog/movie/top/skip=2.json".into(), metas(json!([])));
    s.pages.insert("https://v3-cinemeta.example/catalog/series/top.json".into(), metas(json!([
        {"id": "tt0944947", "type": "series", "name": "Game of Thrones"},
    ])));
    s.pages.insert("https://v3-cinemeta.example/catalog/series/top/skip=1.json".into(), metas(json!([])));
    s.pages.insert("https://kitsu.example/catalog/anime/kitsu-anime-trending.json".into(), metas(json!([
        {"id": "kitsu:1376", "type": "series", "name": "Fullmetal Alchemist: Brotherhood"},
    ])));
    s.pages.insert("https://kitsu.example/catalog/anime/kitsu-anime-trending/skip=1.json".into(), metas(json!([])));
    s.pages.insert("https://v3-cinemeta.example/meta/series/tt0944947.json".into(), json!({"meta": {
        "id": "tt0944947", "type": "series", "name": "Game of Thrones", "poster": "https://img.example/got.jpg",
        "runtime": "57 min",
        "videos": [
            {"id": "tt0944947:1:1", "season": 1, "episode": 1, "name": "Winter Is Coming"},
            {"id": "tt0944947:1:2", "season": 1, "episode": 2, "name": "The Kingsroad"},
            {"id": "tt0944947:2:1", "season": 2, "episode": 1, "name": "The North Remembers"},
        ]}}));
    s.pages.insert("https://v3-cinemeta.example/meta/movie/tt0111161.json".into(), json!({"meta": {
        "id": "tt0111161", "type": "movie", "name": "The Shawshank Redemption", "runtime": "142 min",
        "cast": ["Tim Robbins"], "director": ["Frank Darabont"]}}));
    s.pages.insert("https://v3-cinemeta.example/catalog/movie/top/search=alien.json".into(), metas(json!([
        {"id": "tt0078748", "type": "movie", "name": "Alien"}])));
    s.pages.insert("https://v3-cinemeta.example/catalog/series/top/search=alien.json".into(), metas(json!([])));
    s.pages.insert("https://torrentio.example/stream/movie/tt0111161.json".into(), json!({"streams": [
        {"name": "[RD download] Torrentio\n4k", "title": "Shawshank.2160p.WEB-DL.DDP5.1.x265", "url": "https://t/r/1"},
        {"name": "[RD+] Torrentio\n1080p", "title": "Shawshank.1080p.BluRay.DD5.1.x264\n👤 40 💾 9.1 GB", "url": "https://t/r/2"},
        {"name": "Torrentio\n4k", "title": "bare torrent", "infoHash": "abcdef"},
    ]}));
    s.library = vec![
        json!({"_id": "tt0944947", "type": "series", "name": "Game of Thrones", "removed": false, "temp": false,
               "state": {"timeOffset": 600000, "duration": 3600000, "video_id": "tt0944947:1:2",
                         "lastWatched": "2026-10-09T20:00:00.000Z"}}),
        json!({"_id": "tt0111161", "type": "movie", "name": "The Shawshank Redemption", "removed": true, "temp": false,
               "state": {"timeOffset": 1000, "lastWatched": "2026-10-08T20:00:00.000Z"}}),
        json!({"_id": "tt0068646", "type": "movie", "name": "The Godfather", "removed": false, "temp": false,
               "state": {"timeOffset": 0}}),
    ];
    s
}

#[test]
fn the_libraries_are_movies_shows_and_anime() {
    let b = content_bridge();
    let listed = json_of(&b.handle(&authed("GET", "/library/sections")));
    let dirs = listed["MediaContainer"]["Directory"].as_array().expect("sections");
    let summary: Vec<(&str, &str, &str)> = dirs.iter()
        .map(|d| (d["key"].as_str().unwrap(), d["type"].as_str().unwrap(), d["title"].as_str().unwrap()))
        .collect();
    // Anime is its own library type, so the app gives it a tab of its own.
    assert_eq!(summary, vec![("1", "movie", "Movies"), ("2", "show", "TV Shows"), ("3", "anime", "Anime")]);
}

#[test]
fn a_library_grid_lists_its_first_catalog_and_ends_where_the_catalog_does() {
    let b = content_bridge();
    let grid = json_of(&b.handle(&authed("GET",
        "/library/sections/1/all?X-Plex-Container-Start=0&X-Plex-Container-Size=60&includeMeta=1")));
    let mc = &grid["MediaContainer"];
    assert_eq!(mc["totalSize"], 2);
    let first = &mc["Metadata"][0];
    assert_eq!(first["title"], "The Shawshank Redemption");
    assert_eq!(first["type"], "movie");
    assert_eq!(first["year"], 1994);
    assert_eq!(first["librarySectionID"], 1);
    // Playable from the grid itself: a film page opened from here plays this row's part.
    let part = first["Media"][0]["Part"][0]["key"].as_str().unwrap();
    assert!(part.starts_with("/library/parts/") && part.ends_with(".mkv"), "{part}");
    let thumb = first["thumb"].as_str().unwrap();
    assert!(thumb.starts_with("/stremio/art/") && thumb.len() < 40, "a short art stand-in: {thumb}");
    assert_eq!(mc["Meta"]["Type"][0]["active"], true);
}

#[test]
fn a_library_shelf_is_each_of_its_catalogs_and_home_leads_with_continue_watching() {
    let b = content_bridge();
    let anime = json_of(&b.handle(&authed("GET", "/hubs/sections/3")));
    let hub = &anime["MediaContainer"]["Hub"][0];
    assert_eq!(hub["title"], "Trending");
    assert_eq!(hub["Metadata"][0]["librarySectionID"], 3);
    // A show's row carries no version; only a film plays from its row.
    assert!(hub["Metadata"][0].get("Media").is_none());
    let more = hub["key"].as_str().unwrap();
    let page = json_of(&b.handle(&authed("GET", more)));
    assert_eq!(page["MediaContainer"]["Metadata"][0]["title"], "Fullmetal Alchemist: Brotherhood");

    let home = json_of(&b.handle(&authed("GET", "/hubs")));
    let hubs = home["MediaContainer"]["Hub"].as_array().unwrap();
    assert_eq!(hubs[0]["hubIdentifier"], "home.continue");
    // Removed and unstarted items are not in it; a series is its show, saying where it stopped.
    let cw = hubs[0]["Metadata"].as_array().unwrap();
    assert_eq!(cw.len(), 1);
    assert_eq!(cw[0]["type"], "show");
    assert_eq!(cw[0]["title"], "Game of Thrones");
    assert_eq!(cw[0]["summary"], "Continue S1 · E2");
    assert_eq!(cw[0]["ratingKey"], Rk::item("series", "tt0944947").unwrap().encode());
    assert_eq!(hubs.len(), 5, "Continue Watching, a shelf from each library, My Library: {hubs:?}");
    // My Library: what the account saved (not removed), marked as saved
    let mine = hubs.last().unwrap();
    assert!(is_listing(mine, "library"), "{mine}");
    let titles: Vec<&str> = mine["Metadata"].as_array().unwrap().iter().map(|r| r["title"].as_str().unwrap()).collect();
    assert!(titles.contains(&"Game of Thrones") && titles.contains(&"The Godfather"), "{titles:?}");
    assert!(!titles.contains(&"The Shawshank Redemption"), "a removed item is not saved");
    // a catalog row of a saved title says so, which is what the card menu's Library row reads
    let movies = hubs.iter().find(|h| h["title"] == "Popular" && h["type"] == "movie").unwrap();
    let godfather = movies["Metadata"].as_array().unwrap().iter().find(|r| r["title"] == "The Godfather").unwrap();
    assert_eq!(godfather["stremioInLibrary"], true);
}

#[test]
fn home_rows_follow_the_viewers_switches_and_order() {
    let b = content_bridge();
    let config = json_of(&b.handle(&authed("GET", "/stremio/config")));
    let ids: Vec<String> = config["rows"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap().to_string()).collect();
    assert_eq!(ids[0], "continue");
    assert!(ids.contains(&"library".to_string()) && ids.contains(&"new_episodes".to_string()));
    // hide Continue Watching, move My Library to the top
    b.handle(&authed("GET", "/stremio/config?op=show&id=continue&on=0"));
    let after = json_of(&b.handle(&authed("GET", "/stremio/config?op=move&id=library&to=0")));
    assert_eq!(after["rows"][0]["id"], "library");
    assert_eq!(after["rows"][1]["id"], "continue");
    assert_eq!(after["rows"][1]["shown"], false);
    let home = json_of(&b.handle(&authed("GET", "/hubs")));
    let hubs = home["MediaContainer"]["Hub"].as_array().unwrap();
    assert!(is_listing(&hubs[0], "library"));
    assert!(hubs.iter().all(|h| h["hubIdentifier"] != "home.continue"), "a hidden row is not on Home");
    assert!(super::take_home_changed(), "the app is told to refetch Home");
}

#[test]
fn an_addon_switched_off_for_catalogs_leaves_every_library_and_for_streams_every_list() {
    let b = content_bridge();
    let config = json_of(&b.handle(&authed("GET", "/stremio/config")));
    let addons = config["addons"].as_array().unwrap();
    let kitsu = addons.iter().find(|a| a["name"] == "Kitsu").unwrap();
    assert_eq!(kitsu["catalogs"], true);
    assert!(kitsu["streams"].is_null(), "Kitsu offers no streams to switch");
    let id = kitsu["id"].as_str().unwrap();
    b.handle(&authed("GET", &format!("/stremio/config?op=catalogs&id={id}&on=0")));
    let listed = json_of(&b.handle(&authed("GET", "/library/sections")));
    let titles: Vec<&str> = listed["MediaContainer"]["Directory"].as_array().unwrap().iter()
        .map(|d| d["title"].as_str().unwrap()).collect();
    assert_eq!(titles, ["Movies", "TV Shows"], "no anime catalog is left");

    let torrentio = addons.iter().find(|a| a["name"] == "Torrentio").unwrap();
    assert_eq!(torrentio["streams"], true);
    let tid = torrentio["id"].as_str().unwrap();
    b.handle(&authed("GET", &format!("/stremio/config?op=streams&id={tid}&on=0")));
    let rk = Rk::item("movie", "tt0111161").unwrap().encode();
    let got = json_of(&b.handle(&authed("GET", &format!("/stremio/streams/{rk}"))));
    assert_eq!(got["streams"].as_array().unwrap().len(), 0);
}

#[test]
fn saving_and_removing_a_title_writes_the_library() {
    let s = content_scripted();
    let b = bridge_with(s);
    let rk = Rk::item("movie", "tt0111161").unwrap().encode();
    let added = json_of(&b.handle(&authed("GET", &format!("/stremio/library/{rk}?add=1"))));
    assert_eq!(added["inLibrary"], true);
    let removed = json_of(&b.handle(&authed("GET", &format!("/stremio/library/{rk}?add=0"))));
    assert_eq!(removed["inLibrary"], false);
}

#[test]
fn a_show_page_has_its_seasons_and_episodes_from_the_metadata_provider() {
    let b = content_bridge();
    let show = Rk::item("series", "tt0944947").unwrap().encode();
    let detail = json_of(&b.handle(&authed("GET", &format!("/library/metadata/{show}"))));
    assert_eq!(detail["MediaContainer"]["Metadata"][0]["childCount"], 2);
    // The show page resumes the episode the account stopped in.
    let deck = &detail["MediaContainer"]["Metadata"][0]["OnDeck"]["Metadata"];
    assert_eq!(deck["index"], 2);
    assert_eq!(deck["parentIndex"], 1);
    assert_eq!(deck["viewOffset"], 600000);
    let seasons = json_of(&b.handle(&authed("GET", &format!("/library/metadata/{show}/children"))));
    let seasons = seasons["MediaContainer"]["Metadata"].as_array().unwrap();
    assert_eq!(seasons.len(), 2);
    assert_eq!(seasons[0]["title"], "Season 1");
    assert_eq!(seasons[0]["leafCount"], 2);
    let s1 = seasons[0]["ratingKey"].as_str().unwrap();
    let eps = json_of(&b.handle(&authed("GET", &format!("/library/metadata/{s1}/children"))));
    let eps = eps["MediaContainer"]["Metadata"].as_array().unwrap();
    assert_eq!(eps.len(), 2);
    assert_eq!(eps[1]["title"], "The Kingsroad");
    assert_eq!(eps[1]["grandparentTitle"], "Game of Thrones");
    assert!(eps[1]["Media"][0]["Part"][0]["key"].as_str().unwrap().starts_with("/library/parts/"));
    let leaves = json_of(&b.handle(&authed("GET", &format!("/library/metadata/{show}/allLeaves"))));
    assert_eq!(leaves["MediaContainer"]["size"], 3);
    let ep = eps[1]["ratingKey"].as_str().unwrap();
    let one = json_of(&b.handle(&authed("GET", &format!("/library/metadata/{ep}"))));
    assert_eq!(one["MediaContainer"]["Metadata"][0]["index"], 2);
    // The last 3 % stands in for the credits, so Up Next offers the next episode there.
    let marker = &one["MediaContainer"]["Metadata"][0]["Marker"][0];
    assert_eq!(marker["type"], "credits");
    assert_eq!(marker["final"], true);
    assert_eq!(marker["endTimeOffset"], 57 * 60_000);
    assert_eq!(marker["startTimeOffset"], 57 * 60_000 - 102_600);
}

#[test]
fn a_film_page_has_its_people_and_a_playable_version() {
    let b = content_bridge();
    let rk = Rk::item("movie", "tt0111161").unwrap().encode();
    let detail = json_of(&b.handle(&authed("GET", &format!("/library/metadata/{rk}"))));
    let m = &detail["MediaContainer"]["Metadata"][0];
    assert_eq!(m["duration"], 142 * 60_000);
    assert_eq!(m["Role"][0]["tag"], "Tim Robbins");
    assert_eq!(m["Director"][0]["tag"], "Frank Darabont");
    let part = &m["Media"][0]["Part"][0];
    assert_eq!(part["key"], format!("/library/parts/{}/{rk}/file.mkv", part["id"]));
    // A title no metadata provider has is a 404, not an empty page.
    let unknown = Rk::item("movie", "tt9999999").unwrap().encode();
    assert_eq!(b.handle(&authed("GET", &format!("/library/metadata/{unknown}"))).status, 404);
}

#[test]
fn search_asks_the_searchable_catalogs_and_groups_by_kind() {
    let b = content_bridge();
    let found = json_of(&b.handle(&authed("GET", "/hubs/search?query=alien&limit=5")));
    let hubs = found["MediaContainer"]["Hub"].as_array().unwrap();
    assert_eq!(hubs[0]["type"], "movie");
    assert_eq!(hubs[0]["Metadata"][0]["title"], "Alien");
    assert_eq!(hubs[1]["type"], "show");
    assert_eq!(hubs[1]["size"], 0);
}

#[test]
fn an_untranslated_read_is_an_empty_container_and_a_write_is_accepted() {
    let b = bridge(Vec::new());
    let other = b.handle(&authed("GET", "/media/providers"));
    assert_eq!(other.status, 200);
    assert_eq!(json_of(&other)["MediaContainer"]["size"], 0);
    assert_eq!(b.handle(&authed("PUT", "/:/timeline")).status, 200);
}

#[test]
fn a_films_streams_are_ranked_and_the_cached_one_is_picked() {
    let b = content_bridge();
    let rk = Rk::item("movie", "tt0111161").unwrap().encode();
    let got = json_of(&b.handle(&authed("GET", &format!("/stremio/streams/{rk}"))));
    let rows = got["streams"].as_array().expect("streams");
    assert_eq!(rows.len(), 2, "the bare torrent is never offered");
    assert_eq!(rows[0]["ready"], "Cached");
    assert_eq!(rows[0]["best"], true);
    assert_eq!(rows[1]["ready"], "Uncached");
    assert!(rows.iter().all(|r| r.get("url").is_none()), "no debrid link leaves the bridge");
}

#[test]
fn the_stream_picker_lists_playable_streams_and_names_the_one_that_plays() {
    let b = content_bridge();
    let rk = Rk::item("movie", "tt0111161").unwrap().encode();
    let got = json_of(&b.handle(&authed("GET", &format!("/stremio/choices/{rk}"))));
    let mc = &got["MediaContainer"];
    let rows = mc["Metadata"].as_array().expect("rows");
    assert_eq!(rows.len(), 2);
    assert_eq!(mc["offset"], 0, "the best stream plays until the viewer picks another");
    assert!(rows[0]["title"].as_str().unwrap().contains("Instant"), "{}", rows[0]["title"]);
    assert!(rows.iter().all(|r| !r.to_string().contains("http")), "no stream link leaves the bridge");
    // choosing the second one makes it the one that plays
    let chose = b.handle(&authed("GET", &format!("/stremio/choose/{rk}?index=1")));
    assert_eq!(chose.status, 200);
    assert!(json_of(&chose).get("MediaContainer").is_some(), "the app's client reads a container");
    let again = json_of(&b.handle(&authed("GET", &format!("/stremio/choices/{rk}"))));
    assert_eq!(again["MediaContainer"]["offset"], 1);
}

// ---------------------------------------------------------------- playback

#[test]
fn a_dead_first_stream_falls_through_to_the_next_that_answers() {
    let mut s = content_scripted();
    s.pages.insert("https://torrentio.example/stream/movie/tt0111161.json".into(), json!({"streams": [
        {"name": "[RD+] Torrentio\n1080p", "title": "Dead.1080p.WEB.x264-AAA", "url": "https://t/dead"},
        {"name": "[RD+] Torrentio\n1080p", "title": "Alive.1080p.WEB.x264-BBB", "url": "https://t/alive"},
    ]}));
    s.files.insert("https://t/alive".into(), sample_mkv());
    let b = bridge_with(s);
    let rk = Rk::item("movie", "tt0111161").unwrap().encode();
    let detail = json_of(&b.handle(&authed("GET", &format!("/library/metadata/{rk}"))));
    let part = detail["MediaContainer"]["Metadata"][0]["Media"][0]["Part"][0]["key"].as_str().unwrap().to_string();
    let played = b.handle(&authed("GET", &part));
    assert_eq!(played.location.as_deref(), Some("https://t/alive"));
}

#[test]
fn the_next_episode_plays_from_the_release_the_last_one_did() {
    let mut s = content_scripted();
    for ep in ["1:1", "1:2"] {
        s.pages.insert(format!("https://torrentio.example/stream/series/tt0944947:{ep}.json"), json!({"streams": [
            {"name": "[RD+] Torrentio\n1080p", "title": format!("GoT.S01E0{}.1080p.WEB.x264-TOP", &ep[2..]), "url": format!("https://t/top/{ep}")},
            {"name": "[RD+] Torrentio\n1080p", "title": format!("GoT.S01E0{}.1080p.WEB.x264-MINE", &ep[2..]), "url": format!("https://t/mine/{ep}")},
        ]}));
    }
    let b = bridge_with(s);
    let e1 = Rk::Episode { stype: "series".into(), id: "tt0944947".into(), video: "tt0944947:1:1".into() }.encode();
    let e2 = Rk::Episode { stype: "series".into(), id: "tt0944947".into(), video: "tt0944947:1:2".into() }.encode();
    // the viewer picks MINE for episode 1
    b.handle(&authed("GET", &format!("/stremio/choose/{e1}?index=1")));
    let _ = b.handle(&authed("GET", &format!("/library/metadata/{e1}")));
    // episode 2, never picked, follows the same release group
    let page = json_of(&b.handle(&authed("GET", &format!("/stremio/choices/{e2}"))));
    let playing = page["MediaContainer"]["offset"].as_i64().unwrap();
    let row = page["MediaContainer"]["Metadata"].as_array().unwrap().iter().find(|r| r["index"] == playing).unwrap();
    assert!(row["summary"].as_str().unwrap().contains("MINE"), "{row}");
}

#[test]
fn aniskip_openings_and_endings_become_markers() {
    let v = json!({"found": true, "results": [
        {"interval": {"startTime": 90.5, "endTime": 180.0}, "skipType": "op"},
        {"interval": {"startTime": 1300.0, "endTime": 1390.0}, "skipType": "ed"},
        {"interval": {"startTime": 5.0, "endTime": 2.0}, "skipType": "op"},
    ]});
    let m = super::bridge::aniskip_markers_for_test(&v, 1_420_000);
    assert_eq!(m.len(), 2);
    assert_eq!(m[0]["type"], "intro");
    assert_eq!(m[0]["startTimeOffset"], 90_500);
    assert_eq!(m[1]["type"], "credits");
    assert_eq!(m[1]["final"], true);
    assert!(super::bridge::aniskip_markers_for_test(&json!({"found": false}), 1_000).is_empty());
}

/// An EBML element.
fn el(id: u32, data: &[u8]) -> Vec<u8> {
    let idb = id.to_be_bytes();
    let skip = idb.iter().take_while(|b| **b == 0).count();
    let mut out = idb[skip..].to_vec();
    if data.len() < 127 {
        out.push(0x80 | data.len() as u8);
    } else {
        out.push(0x01);
        out.extend_from_slice(&(data.len() as u64).to_be_bytes()[1..]);
    }
    out.extend_from_slice(data);
    out
}

/// A Matroska file with HEVC video, TrueHD and E-AC-3 English audio, and an English SRT.
fn sample_mkv() -> Vec<u8> {
    let track = |n: u8, kind: u8, codec: &str, lang: &str, extra: Vec<u8>| {
        el(0xAE, &[el(0xD7, &[n]), el(0x83, &[kind]), el(0x86, codec.as_bytes()),
                   el(0x22B59C, lang.as_bytes()), extra].concat())
    };
    let tracks = el(0x1654AE6B, &[
        track(1, 1, "V_MPEGH/ISO/HEVC", "und", el(0xE0, &[el(0xB0, &[0x0F, 0x00]), el(0xBA, &[0x08, 0x70])].concat())),
        track(2, 2, "A_TRUEHD", "eng", el(0xE1, &el(0x9F, &[8]))),
        track(3, 2, "A_EAC3", "eng", el(0xE1, &el(0x9F, &[6]))),
        track(4, 17, "S_TEXT/UTF8", "eng", Vec::new()),
    ].concat());
    [el(0x1A45DFA3, &el(0x4282, b"matroska")), el(0x18538067, &[tracks, el(0x1F43B675, &[0; 8])].concat())].concat()
}

fn playback_bridge() -> Bridge {
    let mut s = content_scripted();
    s.files.insert("https://t/r/2".into(), sample_mkv());
    bridge_with(s)
}

#[test]
fn a_film_plays_its_chosen_stream_with_the_tracks_read_from_the_file() {
    let b = playback_bridge();
    let rk = Rk::item("movie", "tt0111161").unwrap().encode();
    let detail = json_of(&b.handle(&authed("GET", &format!("/library/metadata/{rk}"))));
    let media = &detail["MediaContainer"]["Metadata"][0]["Media"][0];
    assert_eq!(media["videoCodec"], "hevc");
    assert_eq!(media["audioCodec"], "eac3", "the playable English track, not the TrueHD default");
    assert_eq!(media["videoResolution"], "4k");
    let streams = media["Part"][0]["Stream"].as_array().unwrap();
    assert_eq!(streams.len(), 4, "the file has an English subtitle, so no external one");
    assert_eq!(streams[3]["selected"], true);

    // The part key is one the player direct-plays: a file name it streams itself (`.mkv`), and
    // the part id where it reads it from, after `parts` — the id track choices are PUT to.
    let part = media["Part"][0]["key"].as_str().unwrap();
    assert!(part.ends_with(".mkv"), "{part}: not a file the player streams itself");
    let id_seg = part.split('/').skip_while(|s| *s != "parts").nth(1);
    assert_eq!(id_seg, Some(media["Part"][0]["id"].to_string().as_str()));
    // …and it redirects to the cached stream the ranking chose.
    let played = b.handle(&authed("GET", part));
    assert_eq!(played.status, 302);
    assert_eq!(played.location.as_deref(), Some("https://t/r/2"));

    // The decision is direct play, every stream copied.
    let decision = json_of(&b.handle(&authed("GET",
        &format!("/video/:/transcode/universal/decision?path=%2Flibrary%2Fmetadata%2F{rk}&directPlay=1"))));
    let mc = &decision["MediaContainer"];
    assert_eq!(mc["generalDecisionCode"], 1000);
    assert_eq!(mc["Metadata"][0]["Media"][0]["Part"][0]["decision"], "directplay");
}

#[test]
fn opensubtitles_is_searched_by_language_and_an_installed_file_joins_the_item() {
    let mut s = content_scripted();
    s.files.insert("https://t/r/2".into(), sample_mkv());
    s.pages.insert("https://opensubtitles-v3.strem.io/subtitles/movie/tt0111161.json".into(), json!({"subtitles": [
        {"id": "1", "url": "https://subs.example/en/1.srt", "lang": "eng"},
        {"id": "2", "url": "https://subs.example/es/2.srt", "lang": "spa"},
        {"id": "3", "url": "https://subs.example/es/3.srt", "lang": "spa"},
    ]}));
    s.pages.insert("https://subs.example/es/2.srt".into(), json!("1\n00:00:01,000 --> 00:00:02,000\nHola\n"));
    let b = bridge_with(s);
    let rk = Rk::item("movie", "tt0111161").unwrap().encode();
    // the item is read first, as the player has before its Search page opens
    let _ = b.handle(&authed("GET", &format!("/library/metadata/{rk}")));
    let found = json_of(&b.handle(&authed("GET",
        &format!("/library/metadata/{rk}/subtitles?language=es&hearingImpaired=0&forced=0"))));
    let rows = found["MediaContainer"]["Stream"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "only the Spanish files");
    assert!(rows.iter().all(|r| r["languageCode"] == "spa" && r["providerTitle"] == "OpenSubtitles"));
    let key = rows[0]["key"].as_str().unwrap().to_string();
    assert!(!rows.iter().any(|r| r.to_string().contains("subs.example")), "no provider address in the list");
    // install: 200, and the next read of the item carries it as an outside subtitle
    let put = b.handle(&authed("PUT", &format!("/library/metadata/{rk}/subtitles?key={key}")));
    assert_eq!(put.status, 200);
    let detail = json_of(&b.handle(&authed("GET", &format!("/library/metadata/{rk}"))));
    let streams = detail["MediaContainer"]["Metadata"][0]["Media"][0]["Part"][0]["Stream"].as_array().unwrap();
    let added = streams.iter().find(|s| s["key"] == key.as_str()).expect("the installed subtitle");
    assert_eq!(added["languageCode"], "spa");
    assert_eq!(added["selected"], false, "the viewer selects it; installing does not");
    // and the bridge serves the file at that key
    assert_eq!(b.handle(&authed("GET", &key)).status, 200);
    // a key from no search is accepted and changes nothing, as on the media server
    let stale = b.handle(&authed("PUT", &format!("/library/metadata/{rk}/subtitles?key=/library/streams/123")));
    assert_eq!(stale.status, 200);
}

#[test]
fn a_track_choice_sticks_to_the_item() {
    let b = playback_bridge();
    let rk = Rk::item("movie", "tt0111161").unwrap().encode();
    let detail = json_of(&b.handle(&authed("GET", &format!("/library/metadata/{rk}"))));
    let part = detail["MediaContainer"]["Metadata"][0]["Media"][0]["Part"][0]["id"].as_i64().unwrap();
    let truehd = detail["MediaContainer"]["Metadata"][0]["Media"][0]["Part"][0]["Stream"][1]["id"].as_i64().unwrap();
    let put = format!("/library/parts/{part}?audioStreamID={truehd}&subtitleStreamID=0");
    assert_eq!(b.handle(&authed("PUT", &put)).status, 200);
    let again = json_of(&b.handle(&authed("GET", &format!("/library/metadata/{rk}"))));
    let media = &again["MediaContainer"]["Metadata"][0]["Media"][0];
    assert_eq!(media["audioCodec"], "truehd");
    assert_eq!(media["Part"][0]["Stream"][3]["selected"], false, "subtitles off");
}

#[test]
fn nothing_cached_is_a_refusal_the_player_can_read_out() {
    let mut s = content_scripted();
    s.pages.insert("https://v3-cinemeta.example/meta/movie/tt0068646.json".into(),
                   json!({"meta": {"id": "tt0068646", "type": "movie", "name": "The Godfather"}}));
    let b = bridge_with(s);
    let rk = Rk::item("movie", "tt0068646").unwrap().encode();
    // A title with no streams has no version, so its page shows no play button…
    let page = json_of(&b.handle(&authed("GET", &format!("/library/metadata/{rk}"))));
    assert!(page["MediaContainer"]["Metadata"][0].get("Media").is_none());
    // …and the decision says why, in the field the player reads out.
    let decision = json_of(&b.handle(&authed("GET",
        &format!("/video/:/transcode/universal/decision?path=/library/metadata/{rk}"))));
    let mc = &decision["MediaContainer"];
    assert_eq!(mc["generalDecisionCode"], 2000);
    assert!(mc["transcodeDecisionText"].as_str().unwrap().contains("cached"));
}

#[test]
fn an_episode_queue_holds_the_whole_show_with_the_playing_episode_selected() {
    let b = content_bridge();
    let ep = Rk::Episode { stype: "series".into(), id: "tt0944947".into(), video: "tt0944947:1:2".into() }.encode();
    let uri = format!("server%3A%2F%2Fstremio-bridge%2Fcom.plexapp.plugins.library%2Flibrary%2Fmetadata%2F{ep}");
    let q = json_of(&b.handle(&authed("POST", &format!("/playQueues?type=video&uri={uri}&continuous=1"))));
    let mc = &q["MediaContainer"];
    let rows = mc["Metadata"].as_array().unwrap();
    let titles: Vec<&str> = rows.iter().map(|r| r["title"].as_str().unwrap()).collect();
    // Every episode, in order, so the player's Episodes strip can switch to any of them…
    assert_eq!(titles, vec!["Winter Is Coming", "The Kingsroad", "The North Remembers"]);
    assert_eq!(rows[2]["playQueueItemID"], 3);
    assert_eq!(rows[2]["parentIndex"], 2);
    // …with the one playing selected, which is what Up Next counts on to find the next one.
    assert_eq!(mc["playQueueSelectedItemID"], 2);
    assert_eq!(mc["playQueueSelectedItemOffset"], 1);
    // Resume where the account stopped.
    assert_eq!(rows[1]["viewOffset"], 600000);
}

#[test]
fn progress_is_written_to_the_stremio_library_and_finishing_marks_it_watched() {
    let s = content_scripted();
    let written = std::sync::Arc::new(Mutex::new(Vec::<Value>::new()));
    let b = Bridge::new(Box::new(Recording { inner: s, written: written.clone() }), 32498);
    let ep = Rk::Episode { stype: "series".into(), id: "tt0944947".into(), video: "tt0944947:2:1".into() }.encode();
    b.handle(&authed("GET", &format!("/:/timeline?ratingKey={ep}&state=paused&time=120000&duration=3000000")));
    // A second playing report inside the minute is not written.
    b.handle(&authed("GET", &format!("/:/timeline?ratingKey={ep}&state=playing&time=125000&duration=3000000")));
    b.handle(&authed("GET", &format!("/:/timeline?ratingKey={ep}&state=stopped&time=2900000&duration=3000000")));
    let w = written.lock().unwrap();
    assert_eq!(w.len(), 2);
    assert_eq!(w[0]["_id"], "tt0944947");
    assert_eq!(w[0]["state"]["timeOffset"], 120000);
    assert_eq!(w[0]["state"]["video_id"], "tt0944947:2:1");
    assert_eq!(w[1]["state"]["flaggedWatched"], 1);
    assert_eq!(w[1]["state"]["timeOffset"], 0);
}

/// A scripted account whose library writes the test can read after the bridge owns it.
struct Recording {
    inner: Scripted,
    written: std::sync::Arc<Mutex<Vec<Value>>>,
}

impl Stremio for Recording {
    fn create_link(&self) -> Result<LinkCode, ApiError> { self.inner.create_link() }
    fn read_link(&self, code: &str) -> Result<Option<String>, ApiError> { self.inner.read_link(code) }
    fn user(&self, k: &str) -> Result<User, ApiError> { self.inner.user(k) }
    fn addons(&self, k: &str) -> Result<Vec<Addon>, ApiError> { self.inner.addons(k) }
    fn library(&self, k: &str) -> Result<Vec<Value>, ApiError> { self.inner.library(k) }
    fn library_put(&self, _k: &str, items: &[Value]) -> Result<(), ApiError> {
        self.written.lock().unwrap().extend(items.iter().cloned());
        Ok(())
    }
    fn fetch(&self, url: &str) -> Result<Vec<u8>, ApiError> { self.inner.fetch(url) }
    fn fetch_range(&self, url: &str, from: u64, len: u64) -> Result<Vec<u8>, ApiError> {
        self.inner.fetch_range(url, from, len)
    }
}

// ---------------------------------------------------------------- conveniences

/// `days` from today as `YYYY-MM-DD` (negative = ago).
fn day_from_today(days: i64) -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64 + days * 86_400;
    let (y, m, d) = plx_net::net::civil_date(secs);
    format!("{y:04}-{m:02}-{d:02}")
}

#[test]
fn a_film_page_says_what_will_play_and_the_track_panel_names_the_release() {
    let b = playback_bridge();
    let rk = Rk::item("movie", "tt0111161").unwrap().encode();
    let detail = json_of(&b.handle(&authed("GET", &format!("/library/metadata/{rk}"))));
    let m = &detail["MediaContainer"]["Metadata"][0];
    let summary = m["summary"].as_str().unwrap();
    assert!(summary.starts_with("Ready to play: 1080p"), "{summary}");
    assert!(summary.contains("Instant"), "{summary}");
    let file = m["Media"][0]["Part"][0]["file"].as_str().unwrap();
    assert!(file.starts_with("Torrentio · Shawshank.1080p"), "{file}");
    assert!(!file.contains("http"), "no stream link in the panel");
}

#[test]
fn a_library_has_genre_shelves_from_its_catalogs_genre_choice() {
    let mut s = content_scripted();
    let cat = &mut s.addons[0].manifest.catalogs[0];
    cat.extra.push(ExtraProp { name: "genre".into(), is_required: false,
                               options: vec!["Action".into(), "Drama".into()] });
    s.pages.insert("https://v3-cinemeta.example/catalog/movie/top/genre=Action.json".into(), metas(json!([
        {"id": "tt0172495", "type": "movie", "name": "Gladiator"}])));
    let b = bridge_with(s);
    let movies = json_of(&b.handle(&authed("GET", "/hubs/sections/1")));
    let hubs = movies["MediaContainer"]["Hub"].as_array().unwrap();
    let action = hubs.iter().find(|h| h["title"] == "Action").expect("an Action shelf");
    assert_eq!(action["Metadata"][0]["title"], "Gladiator");
    assert!(hubs.iter().all(|h| h["title"] != "Drama"), "a genre with nothing in it is left out");
    let page = json_of(&b.handle(&authed("GET", action["key"].as_str().unwrap())));
    assert_eq!(page["MediaContainer"]["Metadata"][0]["title"], "Gladiator");
}

#[test]
fn coming_soon_lists_the_next_unreleased_episode_of_a_saved_show() {
    let mut s = content_scripted();
    let (aired, soon, later) = (day_from_today(-3), day_from_today(5), day_from_today(12));
    s.pages.insert("https://v3-cinemeta.example/meta/series/tt0944947.json".into(), json!({"meta": {
        "id": "tt0944947", "type": "series", "name": "Game of Thrones",
        "videos": [
            {"id": "tt0944947:1:1", "season": 1, "episode": 1, "released": format!("{aired}T00:00:00.000Z")},
            {"id": "tt0944947:1:2", "season": 1, "episode": 2, "released": format!("{soon}T00:00:00.000Z")},
            {"id": "tt0944947:1:3", "season": 1, "episode": 3, "released": format!("{later}T00:00:00.000Z")},
        ]}}));
    let b = bridge_with(s);
    let home = json_of(&b.handle(&authed("GET", "/hubs")));
    let hubs = home["MediaContainer"]["Hub"].as_array().unwrap();
    let soon_hub = hubs.iter().find(|h| is_listing(h, "coming_soon")).expect("a Coming Soon row");
    let rows = soon_hub["Metadata"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "one card per show");
    let line = rows[0]["summary"].as_str().unwrap();
    assert!(line.starts_with("S1 · E2 · Airs "), "{line}");
    // the episode that already aired is New Episodes', not Coming Soon's
    assert!(hubs.iter().any(|h| is_listing(h, "new_episodes")));
}

#[test]
fn removing_from_continue_watching_rewinds_the_item_as_stremio_does() {
    let s = content_scripted();
    let written = std::sync::Arc::new(Mutex::new(Vec::<Value>::new()));
    let b = Bridge::new(Box::new(Recording { inner: s, written: written.clone() }), 32498);
    let show = Rk::item("series", "tt0944947").unwrap().encode();
    let r = b.handle(&authed("PUT", &format!("/actions/removeFromContinueWatching?ratingKey={show}")));
    assert_eq!(r.status, 200);
    let w = written.lock().unwrap();
    assert_eq!(w.len(), 1);
    assert_eq!(w[0]["_id"], "tt0944947");
    assert_eq!(w[0]["state"]["timeOffset"], 0);
    assert_eq!(w[0]["state"]["video_id"], "tt0944947:1:2", "the show still knows where it was");
    assert_eq!(w[0]["removed"], false, "a saved show stays saved");
}

#[test]
fn an_aniskip_recap_is_skipped_like_an_opening_only_when_there_is_none() {
    let recap_only = json!({"found": true, "results": [
        {"interval": {"startTime": 0.0, "endTime": 60.0}, "skipType": "recap"}]});
    let m = super::bridge::aniskip_markers_for_test(&recap_only, 1_420_000);
    assert_eq!(m.len(), 1);
    assert_eq!(m[0]["type"], "intro");
    let both = json!({"found": true, "results": [
        {"interval": {"startTime": 0.0, "endTime": 60.0}, "skipType": "recap"},
        {"interval": {"startTime": 90.0, "endTime": 180.0}, "skipType": "op"}]});
    let m = super::bridge::aniskip_markers_for_test(&both, 1_420_000);
    assert_eq!(m.len(), 1);
    assert_eq!(m[0]["startTimeOffset"], 90_000);
}

/// Game of Thrones with its first episode ticked in Stremio's watched field.
fn watched_scripted() -> Scripted {
    let mut s = content_scripted();
    s.library[0]["state"]["watched"] = json!("tt0944947:1:1:1:eJxjBAAAAgAC");
    s
}

#[test]
fn episodes_and_seasons_carry_the_ticks_stremio_keeps() {
    let b = bridge_with(watched_scripted());
    let show = Rk::item("series", "tt0944947").unwrap().encode();
    let seasons = json_of(&b.handle(&authed("GET", &format!("/library/metadata/{show}/children"))));
    let seasons = seasons["MediaContainer"]["Metadata"].as_array().unwrap();
    assert_eq!(seasons[0]["viewedLeafCount"], 1);
    assert_eq!(seasons[1]["viewedLeafCount"], 0);
    let s1 = seasons[0]["ratingKey"].as_str().unwrap();
    let eps = json_of(&b.handle(&authed("GET", &format!("/library/metadata/{s1}/children"))));
    let eps = eps["MediaContainer"]["Metadata"].as_array().unwrap();
    assert_eq!(eps[0]["viewCount"], 1);
    assert!(eps[1].get("viewCount").is_none());
    let page = json_of(&b.handle(&authed("GET", &format!("/library/metadata/{show}"))));
    assert_eq!(page["MediaContainer"]["Metadata"][0]["viewedLeafCount"], 1);
}

#[test]
fn marking_a_season_watched_ticks_its_episodes_and_keeps_the_resume_point() {
    let written = std::sync::Arc::new(Mutex::new(Vec::<Value>::new()));
    let b = Bridge::new(Box::new(Recording { inner: watched_scripted(), written: written.clone() }), 32498);
    let s1 = Rk::Season { stype: "series".into(), id: "tt0944947".into(), season: 1 }.encode();
    b.handle(&authed("GET", &format!("/:/scrobble?key={s1}&identifier=com.plexapp.plugins.library")));
    let w = written.lock().unwrap();
    assert_eq!(w.len(), 1);
    let field = w[0]["state"]["watched"].as_str().unwrap();
    assert!(field.starts_with("tt0944947:1:2:2:"), "the last watched episode anchors it: {field}");
    assert_eq!(w[0]["state"]["timeOffset"], 600000, "a season mark is not a viewing");
    assert_eq!(w[0]["state"]["video_id"], "tt0944947:1:2");
}

#[test]
fn a_watched_field_this_app_cannot_line_up_is_left_alone() {
    let mut s = content_scripted();
    s.library[0]["state"]["watched"] = json!("tt0944947:9:9:4:eJxjBAAAAgAC");
    let written = std::sync::Arc::new(Mutex::new(Vec::<Value>::new()));
    let b = Bridge::new(Box::new(Recording { inner: s, written: written.clone() }), 32498);
    let s1 = Rk::Season { stype: "series".into(), id: "tt0944947".into(), season: 1 }.encode();
    b.handle(&authed("GET", &format!("/:/scrobble?key={s1}")));
    assert!(written.lock().unwrap().is_empty(), "nothing is written over a field that did not read");
}

#[test]
fn search_suggests_the_top_of_the_movie_and_show_catalogs() {
    let b = content_bridge();
    let got = json_of(&b.handle(&authed("GET", "/stremio/suggest")));
    let terms: Vec<&str> = got["terms"].as_array().unwrap().iter().map(|t| t.as_str().unwrap()).collect();
    assert_eq!(terms, ["The Shawshank Redemption", "The Godfather", "Game of Thrones"]);
}

#[test]
fn picked_for_you_follows_the_genres_of_what_was_watched_and_skips_what_is_known() {
    let mut s = content_scripted();
    s.addons[0].manifest.catalogs[1].extra.push(ExtraProp { name: "genre".into(), is_required: false,
        options: vec!["Drama".into(), "Comedy".into()] });
    s.pages.insert("https://v3-cinemeta.example/meta/series/tt0944947.json".into(), json!({"meta": {
        "id": "tt0944947", "type": "series", "name": "Game of Thrones", "genres": ["Drama", "Fantasy"],
        "videos": [{"id": "tt0944947:1:1", "season": 1, "episode": 1}]}}));
    s.pages.insert("https://v3-cinemeta.example/catalog/series/top/genre=Drama.json".into(), metas(json!([
        {"id": "tt0944947", "type": "series", "name": "Game of Thrones"},
        {"id": "tt7660850", "type": "series", "name": "Succession"},
    ])));
    let b = bridge_with(s);
    let home = json_of(&b.handle(&authed("GET", "/hubs")));
    let hubs = home["MediaContainer"]["Hub"].as_array().unwrap();
    let mine = hubs.iter().find(|h| is_listing(h, "for_you")).expect("a Picked for You row");
    let titles: Vec<&str> = mine["Metadata"].as_array().unwrap().iter().map(|r| r["title"].as_str().unwrap()).collect();
    assert_eq!(titles, ["Succession"], "what was watched is not suggested back");
    assert!(is_listing(&hubs[1], "for_you"), "it follows Continue Watching");
}

#[test]
fn a_shelf_heading_opens_the_whole_catalog_as_one_scrolling_listing() {
    let b = content_bridge();
    let movies = json_of(&b.handle(&authed("GET", "/hubs/sections/1")));
    let hub = &movies["MediaContainer"]["Hub"][0];
    let id = hub["hubIdentifier"].as_str().unwrap();
    assert!(id.starts_with("custom.collection.1.stremio-"), "a linked shelf of the Movies library: {id}");
    assert!(hub.get("size").is_none(), "no count is claimed for a catalog");
    let key = hub["key"].as_str().unwrap();
    let rk = key.trim_start_matches("/library/collections/").trim_end_matches("/children");
    let head = json_of(&b.handle(&authed("GET", &format!("/library/metadata/{rk}"))));
    assert_eq!(head["MediaContainer"]["Metadata"][0]["type"], "collection");
    assert_eq!(head["MediaContainer"]["Metadata"][0]["title"], "Popular · Movies");
    let page = json_of(&b.handle(&authed("GET", &format!("{key}?X-Plex-Container-Start=0&X-Plex-Container-Size=60"))));
    assert_eq!(page["MediaContainer"]["Metadata"][0]["title"], "The Shawshank Redemption");
    assert_eq!(page["MediaContainer"]["Metadata"].as_array().unwrap().len(), 2);
}

#[test]
fn search_offers_every_result_on_its_own_scrolling_page() {
    let b = content_bridge();
    let found = json_of(&b.handle(&authed("GET", "/hubs/search?query=alien&limit=5")));
    let hubs = found["MediaContainer"]["Hub"].as_array().unwrap();
    let all = hubs.iter().find(|h| h["type"] == "collection").expect("an all-results card");
    let card = &all["Metadata"][0];
    assert_eq!(card["title"], "All movies · alien");
    let rk = card["ratingKey"].as_str().unwrap();
    let head = json_of(&b.handle(&authed("GET", &format!("/library/metadata/{rk}"))));
    assert_eq!(head["MediaContainer"]["Metadata"][0]["title"], "All movies · alien");
    let page = json_of(&b.handle(&authed("GET", &format!("/library/collections/{rk}/children?X-Plex-Container-Start=0&X-Plex-Container-Size=60"))));
    assert_eq!(page["MediaContainer"]["Metadata"][0]["title"], "Alien");
    assert_eq!(page["MediaContainer"]["totalSize"], 1);
}

#[test]
fn a_title_page_offers_more_like_it_leaving_out_what_was_watched() {
    let mut s = content_scripted();
    s.addons[0].manifest.catalogs[0].extra.push(ExtraProp { name: "genre".into(), is_required: false,
        options: vec!["Drama".into(), "Crime".into()] });
    s.pages.insert("https://v3-cinemeta.example/meta/movie/tt0111161.json".into(), json!({"meta": {
        "id": "tt0111161", "type": "movie", "name": "The Shawshank Redemption", "genres": ["Drama"]}}));
    s.pages.insert("https://v3-cinemeta.example/catalog/movie/top/genre=Drama.json".into(), metas(json!([
        {"id": "tt0111161", "type": "movie", "name": "The Shawshank Redemption"},
        {"id": "tt0068646", "type": "movie", "name": "The Godfather"},
        {"id": "tt0050083", "type": "movie", "name": "12 Angry Men"},
    ])));
    let b = bridge_with(s);
    let rk = Rk::item("movie", "tt0111161").unwrap().encode();
    let got = json_of(&b.handle(&authed("GET", &format!("/library/metadata/{rk}/related"))));
    let hub = &got["MediaContainer"]["Hub"][0];
    assert_eq!(hub["title"], "More Like This");
    let titles: Vec<&str> = hub["Metadata"].as_array().unwrap().iter().map(|r| r["title"].as_str().unwrap()).collect();
    assert_eq!(titles, ["The Godfather", "12 Angry Men"], "not the title itself");
}

#[test]
fn an_actor_page_lists_the_titles_whose_cast_names_them() {
    let mut s = content_scripted();
    s.pages.insert("https://v3-cinemeta.example/catalog/movie/top/search=Tim%20Robbins.json".into(), metas(json!([
        {"id": "tt0111161", "type": "movie", "name": "The Shawshank Redemption"},
        {"id": "tt0068646", "type": "movie", "name": "The Godfather"},
    ])));
    s.pages.insert("https://v3-cinemeta.example/meta/movie/tt0068646.json".into(), json!({"meta": {
        "id": "tt0068646", "type": "movie", "name": "The Godfather", "cast": ["Marlon Brando"]}}));
    let b = bridge_with(s);
    let rk = Rk::item("movie", "tt0111161").unwrap().encode();
    let page = json_of(&b.handle(&authed("GET", &format!("/library/metadata/{rk}"))));
    let role = &page["MediaContainer"]["Metadata"][0]["Role"][0];
    assert_eq!(role["tag"], "Tim Robbins");
    assert!(role.get("tagKey").is_none(), "no plex.tv person key: the account key would follow it there");
    let id = role["id"].as_i64().unwrap();
    assert_eq!(id, super::catalog::person_id("Tim Robbins"));
    let media = json_of(&b.handle(&authed("GET", &format!("/library/people/{id}/media"))));
    let titles: Vec<&str> = media["MediaContainer"]["Metadata"].as_array().unwrap().iter().map(|r| r["title"].as_str().unwrap()).collect();
    assert_eq!(titles, ["The Shawshank Redemption"], "a search hit that does not cast them is left out");
}

#[test]
fn the_stream_picker_can_list_only_instant_streams() {
    let b = content_bridge();
    let rk = Rk::item("movie", "tt0111161").unwrap().encode();
    let all = json_of(&b.handle(&authed("GET", &format!("/stremio/choices/{rk}"))));
    let instant = json_of(&b.handle(&authed("GET", &format!("/stremio/choices/{rk}?instant=1"))));
    assert_eq!(all["MediaContainer"]["Metadata"].as_array().unwrap().len(), 2);
    let rows = instant["MediaContainer"]["Metadata"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0]["title"].as_str().unwrap().contains("Instant"));
}

#[test]
fn turning_subtitles_off_on_one_episode_carries_to_the_next() {
    let mut s = content_scripted();
    for ep in ["1:1", "1:2"] {
        s.pages.insert(format!("https://torrentio.example/stream/series/tt0944947:{ep}.json"), json!({"streams": [
            {"name": "[RD+] Torrentio\n1080p", "title": format!("GoT.S01E0{}.1080p.WEB.x264-TOP", &ep[2..]), "url": format!("https://t/top/{ep}")},
        ]}));
        s.files.insert(format!("https://t/top/{ep}"), sample_mkv());
    }
    let b = bridge_with(s);
    let e1 = Rk::Episode { stype: "series".into(), id: "tt0944947".into(), video: "tt0944947:1:1".into() }.encode();
    let e2 = Rk::Episode { stype: "series".into(), id: "tt0944947".into(), video: "tt0944947:1:2".into() }.encode();
    let first = json_of(&b.handle(&authed("GET", &format!("/library/metadata/{e1}"))));
    let part = first["MediaContainer"]["Metadata"][0]["Media"][0]["Part"][0]["id"].as_i64().unwrap();
    assert_eq!(b.handle(&authed("PUT", &format!("/library/parts/{part}?subtitleStreamID=0"))).status, 200);
    let second = json_of(&b.handle(&authed("GET", &format!("/library/metadata/{e2}"))));
    let streams = second["MediaContainer"]["Metadata"][0]["Media"][0]["Part"][0]["Stream"].as_array().unwrap();
    assert!(streams.iter().filter(|s| s["streamType"] == 3).all(|s| s["selected"] != true),
            "the next episode starts with subtitles off too: {streams:?}");
}

#[test]
fn a_release_in_the_viewers_audio_language_ranks_first() {
    let mut s = content_scripted();
    s.pages.insert("https://torrentio.example/stream/movie/tt0111161.json".into(), json!({"streams": [
        {"name": "[RD+] Torrentio\n1080p", "title": "Shawshank.1080p.BluRay.x264-ENG\n🇬🇧", "url": "https://t/en"},
        {"name": "[RD+] Torrentio\n1080p", "title": "Shawshank.1080p.BluRay.Castellano.x264\n🇪🇸 / 🇬🇧", "url": "https://t/es"},
    ]}));
    let b = bridge_with(s);
    let rk = Rk::item("movie", "tt0111161").unwrap().encode();
    let english = json_of(&b.handle(&authed("GET", &format!("/stremio/streams/{rk}"))));
    assert!(english["streams"][0]["detail"].as_str().unwrap().contains("ENG"), "English first for an English viewer");
    assert_eq!(b.handle(&authed("PUT", "/api/v2/user/profile?defaultAudioLanguage=es")).status, 200);
    let spanish = json_of(&b.handle(&authed("GET", &format!("/stremio/streams/{rk}"))));
    assert!(spanish["streams"][0]["detail"].as_str().unwrap().contains("Castellano"), "{spanish}");
}

#[test]
fn a_stream_that_did_not_start_ranks_last_for_that_item() {
    let mut s = content_scripted();
    s.pages.insert("https://torrentio.example/stream/movie/tt0111161.json".into(), json!({"streams": [
        {"name": "[RD+] Torrentio\n1080p", "title": "Dead.1080p.WEB.x264-AAA", "url": "https://t/dead"},
        {"name": "[RD+] Torrentio\n1080p", "title": "Alive.1080p.WEB.x264-BBB", "url": "https://t/alive"},
    ]}));
    s.files.insert("https://t/alive".into(), sample_mkv());
    let b = bridge_with(s);
    let rk = Rk::item("movie", "tt0111161").unwrap().encode();
    let before = json_of(&b.handle(&authed("GET", &format!("/stremio/streams/{rk}"))));
    assert!(before["streams"][0]["detail"].as_str().unwrap().starts_with("Dead"));
    let _ = b.handle(&authed("GET", &format!("/library/metadata/{rk}")));
    let after = json_of(&b.handle(&authed("GET", &format!("/stremio/streams/{rk}"))));
    assert!(after["streams"][0]["detail"].as_str().unwrap().starts_with("Alive"), "{after}");
    assert!(after["streams"][1]["detail"].as_str().unwrap().starts_with("Dead"));
}

#[test]
fn release_text_names_its_languages_and_dubs() {
    use super::bridge::release_language_for_test as names;
    assert!(names("Show.S01.1080p.Castellano.WEB", "es"));
    assert!(names("Film 1080p\n🇫🇷 / 🇬🇧", "fr"));
    assert!(!names("Spaceballs.1987.1080p", "es"), "a word that merely contains a needle is not a language");
    assert!(names("[SubsPlease] Show - 02 (1080p) Dual Audio", "") == false);
}

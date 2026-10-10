//! The media server's content routes: libraries, shelves, Home, search, title pages, seasons,
//! episodes, Continue Watching and artwork — all filled from the account's addons.
//!
//! Every addon response is memoised for a few minutes by URL ([`Bridge::fetch_json`]), so the
//! bursts a screen makes (a library's shelves, then its grid, then the same title twice) cost one
//! addon call each. Independent addon calls for one answer (Home's shelves) run side by side
//! ([`parallel`]).

use super::super::api::{Addon, ApiError};
use super::super::catalog::{self, Rk, Shelf as RowShelf, Title};
use super::super::http::{Request, Response};
use super::super::library::{self, Lib, Shelf};
use super::{api_failure, container, Bridge};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long an addon's catalog page or meta is reused.
const JSON_TTL: Duration = Duration::from_secs(600);
/// How long the account's library (watch progress) is reused.
pub(super) const LIBRARY_TTL: Duration = Duration::from_secs(60);
/// Rows per shelf on Home and on a library's shelves.
const SHELF_ROWS: usize = 20;
/// Shelves per library on Home.
const HOME_SHELVES_PER_LIB: usize = 2;
/// Search catalogs asked per kind, and rows kept per kind.
const SEARCH_CATALOGS: usize = 2;
/// Pages of a catalog fetched at most to serve one request.
const MAX_PAGES: usize = 10;

/// Runs `jobs` side by side and returns their results in order. A job whose thread the OS refuses
/// runs inline instead, so a refusal costs time, never an answer.
pub(super) fn parallel<'a, T: Send + 'a>(jobs: Vec<Box<dyn FnOnce() -> T + Send + 'a>>) -> Vec<Option<T>> {
    std::thread::scope(|scope| {
        let pending: Vec<_> = jobs
            .into_iter()
            .map(|job| {
                let slot = Arc::new(Mutex::new(Some(job)));
                let theirs = slot.clone();
                let spawned = std::thread::Builder::new()
                    .name("stremio-fetch".into())
                    .stack_size(512 * 1024)
                    .spawn_scoped(scope, move || {
                        let job = theirs.lock().unwrap_or_else(|e| e.into_inner()).take();
                        job.map(|f| f())
                    });
                match spawned {
                    Ok(handle) => Err(handle),
                    Err(_) => {
                        let job = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
                        Ok(job.map(|f| f()))
                    }
                }
            })
            .collect();
        pending
            .into_iter()
            .map(|p| match p {
                Ok(done) => done,
                Err(handle) => handle.join().ok().flatten(),
            })
            .collect()
    })
}

/// The season and episode a video id names: `tt0944947:1:2` is season 1, episode 2; a Kitsu
/// `kitsu:1376:5` is episode 5 of its one season.
pub(super) fn season_episode(video: &str) -> Option<(i64, i64)> {
    let mut parts = video.rsplit(':');
    let episode = parts.next()?.parse().ok()?;
    let season = if video.starts_with("kitsu:") { 1 } else { parts.next()?.parse().ok()? };
    Some((season, episode))
}

/// Percent-encodes an addon path segment: an id or an `extra` value.
fn segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b':' | b'.' | b'_' | b'-') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The paging window a request asks for: `X-Plex-Container-Start`/`-Size`, as a query parameter
/// or a header.
fn window(req: &Request, default_size: usize) -> (usize, usize) {
    let read = |name: &str| {
        req.query(name).or_else(|| req.header(name)).and_then(|v| v.parse::<usize>().ok())
    };
    (read("X-Plex-Container-Start").unwrap_or(0),
     read("X-Plex-Container-Size").unwrap_or(default_size).min(500))
}

fn row_shelf(lib: Lib) -> RowShelf<'static> {
    RowShelf { section: lib.key(), section_title: lib.title() }
}

/// What a library's shelves look like: the catalogs placed in it, numbered by their place in the
/// account's whole shelf list (the number `/hubs/stremio/{n}` names).
fn shelves_of(all: &[Shelf], lib: Lib) -> Vec<(usize, Shelf)> {
    all.iter().copied().enumerate().filter(|(_, s)| s.lib == lib).collect()
}

/// A shelf's heading: the catalog's name, with its addon's when another shelf in the same library
/// has the same name ("Popular · Cinemeta" beside "Popular · Kitsu").
fn shelf_title(addons: &[Addon], all: &[Shelf], s: Shelf) -> String {
    let catalog = &addons[s.addon].manifest.catalogs[s.catalog];
    let name = catalog.title();
    let clash = all.iter().any(|o| {
        o.lib == s.lib && *o != s && addons[o.addon].manifest.catalogs[o.catalog].title() == name
    });
    let addon = addons[s.addon].manifest.name.trim();
    if clash && !addon.is_empty() { format!("{name} · {addon}") } else { name.to_string() }
}

impl Bridge {
    /// An addon's JSON at `url`, from the memo when fresh.
    pub(super) fn fetch_json(&self, url: &str) -> Result<Arc<Value>, ApiError> {
        {
            let memo = self.json.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((at, v)) = memo.get(url) {
                if at.elapsed() < JSON_TTL {
                    return Ok(v.clone());
                }
            }
        }
        let body = self.stremio.fetch(url)?;
        let value: Value = serde_json::from_slice(&body).map_err(|_| ApiError::Unavailable)?;
        let value = Arc::new(value);
        let mut memo = self.json.lock().unwrap_or_else(|e| e.into_inner());
        if memo.len() >= 512 {
            memo.retain(|_, (at, _)| at.elapsed() < JSON_TTL);
            if memo.len() >= 512 {
                memo.clear();
            }
        }
        memo.insert(url.to_string(), (std::time::Instant::now(), value.clone()));
        Ok(value)
    }

    /// One page of a shelf's catalog, starting `skip` titles in (or a search for `query`).
    fn catalog_page(&self, addon: &Addon, s: (usize, usize), skip: usize, query: Option<&str>) -> Vec<Title> {
        let catalog = &addon.manifest.catalogs[s.1];
        let extra = match (query, skip) {
            (Some(q), _) => format!("/search={}", segment(q)),
            (None, 0) => String::new(),
            (None, n) => format!("/skip={n}"),
        };
        let url = format!("{}/catalog/{}/{}{extra}.json", addon.base_url(),
            segment(&catalog.kind), segment(&catalog.id));
        let Ok(v) = self.fetch_json(&url) else { return Vec::new() };
        match v.get("metas") {
            Some(Value::Array(metas)) => metas.iter().filter_map(|m| Title::read(m, &catalog.kind)).collect(),
            _ => Vec::new(),
        }
    }

    /// The titles `start..start + size` of a shelf, fetching as many pages as that takes. The
    /// second value is whether the catalog ended within them.
    fn shelf_window(&self, addon: &Addon, s: Shelf, start: usize, size: usize) -> (Vec<Title>, bool) {
        let mut titles: Vec<Title> = Vec::new();
        let mut ended = false;
        for _ in 0..MAX_PAGES {
            if titles.len() >= start + size {
                break;
            }
            let page = self.catalog_page(addon, (s.addon, s.catalog), titles.len(), None);
            if page.is_empty() {
                ended = true;
                break;
            }
            // An addon that ignores `skip` answers the first page again: that is the end too.
            if titles.first().is_some_and(|t| page.first().is_some_and(|p| p.id == t.id)) {
                ended = true;
                break;
            }
            titles.extend(page);
        }
        let window: Vec<Title> = titles.into_iter().skip(start).take(size).collect();
        (window, ended)
    }

    fn rows(&self, titles: &[Title], lib: Lib) -> Vec<Value> {
        titles.iter().map(|t| catalog::title_row(t, row_shelf(lib), &self.art)).collect()
    }

    // ---------------------------------------------------------------- libraries

    pub(super) fn library_sections(&self, token: &str) -> Response {
        let addons = match self.addons(token) {
            Ok(addons) => addons,
            Err(e) => return api_failure(e),
        };
        let all = library::shelves(&addons);
        let rows: Vec<Value> = library::libraries(&all)
            .into_iter()
            .map(|lib| json!({"key": lib.key().to_string(), "type": lib.section_type(), "title": lib.title(),
                              "agent": "tv.plex.agents.movie", "scanner": "Plex Movie",
                              "language": "en-US", "refreshing": false, "allowSync": false}))
            .collect();
        Response::json(200, &container(json!({"size": rows.len(), "Directory": rows})))
    }

    /// A library's grid: its first shelf, paged.
    pub(super) fn section_all(&self, req: &Request, token: &str, key: &str) -> Response {
        let Some(lib) = Lib::from_key(key) else { return Response::empty(404) };
        let addons = match self.addons(token) {
            Ok(addons) => addons,
            Err(e) => return api_failure(e),
        };
        let all = library::shelves(&addons);
        let (start, size) = window(req, 60);
        let Some((_, first)) = shelves_of(&all, lib).into_iter().next() else {
            return Response::json(200, &container(json!({"Metadata": [], "totalSize": 0, "offset": start})));
        };
        let (titles, ended) = self.shelf_window(&addons[first.addon], first, start, size);
        let rows = self.rows(&titles, lib);
        // An unended catalog promises one more page, so the grid keeps asking.
        let total = if ended { start + rows.len() } else { start + rows.len() + size.max(1) };
        let mut fields = json!({"size": rows.len(), "totalSize": total, "offset": start, "Metadata": rows});
        if req.query("includeMeta") == Some("1") {
            if let Some(o) = fields.as_object_mut() {
                o.insert("Meta".into(), json!({"Type": [{
                    "key": format!("/library/sections/{key}/all?type={}", if lib == Lib::Movies { 1 } else { 2 }),
                    "type": lib.wire(), "title": lib.title(), "active": true,
                    "Sort": [{"key": "titleSort", "defaultDirection": "asc", "title": "Popular"}],
                }]}));
            }
        }
        Response::json(200, &container(fields))
    }

    /// A library's shelves: every catalog placed in it, the first rows of each.
    pub(super) fn section_hubs(&self, token: &str, key: &str) -> Response {
        let Some(lib) = Lib::from_key(key) else { return Response::empty(404) };
        let addons = match self.addons(token) {
            Ok(addons) => addons,
            Err(e) => return api_failure(e),
        };
        let all = library::shelves(&addons);
        let hubs = self.hubs_for(&addons, &all, &shelves_of(&all, lib));
        Response::json(200, &container(json!({"size": hubs.len(), "Hub": hubs})))
    }

    /// The hubs for `shelves`, fetched side by side; an empty shelf is left out.
    fn hubs_for(&self, addons: &[Addon], all: &[Shelf], shelves: &[(usize, Shelf)]) -> Vec<Value> {
        let jobs: Vec<Box<dyn FnOnce() -> Vec<Title> + Send + '_>> = shelves
            .iter()
            .map(|(_, s)| {
                let s = *s;
                Box::new(move || self.shelf_window(&addons[s.addon], s, 0, SHELF_ROWS).0)
                    as Box<dyn FnOnce() -> Vec<Title> + Send + '_>
            })
            .collect();
        parallel(jobs)
            .into_iter()
            .zip(shelves)
            .filter_map(|(titles, (n, s))| {
                let titles = titles.filter(|t| !t.is_empty())?;
                let rows = self.rows(&titles, s.lib);
                Some(json!({
                    "title": shelf_title(addons, all, *s), "type": s.lib.wire(),
                    "hubIdentifier": format!("stremio.shelf.{n}"), "key": format!("/hubs/stremio/{n}"),
                    "more": true, "size": rows.len(), "Metadata": rows,
                }))
            })
            .collect()
    }

    /// One shelf, paged: what a shelf's "see all" opens.
    pub(super) fn shelf_page(&self, req: &Request, token: &str, n: &str) -> Response {
        let addons = match self.addons(token) {
            Ok(addons) => addons,
            Err(e) => return api_failure(e),
        };
        let all = library::shelves(&addons);
        let Some(s) = n.parse::<usize>().ok().and_then(|n| all.get(n).copied()) else {
            return Response::empty(404);
        };
        let (start, size) = window(req, 60);
        let (titles, ended) = self.shelf_window(&addons[s.addon], s, start, size);
        let rows = self.rows(&titles, s.lib);
        let total = if ended { start + rows.len() } else { start + rows.len() + size.max(1) };
        Response::json(200, &container(json!({"size": rows.len(), "totalSize": total, "offset": start,
                                              "Metadata": rows})))
    }

    // ---------------------------------------------------------------- Home

    /// Home: Continue Watching, then the first shelves of each library.
    pub(super) fn home_hubs(&self, req: &Request, token: &str) -> Response {
        let addons = match self.addons(token) {
            Ok(addons) => addons,
            Err(e) => return api_failure(e),
        };
        let all = library::shelves(&addons);
        let picked: Vec<(usize, Shelf)> = Lib::ALL
            .into_iter()
            .flat_map(|lib| shelves_of(&all, lib).into_iter().take(HOME_SHELVES_PER_LIB))
            .collect();
        let mut hubs = Vec::new();
        if req.query("excludeContinueWatching") != Some("1") {
            let rows = self.continue_rows(token, SHELF_ROWS);
            hubs.push(json!({"title": "Continue Watching", "type": "mixed",
                             "hubIdentifier": "home.continue", "key": "/hubs/continueWatching",
                             "size": rows.len(), "Metadata": rows}));
        }
        hubs.extend(self.hubs_for(&addons, &all, &picked));
        Response::json(200, &container(json!({"size": hubs.len(), "Hub": hubs})))
    }

    pub(super) fn continue_watching(&self, req: &Request, token: &str) -> Response {
        let count = req.query("count").and_then(|c| c.parse().ok()).unwrap_or(SHELF_ROWS);
        let rows = self.continue_rows(token, count);
        Response::json(200, &container(json!({"size": 1, "Hub": [{
            "title": "Continue Watching", "type": "mixed", "hubIdentifier": "home.continue",
            "key": "/hubs/continueWatching", "size": rows.len(), "Metadata": rows}]})))
    }

    /// The account's library items, from the memo when fresh.
    pub(super) fn library_items(&self, token: &str) -> Arc<Vec<Value>> {
        {
            let memo = self.library.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((t, at, items)) = memo.as_ref() {
                if t == token && at.elapsed() < LIBRARY_TTL {
                    return items.clone();
                }
            }
        }
        let items = Arc::new(self.stremio.library(token).unwrap_or_default());
        *self.library.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((token.to_string(), std::time::Instant::now(), items.clone()));
        items
    }

    /// Continue Watching, as Stremio decides it: saved or temporary items, not removed, with a
    /// position into them, most recently watched first.
    fn continue_rows(&self, token: &str, count: usize) -> Vec<Value> {
        let items = self.library_items(token);
        let mut watching: Vec<&Value> = items
            .iter()
            .filter(|it| {
                let removed = it.get("removed").and_then(Value::as_bool).unwrap_or(false);
                let temp = it.get("temp").and_then(Value::as_bool).unwrap_or(false);
                let offset = it.pointer("/state/timeOffset").and_then(Value::as_i64).unwrap_or(0);
                (!removed || temp) && offset > 0
                    && it.get("type").and_then(Value::as_str).is_some_and(|t| t != "other")
            })
            .collect();
        // ISO timestamps sort as text.
        watching.sort_by(|a, b| {
            let at = |v: &Value| v.pointer("/state/lastWatched").and_then(Value::as_str).unwrap_or("").to_string();
            at(b).cmp(&at(a))
        });
        let n = watching.len().min(count);
        watching
            .into_iter()
            .take(n)
            .enumerate()
            .filter_map(|(i, it)| self.continue_row(it, (n - i) as i64))
            .collect()
    }

    /// One Continue Watching row: the film, or — for a series — the SHOW, so opening it lands on
    /// the show's page with every season and episode, its Resume button already on the episode
    /// the account stopped in (the show page's `OnDeck`, [`Bridge::on_deck`]). That is how
    /// Stremio's own apps treat a series in Continue Watching.
    fn continue_row(&self, it: &Value, order: i64) -> Option<Value> {
        let id = it.get("_id")?.as_str()?;
        let stype = it.get("type")?.as_str()?;
        let name = it.get("name").and_then(Value::as_str).unwrap_or(id);
        let offset = it.pointer("/state/timeOffset").and_then(Value::as_i64).unwrap_or(0);
        let duration = it.pointer("/state/duration").and_then(Value::as_i64).unwrap_or(0);
        let poster = it.get("poster").and_then(Value::as_str).filter(|p| !p.is_empty())
            .map(|p| self.art.path(p));
        let lib = Lib::of_title(stype, id);
        let video = it.pointer("/state/video_id").and_then(Value::as_str).filter(|v| !v.is_empty());
        let rk = Rk::item(stype, id)?;
        let rk_s = rk.encode();
        let mut row = json!({
            "ratingKey": rk_s, "key": format!("/library/metadata/{rk_s}"), "title": name,
            // A stand-in clock: newest first is the order the account gave.
            "lastViewedAt": 1_700_000_000 + order,
            "librarySectionID": lib.key(), "librarySectionTitle": lib.title(),
        });
        let obj = row.as_object_mut()?;
        if let Some(p) = &poster {
            obj.insert("thumb".into(), json!(p));
        }
        if matches!(rk, Rk::Item { kind: catalog::Kind::Movie, .. }) {
            obj.insert("type".into(), json!("movie"));
            obj.insert("viewOffset".into(), json!(offset));
            obj.insert("duration".into(), json!(duration));
            obj.insert("Media".into(),
                catalog::placeholder_media(&rk_s, (duration > 0).then_some(duration / 60_000)));
        } else {
            obj.insert("type".into(), json!("show"));
            // Where it stopped, as a line under the title: "S1 · E2".
            if let Some((s, e)) = video.and_then(season_episode) {
                obj.insert("summary".into(), json!(format!("Continue S{s} · E{e}")));
            }
        }
        Some(row)
    }

    /// A show page's `OnDeck`: the episode the account stopped in, with where. The page's Play
    /// becomes Resume on it.
    fn on_deck(&self, token: &str, t: &Title, shelf: RowShelf<'_>) -> Option<Value> {
        let items = self.library_items(token);
        let it = items.iter().find(|it| it.get("_id").and_then(Value::as_str) == Some(t.id.as_str()))?;
        let video = it.pointer("/state/video_id").and_then(Value::as_str)?;
        let offset = it.pointer("/state/timeOffset").and_then(Value::as_i64).unwrap_or(0);
        let e = t.videos.iter().find(|e| e.video == video)?;
        let mut row = catalog::episode_row(t, e, shelf, &self.art, true);
        if offset > 0 {
            row["viewOffset"] = json!(offset);
        }
        Some(json!({"Metadata": row}))
    }

    // ---------------------------------------------------------------- search

    pub(super) fn search(&self, req: &Request, token: &str) -> Response {
        let query = req.query("query").unwrap_or("").trim().to_string();
        let limit = req.query("limit").and_then(|l| l.parse::<usize>().ok()).unwrap_or(10).clamp(1, 50);
        let addons = match self.addons(token) {
            Ok(addons) => addons,
            Err(e) => return api_failure(e),
        };
        if query.is_empty() {
            return Response::json(200, &container(json!({"Hub": []})));
        }
        let mut hubs = Vec::new();
        for (stype, wire) in [("movie", "movie"), ("series", "show")] {
            let catalogs: Vec<(usize, usize)> =
                library::search_catalogs(&addons, stype).into_iter().take(SEARCH_CATALOGS).collect();
            let q = query.as_str();
            let addons_ref = &addons;
            let jobs: Vec<Box<dyn FnOnce() -> Vec<Title> + Send + '_>> = catalogs
                .iter()
                .map(|&(a, c)| {
                    Box::new(move || self.catalog_page(&addons_ref[a], (a, c), 0, Some(q)))
                        as Box<dyn FnOnce() -> Vec<Title> + Send + '_>
                })
                .collect();
            let mut seen = std::collections::HashSet::new();
            let titles: Vec<Title> = parallel(jobs)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|t| seen.insert(t.id.clone()))
                .take(limit)
                .collect();
            let rows: Vec<Value> = titles
                .iter()
                .map(|t| catalog::title_row(t, row_shelf(Lib::of_title(&t.stype, &t.id)), &self.art))
                .collect();
            hubs.push(json!({"title": wire, "type": wire, "hubIdentifier": wire,
                             "size": rows.len(), "Metadata": rows}));
        }
        Response::json(200, &container(json!({"size": hubs.len(), "Hub": hubs})))
    }

    // ---------------------------------------------------------------- titles

    /// A title's full meta from the first metadata provider that has it.
    pub(super) fn title(&self, addons: &[Addon], stype: &str, id: &str) -> Option<Title> {
        library::meta_providers(addons, stype, id).find_map(|addon| {
            let url = format!("{}/meta/{}/{}.json", addon.base_url(), segment(stype), segment(id));
            let v = self.fetch_json(&url).ok()?;
            Title::read(v.get("meta")?, stype)
        })
    }

    fn rk_title(&self, token: &str, rk: &Rk) -> Result<Title, Response> {
        let addons = self.addons(token).map_err(api_failure)?;
        let (stype, id) = rk.show();
        self.title(&addons, stype, id).ok_or_else(|| Response::empty(404))
    }

    /// `/library/metadata/{rk}`: a title, season or episode. A film or episode carries its
    /// resolved, playable version and where the account stopped in it (`super::play`).
    pub(super) fn metadata(&self, token: &str, rk_s: &str) -> Response {
        let Some(rk) = Rk::decode(rk_s) else { return Response::empty(404) };
        if matches!(rk, Rk::Item { kind: catalog::Kind::Movie, .. } | Rk::Episode { .. }) {
            return match self.metadata_row(token, &rk, rk_s) {
                Ok(row) => Response::json(200, &container(json!({"size": 1, "Metadata": [row]}))),
                Err(r) => r,
            };
        }
        let t = match self.rk_title(token, &rk) {
            Ok(t) => t,
            Err(r) => return r,
        };
        let shelf = row_shelf(Lib::of_title(&t.stype, &t.id));
        let row = match &rk {
            Rk::Item { .. } => {
                let mut row = catalog::detail_row(&t, shelf, &self.art);
                if let Some(deck) = self.on_deck(token, &t, shelf) {
                    row["OnDeck"] = deck;
                }
                row
            }
            Rk::Season { season, .. } => {
                match catalog::season_rows(&t, shelf, &self.art).into_iter()
                    .find(|r| r.get("index").and_then(Value::as_i64) == Some(*season)) {
                    Some(r) => r,
                    None => return Response::empty(404),
                }
            }
            Rk::Episode { video, .. } => match t.videos.iter().find(|e| &e.video == video) {
                Some(e) => catalog::episode_row(&t, e, shelf, &self.art, true),
                None => return Response::empty(404),
            },
        };
        Response::json(200, &container(json!({"size": 1, "Metadata": [row]})))
    }

    /// `/library/metadata/{rk}/children`: a show's seasons, or a season's episodes.
    pub(super) fn children(&self, token: &str, rk_s: &str) -> Response {
        let Some(rk) = Rk::decode(rk_s) else { return Response::empty(404) };
        let t = match self.rk_title(token, &rk) {
            Ok(t) => t,
            Err(r) => return r,
        };
        let shelf = row_shelf(Lib::of_title(&t.stype, &t.id));
        let rows: Vec<Value> = match &rk {
            Rk::Item { .. } => catalog::season_rows(&t, shelf, &self.art),
            Rk::Season { season, .. } => t
                .episodes(*season)
                .into_iter()
                .map(|e| catalog::episode_row(&t, e, shelf, &self.art, true))
                .collect(),
            Rk::Episode { .. } => Vec::new(),
        };
        Response::json(200, &container(json!({"size": rows.len(), "Metadata": rows})))
    }

    /// `/library/metadata/{rk}/allLeaves`: every episode of a show, in order.
    pub(super) fn all_leaves(&self, token: &str, rk_s: &str) -> Response {
        let Some(rk) = Rk::decode(rk_s) else { return Response::empty(404) };
        let t = match self.rk_title(token, &rk) {
            Ok(t) => t,
            Err(r) => return r,
        };
        let shelf = row_shelf(Lib::of_title(&t.stype, &t.id));
        let rows: Vec<Value> = t
            .seasons()
            .into_iter()
            .flat_map(|s| t.episodes(s))
            .map(|e| catalog::episode_row(&t, e, shelf, &self.art, true))
            .collect();
        Response::json(200, &container(json!({"size": rows.len(), "Metadata": rows})))
    }

    // ---------------------------------------------------------------- artwork

    /// The image behind an art stand-in, as fetched. Only JPEG and PNG pass: those are the formats
    /// the app decodes, and anything else would be a broken tile rather than an empty one.
    pub(super) fn art(&self, key: &str) -> Response {
        let Some(url) = self.art.url(key) else { return Response::empty(404) };
        let Ok(bytes) = self.stremio.fetch(&url) else { return Response::empty(502) };
        let kind = if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            "image/jpeg"
        } else if bytes.starts_with(b"\x89PNG") {
            "image/png"
        } else {
            return Response::empty(404);
        };
        Response { status: 200, content_type: kind, body: bytes, location: None }
    }

    /// `/photo/:/transcode?url=/stremio/art/{key}&…`: the app's sized-artwork request.
    pub(super) fn photo(&self, req: &Request) -> Response {
        match req.query("url").and_then(|u| u.strip_prefix("/stremio/art/")) {
            Some(key) => self.art(key),
            None => Response::empty(404),
        }
    }
}

// ---------------------------------------------------------------- streams

impl Bridge {
    /// Every stream provider's answers for the film or episode `rk` names, ranked best first.
    pub(super) fn ranked_streams(&self, token: &str, rk: &Rk)
        -> Result<Vec<(super::super::streams::Stream, super::super::streams::Facts, i64)>, Response> {
        use super::super::streams::{rank, Stream};
        let (stype, id) = match rk {
            Rk::Item { stype, id, .. } => (stype.as_str(), id.as_str()),
            Rk::Episode { stype, video, .. } => (stype.as_str(), video.as_str()),
            Rk::Season { .. } => return Err(Response::empty(404)),
        };
        let addons = self.addons(token).map_err(api_failure)?;
        let providers: Vec<&Addon> = library::stream_providers(&addons, stype, id).collect();
        let jobs: Vec<Box<dyn FnOnce() -> Vec<Stream> + Send + '_>> = providers
            .iter()
            .map(|&addon| {
                Box::new(move || {
                    let url = format!("{}/stream/{}/{}.json", addon.base_url(), segment(stype), segment(id));
                    let name = addon.manifest.name.clone();
                    match self.fetch_json(&url) {
                        Ok(v) => match v.get("streams") {
                            Some(Value::Array(a)) => a.iter().filter_map(|s| Stream::read(&name, s)).collect(),
                            _ => Vec::new(),
                        },
                        Err(_) => Vec::new(),
                    }
                }) as Box<dyn FnOnce() -> Vec<Stream> + Send + '_>
            })
            .collect();
        let ranked = rank(parallel(jobs).into_iter().flatten().flatten().collect());
        Ok(super::super::streams::narrow(ranked, super::super::streams::prefs()))
    }

    /// `/stremio/streams/{rk}`: the ranked list, as a viewer would read it, and which one would
    /// start. The URLs are left out: a debrid link carries the service's key.
    pub(super) fn streams_route(&self, token: &str, rk_s: &str) -> Response {
        use super::super::streams::best;
        let Some(rk) = Rk::decode(rk_s) else { return Response::empty(404) };
        let ranked = match self.ranked_streams(token, &rk) {
            Ok(r) => r,
            Err(r) => return r,
        };
        let pick = best(&ranked);
        let rows: Vec<Value> = ranked
            .iter()
            .enumerate()
            .map(|(i, (s, f, score))| json!({
                "addon": s.addon, "name": s.name, "detail": s.detail, "filename": s.filename,
                "size": s.size, "playable": s.url.is_some(), "ready": format!("{:?}", f.ready),
                "english": format!("{:?}", f.english), "resolution": f.resolution,
                "score": score, "best": pick == Some(i),
            }))
            .collect();
        Response::json(200, &json!({"streams": rows}))
    }

    /// `/stremio/choices/{rk}`: the streams a viewer can pick from, in the media server's shape so
    /// the app reads it with its ordinary client: one `Metadata` row per playable stream, `index`
    /// its place in the ranked list (what `/stremio/choose` takes), `title` the line a picker
    /// shows ("2160p · HDR · Cached · 18.2 GB"), `summary` the addon and file name. The container's
    /// `offset` is the index that plays now (the viewer's choice, else the best), -1 for none.
    /// No URL leaves the bridge: a debrid link carries the service's key.
    pub(super) fn choices(&self, token: &str, rk_s: &str) -> Response {
        let Some(rk) = Rk::decode(rk_s) else { return Response::empty(404) };
        let ranked = match self.ranked_streams(token, &rk) {
            Ok(r) => r,
            Err(r) => return r,
        };
        let chosen = self.chosen_index(rk_s, &ranked);
        let rows: Vec<Value> = ranked
            .iter()
            .enumerate()
            .filter(|(_, (s, _, _))| s.url.is_some())
            .map(|(i, (s, f, _))| {
                let file = s.filename.as_deref().unwrap_or(s.name.as_str());
                json!({
                    "ratingKey": rk_s, "key": format!("/stremio/choose/{rk_s}?index={i}"),
                    "type": "clip", "index": i, "title": choice_label(f),
                    "summary": format!("{} — {}", s.addon, file.lines().next().unwrap_or(file)),
                })
            })
            .collect();
        Response::json(200, &container(json!({
            "size": rows.len(), "offset": chosen.map_or(-1, |i| i as i64), "Metadata": rows,
        })))
    }
}

/// One stream as a picker line: resolution, picture, readiness, size — the facts a viewer chooses
/// by, in that order, and only the ones the release says.
fn choice_label(f: &super::super::streams::Facts) -> String {
    use super::super::streams::Ready;
    let mut parts: Vec<String> = Vec::new();
    parts.push(match f.resolution {
        2160 => "4K".to_string(),
        0 => "SD".to_string(),
        r => format!("{r}p"),
    });
    if f.dv_only {
        parts.push("Dolby Vision".into());
    } else if f.hdr {
        parts.push("HDR".into());
    }
    if f.remux {
        parts.push("Remux".into());
    }
    if f.hevc {
        parts.push("HEVC".into());
    } else if f.av1 {
        parts.push("AV1".into());
    }
    parts.push(match f.ready {
        Ready::Cached => "Instant".into(),
        Ready::Uncached => "Not cached".into(),
        Ready::Direct => "Direct".into(),
    });
    if let Some(gb) = f.size_gb {
        parts.push(if gb >= 1.0 { format!("{gb:.1} GB") } else { format!("{:.0} MB", gb * 1024.0) });
    }
    parts.join(" · ")
}

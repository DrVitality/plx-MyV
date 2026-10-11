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
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long an addon's catalog page or meta is reused.
const JSON_TTL: Duration = Duration::from_secs(600);
/// How long the account's library (watch progress) is reused.
pub(super) const LIBRARY_TTL: Duration = Duration::from_secs(60);
/// Rows per shelf on Home and on a library's shelves.
const SHELF_ROWS: usize = 20;
/// Search catalogs asked per kind, and rows kept per kind.
const SEARCH_CATALOGS: usize = 2;
/// Pages of a catalog fetched at most to serve one request.
const MAX_PAGES: usize = 10;

/// The ratingKey prefix of a **listing** — a shelf (or a search's results) opened as its own page,
/// served as a media-server collection so the app's collection page scrolls through all of it.
pub(super) const LISTING: &str = "stremio-";

/// Search suggestions taken from each of the movie and show catalogs.
const SUGGESTIONS_PER_LIB: usize = 3;

/// Watched titles Picked for You learns from, most recent first.
const FOR_YOU_FROM: usize = 12;
/// Genres per library Picked for You asks its catalogs for.
const FOR_YOU_GENRES: usize = 2;
/// Cards on Picked for You.
const FOR_YOU_ROWS: usize = 30;

/// Continue Watching cards resolved ahead of a press.
const WARM_CONTINUE: usize = 3;

/// Where last run's Home is kept (`None` in tests).
fn home_cache_path() -> Option<std::path::PathBuf> {
    if cfg!(test) { None } else { Some(plx_base::paths::persistent_state_root().join("stremio-home.json")) }
}

/// Genre shelves per library.
const GENRE_SHELVES: usize = 8;
/// How far ahead Coming Soon looks, in days.
const COMING_SOON_DAYS: i64 = 60;
/// Cards on Home's My Library row.
const LIBRARY_ROWS: usize = 40;
/// How recent an episode New Episodes shows, in days.
const NEW_EPISODE_DAYS: i64 = 21;
/// Saved shows New Episodes looks at, most recently changed first.
const NEW_EPISODE_SHOWS: usize = 24;

/// Has the account watched this film? Stremio marks a finish with `flaggedWatched`, and counts
/// viewings in `timesWatched`.
pub(super) fn film_watched(it: &Value) -> bool {
    it.pointer("/state/flaggedWatched").and_then(Value::as_i64).unwrap_or(0) > 0
        || it.pointer("/state/timesWatched").and_then(Value::as_i64).unwrap_or(0) > 0
}

/// Is this library item one the account saved (not removed, not a temporary watch entry)?
pub(super) fn is_saved(it: &Value) -> bool {
    !it.get("removed").and_then(Value::as_bool).unwrap_or(false)
        && !it.get("temp").and_then(Value::as_bool).unwrap_or(false)
        && it.get("type").and_then(Value::as_str).is_some_and(|t| t != "other")
}

/// The UTC date `days_ago` days before today, as `YYYY-MM-DD` (how Stremio dates episodes).
fn iso_day(days_ago: i64) -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
        - days_ago * 86_400;
    let (y, m, d) = plx_net::net::civil_date(secs);
    format!("{y:04}-{m:02}-{d:02}")
}

/// [`iso_day`], for the bridge's other modules.
pub(super) fn iso_day_pub(days_ago: i64) -> String {
    iso_day(days_ago)
}

/// `2026-10-14` as `Oct 14`.
fn pretty_day(day: &str) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let mut parts = day.split('-').skip(1);
    match (parts.next().and_then(|m| m.parse::<usize>().ok()), parts.next().and_then(|d| d.parse::<u32>().ok())) {
        (Some(m @ 1..=12), Some(d)) => format!("{} {d}", MONTHS[m - 1]),
        _ => day.to_string(),
    }
}

/// Runs `jobs` side by side and returns their results in order. A job whose thread the OS refuses
/// runs inline instead, so a refusal costs time, never an answer.
/// The most the stream addons are waited for, all together.
const STREAMS_BUDGET: std::time::Duration = std::time::Duration::from_secs(8);
/// Once one addon has listed streams, the most the others are waited for: one dead or slow addon
/// among a dozen used to hold every play and every stream list for its whole request timeout.
const STREAMS_GRACE: std::time::Duration = std::time::Duration::from_millis(2500);

/// One addon's streams for an item (none when it failed).
fn streams_from(b: &Bridge, url: &str, name: &str) -> Vec<super::super::streams::Stream> {
    match b.fetch_json(url) {
        Ok(v) => match v.get("streams") {
            Some(Value::Array(a)) => a.iter().filter_map(|s| super::super::streams::Stream::read(name, s)).collect(),
            _ => Vec::new(),
        },
        Err(_) => Vec::new(),
    }
}

/// Ask every stream addon in `asks` (`(url, addon name)`) at once and gather what has answered
/// within [`STREAMS_BUDGET`] — or [`STREAMS_GRACE`] after the first that listed any. An addon that
/// answers later still lands in the JSON memo, so the next ask of the item has it.
fn gather_streams(me: &Arc<Bridge>, asks: Vec<(String, String)>) -> Vec<super::super::streams::Stream> {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut out = Vec::new();
    let mut waiting = 0usize;
    for (url, name) in asks {
        let (tx, me2) = (tx.clone(), me.clone());
        let (url2, name2) = (url.clone(), name.clone());
        let spawned = plx_base::task::spawn("stremio-streams", move || {
            let _ = tx.send(streams_from(&me2, &url2, &name2));
        });
        if spawned.is_some() {
            waiting += 1;
        } else {
            out.extend(streams_from(me, &url, &name));
        }
    }
    drop(tx);
    let start = std::time::Instant::now();
    let mut deadline = start + STREAMS_BUDGET;
    if !out.is_empty() {
        deadline = deadline.min(start + STREAMS_GRACE);
    }
    while waiting > 0 {
        match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
            Ok(streams) => {
                waiting -= 1;
                if !streams.is_empty() && out.is_empty() {
                    deadline = deadline.min(std::time::Instant::now() + STREAMS_GRACE);
                }
                out.extend(streams);
            }
            Err(_) => {
                plx_base::eventlog::log(&format!("stremio: {waiting} stream addon(s) still silent; going on without them"));
                break;
            }
        }
    }
    out
}

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
        let started = std::time::Instant::now();
        let fetched = self.stremio.fetch(url);
        self.note_health(url, fetched.is_ok(), started.elapsed());
        let body = fetched?;
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
        self.catalog_page_in(addon, s, skip, query, None)
    }

    /// [`catalog_page`](Self::catalog_page), narrowed to one of the catalog's genres.
    fn catalog_page_in(&self, addon: &Addon, s: (usize, usize), skip: usize, query: Option<&str>,
                       genre: Option<&str>) -> Vec<Title> {
        let catalog = &addon.manifest.catalogs[s.1];
        let mut parts: Vec<String> = Vec::new();
        if let Some(q) = query {
            parts.push(format!("search={}", segment(q)));
        }
        if let Some(g) = genre {
            parts.push(format!("genre={}", segment(g)));
        }
        if skip > 0 && query.is_none() {
            parts.push(format!("skip={skip}"));
        }
        let extra = if parts.is_empty() { String::new() } else { format!("/{}", parts.join("&")) };
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
        let saved = self.saved_ids();
        let seen = self.watched_film_ids();
        titles
            .iter()
            .map(|t| {
                let mut row = catalog::title_row(t, row_shelf(lib), &self.art);
                if saved.contains(&t.id) {
                    row["stremioInLibrary"] = json!(true);
                }
                if seen.contains(&t.id) && row.get("type").and_then(Value::as_str) == Some("movie") {
                    row["viewCount"] = json!(1);
                }
                row
            })
            .collect()
    }

    /// The ids the account has SAVED in its library (not removed, not a temporary watch entry),
    /// as last read — never a fetch of its own, so a row built before the library was read simply
    /// carries no mark.
    pub(super) fn saved_ids(&self) -> std::collections::HashSet<String> {
        let memo = self.library.lock().unwrap_or_else(|e| e.into_inner());
        memo.as_ref()
            .map(|(_, _, items)| {
                items.iter()
                    .filter(|it| is_saved(it))
                    .filter_map(|it| it.get("_id").and_then(Value::as_str).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The films the account has watched (Stremio's `flaggedWatched` or a finished viewing), as
    /// last read — like [`saved_ids`](Self::saved_ids), never a fetch of its own.
    pub(super) fn watched_film_ids(&self) -> std::collections::HashSet<String> {
        let memo = self.library.lock().unwrap_or_else(|e| e.into_inner());
        memo.as_ref()
            .map(|(_, _, items)| {
                items.iter()
                    .filter(|it| it.get("type").and_then(Value::as_str) == Some("movie") && film_watched(it))
                    .filter_map(|it| it.get("_id").and_then(Value::as_str).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    // ---------------------------------------------------------------- libraries

    pub(super) fn library_sections(&self, token: &str) -> Response {
        let addons = match self.addons(token) {
            Ok(addons) => addons,
            Err(e) => return api_failure(e),
        };
        let all = self.shelves(&addons);
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
        let all = self.shelves(&addons);
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
        let all = self.shelves(&addons);
        let mut hubs = self.hubs_for(&addons, &all, &shelves_of(&all, lib));
        hubs.extend(self.genre_hubs(&addons, &all, lib));
        link_listings(&mut hubs, lib.key());
        Response::json(200, &container(json!({"size": hubs.len(), "Hub": hubs})))
    }

    /// The genres a library is browsed by: the first of its catalogs that offers a `genre`
    /// choice, and its first [`GENRE_SHELVES`] genres. `(shelf number, genre index, genre)`.
    fn library_genres(&self, addons: &[Addon], all: &[Shelf], lib: Lib) -> Vec<(usize, usize, String)> {
        shelves_of(all, lib)
            .into_iter()
            .find_map(|(n, s)| {
                let c = &addons[s.addon].manifest.catalogs[s.catalog];
                let genres = &c.extra.iter().find(|e| e.name == "genre" && !e.options.is_empty())?.options;
                Some(genres.iter().take(GENRE_SHELVES).enumerate().map(|(g, name)| (n, g, name.clone())).collect())
            })
            .unwrap_or_default()
    }

    /// A shelf per genre, after a library's catalog shelves.
    fn genre_hubs(&self, addons: &[Addon], all: &[Shelf], lib: Lib) -> Vec<Value> {
        let genres = self.library_genres(addons, all, lib);
        let jobs: Vec<Box<dyn FnOnce() -> Vec<Title> + Send + '_>> = genres
            .iter()
            .map(|(n, _, g)| {
                let s = all[*n];
                Box::new(move || self.catalog_page_in(&addons[s.addon], (s.addon, s.catalog), 0, None, Some(g.as_str())))
                    as Box<dyn FnOnce() -> Vec<Title> + Send + '_>
            })
            .collect();
        parallel(jobs)
            .into_iter()
            .zip(&genres)
            .filter_map(|(titles, (n, g, name))| {
                let titles: Vec<Title> = titles.filter(|t| !t.is_empty())?.into_iter().take(SHELF_ROWS).collect();
                let rows = self.rows(&titles, lib);
                Some(json!({
                    "title": name, "type": lib.wire(),
                    "hubIdentifier": format!("stremio.genre.{n}.{g}"), "key": format!("/hubs/stremio/g{n}-{g}"),
                    "more": true, "size": rows.len(), "Metadata": rows,
                }))
            })
            .collect()
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
                let title = shelf_title(addons, all, *s);
                // a popular or trending catalog feeds the Home hero (`pms`'s `.popular` hubs)
                let words = format!("{} {}", addons[s.addon].manifest.catalogs[s.catalog].id, title).to_ascii_lowercase();
                let featured = words.contains("popular") || words.contains("trending") || words.contains(" top")
                    || addons[s.addon].manifest.catalogs[s.catalog].id == "top";
                let id = if featured { format!("stremio.shelf.{n}.popular") } else { format!("stremio.shelf.{n}") };
                Some(json!({
                    "title": title, "type": s.lib.wire(),
                    "hubIdentifier": id, "key": format!("/hubs/stremio/{n}"),
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
        // the bridge's own rows, whole
        let own = match n {
            "library" => Some(self.library_rows(token, usize::MAX)),
            "new_episodes" => Some(self.new_episode_rows(token, &addons)),
            "coming_soon" => Some(self.coming_soon_rows(token, &addons)),
            "for_you" => Some(self.for_you_rows(token, &addons)),
            _ => None,
        };
        // a genre shelf: `g{shelf}-{genre}`, paged through the catalog with its genre chosen
        if let Some((n, g)) = n.strip_prefix('g').and_then(|r| r.split_once('-')) {
            let all = self.shelves(&addons);
            let (Some(s), Some(g)) = (n.parse::<usize>().ok().and_then(|n| all.get(n).copied()), g.parse::<usize>().ok()) else {
                return Response::empty(404);
            };
            let Some((_, _, genre)) = self.library_genres(&addons, &all, s.lib).into_iter().find(|(gn, gi, _)| all.get(*gn) == Some(&s) && *gi == g) else {
                return Response::empty(404);
            };
            let (start, _) = window(req, 60);
            let titles = self.catalog_page_in(&addons[s.addon], (s.addon, s.catalog), start, None, Some(&genre));
            let rows = self.rows(&titles, s.lib);
            let total = if rows.is_empty() { start } else { start + rows.len() + 1 };
            return Response::json(200, &container(json!({"size": rows.len(), "totalSize": total, "offset": start,
                                                         "Metadata": rows})));
        }
        if let Some(rows) = own {
            let (start, size) = window(req, 60);
            let page: Vec<Value> = rows.iter().skip(start).take(size).cloned().collect();
            return Response::json(200, &container(json!({"size": page.len(), "totalSize": rows.len(),
                                                         "offset": start, "Metadata": page})));
        }
        let all = self.shelves(&addons);
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

    /// Home: the rows the viewer switched on, in their order (Settings > Addons & Home; by
    /// default Continue Watching, New Episodes, the first shelves of each library, My Library).
    pub(super) fn home_hubs(&self, req: &Request, token: &str) -> Response {
        let whole = req.query("excludeContinueWatching") != Some("1");
        // the first Home of a run: last run's, at once, while the app is told to fetch it fresh
        // the saved copy is the account's own: its first line says whose (a hash, never the key)
        let owner = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            token.hash(&mut h);
            format!("{:016x}\n", h.finish())
        };
        if whole && !self.home_cache_served.swap(true, std::sync::atomic::Ordering::Relaxed) {
            let saved = home_cache_path().and_then(|p| std::fs::read(p).ok());
            if let Some(body) = saved.as_deref().and_then(|b| b.strip_prefix(owner.as_bytes())).filter(|b| !b.is_empty()) {
                super::super::note_home_changed();
                return Response { status: 200, content_type: "application/json", body: body.to_vec(), location: None };
            }
        }
        let response = self.home_hubs_fresh(req, token);
        if whole && response.status == 200 {
            if let Some(p) = home_cache_path() {
                let tmp = p.with_extension("json.tmp");
                let mut saved = owner.into_bytes();
                saved.extend_from_slice(&response.body);
                if std::fs::write(&tmp, &saved).is_ok() {
                    let _ = std::fs::rename(&tmp, &p);
                }
            }
            // the first few things on Continue Watching, ready before they are pressed
            for rk in self.continue_targets(token, WARM_CONTINUE) {
                self.warm(token, rk);
            }
            self.start_reminders(token);
        }
        response
    }

    /// The films and episodes the first `n` Continue Watching cards would play.
    fn continue_targets(&self, token: &str, n: usize) -> Vec<String> {
        let mut rows = self.continue_rows(token, n);
        rows.truncate(n);
        let items = self.library_items(token);
        rows.iter()
            .filter_map(|r| r.get("ratingKey")?.as_str().map(str::to_string))
            .filter_map(|rk_s| match Rk::decode(&rk_s)? {
                Rk::Item { kind: catalog::Kind::Movie, .. } => Some(rk_s),
                Rk::Item { stype, id, .. } => {
                    let it = items.iter().find(|it| it.get("_id").and_then(Value::as_str) == Some(id.as_str()))?;
                    let video = it.pointer("/state/video_id").and_then(Value::as_str)?.to_string();
                    Some(Rk::Episode { stype, id, video }.encode())
                }
                _ => None,
            })
            .collect()
    }

    /// The episode a show's Play starts: where the account stopped, else the first episode.
    fn next_up(&self, token: &str, t: &Title) -> Option<String> {
        let items = self.library_items(token);
        let stopped = items.iter()
            .find(|it| it.get("_id").and_then(Value::as_str) == Some(t.id.as_str()))
            .and_then(|it| it.pointer("/state/video_id").and_then(Value::as_str))
            .filter(|v| t.videos.iter().any(|e| e.video == *v))
            .map(str::to_string);
        stopped.or_else(|| {
            t.seasons().into_iter().find(|s| *s > 0).and_then(|s| t.episodes(s).first().map(|e| e.video.clone()))
        })
    }

    fn home_hubs_fresh(&self, req: &Request, token: &str) -> Response {
        use super::config::HomeRow;
        let addons = match self.addons(token) {
            Ok(addons) => addons,
            Err(e) => return api_failure(e),
        };
        let all = self.shelves(&addons);
        // read first: the library marks every catalog row built below
        let _ = self.library_items(token);
        let rows: Vec<HomeRow> = self.with_config(|c| c.home_rows(&addons, &all))
            .into_iter()
            .filter(|(_, _, shown)| *shown)
            .map(|(_, row, _)| row)
            .collect();
        let picked: Vec<(usize, Shelf)> = rows
            .iter()
            .filter_map(|r| match r {
                HomeRow::Shelf(n) => all.get(*n).map(|s| (*n, *s)),
                _ => None,
            })
            .collect();
        let mut by_shelf: std::collections::HashMap<String, Value> = self
            .hubs_for(&addons, &all, &picked)
            .into_iter()
            .filter_map(|h| Some((h.get("key")?.as_str()?.to_string(), h)))
            .collect();
        let mut hubs = Vec::new();
        for row in rows {
            match row {
                HomeRow::Continue => {
                    if req.query("excludeContinueWatching") != Some("1") {
                        let rows = self.continue_rows(token, SHELF_ROWS);
                        hubs.push(json!({"title": "Continue Watching", "type": "mixed",
                                         "hubIdentifier": "home.continue", "key": "/hubs/continueWatching",
                                         "size": rows.len(), "Metadata": rows}));
                    }
                }
                HomeRow::ForYou => {
                    let rows = self.for_you_rows(token, &addons);
                    if !rows.is_empty() {
                        hubs.push(json!({"title": "Picked for You", "type": "mixed",
                                         "hubIdentifier": "stremio.for_you", "key": "/hubs/stremio/for_you",
                                         "size": rows.len(), "Metadata": rows}));
                    }
                }
                HomeRow::NewEpisodes => {
                    let rows = self.new_episode_rows(token, &addons);
                    if !rows.is_empty() {
                        hubs.push(json!({"title": "New Episodes", "type": "episode",
                                         "hubIdentifier": "stremio.new_episodes", "key": "/hubs/stremio/new_episodes",
                                         "size": rows.len(), "Metadata": rows}));
                    }
                }
                HomeRow::ComingSoon => {
                    let rows = self.coming_soon_rows(token, &addons);
                    if !rows.is_empty() {
                        hubs.push(json!({"title": "Coming Soon", "type": "show",
                                         "hubIdentifier": "stremio.coming_soon", "key": "/hubs/stremio/coming_soon",
                                         "size": rows.len(), "Metadata": rows}));
                    }
                }
                HomeRow::Library => {
                    let rows = self.library_rows(token, LIBRARY_ROWS);
                    if !rows.is_empty() {
                        hubs.push(json!({"title": "My Library", "type": "mixed",
                                         "hubIdentifier": "stremio.library", "key": "/hubs/stremio/library",
                                         "size": rows.len(), "Metadata": rows}));
                    }
                }
                HomeRow::Shelf(n) => {
                    if let Some(h) = by_shelf.remove(&format!("/hubs/stremio/{n}")) {
                        hubs.push(h);
                    }
                }
            }
        }
        link_listings(&mut hubs, 0);
        Response::json(200, &container(json!({"size": hubs.len(), "Hub": hubs})))
    }

    /// My Library: what the account saved, most recently changed first.
    pub(super) fn library_rows(&self, token: &str, count: usize) -> Vec<Value> {
        let items = self.library_items(token);
        let mut saved: Vec<&Value> = items.iter().filter(|it| is_saved(it)).collect();
        saved.sort_by(|a, b| {
            let at = |v: &Value| v.get("_mtime").and_then(Value::as_str).unwrap_or("").to_string();
            at(b).cmp(&at(a))
        });
        saved.into_iter().take(count).filter_map(|it| self.library_row(it)).collect()
    }

    /// One saved item as a card: the film (playable from the card), or the show.
    fn library_row(&self, it: &Value) -> Option<Value> {
        let id = it.get("_id")?.as_str()?;
        let stype = it.get("type")?.as_str()?;
        let rk = Rk::item(stype, id)?;
        let rk_s = rk.encode();
        let lib = Lib::of_title(stype, id);
        let mut row = json!({
            "ratingKey": rk_s, "key": format!("/library/metadata/{rk_s}"),
            "title": it.get("name").and_then(Value::as_str).unwrap_or(id),
            "librarySectionID": lib.key(), "librarySectionTitle": lib.title(),
            "stremioInLibrary": true,
        });
        let obj = row.as_object_mut()?;
        if let Some(p) = it.get("poster").and_then(Value::as_str).filter(|p| !p.is_empty()) {
            obj.insert("thumb".into(), json!(self.art.path(p)));
        }
        if matches!(rk, Rk::Item { kind: catalog::Kind::Movie, .. }) {
            obj.insert("type".into(), json!("movie"));
            let duration = it.pointer("/state/duration").and_then(Value::as_i64).unwrap_or(0);
            obj.insert("Media".into(), catalog::placeholder_media(&rk_s, (duration > 0).then_some(duration / 60_000)));
        } else {
            obj.insert("type".into(), json!("show"));
        }
        Some(row)
    }

    /// The full metas of the shows the account saved (most recently changed first, at most
    /// [`NEW_EPISODE_SHOWS`]), fetched side by side.
    fn saved_show_titles(&self, token: &str, addons: &[Addon]) -> Vec<Title> {
        let items = self.library_items(token);
        let mut saved: Vec<&Value> = items.iter().filter(|it| is_saved(it)).collect();
        saved.sort_by(|a, b| {
            let at = |v: &Value| v.get("_mtime").and_then(Value::as_str).unwrap_or("").to_string();
            at(b).cmp(&at(a))
        });
        let shows: Vec<(String, String)> = saved
            .into_iter()
            .filter_map(|it| {
                let stype = it.get("type")?.as_str()?;
                let id = it.get("_id")?.as_str()?;
                if stype == "movie" || stype == "other" {
                    return None;
                }
                Some((stype.to_string(), id.to_string()))
            })
            .take(NEW_EPISODE_SHOWS)
            .collect();
        let jobs: Vec<Box<dyn FnOnce() -> Option<Title> + Send + '_>> = shows
            .iter()
            .map(|(stype, id)| {
                Box::new(move || self.title(addons, stype, id)) as Box<dyn FnOnce() -> Option<Title> + Send + '_>
            })
            .collect();
        parallel(jobs).into_iter().flatten().flatten().collect()
    }

    /// New Episodes: for each show the account saved, its newest episode released within the
    /// last [`NEW_EPISODE_DAYS`] days, newest first.
    pub(super) fn new_episode_rows(&self, token: &str, addons: &[Addon]) -> Vec<Value> {
        let (today, since) = (iso_day(0), iso_day(NEW_EPISODE_DAYS));
        let mut found: Vec<(String, Value)> = self
            .saved_show_titles(token, addons)
            .into_iter()
            .filter_map(|t| {
                let e = t.videos.iter()
                    .filter(|e| e.season > 0 && e.released.as_deref().is_some_and(|d| d >= since.as_str() && d <= today.as_str()))
                    .max_by(|a, b| a.released.cmp(&b.released).then((a.season, a.episode).cmp(&(b.season, b.episode))))?;
                let lib = Lib::of_title(&t.stype, &t.id);
                let mut row = catalog::episode_row(&t, e, row_shelf(lib), &self.art, true);
                row["stremioInLibrary"] = json!(true);
                Some((e.released.clone().unwrap_or_default(), row))
            })
            .collect();
        found.sort_by(|a, b| b.0.cmp(&a.0));
        found.into_iter().map(|(_, row)| row).collect()
    }

    /// Coming Soon: for each saved show, its next episode not yet out (within
    /// [`COMING_SOON_DAYS`]), soonest first, its air date as the line under it.
    pub(super) fn coming_soon_rows(&self, token: &str, addons: &[Addon]) -> Vec<Value> {
        let (today, until) = (iso_day(0), iso_day(-COMING_SOON_DAYS));
        let mut found: Vec<(String, Value)> = self
            .saved_show_titles(token, addons)
            .into_iter()
            .filter_map(|t| {
                let e = t.videos.iter()
                    .filter(|e| e.season > 0 && e.released.as_deref().is_some_and(|d| d > today.as_str() && d <= until.as_str()))
                    .min_by(|a, b| a.released.cmp(&b.released).then((a.season, a.episode).cmp(&(b.season, b.episode))))?;
                let lib = Lib::of_title(&t.stype, &t.id);
                // the SHOW's card (the episode is not out, so there is nothing to play): its line
                // says which episode, and when
                let mut row = catalog::title_row(&t, row_shelf(lib), &self.art);
                let day = e.released.clone().unwrap_or_default();
                row["summary"] = json!(format!("S{} · E{} · Airs {}", e.season, e.episode, pretty_day(&day)));
                row["stremioInLibrary"] = json!(true);
                Some((day, row))
            })
            .collect();
        found.sort_by(|a, b| a.0.cmp(&b.0));
        found.into_iter().map(|(_, row)| row).collect()
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
        let raw = req.query("query").unwrap_or("");
        let (text, filters) = Browse::split(raw);
        let query = text.trim().to_string();
        let limit = req.query("limit").and_then(|l| l.parse::<usize>().ok()).unwrap_or(10).clamp(1, 50);
        let addons = match self.addons(token) {
            Ok(addons) => addons,
            Err(e) => return api_failure(e),
        };
        if query.is_empty() {
            return match filters {
                Some(f) => Response::json(200, &container(json!({"Hub": self.browse(token, &addons, &f)}))),
                None => Response::json(200, &container(json!({"Hub": []}))),
            };
        }
        let mut hubs = Vec::new();
        let mut listings = Vec::new();
        let wanted = filters.map_or(Kind2::All, |f| f.kind);
        for (stype, wire) in [("movie", "movie"), ("series", "show")] {
            if !wanted.has_stype(stype) {
                continue;
            }
            let catalogs: Vec<(usize, usize)> =
                library::search_catalogs(&addons, stype).into_iter()
                    .filter(|(a, _)| self.with_config(|c| c.addon_on(&addons[*a], super::super::api::Resource::Catalog)))
                    .take(SEARCH_CATALOGS).collect();
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
            // every result, scrolled through on its own page: a collection card at the end
            if !titles.is_empty() {
                let rk = format!("{LISTING}q-{}-{}", &stype[..1], hex(&query));
                let thumb = titles.iter().find_map(|t| t.poster.as_deref()).map(|u| self.art.path(u));
                let label = if wire == "movie" { "All movies" } else { "All shows" };
                listings.push(json!({"ratingKey": rk, "key": format!("/library/collections/{rk}/children"),
                    "type": "collection", "subtype": wire, "title": format!("{label} · {query}"), "thumb": thumb}));
            }
            hubs.push(json!({"title": wire, "type": wire, "hubIdentifier": wire,
                             "size": rows.len(), "Metadata": rows}));
        }
        if !listings.is_empty() {
            hubs.push(json!({"title": "collection", "type": "collection", "hubIdentifier": "collection",
                             "size": listings.len(), "Metadata": listings}));
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
            return match self.metadata_row(token, &rk, rk_s, Some(super::play::PAGE_WAIT)) {
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
                self.note_people(&t);
                if let Some(deck) = self.on_deck(token, &t, shelf) {
                    row["OnDeck"] = deck;
                }
                // the episode Play would start, resolved while the page is read
                if let Some(e) = self.next_up(token, &t) {
                    self.warm(token, Rk::Episode { stype: t.stype.clone(), id: t.id.clone(), video: e }.encode());
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
        let mut rows = [row];
        self.mark_watched_rows(token, &t, &mut rows);
        Response::json(200, &container(json!({"size": 1, "Metadata": rows})))
    }

    /// `/library/metadata/{rk}/children`: a show's seasons, or a season's episodes.
    pub(super) fn children(&self, token: &str, rk_s: &str) -> Response {
        let Some(rk) = Rk::decode(rk_s) else { return Response::empty(404) };
        let t = match self.rk_title(token, &rk) {
            Ok(t) => t,
            Err(r) => return r,
        };
        let shelf = row_shelf(Lib::of_title(&t.stype, &t.id));
        let mut rows: Vec<Value> = match &rk {
            Rk::Item { .. } => catalog::season_rows(&t, shelf, &self.art),
            Rk::Season { season, .. } => t
                .episodes(*season)
                .into_iter()
                .map(|e| catalog::episode_row(&t, e, shelf, &self.art, true))
                .collect(),
            Rk::Episode { .. } => Vec::new(),
        };
        self.mark_watched_rows(token, &t, &mut rows);
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
        let mut rows: Vec<Value> = t
            .seasons()
            .into_iter()
            .flat_map(|s| t.episodes(s))
            .map(|e| catalog::episode_row(&t, e, shelf, &self.art, true))
            .collect();
        self.mark_watched_rows(token, &t, &mut rows);
        Response::json(200, &container(json!({"size": rows.len(), "Metadata": rows})))
    }

    // ---------------------------------------------------------------- artwork

    /// The image behind an art stand-in, as fetched. Only JPEG and PNG pass: those are the formats
    /// the app decodes, and anything else would be a broken tile rather than an empty one.
    pub(super) fn art(&self, key: &str) -> Response {
        let Some(url) = self.art.url(key) else {
            note_art_trouble("https://(unknown key)/", "was asked for by a key this run never made");
            return Response::empty(404);
        };
        // Read into a local FIRST: a guard taken in a `match` scrutinee lives to the end of the
        // match, and the miss arm locks the cache again to store — a self-deadlock that froze the
        // first poster request and then every request after it (each takes the lock to warm art).
        let cached = self.art_cache.lock().unwrap_or_else(|e| e.into_inner()).get(key);
        let bytes = match cached {
            Some(b) => b,
            None => {
                let fetched = {
                    // a bounded number at once: fifty posters asked for in one scroll otherwise
                    // race one another to the image host and time out together
                    let _slot = ArtSlot::take();
                    self.stremio.fetch_image(&url).or_else(|_| self.stremio.fetch_image(&url))
                };
                let Ok(bytes) = fetched else {
                    note_art_trouble(&url, "did not answer");
                    return Response::empty(502);
                };
                // the app decodes JPEG and PNG; anything else the image library reads (WebP, which
                // metahub serves for part of its catalogue) is re-encoded as a JPEG here
                let bytes = if image_kind(&bytes).is_some() {
                    bytes
                } else {
                    match as_jpeg(&bytes) {
                        Some(jpeg) => jpeg,
                        None => {
                            note_art_trouble(&url, &format!("answered with {} the app cannot show", sniff(&bytes)));
                            return Response::empty(404);
                        }
                    }
                };
                let bytes = Arc::new(bytes);
                self.art_cache.lock().unwrap_or_else(|e| e.into_inner()).put(key, bytes.clone());
                bytes
            }
        };
        let Some(kind) = image_kind(&bytes) else { return Response::empty(404) };
        Response { status: 200, content_type: kind, body: bytes.to_vec(), location: None }
    }

    /// Fetch, in the background, the posters a listing just named (`body` is its JSON), so the
    /// app's requests for them a moment later are answered from memory instead of each waiting on
    /// a remote image host.
    pub(super) fn warm_art(&self, body: &[u8]) {
        let keys: Vec<String> = {
            let cache = self.art_cache.lock().unwrap_or_else(|e| e.into_inner());
            let mut warming = self.art_warming.lock().unwrap_or_else(|e| e.into_inner());
            art_keys(body)
                .filter(|k| !cache.has(k))
                .filter(|k| warming.insert(k.clone()))
                .take(ART_WARM_MAX)
                .collect()
        };
        if keys.is_empty() {
            return;
        }
        let Some(me) = self.me.get().and_then(std::sync::Weak::upgrade) else {
            self.art_warming.lock().unwrap_or_else(|e| e.into_inner()).clear();
            return;
        };
        let spawned = plx_base::task::spawn("stremio-art", move || {
            let bridge: &Bridge = &me;
            for chunk in keys.chunks(ART_WARM_WIDTH) {
                let jobs: Vec<Box<dyn FnOnce() + Send + '_>> = chunk
                    .iter()
                    .map(|k| Box::new(move || { let _ = bridge.art(k); }) as Box<dyn FnOnce() + Send + '_>)
                    .collect();
                parallel(jobs);
                let mut warming = bridge.art_warming.lock().unwrap_or_else(|e| e.into_inner());
                for k in chunk {
                    warming.remove(k);
                }
            }
        });
        if spawned.is_none() {
            self.art_warming.lock().unwrap_or_else(|e| e.into_inner()).clear();
        }
    }

    /// `/photo/:/transcode?url=/stremio/art/{key}&…`: the app's sized-artwork request.
    pub(super) fn photo(&self, req: &Request) -> Response {
        let Some(url) = req.query("url") else { return Response::empty(404) };
        if let Some(key) = url.strip_prefix("/stremio/art/") {
            return self.art(key);
        }
        // `/library/metadata/{rk}/clearLogo`: the title's logo, which the heroes draw in place of
        // its name — metahub's, for an IMDb title (a show's, for one of its episodes)
        if let Some(rk) = url.strip_prefix("/library/metadata/").and_then(|r| r.strip_suffix("/clearLogo")) {
            let id = match Rk::decode(rk) {
                Some(Rk::Item { id, .. } | Rk::Season { id, .. } | Rk::Episode { id, .. }) => id,
                None => return Response::empty(404),
            };
            let Some(logo) = catalog::metahub("logo", &id) else { return Response::empty(404) };
            let path = self.art.path(&logo);
            let answer = self.art(path.trim_start_matches("/stremio/art/"));
            // most titles have no logo: say so for good (a 404), so the hero settles on the name
            // instead of retrying a fetch that will never succeed
            return if answer.status == 200 { answer } else { Response::empty(404) };
        }
        Response::empty(404)
    }
}

/// Posters fetched ahead per listing, at most: a screenful of shelves.
const ART_WARM_MAX: usize = 60;
/// Posters fetched ahead at once.
const ART_WARM_WIDTH: usize = 6;
/// The fetched-artwork memory's budget, bytes: a few hundred posters, small beside the app's own.
const ART_CACHE_BYTES: usize = 24 * 1024 * 1024;

/// Artwork fetched from image hosts at once, at most (the app's own requests and the warm-ups).
const ART_FETCH_WIDTH: usize = 8;

static ART_FETCHING: Mutex<usize> = Mutex::new(0);
static ART_FREED: std::sync::Condvar = std::sync::Condvar::new();

/// One of [`ART_FETCH_WIDTH`] fetch slots, held while an image is fetched.
struct ArtSlot;

impl ArtSlot {
    fn take() -> ArtSlot {
        let mut n = ART_FETCHING.lock().unwrap_or_else(|e| e.into_inner());
        while *n >= ART_FETCH_WIDTH {
            n = ART_FREED.wait(n).unwrap_or_else(|e| e.into_inner());
        }
        *n += 1;
        ArtSlot
    }
}

impl Drop for ArtSlot {
    fn drop(&mut self) {
        let mut n = ART_FETCHING.lock().unwrap_or_else(|e| e.into_inner());
        *n = n.saturating_sub(1);
        ART_FREED.notify_one();
    }
}

/// Say once per image host (and kind of trouble) that its artwork could not be used — the host
/// only: an image address can carry a service's key.
fn note_art_trouble(url: &str, what: &str) {
    static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let host = url.splitn(4, '/').nth(2).unwrap_or("?");
    let line = format!("stremio: artwork from {host} {what}");
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if seen.len() < 32 && !seen.contains(&line) {
        plx_base::eventlog::log(&line);
        seen.push(line);
    }
}

/// An image the app cannot decode itself (WebP, say) as a JPEG it can, at most poster-sized; `None`
/// when the bytes are no image the library reads.
fn as_jpeg(bytes: &[u8]) -> Option<Vec<u8>> {
    let img = image::load_from_memory(bytes).ok()?;
    let img = if img.width() > ART_MAX_SIDE || img.height() > ART_MAX_SIDE {
        img.thumbnail(ART_MAX_SIDE, ART_MAX_SIDE)
    } else {
        img
    };
    let rgb = img.to_rgb8();
    let mut out = Vec::new();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 88);
    enc.encode(rgb.as_raw(), rgb.width(), rgb.height(), image::ExtendedColorType::Rgb8).ok()?;
    Some(out)
}

/// The longest side a converted image keeps: a backdrop at 1080p, more than any poster needs.
const ART_MAX_SIDE: u32 = 1920;

/// What `bytes` look like, for the log: a format name, or that they are text.
fn sniff(bytes: &[u8]) -> &'static str {
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "a WebP"
    } else if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        "an AVIF/HEIF"
    } else if bytes.starts_with(b"GIF8") {
        "a GIF"
    } else if bytes.first().is_some_and(|b| *b == b'<' || *b == b'{') {
        "a text page"
    } else {
        "an unknown format"
    }
}

/// The image type `bytes` start with, as an HTTP content type; `None` for anything else.
fn image_kind(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"\x89PNG") {
        Some("image/png")
    } else {
        None
    }
}

/// The art keys a JSON listing names (`/stremio/art/{key}`), in order, each once.
fn art_keys(body: &[u8]) -> impl Iterator<Item = String> + '_ {
    const MARK: &[u8] = b"/stremio/art/";
    let mut seen = std::collections::HashSet::new();
    let mut at = 0;
    std::iter::from_fn(move || loop {
        let start = at + body.get(at..)?.windows(MARK.len()).position(|w| w == MARK)? + MARK.len();
        let len = body[start..].iter().take_while(|b| b.is_ascii_alphanumeric() || b"-_~".contains(b)).count();
        at = start + len;
        if len == 0 {
            continue;
        }
        let key = String::from_utf8_lossy(&body[start..start + len]).into_owned();
        if seen.insert(key.clone()) {
            return Some(key);
        }
    })
}

/// Fetched artwork by key, oldest dropped first past [`ART_CACHE_BYTES`].
#[derive(Default)]
pub(super) struct ArtCache {
    map: HashMap<String, Arc<Vec<u8>>>,
    order: std::collections::VecDeque<String>,
    bytes: usize,
}

impl ArtCache {
    fn get(&self, key: &str) -> Option<Arc<Vec<u8>>> {
        self.map.get(key).cloned()
    }

    fn has(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }

    fn put(&mut self, key: &str, bytes: Arc<Vec<u8>>) {
        if bytes.len() > ART_CACHE_BYTES / 4 || self.map.contains_key(key) {
            return;
        }
        self.bytes += bytes.len();
        self.map.insert(key.to_string(), bytes);
        self.order.push_back(key.to_string());
        while self.bytes > ART_CACHE_BYTES {
            let Some(old) = self.order.pop_front() else { break };
            if let Some(b) = self.map.remove(&old) {
                self.bytes -= b.len();
            }
        }
    }
}

#[cfg(test)]
mod art_cache_tests {
    use super::*;

    /// A WebP poster (what metahub answers for part of its catalogue) reaches the app as a JPEG.
    #[test]
    fn an_image_the_app_cannot_decode_is_handed_over_as_a_jpeg() {
        let mut webp = Vec::new();
        let img = image::RgbaImage::from_pixel(4, 6, image::Rgba([200, 30, 40, 255]));
        image::codecs::webp::WebPEncoder::new_lossless(&mut webp)
            .encode(img.as_raw(), 4, 6, image::ExtendedColorType::Rgba8)
            .unwrap();
        assert_eq!(sniff(&webp), "a WebP");
        let jpeg = as_jpeg(&webp).expect("converted");
        assert_eq!(image_kind(&jpeg), Some("image/jpeg"));
        assert!(as_jpeg(b"<html>not an image</html>").is_none());
        assert_eq!(sniff(b"<html>"), "a text page");
    }

    #[test]
    fn a_listing_names_its_posters_once_each_in_order() {
        let body = br#"{"a":"/stremio/art/~abc","b":"/photo/:/transcode?url=/stremio/art/0123456789abcdef&w=1","c":"/stremio/art/~abc","d":"/stremio/art/"}"#;
        let keys: Vec<String> = art_keys(body).collect();
        assert_eq!(keys, vec!["~abc".to_string(), "0123456789abcdef".to_string()]);
    }

    #[test]
    fn the_artwork_memory_stays_within_its_budget_dropping_the_oldest() {
        let mut c = ArtCache::default();
        let big = Arc::new(vec![0u8; ART_CACHE_BYTES / 5]);
        for i in 0..7 {
            c.put(&format!("k{i}"), big.clone());
        }
        assert!(c.bytes <= ART_CACHE_BYTES);
        assert!(!c.has("k0"), "the oldest went first");
        assert!(c.has("k6"));
        c.put("huge", Arc::new(vec![0u8; ART_CACHE_BYTES / 2]));
        assert!(!c.has("huge"), "one image may not take the whole memory");
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
        let providers: Vec<&Addon> = library::stream_providers(&addons, stype, id)
            .filter(|a| self.with_config(|c| c.addon_on(a, super::super::api::Resource::Stream)))
            .collect();
        let asks: Vec<(String, String)> = providers
            .iter()
            .map(|addon| (format!("{}/stream/{}/{}.json", addon.base_url(), segment(stype), segment(id)),
                          addon.manifest.name.clone()))
            .collect();
        let found: Vec<Stream> = match self.me.get().and_then(std::sync::Weak::upgrade) {
            Some(me) => gather_streams(&me, asks),
            None => {
                let jobs: Vec<Box<dyn FnOnce() -> Vec<Stream> + Send + '_>> = asks
                    .into_iter()
                    .map(|(url, name)| Box::new(move || streams_from(self, &url, &name))
                        as Box<dyn FnOnce() -> Vec<Stream> + Send + '_>)
                    .collect();
                parallel(jobs).into_iter().flatten().flatten().collect()
            }
        };
        let mut ranked = rank(found);
        self.adjust_ranking(rk, &mut ranked);
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
    pub(super) fn choices(&self, req: &Request, token: &str, rk_s: &str) -> Response {
        let instant = req.query("instant") == Some("1");
        let Some(rk) = Rk::decode(rk_s) else { return Response::empty(404) };
        let ranked = match self.ranked_streams(token, &rk) {
            Ok(r) => r,
            Err(r) => return r,
        };
        let chosen = self.chosen_index(rk_s, &ranked);
        let rows: Vec<Value> = ranked
            .iter()
            .enumerate()
            .filter(|(_, (s, f, _))| s.url.is_some()
                && !(instant && f.ready == super::super::streams::Ready::Uncached))
            .map(|(i, (s, f, _))| {
                // the file's name, else the release name the addon wrote first
                let file = s.filename.as_deref()
                    .or_else(|| s.detail.lines().next().filter(|l| !l.trim().is_empty()))
                    .unwrap_or(s.name.as_str());
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
pub(super) fn choice_label(f: &super::super::streams::Facts) -> String {
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

// ---------------------------------------------------------------- the account's library

impl Bridge {
    /// `/stremio/library/{rk}?add=1|0`: save the title (a film, or an episode's show) in the
    /// account's library, or take it out. Answers `{"inLibrary": bool}` with what now holds.
    pub(super) fn library_toggle(&self, req: &Request, token: &str, rk_s: &str) -> Response {
        let Some(rk) = Rk::decode(rk_s) else { return Response::empty(404) };
        let add = req.query("add") != Some("0");
        let (stype, id) = rk.show();
        let now = super::play::now_iso();
        let items = self.library_items(token);
        let item = match items.iter().find(|it| it.get("_id").and_then(Value::as_str) == Some(id)) {
            Some(existing) => {
                let mut it = existing.clone();
                it["removed"] = json!(!add);
                it["temp"] = json!(false);
                it["_mtime"] = json!(now);
                it
            }
            None if !add => return Response::json(200, &json!({"inLibrary": false})),
            None => {
                let addons = match self.addons(token) {
                    Ok(addons) => addons,
                    Err(e) => return api_failure(e),
                };
                let Some(t) = self.title(&addons, stype, id) else { return Response::empty(404) };
                json!({
                    "_id": id, "name": t.name, "type": stype, "poster": t.poster,
                    "posterShape": "poster", "removed": false, "temp": false, "_ctime": now, "_mtime": now,
                    "state": {"lastWatched": null, "timeWatched": 0, "timeOffset": 0, "overallTimeWatched": 0,
                              "timesWatched": 0, "flaggedWatched": 0, "duration": 0, "video_id": null,
                              "watched": null, "noNotif": false},
                    "behaviorHints": {"defaultVideoId": null, "featuredVideoId": null, "hasScheduledVideos": false},
                })
            }
        };
        match self.stremio.library_put(token, &[item]) {
            Ok(()) => {
                *self.library.lock().unwrap_or_else(|e| e.into_inner()) = None;
                super::super::note_home_changed();
                Response::json(200, &json!({"inLibrary": add}))
            }
            Err(e) => api_failure(e),
        }
    }

    /// `/stremio/config`: the Home rows and addons Settings lists, after the change the query
    /// names, if any — `op=show&id=…&on=0|1`, `op=move&id=…&to=N`, `op=streams|catalogs&id=…&on=0|1`.
    pub(super) fn config_route(&self, req: &Request, token: &str) -> Response {
        use super::super::api::Resource;
        let addons = match self.addons(token) {
            Ok(addons) => addons,
            Err(e) => return api_failure(e),
        };
        let on = req.query("on") != Some("0");
        let id = req.query("id").unwrap_or("").to_string();
        let changed = match req.query("op") {
            Some("show") => {
                let all = self.shelves(&addons);
                self.change_config(|c| c.show_row(&addons, &all, &id, on));
                true
            }
            Some("move") => {
                let all = self.shelves(&addons);
                let to = req.query("to").and_then(|t| t.parse().ok()).unwrap_or(0);
                self.change_config(|c| c.move_row(&addons, &all, &id, to));
                true
            }
            Some("streams") => {
                self.change_config(|c| c.set_addon(&id, Resource::Stream, on));
                true
            }
            Some("catalogs") => {
                self.change_config(|c| c.set_addon(&id, Resource::Catalog, on));
                true
            }
            _ => false,
        };
        if changed {
            super::super::note_home_changed();
        }
        let all = self.shelves(&addons);
        let rows: Vec<Value> = self
            .with_config(|c| c.home_rows(&addons, &all))
            .into_iter()
            .map(|(id, row, shown)| {
                let (title, detail) = match row {
                    super::config::HomeRow::Continue => ("Continue Watching".to_string(), "What you are watching".to_string()),
                    super::config::HomeRow::NewEpisodes => ("New Episodes".to_string(), "Shows in your library".to_string()),
                    super::config::HomeRow::Library => ("My Library".to_string(), "What you saved".to_string()),
                    super::config::HomeRow::ComingSoon => ("Coming Soon".to_string(), "Next episodes of your shows".to_string()),
                    super::config::HomeRow::ForYou => ("Picked for You".to_string(), "Like what you watched".to_string()),
                    super::config::HomeRow::Shelf(n) => {
                        let s = all[n];
                        (shelf_title(&addons, &all, s), format!("{} · {}", s.lib.title(), addons[s.addon].manifest.name.trim()))
                    }
                };
                json!({"id": id, "title": title, "detail": detail, "shown": shown})
            })
            .collect();
        let list: Vec<Value> = addons
            .iter()
            .map(|a| {
                let streams = a.has(Resource::Stream);
                let catalogs = a.has(Resource::Catalog) && a.manifest.catalogs.iter().any(|c| c.browsable());
                let mut does = Vec::new();
                if streams { does.push("streams"); }
                if catalogs { does.push("catalogs"); }
                if a.has(Resource::Meta) { does.push("details"); }
                if a.has(Resource::Subtitles) { does.push("subtitles"); }
                let health = self.health.lock().unwrap_or_else(|e| e.into_inner())
                    .get(&Self::host_of(a.base_url())).copied();
                let status = match health {
                    Some((true, took)) => format!(" · answered in {:.1} s", took.as_secs_f64()),
                    Some((false, _)) => " · not answering".to_string(),
                    None => String::new(),
                };
                self.with_config(|c| json!({
                    "id": super::config::addon_id(a), "name": a.manifest.name.trim(),
                    "detail": format!("{}{status}", does.join(", ")),
                    "streams": streams.then(|| c.addon_on(a, Resource::Stream)),
                    "catalogs": catalogs.then(|| c.addon_on(a, Resource::Catalog)),
                }))
            })
            .collect();
        let mut tv = self.tv_info(token);
        tv["phone"] = match super::super::remote::info() {
            Some((url, pin)) => json!({"url": url, "pin": pin}),
            None => Value::Null,
        };
        Response::json(200, &json!({"rows": rows, "addons": list, "tv": tv}))
    }
}

impl Bridge {
    /// `PUT /actions/removeFromContinueWatching?ratingKey=…`: the item leaves Continue Watching —
    /// its position is cleared (a temporary entry is dropped), its saved state kept.
    pub(super) fn remove_from_continue(&self, req: &Request, token: &str) -> Response {
        let Some(rk) = req.query("ratingKey").and_then(Rk::decode) else { return Response::empty(400) };
        let (_, id) = rk.show();
        let items = self.library_items(token);
        let Some(existing) = items.iter().find(|it| it.get("_id").and_then(Value::as_str) == Some(id)) else {
            return Response::empty(200);
        };
        let mut it = existing.clone();
        it["state"]["timeOffset"] = json!(0);
        if it.get("temp").and_then(Value::as_bool).unwrap_or(false) {
            it["removed"] = json!(true);
        }
        it["_mtime"] = json!(super::play::now_iso());
        match self.stremio.library_put(token, &[it]) {
            Ok(()) => {
                *self.library.lock().unwrap_or_else(|e| e.into_inner()) = None;
                super::super::note_home_changed();
                Response::empty(200)
            }
            Err(e) => api_failure(e),
        }
    }
}

impl Bridge {
    /// `GET /stremio/suggest`: what Search suggests before anything is typed — the first few titles
    /// of the first movie and show catalogs (what is popular now), as search terms.
    pub(super) fn suggest(&self, token: &str) -> Response {
        let addons = match self.addons(token) {
            Ok(addons) => addons,
            Err(e) => return api_failure(e),
        };
        let all = self.shelves(&addons);
        let mut terms: Vec<String> = Vec::new();
        for lib in [Lib::Movies, Lib::Shows] {
            let Some((_, s)) = shelves_of(&all, lib).into_iter().next() else { continue };
            for t in self.catalog_page(&addons[s.addon], (s.addon, s.catalog), 0, None).into_iter().take(SUGGESTIONS_PER_LIB) {
                if !terms.contains(&t.name) {
                    terms.push(t.name);
                }
            }
        }
        Response::json(200, &json!({"terms": terms}))
    }
}

impl Bridge {
    /// **Picked for You**: titles like what the account watched. Its most recently watched titles
    /// ([`FOR_YOU_FROM`]) are read for their genres, weighted toward the latest; each library's top
    /// genres ([`FOR_YOU_GENRES`]) are asked of that library's catalog that offers a genre choice;
    /// the answers are interleaved, and anything already in the account's library (watched,
    /// saved or started) is left out. Empty when nothing has been watched, or no catalog filters
    /// by genre.
    pub(super) fn for_you_rows(&self, token: &str, addons: &[Addon]) -> Vec<Value> {
        let items = self.library_items(token);
        let known: std::collections::HashSet<String> = items.iter()
            .filter_map(|it| it.get("_id").and_then(Value::as_str).map(str::to_string))
            .collect();
        let mut watched: Vec<&Value> = items.iter().filter(|it| watched_at_all(it)).collect();
        watched.sort_by(|a, b| {
            let at = |v: &Value| v.pointer("/state/lastWatched").and_then(Value::as_str).unwrap_or("").to_string();
            at(b).cmp(&at(a))
        });
        let wanted: Vec<(String, String)> = watched.iter()
            .filter_map(|it| Some((it.get("type")?.as_str()?.to_string(), it.get("_id")?.as_str()?.to_string())))
            .filter(|(stype, _)| stype != "other")
            .take(FOR_YOU_FROM)
            .collect();
        if wanted.is_empty() {
            return Vec::new();
        }
        let jobs: Vec<Box<dyn FnOnce() -> Option<Title> + Send + '_>> = wanted.iter()
            .map(|(stype, id)| Box::new(move || self.title(addons, stype, id)) as Box<dyn FnOnce() -> Option<Title> + Send + '_>)
            .collect();
        let titles: Vec<Title> = parallel(jobs).into_iter().flatten().flatten().collect();
        // genre weights per library: the latest watch counts most
        let mut weights: Vec<(Lib, String, usize)> = Vec::new();
        let n = titles.len();
        for (rank, t) in titles.iter().enumerate() {
            let lib = Lib::of_title(&t.stype, &t.id);
            for g in &t.genres {
                match weights.iter_mut().find(|(l, name, _)| *l == lib && name.eq_ignore_ascii_case(g)) {
                    Some(w) => w.2 += n - rank,
                    None => weights.push((lib, g.clone(), n - rank)),
                }
            }
        }
        weights.sort_by(|a, b| b.2.cmp(&a.2));
        let all = self.shelves(addons);
        let mut asks: Vec<(Shelf, String)> = Vec::new();
        for lib in Lib::ALL {
            // the library's first catalog with a genre choice, and its genres as it spells them
            let Some((s, offered)) = shelves_of(&all, lib).into_iter().find_map(|(_, s)| {
                let c = &addons[s.addon].manifest.catalogs[s.catalog];
                let e = c.extra.iter().find(|e| e.name == "genre" && !e.options.is_empty())?;
                Some((s, e.options.clone()))
            }) else { continue };
            for (_, g, _) in weights.iter().filter(|(l, _, _)| *l == lib) {
                if asks.iter().filter(|(a, _)| a.lib == lib).count() >= FOR_YOU_GENRES {
                    break;
                }
                if let Some(spelled) = offered.iter().find(|o| o.eq_ignore_ascii_case(g)) {
                    asks.push((s, spelled.clone()));
                }
            }
        }
        let jobs: Vec<Box<dyn FnOnce() -> Vec<Title> + Send + '_>> = asks.iter()
            .map(|(s, g)| {
                let s = *s;
                Box::new(move || self.catalog_page_in(&addons[s.addon], (s.addon, s.catalog), 0, None, Some(g.as_str())))
                    as Box<dyn FnOnce() -> Vec<Title> + Send + '_>
            })
            .collect();
        let pages: Vec<Vec<Title>> = parallel(jobs).into_iter().map(Option::unwrap_or_default).collect();
        let mut seen = known;
        let mut out = Vec::new();
        let longest = pages.iter().map(Vec::len).max().unwrap_or(0);
        'fill: for i in 0..longest {
            for page in &pages {
                let Some(t) = page.get(i) else { continue };
                if !seen.insert(t.id.clone()) {
                    continue;
                }
                out.push(catalog::title_row(t, row_shelf(Lib::of_title(&t.stype, &t.id)), &self.art));
                if out.len() >= FOR_YOU_ROWS {
                    break 'fill;
                }
            }
        }
        out
    }
}

/// Has the account watched any of this library item: finished it, or got some way in?
fn watched_at_all(it: &Value) -> bool {
    let n = |k: &str| it.pointer(k).and_then(Value::as_i64).unwrap_or(0);
    n("/state/timesWatched") > 0 || n("/state/flaggedWatched") > 0 || n("/state/timeOffset") > 0
        || n("/state/timeWatched") > 0 || it.pointer("/state/watched").and_then(Value::as_str).is_some_and(|w| !w.is_empty())
}

/// Every bridge shelf on a hubs answer becomes a **linked** shelf: a promoted collection hub
/// (`custom.collection.{section}.{rk}.{rk}`, listing `/library/collections/{rk}/children`), so
/// its heading opens the whole shelf on its own page, read as one long grid that loads as it is
/// scrolled. Its `size` is dropped: a catalog has no count to state, and the heading would
/// otherwise announce the twenty cards on the shelf as the whole of it.
fn link_listings(hubs: &mut [Value], section: i64) {
    for hub in hubs.iter_mut() {
        let Some(n) = hub.get("key").and_then(Value::as_str).and_then(|k| k.strip_prefix("/hubs/stremio/")).map(str::to_string) else {
            continue;
        };
        let rk = format!("{LISTING}{n}");
        let sec = if section != 0 {
            section
        } else {
            hub.pointer("/Metadata/0/librarySectionID").and_then(Value::as_i64).unwrap_or(0)
        };
        hub["key"] = json!(format!("/library/collections/{rk}/children"));
        hub["hubIdentifier"] = json!(format!("custom.collection.{sec}.{rk}.{rk}"));
        if let Some(obj) = hub.as_object_mut() {
            obj.remove("size");
        }
    }
}

/// Lower-case hex of a text's bytes (a search query inside a ratingKey).
fn hex(text: &str) -> String {
    text.bytes().map(|b| format!("{b:02x}")).collect()
}

/// The text back from [`hex`].
fn unhex(h: &str) -> Option<String> {
    if h.len() % 2 != 0 {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..h.len()).step_by(2).map(|i| u8::from_str_radix(h.get(i..i + 2)?, 16).ok()).collect();
    String::from_utf8(bytes?).ok()
}

impl Bridge {
    /// `GET /library/metadata/stremio-…`: a listing's header, as a collection row.
    pub(super) fn listing_header(&self, token: &str, rk: &str) -> Response {
        let Some(title) = self.listing_title(token, rk.trim_start_matches(LISTING)) else {
            return Response::empty(404);
        };
        Response::json(200, &container(json!({"size": 1, "Metadata": [{
            "ratingKey": rk, "key": format!("/library/collections/{rk}/children"),
            "type": "collection", "title": title, "summary": "",
        }]})))
    }

    /// `GET /library/collections/stremio-…/children`: a listing's rows, a window at a time (the
    /// collection page asks for the next window as the grid scrolls toward its end).
    pub(super) fn listing_children(&self, req: &Request, token: &str, rk: &str) -> Response {
        let Some(n) = rk.strip_prefix(LISTING) else { return Response::empty(404) };
        if let Some(rest) = n.strip_prefix("q-") {
            return self.search_listing(req, token, rest);
        }
        if let Some(rest) = n.strip_prefix("d-") {
            return self.browse_listing(req, token, rest);
        }
        self.shelf_page(req, token, n)
    }

    /// What a listing is called.
    fn listing_title(&self, token: &str, n: &str) -> Option<String> {
        let own = match n {
            "library" => Some("My Library"),
            "new_episodes" => Some("New Episodes"),
            "coming_soon" => Some("Coming Soon"),
            "for_you" => Some("Picked for You"),
            _ => None,
        };
        if let Some(t) = own {
            return Some(t.to_string());
        }
        if let Some(rest) = n.strip_prefix("d-") {
            let (shelf, genre) = rest.split_once('-')?;
            let addons = self.addons(token).ok()?;
            let all = self.shelves(&addons);
            let s = all.get(shelf.parse::<usize>().ok()?).copied()?;
            let genre = unhex(genre)?;
            let name = shelf_title(&addons, &all, s);
            return Some(if genre.is_empty() {
                format!("{name} · {}", s.lib.title())
            } else {
                format!("{genre} · {name} · {}", s.lib.title())
            });
        }
        if let Some(rest) = n.strip_prefix("q-") {
            let (kind, q) = rest.split_once('-')?;
            let label = if kind == "m" { "All movies" } else { "All shows" };
            return Some(format!("{label} · {}", unhex(q)?));
        }
        let addons = self.addons(token).ok()?;
        let all = self.shelves(&addons);
        if let Some((s, g)) = n.strip_prefix('g').and_then(|r| r.split_once('-')) {
            let s = all.get(s.parse::<usize>().ok()?).copied()?;
            let g = g.parse::<usize>().ok()?;
            return self.library_genres(&addons, &all, s.lib).into_iter()
                .find(|(gn, gi, _)| all.get(*gn) == Some(&s) && *gi == g)
                .map(|(_, _, name)| format!("{name} · {}", s.lib.title()));
        }
        let s = all.get(n.parse::<usize>().ok()?).copied()?;
        Some(format!("{} · {}", shelf_title(&addons, &all, s), s.lib.title()))
    }

    /// Every result of a search of one kind (`m` or `s`, then the query in [`hex`]), windowed.
    fn search_listing(&self, req: &Request, token: &str, rest: &str) -> Response {
        let Some((kind, q)) = rest.split_once('-') else { return Response::empty(404) };
        let (Some(query), stype) = (unhex(q), if kind == "m" { "movie" } else { "series" }) else {
            return Response::empty(404);
        };
        let addons = match self.addons(token) {
            Ok(addons) => addons,
            Err(e) => return api_failure(e),
        };
        let catalogs: Vec<(usize, usize)> = library::search_catalogs(&addons, stype).into_iter()
            .filter(|(a, _)| self.with_config(|c| c.addon_on(&addons[*a], super::super::api::Resource::Catalog)))
            .collect();
        let q = query.as_str();
        let addons_ref = &addons;
        let jobs: Vec<Box<dyn FnOnce() -> Vec<Title> + Send + '_>> = catalogs.iter()
            .map(|&(a, c)| Box::new(move || self.catalog_page(&addons_ref[a], (a, c), 0, Some(q)))
                as Box<dyn FnOnce() -> Vec<Title> + Send + '_>)
            .collect();
        let mut seen = std::collections::HashSet::new();
        let rows: Vec<Value> = parallel(jobs).into_iter().flatten().flatten()
            .filter(|t| seen.insert(t.id.clone()))
            .map(|t| catalog::title_row(&t, row_shelf(Lib::of_title(&t.stype, &t.id)), &self.art))
            .collect();
        let (start, size) = window(req, 60);
        let page: Vec<Value> = rows.iter().skip(start).take(size).cloned().collect();
        Response::json(200, &container(json!({"size": page.len(), "totalSize": rows.len(), "offset": start,
                                              "Metadata": page})))
    }
}

// ---------------------------------------------------------------- browse (Search's filters)

/// Search's filter TYPE ([`Browse`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind2 {
    All,
    Movies,
    Shows,
    Anime,
}

impl Kind2 {
    fn libs(self) -> &'static [Lib] {
        match self {
            Kind2::All => &[Lib::Movies, Lib::Shows],
            Kind2::Movies => &[Lib::Movies],
            Kind2::Shows => &[Lib::Shows],
            Kind2::Anime => &[Lib::Anime],
        }
    }

    /// Does a typed search of Stremio type `stype` belong under this choice?
    fn has_stype(self, stype: &str) -> bool {
        match self {
            Kind2::All | Kind2::Anime => true,
            Kind2::Movies => stype == "movie",
            Kind2::Shows => stype == "series",
        }
    }
}

/// Search's filter ORDER.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Order {
    ForYou,
    Popular,
    TopRated,
    Newest,
}

/// The filters the Search page sends inside its query (`screens::search::filters`): the text,
/// then `::f|{type}|{order}|{genre}`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Browse {
    kind: Kind2,
    order: Order,
    /// Empty for every genre.
    genre: String,
}

impl Browse {
    /// The query's text and, when it carries them, its filters.
    fn split(query: &str) -> (&str, Option<Browse>) {
        let Some(at) = query.find("::f") else { return (query, None) };
        let mut parts = query[at + 3..].split('|').skip(1);
        let kind = match parts.next().unwrap_or("all") {
            "movie" => Kind2::Movies,
            "show" => Kind2::Shows,
            "anime" => Kind2::Anime,
            _ => Kind2::All,
        };
        let order = match parts.next().unwrap_or("foryou") {
            "popular" => Order::Popular,
            "rated" => Order::TopRated,
            "new" => Order::Newest,
            _ => Order::ForYou,
        };
        let genre = parts.next().unwrap_or("").trim().to_string();
        (&query[..at], Some(Browse { kind, order, genre }))
    }
}

/// How well a catalog's id and name answer an order: higher is better, 0 when nothing says so.
fn order_fit(id: &str, name: &str, order: Order) -> u8 {
    let words = format!("{id} {name}").to_ascii_lowercase();
    let has = |xs: &[&str]| xs.iter().any(|x| words.contains(x));
    match order {
        Order::Popular | Order::ForYou => if id == "top" || has(&["popular", "trending"]) { 2 } else { 0 },
        Order::TopRated => if has(&["imdbrating", "rating", "rated", "best"]) { 2 } else { 0 },
        Order::Newest => if id == "year" || has(&["new", "latest", "recent", "year", "airing"]) { 2 } else { 0 },
    }
}

impl Bridge {
    /// The shelf of library `lib` that best answers `order` and can be narrowed to `genre` (any
    /// shelf when `genre` is empty), with the genre spelled as that catalog spells it.
    fn browse_shelf(&self, addons: &[Addon], all: &[Shelf], lib: Lib, order: Order, genre: &str)
        -> Option<(usize, Shelf, Option<String>)> {
        let mut best: Option<(u8, usize, Shelf, Option<String>)> = None;
        for (n, s) in shelves_of(all, lib) {
            let c = &addons[s.addon].manifest.catalogs[s.catalog];
            let spelled = if genre.is_empty() {
                None
            } else {
                let offered = c.extra.iter().find(|e| e.name == "genre" && !e.options.is_empty());
                match offered.and_then(|e| e.options.iter().find(|o| o.eq_ignore_ascii_case(genre))) {
                    Some(o) => Some(o.clone()),
                    None => continue,
                }
            };
            // a catalog whose genre choice is REQUIRED (Cinemeta's by-year) is only usable with one
            let required = c.extra.iter().any(|e| e.name == "genre" && e.is_required);
            if required && spelled.is_none() {
                continue;
            }
            let fit = order_fit(&c.id, c.title(), order);
            if best.as_ref().is_none_or(|b| fit > b.0) {
                best = Some((fit, n, s, spelled));
            }
        }
        best.map(|(_, n, s, g)| (n, s, g))
    }

    /// Search with nothing typed: a shelf per library under the filters, and a card per shelf
    /// that opens it whole. FOR YOU is the titles picked from what the account watched
    /// ([`Self::for_you_rows`]), narrowed to the type and genre; with nothing watched yet it is
    /// what is popular.
    fn browse(&self, token: &str, addons: &[Addon], f: &Browse) -> Vec<Value> {
        let all = self.shelves(addons);
        let mut hubs = Vec::new();
        let mut cards = Vec::new();
        if f.order == Order::ForYou {
            let picked: Vec<Value> = self.for_you_rows(token, addons).into_iter()
                .filter(|row| f.genre.is_empty() || row_has_genre(row, &f.genre))
                .collect();
            for lib in f.kind.libs() {
                let wire = lib.wire();
                let rows: Vec<Value> = picked.iter()
                    .filter(|row| row.get("type").and_then(Value::as_str) == Some(if wire == "movie" { "movie" } else { "show" }))
                    .filter(|row| *lib != Lib::Anime || row.get("librarySectionID").and_then(Value::as_i64) == Some(row_shelf(Lib::Anime).section))
                    .cloned()
                    .collect();
                if !rows.is_empty() {
                    hubs.push(json!({"title": wire, "type": wire, "hubIdentifier": wire, "size": rows.len(), "Metadata": rows}));
                }
            }
            if !hubs.is_empty() {
                cards.push(json!({"ratingKey": format!("{LISTING}for_you"), "key": format!("/library/collections/{LISTING}for_you/children"),
                    "type": "collection", "title": "Picked for You", "thumb": hubs[0].pointer("/Metadata/0/thumb").cloned()}));
            }
        }
        if hubs.is_empty() {
            // the shelves that answer the order (popular when "for you" has nothing to go on)
            let order = if f.order == Order::ForYou { Order::Popular } else { f.order };
            let picks: Vec<(Lib, usize, Shelf, Option<String>)> = f.kind.libs().iter()
                .filter_map(|lib| self.browse_shelf(addons, &all, *lib, order, &f.genre).map(|(n, s, g)| (*lib, n, s, g)))
                .collect();
            let jobs: Vec<Box<dyn FnOnce() -> Vec<Title> + Send + '_>> = picks.iter()
                .map(|(_, _, s, g)| {
                    let (s, g) = (*s, g.clone());
                    Box::new(move || self.catalog_page_in(&addons[s.addon], (s.addon, s.catalog), 0, None, g.as_deref()))
                        as Box<dyn FnOnce() -> Vec<Title> + Send + '_>
                })
                .collect();
            for ((lib, n, s, g), titles) in picks.iter().zip(parallel(jobs)) {
                let titles = titles.unwrap_or_default();
                if titles.is_empty() {
                    continue;
                }
                let rows = self.rows(&titles, *lib);
                let wire = lib.wire();
                // a library's kind may already hold a shelf (anime and shows are both shows)
                if hubs.iter().any(|h| h.get("type").and_then(Value::as_str) == Some(wire)) {
                    continue;
                }
                let genre = g.clone().unwrap_or_default();
                let rk = format!("{LISTING}d-{n}-{}", hex(&genre));
                let name = shelf_title(addons, &all, *s);
                let title = if genre.is_empty() { format!("All · {name}") } else { format!("All {genre} · {name}") };
                cards.push(json!({"ratingKey": rk, "key": format!("/library/collections/{rk}/children"),
                    "type": "collection", "subtype": wire, "title": title,
                    "thumb": titles.iter().find_map(|t| t.poster.as_deref()).map(|u| self.art.path(u))}));
                hubs.push(json!({"title": wire, "type": wire, "hubIdentifier": wire, "size": rows.len(), "Metadata": rows}));
            }
        }
        if !cards.is_empty() {
            hubs.push(json!({"title": "collection", "type": "collection", "hubIdentifier": "collection",
                             "size": cards.len(), "Metadata": cards}));
        }
        hubs
    }

    /// A browse shelf whole (`d-{shelf}-{genre in hex}`), a window at a time.
    fn browse_listing(&self, req: &Request, token: &str, rest: &str) -> Response {
        let Some((shelf, genre)) = rest.split_once('-') else { return Response::empty(404) };
        let Some(genre) = unhex(genre) else { return Response::empty(404) };
        let addons = match self.addons(token) {
            Ok(addons) => addons,
            Err(e) => return api_failure(e),
        };
        let all = self.shelves(&addons);
        let Some(s) = shelf.parse::<usize>().ok().and_then(|n| all.get(n).copied()) else {
            return Response::empty(404);
        };
        let (start, size) = window(req, 60);
        let (rows, ended) = if genre.is_empty() {
            let (titles, ended) = self.shelf_window(&addons[s.addon], s, start, size);
            (self.rows(&titles, s.lib), ended)
        } else {
            let titles = self.catalog_page_in(&addons[s.addon], (s.addon, s.catalog), start, None, Some(&genre));
            let ended = titles.is_empty();
            (self.rows(&titles, s.lib), ended)
        };
        let total = if ended { start + rows.len() } else { start + rows.len() + size.max(1) };
        Response::json(200, &container(json!({"size": rows.len(), "totalSize": total, "offset": start, "Metadata": rows})))
    }
}

/// Does a title row carry `genre` among its genres?
fn row_has_genre(row: &Value, genre: &str) -> bool {
    row.get("Genre").and_then(Value::as_array).is_some_and(|tags| tags.iter()
        .filter_map(|t| t.get("tag").and_then(Value::as_str))
        .any(|t| t.eq_ignore_ascii_case(genre)))
}

#[cfg(test)]
mod browse_tests {
    use super::*;

    #[test]
    fn a_query_carries_its_text_and_its_filters() {
        assert_eq!(Browse::split("dune"), ("dune", None));
        let (text, f) = Browse::split("dune::f|movie|rated|Action");
        assert_eq!(text, "dune");
        assert_eq!(f, Some(Browse { kind: Kind2::Movies, order: Order::TopRated, genre: "Action".into() }));
        let (text, f) = Browse::split("::f|all|foryou|");
        assert_eq!(text, "");
        assert_eq!(f, Some(Browse { kind: Kind2::All, order: Order::ForYou, genre: String::new() }));
    }

    #[test]
    fn an_order_is_answered_by_the_catalog_named_for_it() {
        assert!(order_fit("top", "Popular", Order::Popular) > order_fit("imdbRating", "Featured", Order::Popular));
        assert!(order_fit("imdbRating", "Featured", Order::TopRated) > 0);
        assert!(order_fit("year", "New", Order::Newest) > 0);
    }
}

/// More Like This tiles at most.
const RELATED_ROWS: usize = 20;
/// Search results a person page reads the cast of, per kind.
const PERSON_CANDIDATES: usize = 16;

impl Bridge {
    /// Remember the names behind a title's people ids, so their pages can be opened.
    pub(super) fn note_people(&self, t: &Title) {
        let mut people = self.people.lock().unwrap_or_else(|e| e.into_inner());
        if people.len() > 4000 {
            people.clear();
        }
        for name in t.cast.iter().chain(&t.directors).chain(&t.writers) {
            people.insert(catalog::person_id(name), name.clone());
        }
    }

    /// `GET /library/metadata/{rk},{rk},…`: several titles' rows at once (a person page reads its
    /// shelves' rows this way). Plain rows, no streams resolved.
    pub(super) fn metadata_many(&self, token: &str, list: &str) -> Response {
        let Ok(addons) = self.addons(token) else { return Response::empty(502) };
        let rks: Vec<Rk> = list.split(',').filter_map(Rk::decode).take(60).collect();
        let jobs: Vec<Box<dyn FnOnce() -> Option<Title> + Send + '_>> = rks.iter()
            .map(|rk| {
                let addons = &addons;
                Box::new(move || {
                    let (stype, id) = rk.show();
                    self.title(addons, stype, id)
                }) as Box<dyn FnOnce() -> Option<Title> + Send + '_>
            })
            .collect();
        let rows: Vec<Value> = parallel(jobs).into_iter().flatten().flatten()
            .map(|t| catalog::detail_row(&t, row_shelf(Lib::of_title(&t.stype, &t.id)), &self.art))
            .collect();
        Response::json(200, &container(json!({"size": rows.len(), "Metadata": rows})))
    }

    /// `GET /library/metadata/{rk}/related`: **More Like This** — titles of the same kind in the
    /// title's first genres (from the library's catalog that filters by genre), leaving out the
    /// title itself and anything the account has watched.
    pub(super) fn related(&self, token: &str, rk_s: &str) -> Response {
        let empty = || Response::json(200, &container(json!({"size": 0, "Hub": []})));
        let Some(rk) = Rk::decode(rk_s) else { return empty() };
        let Ok(addons) = self.addons(token) else { return empty() };
        let (stype, id) = rk.show();
        let Some(t) = self.title(&addons, stype, id) else { return empty() };
        let lib = Lib::of_title(&t.stype, &t.id);
        let all = self.shelves(&addons);
        let Some((s, offered)) = shelves_of(&all, lib).into_iter().find_map(|(_, s)| {
            let c = &addons[s.addon].manifest.catalogs[s.catalog];
            let e = c.extra.iter().find(|e| e.name == "genre" && !e.options.is_empty())?;
            Some((s, e.options.clone()))
        }) else { return empty() };
        let genres: Vec<String> = t.genres.iter()
            .filter_map(|g| offered.iter().find(|o| o.eq_ignore_ascii_case(g)).cloned())
            .take(2)
            .collect();
        let addons_ref: &[Addon] = &addons;
        let jobs: Vec<Box<dyn FnOnce() -> Vec<Title> + Send + '_>> = genres.iter()
            .map(|g| Box::new(move || self.catalog_page_in(&addons_ref[s.addon], (s.addon, s.catalog), 0, None, Some(g.as_str())))
                as Box<dyn FnOnce() -> Vec<Title> + Send + '_>)
            .collect();
        let pages: Vec<Vec<Title>> = parallel(jobs).into_iter().map(Option::unwrap_or_default).collect();
        let items = self.library_items(token);
        let mut seen: std::collections::HashSet<String> = items.iter()
            .filter(|it| watched_at_all(it))
            .filter_map(|it| it.get("_id").and_then(Value::as_str).map(str::to_string))
            .collect();
        seen.insert(t.id.clone());
        let mut titles: Vec<Title> = Vec::new();
        let longest = pages.iter().map(Vec::len).max().unwrap_or(0);
        'fill: for i in 0..longest {
            for page in &pages {
                if let Some(x) = page.get(i).filter(|x| seen.insert(x.id.clone())) {
                    titles.push(x.clone());
                    if titles.len() >= RELATED_ROWS {
                        break 'fill;
                    }
                }
            }
        }
        if titles.is_empty() {
            return empty();
        }
        let rows = self.rows(&titles, lib);
        let kind = if matches!(rk, Rk::Item { kind: catalog::Kind::Movie, .. }) { "movie" } else { "show" };
        Response::json(200, &container(json!({"size": 1, "Hub": [{
            "hubIdentifier": format!("{kind}.similar"), "title": "More Like This", "type": kind,
            "size": rows.len(), "Metadata": rows,
        }]})))
    }

    /// `GET /library/people/{id}/media`: what a person is in — the search catalogs asked for their
    /// name, each answer kept only when its own cast or crew names them (a search for a name also
    /// finds titles that merely contain it).
    pub(super) fn person_media(&self, token: &str, id: &str) -> Response {
        let empty = || Response::json(200, &container(json!({"size": 0, "Metadata": []})));
        let name = id.parse::<i64>().ok()
            .and_then(|id| self.people.lock().unwrap_or_else(|e| e.into_inner()).get(&id).cloned());
        let Some(name) = name else { return empty() };
        let Ok(addons) = self.addons(token) else { return empty() };
        let mut candidates: Vec<Title> = Vec::new();
        for stype in ["movie", "series"] {
            let catalogs: Vec<(usize, usize)> = library::search_catalogs(&addons, stype).into_iter()
                .filter(|(a, _)| self.with_config(|c| c.addon_on(&addons[*a], super::super::api::Resource::Catalog)))
                .take(1)
                .collect();
            for (a, c) in catalogs {
                candidates.extend(self.catalog_page(&addons[a], (a, c), 0, Some(&name)).into_iter().take(PERSON_CANDIDATES));
            }
        }
        let jobs: Vec<Box<dyn FnOnce() -> Option<Title> + Send + '_>> = candidates.iter()
            .map(|c| {
                let addons = &addons;
                Box::new(move || self.title(addons, &c.stype, &c.id)) as Box<dyn FnOnce() -> Option<Title> + Send + '_>
            })
            .collect();
        let named = |list: &[String]| list.iter().any(|n| n.trim().eq_ignore_ascii_case(name.trim()));
        let rows: Vec<Value> = parallel(jobs).into_iter().flatten().flatten()
            .filter(|t| named(&t.cast) || named(&t.directors) || named(&t.writers))
            .map(|t| catalog::title_row(&t, row_shelf(Lib::of_title(&t.stype, &t.id)), &self.art))
            .collect();
        Response::json(200, &container(json!({"size": rows.len(), "Metadata": rows})))
    }
}

/// Score added to a stream in the viewer's audio language (more than an English release earns, so
/// a Spanish speaker's Spanish release outranks an English one of the same readiness).
const LANGUAGE_BONUS: i64 = 4_500;
/// Score added to a dual-audio or dubbed anime release for a show watched in English.
const DUB_BONUS: i64 = 600;
/// Score taken from a stream that did not start for this item (below even a camera copy).
const DEAD_PENALTY: i64 = 40_000;

/// What a release says about its languages, as each language is commonly written in release names
/// and addon descriptions: flags, names, and scene tags. Two-letter code → lower-case needles.
fn language_needles(code: &str) -> &'static [&'static str] {
    match code {
        "es" => &["🇪🇸", "🇲🇽", "spanish", "español", "espanol", "castellano", "latino", " spa ", ".spa."],
        "fr" => &["🇫🇷", "french", "truefrench", "vff", "vf2", " vf ", ".vf."],
        "de" => &["🇩🇪", "german", "deutsch", " ger ", ".ger."],
        "it" => &["🇮🇹", "italian", "italiano", " ita ", ".ita."],
        "pt" => &["🇧🇷", "🇵🇹", "portuguese", "português", "dublado", " por ", ".por."],
        "ru" => &["🇷🇺", "russian", " rus ", ".rus."],
        "pl" => &["🇵🇱", "polish", " pol ", ".pol."],
        "nl" => &["🇳🇱", "dutch", " nld ", ".nld."],
        "tr" => &["🇹🇷", "turkish", " tur ", ".tur."],
        "ja" => &["🇯🇵", "japanese", " jpn ", ".jpn."],
        "ko" => &["🇰🇷", "korean", " kor ", ".kor."],
        "zh" => &["🇨🇳", "🇹🇼", "chinese", "mandarin", "cantonese"],
        "hi" => &["🇮🇳", "hindi"],
        "ar" => &["🇸🇦", "arabic"],
        _ => &[],
    }
}

/// Does a release's text name language `code`?
pub(super) fn names_language(text: &str, code: &str) -> bool {
    let t = format!(" {} ", text.to_lowercase().replace(['\n', '/', '|', '[', ']', '(', ')', '-'], " "));
    language_needles(code).iter().any(|n| t.contains(n))
}

/// Is a release dual-audio or dubbed?
pub(super) fn is_dub(text: &str) -> bool {
    let t = text.to_lowercase();
    ["dual audio", "dual-audio", "dualaudio", "dubbed", "english dub", "eng dub", "[dub]", " dub "].iter().any(|n| t.contains(n))
}

impl Bridge {
    /// The viewer's touches on a ranked list, before it is narrowed: releases in their audio
    /// language up (when that is not English, which the ranking already favours), dual-audio and
    /// dubbed anime up for a show watched in English, and streams that did not start for this
    /// item down. The order stays stable among equals.
    fn adjust_ranking(&self, rk: &Rk, ranked: &mut [(super::super::streams::Stream, super::super::streams::Facts, i64)]) {
        let rk_s = rk.encode();
        let wanted = {
            let profile = self.profile.lock().unwrap_or_else(|e| e.into_inner());
            profile.get("defaultAudioLanguage").and_then(Value::as_str)
                .and_then(super::subs::two_letter).unwrap_or_else(|| "en".into())
        };
        let (stype, show_id) = rk.show();
        let anime = stype == "anime" || show_id.starts_with("kitsu:") || show_id.starts_with("mal:");
        let (dead, watched_in_english) = self.with_config(|c| {
            let english = c.shows.get(show_id)
                .and_then(|m| m.audio_lang.as_deref())
                .and_then(super::subs::two_letter)
                .is_some_and(|l| l == "en");
            (c.dead.get(&rk_s).cloned().unwrap_or_default(), english)
        });
        let mut changed = false;
        for (s, _, score) in ranked.iter_mut() {
            let text = format!("{}\n{}\n{}", s.name, s.detail, s.filename.as_deref().unwrap_or(""));
            if wanted != "en" && names_language(&text, &wanted) {
                *score += LANGUAGE_BONUS;
                changed = true;
            }
            if anime && watched_in_english && is_dub(&text) {
                *score += DUB_BONUS;
                changed = true;
            }
            if dead.contains(&super::config::stream_key(&s.addon, s.filename.as_deref(), &s.detail)) {
                *score -= DEAD_PENALTY;
                changed = true;
            }
        }
        if changed {
            ranked.sort_by(|a, b| b.2.cmp(&a.2));
        }
    }

    /// A stream of `rk_s` did not start: rank it last for that item from now on.
    pub(super) fn mark_dead(&self, rk_s: &str, s: &super::super::streams::Stream) {
        let key = super::config::stream_key(&s.addon, s.filename.as_deref(), &s.detail);
        if self.with_config(|c| c.dead.get(rk_s).is_some_and(|v| v.contains(&key))) {
            return;
        }
        self.change_config(|c| {
            if c.dead.len() >= super::config::DEAD_ITEMS && !c.dead.contains_key(rk_s) {
                if let Some(k) = c.dead.keys().next().cloned() {
                    c.dead.remove(&k);
                }
            }
            let list = c.dead.entry(rk_s.to_string()).or_default();
            if list.len() >= super::config::DEAD_PER_ITEM {
                list.remove(0);
            }
            list.push(key);
        });
    }

    /// A stream of `rk_s` started after all: it is no longer dead.
    pub(super) fn mark_alive(&self, rk_s: &str, s: &super::super::streams::Stream) {
        let key = super::config::stream_key(&s.addon, s.filename.as_deref(), &s.detail);
        if !self.with_config(|c| c.dead.get(rk_s).is_some_and(|v| v.contains(&key))) {
            return;
        }
        self.change_config(|c| {
            if let Some(list) = c.dead.get_mut(rk_s) {
                list.retain(|k| *k != key);
                if list.is_empty() {
                    c.dead.remove(rk_s);
                }
            }
        });
    }
}

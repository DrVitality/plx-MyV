//! Stremio titles in the media server's shapes: item ids, library rows, seasons and episodes.
//!
//! **Ids.** The app addresses every item by an opaque string (`ratingKey`) it puts in paths like
//! `/library/metadata/{rk}`. A Stremio item is a content TYPE plus an id (`movie` + `tt0111161`,
//! `series` + `kitsu:1376`), and an episode is its show plus a video id (`tt0944947:1:2`), so
//! [`Rk`] packs exactly that into a path-safe string and back: one letter for what the app sees
//! (film, show, season, episode), one for Stremio's type, then the escaped id. Nothing is stored
//! to make that work, so a key the app remembered from an earlier launch still means the same
//! title.
//!
//! **Fields.** Addons disagree about value types (`imdbRating` is a string in one and a number in
//! another, `year` comes as `releaseInfo` text), so items are read from `serde_json::Value` with
//! forgiving helpers rather than a strict struct that one odd field would fail whole.
//!
//! **Artwork.** A poster is a long absolute URL, and the app's artwork store keys every image by a
//! request path of at most 255 bytes that already carries the account key. So a row names its art
//! as `/stremio/art/{key}`: the address itself, compactly encoded, when it fits, else a short
//! hashed stand-in ([`ArtMap`]).

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;

/// What the app sees an item as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Movie,
    Show,
}

impl Kind {
    pub fn wire(self) -> &'static str {
        match self {
            Kind::Movie => "movie",
            Kind::Show => "show",
        }
    }
}

/// An item address. `stype` is Stremio's content type (`movie`, `series`, `anime`), which every
/// addon request names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rk {
    Item { kind: Kind, stype: String, id: String },
    Season { stype: String, id: String, season: i64 },
    Episode { stype: String, id: String, video: String },
}

fn stype_letter(stype: &str) -> Option<char> {
    match stype {
        "movie" => Some('m'),
        "series" => Some('s'),
        "anime" => Some('a'),
        _ => None,
    }
}

fn letter_stype(c: char) -> Option<&'static str> {
    match c {
        'm' => Some("movie"),
        's' => Some("series"),
        'a' => Some("anime"),
        _ => None,
    }
}

/// Path-safe escaping: letters, digits, `.` and `-` stay; every other byte is `_` and two hex
/// digits. `~` is never produced, so it can separate an id from what follows it.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'.' || b == b'-' {
            out.push(b as char);
        } else {
            out.push_str(&format!("_{b:02X}"));
        }
    }
    out
}

fn unescape(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'_' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

impl Rk {
    /// The address of a title of Stremio type `stype`; `None` for a type the app has no screen for.
    pub fn item(stype: &str, id: &str) -> Option<Rk> {
        stype_letter(stype)?;
        let kind = if stype == "movie" { Kind::Movie } else { Kind::Show };
        Some(Rk::Item { kind, stype: stype.to_string(), id: id.to_string() })
    }

    pub fn encode(&self) -> String {
        match self {
            Rk::Item { kind, stype, id } => {
                let k = if *kind == Kind::Movie { 'm' } else { 's' };
                format!("{k}{}{}", stype_letter(stype).unwrap_or('s'), escape(id))
            }
            Rk::Season { stype, id, season } => {
                format!("z{}{}~{season}", stype_letter(stype).unwrap_or('s'), escape(id))
            }
            Rk::Episode { stype, id, video } => {
                format!("e{}{}~{}", stype_letter(stype).unwrap_or('s'), escape(id), escape(video))
            }
        }
    }

    pub fn decode(rk: &str) -> Option<Rk> {
        let mut chars = rk.chars();
        let what = chars.next()?;
        let stype = letter_stype(chars.next()?)?.to_string();
        let rest = chars.as_str();
        match what {
            'm' | 's' => {
                let kind = if what == 'm' { Kind::Movie } else { Kind::Show };
                Some(Rk::Item { kind, stype, id: unescape(rest)? })
            }
            'z' => {
                let (id, season) = rest.split_once('~')?;
                Some(Rk::Season { stype, id: unescape(id)?, season: season.parse().ok()? })
            }
            'e' => {
                let (id, video) = rest.split_once('~')?;
                Some(Rk::Episode { stype, id: unescape(id)?, video: unescape(video)? })
            }
            _ => None,
        }
    }

    /// The title this address belongs to: itself, or the show of a season or episode.
    pub fn show(&self) -> (&str, &str) {
        match self {
            Rk::Item { stype, id, .. } | Rk::Season { stype, id, .. } | Rk::Episode { stype, id, .. } => {
                (stype, id)
            }
        }
    }
}

/// Is this an anime title? Kitsu, MyAnimeList, AniList and AniDB ids are anime by construction,
/// and so is Stremio's own `anime` type.
pub fn is_anime(stype: &str, id: &str) -> bool {
    stype == "anime" || ["kitsu:", "mal:", "anilist:", "anidb:"].iter().any(|p| id.starts_with(p))
}

/// Artwork stand-ins: the app asks the media server for `/stremio/art/{key}` and the bridge fetches
/// the remote image the key names.
///
/// **The key IS the address** whenever it fits: `~` and the URL in base64url (`https://` left
/// implied). A table this process keeps did not survive a restart, while the app's saved Home and
/// its on-disk image cache did: every card painted from them asked for a key the new process had
/// never seen, got a 404 — which the poster store treats as final — and stayed black until the
/// shelf was fetched again. An address too long for the app's 255-byte request key (an RPDB poster
/// carrying its own API key, say) still gets a short hashed stand-in from the table, as before.
#[derive(Default)]
pub struct ArtMap {
    urls: Mutex<HashMap<String, String>>,
}

impl ArtMap {
    const LIMIT: usize = 20_000;
    /// The longest self-describing key: what leaves room in the app's request key for the
    /// transcoder's own fields and the account key.
    const SELF_MAX: usize = 90;

    /// The app-facing path for `url`.
    pub fn path(&self, url: &str) -> String {
        let key = match url.strip_prefix("https://") {
            Some(rest) => format!("~{}", b64url(rest.as_bytes())),
            None => format!("~~{}", b64url(url.as_bytes())),
        };
        if key.len() <= Self::SELF_MAX {
            return format!("/stremio/art/{key}");
        }
        // FNV-1a: stable across launches, so the app's on-disk image cache keeps hitting.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in url.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        let key = format!("{h:016x}");
        let mut urls = self.urls.lock().unwrap_or_else(|e| e.into_inner());
        if urls.len() >= Self::LIMIT {
            urls.clear();
        }
        urls.insert(key.clone(), url.to_string());
        format!("/stremio/art/{key}")
    }

    /// The image address a key names: only `http`/`https` addresses are fetched.
    pub fn url(&self, key: &str) -> Option<String> {
        let url = if let Some(full) = key.strip_prefix("~~") {
            String::from_utf8(unb64url(full)?).ok()?
        } else if let Some(rest) = key.strip_prefix('~') {
            format!("https://{}", String::from_utf8(unb64url(rest)?).ok()?)
        } else {
            self.urls.lock().unwrap_or_else(|e| e.into_inner()).get(key).cloned()?
        };
        (url.starts_with("https://") || url.starts_with("http://")).then_some(url)
    }
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Unpadded base64url — path-safe, so a key needs no escaping in a URL or a query.
fn b64url(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 4 / 3 + 2);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// The bytes back from [`b64url`]; `None` for anything it could not have written.
fn unb64url(s: &str) -> Option<Vec<u8>> {
    if s.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for chunk in s.as_bytes().chunks(4) {
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            let v = B64.iter().position(|b| b == c)? as u32;
            n |= v << (18 - 6 * i);
        }
        for i in 0..chunk.len() - 1 {
            out.push((n >> (16 - 8 * i)) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod art_tests {
    use super::*;

    #[test]
    fn an_art_key_names_its_address_and_survives_a_restart() {
        for url in ["https://images.metahub.space/poster/medium/tt0111161/img", "http://a.b/c?d=1&e=ü", "https://x/y.jpg"] {
            let path = ArtMap::default().path(url);
            let key = path.strip_prefix("/stremio/art/").unwrap();
            assert!(key.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_~".contains(&b)), "{key}");
            // a fresh map (a new process) still resolves it
            assert_eq!(ArtMap::default().url(key).as_deref(), Some(url));
        }
        let long = format!("https://api.ratingposterdb.com/{}/imdb/poster-default/tt0111161.jpg", "k".repeat(90));
        let map = ArtMap::default();
        let path = map.path(&long);
        assert!(path.len() < 40, "a long address gets a short stand-in: {path}");
        assert_eq!(map.url(path.strip_prefix("/stremio/art/").unwrap()).as_deref(), Some(long.as_str()));
        assert_eq!(map.url(&format!("~~{}", b64url(b"file:///etc/passwd"))), None, "only web addresses are fetched");
        assert_eq!(map.url("~!!"), None);
        assert_eq!(map.url("0123456789abcdef"), None, "a stand-in this process never made");
    }
}

// ---------------------------------------------------------------- forgiving field readers

fn text(v: &Value, key: &str) -> Option<String> {
    match v.get(key)? {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn number(v: &Value, key: &str) -> Option<f64> {
    match v.get(key)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn strings(v: &Value, key: &str) -> Vec<String> {
    match v.get(key) {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|x| match x {
                Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
                // Newer metas carry people as objects: `{"name": …}`.
                Value::Object(_) => text(x, "name"),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The first four-digit year in a text like `2010–2015` or `2019-03-14T…`.
fn year_in(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    (0..b.len().saturating_sub(3)).find_map(|i| {
        let w = &b[i..i + 4];
        (w.iter().all(u8::is_ascii_digit) && (w[0] == b'1' || w[0] == b'2'))
            .then(|| std::str::from_utf8(w).ok()?.parse().ok())
            .flatten()
    })
}

/// `YYYY-MM-DD` from an ISO date-time.
fn date_in(s: &str) -> Option<String> {
    let d = s.get(..10)?;
    let ok = d.len() == 10 && d.as_bytes()[4] == b'-' && d.as_bytes()[7] == b'-';
    ok.then(|| d.to_string())
}

/// Minutes from a runtime like `"142 min"`, `"2h 22min"` or a bare number.
fn minutes_in(s: &str) -> Option<i64> {
    let lower = s.to_ascii_lowercase();
    let mut total = 0i64;
    let mut num = String::new();
    let mut any = false;
    for c in lower.chars() {
        if c.is_ascii_digit() {
            num.push(c);
        } else if !num.is_empty() {
            let n: i64 = num.parse().ok()?;
            total += if c == 'h' { n * 60 } else { n };
            any = true;
            num.clear();
        }
    }
    if !num.is_empty() {
        total += num.parse::<i64>().ok()?;
        any = true;
    }
    (any && total > 0).then_some(total)
}

/// One title, read from a catalog row or a full meta.
#[derive(Debug, Clone, PartialEq)]
pub struct Title {
    pub rk: Rk,
    pub stype: String,
    pub id: String,
    pub name: String,
    pub poster: Option<String>,
    pub background: Option<String>,
    pub description: Option<String>,
    pub year: Option<i64>,
    pub released: Option<String>,
    pub rating: Option<f64>,
    pub minutes: Option<i64>,
    pub genres: Vec<String>,
    pub cast: Vec<String>,
    pub directors: Vec<String>,
    pub writers: Vec<String>,
    pub videos: Vec<Episode>,
}

/// One episode of a show.
#[derive(Debug, Clone, PartialEq)]
pub struct Episode {
    pub video: String,
    pub season: i64,
    pub episode: i64,
    pub name: Option<String>,
    pub overview: Option<String>,
    pub released: Option<String>,
    pub thumbnail: Option<String>,
}

impl Title {
    /// A title from a catalog row or meta object. `fallback` is the type of the catalog it came
    /// from, used when the row does not say its own. `None` for a row with no id or name, or of a
    /// type the app has no screen for.
    pub fn read(v: &Value, fallback: &str) -> Option<Title> {
        let id = text(v, "id")?;
        let stype = text(v, "type").unwrap_or_else(|| fallback.to_string());
        let rk = Rk::item(&stype, &id)?;
        let name = text(v, "name")?;
        let released = text(v, "released").as_deref().and_then(date_in);
        let year = number(v, "year")
            .map(|y| y as i64)
            .or_else(|| text(v, "releaseInfo").as_deref().and_then(year_in))
            .or_else(|| released.as_deref().and_then(year_in));
        let videos = match v.get("videos") {
            Some(Value::Array(a)) => a.iter().filter_map(Episode::read).collect(),
            _ => Vec::new(),
        };
        Some(Title {
            rk,
            stype,
            id,
            name,
            poster: text(v, "poster"),
            background: text(v, "background"),
            description: text(v, "description"),
            year,
            released,
            rating: number(v, "imdbRating").filter(|r| *r > 0.0),
            minutes: text(v, "runtime").as_deref().and_then(minutes_in),
            genres: strings(v, "genres"),
            cast: strings(v, "cast"),
            directors: strings(v, "director"),
            writers: strings(v, "writer"),
            videos,
        })
    }

    /// The seasons this show has, in order. Season 0 (specials) last, as Stremio shows them.
    pub fn seasons(&self) -> Vec<i64> {
        let mut seasons: Vec<i64> = self.videos.iter().map(|v| v.season).collect();
        seasons.sort_by_key(|s| if *s == 0 { i64::MAX } else { *s });
        seasons.dedup();
        seasons
    }

    pub fn episodes(&self, season: i64) -> Vec<&Episode> {
        let mut eps: Vec<&Episode> = self.videos.iter().filter(|v| v.season == season).collect();
        eps.sort_by_key(|v| v.episode);
        eps
    }
}

impl Episode {
    fn read(v: &Value) -> Option<Episode> {
        let video = text(v, "id")?;
        // Kitsu numbers episodes without seasons; they are one season, numbered 1.
        let season = number(v, "season").map(|s| s as i64).unwrap_or(1);
        let episode = number(v, "episode").or_else(|| number(v, "number")).map(|e| e as i64)?;
        Some(Episode {
            video,
            season,
            episode,
            name: text(v, "name").or_else(|| text(v, "title")),
            overview: text(v, "overview").or_else(|| text(v, "description")),
            released: text(v, "released").as_deref().and_then(date_in),
            thumbnail: text(v, "thumbnail"),
        })
    }
}

// ---------------------------------------------------------------- the media server's rows

/// Where a row says it lives: the library section and its title.
#[derive(Debug, Clone, Copy)]
pub struct Shelf<'a> {
    pub section: i64,
    pub section_title: &'a str,
}

/// A person's id on the bridge: a stable number from their name (the app opens a person page by
/// it, `/library/people/{id}/media`). Positive and within a JavaScript-safe integer.
///
/// Deliberately NO `tagKey`: the app sends a person's `tagKey` to plex.tv's metadata provider for
/// a biography, with the account key it holds — which here is the Stremio key, and must never
/// leave for plex.tv.
pub fn person_id(name: &str) -> i64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.trim().to_lowercase().bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    ((h >> 16) & 0x3fff_ffff_ffff) as i64 | 1
}

fn tags(names: &[String]) -> Value {
    Value::Array(names.iter().take(20).map(|n| json!({"tag": n, "id": person_id(n)})).collect())
}

fn insert(row: &mut Value, key: &str, value: Option<Value>) {
    if let (Some(obj), Some(value)) = (row.as_object_mut(), value) {
        obj.insert(key.to_string(), value);
    }
}

/// A part id for `rk`: the media server's parts are numbers, and the app puts one in
/// `/library/parts/{id}` when it selects a track. Stable for the same item.
pub fn part_id(rk: &str) -> i64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in rk.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    // Positive, and within a JavaScript-safe integer, as PMS ids are.
    (h >> 12) as i64
}

/// A title as a library row (film or show).
pub fn title_row(t: &Title, shelf: Shelf<'_>, art: &ArtMap) -> Value {
    let rk = t.rk.encode();
    let kind = if let Rk::Item { kind, .. } = &t.rk { *kind } else { Kind::Show };
    let mut row = json!({
        "ratingKey": rk, "key": format!("/library/metadata/{rk}"),
        "guid": format!("stremio://{}/{}", t.stype, t.id),
        "type": kind.wire(), "title": t.name, "titleSort": t.name,
        "librarySectionID": shelf.section, "librarySectionTitle": shelf.section_title,
        "librarySectionKey": format!("/library/sections/{}", shelf.section),
        "addedAt": 0, "updatedAt": 0,
    });
    insert(&mut row, "summary", t.description.as_ref().map(|d| json!(d)));
    insert(&mut row, "year", t.year.map(|y| json!(y)));
    insert(&mut row, "originallyAvailableAt", t.released.as_ref().map(|d| json!(d)));
    insert(&mut row, "audienceRating", t.rating.map(|r| json!(r)));
    insert(&mut row, "audienceRatingImage", t.rating.map(|_| json!("imdb://image.rating")));
    insert(&mut row, "Rating", t.rating.map(|r| json!([{"image": "imdb://image.rating", "value": r, "type": "audience"}])));
    insert(&mut row, "duration", t.minutes.map(|m| json!(m * 60_000)));
    insert(&mut row, "thumb", t.poster.as_deref().map(|u| json!(art.path(u))));
    insert(&mut row, "art", t.background.as_deref().or(t.poster.as_deref()).map(|u| json!(art.path(u))));
    if !t.genres.is_empty() {
        insert(&mut row, "Genre", Some(tags(&t.genres)));
    }
    if kind == Kind::Show && !t.videos.is_empty() {
        insert(&mut row, "childCount", Some(json!(t.seasons().len())));
        insert(&mut row, "leafCount", Some(json!(t.videos.len())));
        insert(&mut row, "viewedLeafCount", Some(json!(0)));
    }
    // A film is playable from EVERY row it appears in, not only its own page: the film page keeps
    // the row it was opened from (a shelf's, a search result's) and Play starts THAT row's part,
    // so a shelf row without one made Play do nothing at all. The stream is picked when it opens.
    if kind == Kind::Movie {
        insert(&mut row, "Media", Some(placeholder_media(&rk, t.minutes)));
    }
    row
}

/// The part URL the player opens for item `rk`: the bridge's play route, spelled the way a media
/// server spells a file — `/library/parts/{id}/{rk}/file.mkv`.
///
/// **The shape is load-bearing, not cosmetic.** The player direct-plays a part only when its key
/// ends in `.mkv`, `.mp4` or `.m4v` (anything else is sent to the server to be repackaged, which
/// there is no server here to do), and it reads the part id — the one track choices are PUT to —
/// from the segment after `parts`. A key that broke either rule played nothing.
pub fn part_key(part: i64, rk: &str, container: &str) -> String {
    let ext = if container == "mp4" { "mp4" } else { "mkv" };
    format!("/library/parts/{part}/{rk}/file.{ext}")
}

/// The playable stand-in a film or episode carries until a stream is chosen: one version whose
/// part is the bridge's play route. The stream behind it is picked when it is opened.
pub fn placeholder_media(rk: &str, minutes: Option<i64>) -> Value {
    let part = part_id(rk);
    json!([{
        "id": part, "container": "mkv", "videoCodec": "h264", "audioCodec": "aac",
        "videoResolution": "1080", "duration": minutes.unwrap_or(0) * 60_000,
        "Part": [{"id": part, "key": part_key(part, rk, "mkv"), "container": "mkv",
                  "duration": minutes.unwrap_or(0) * 60_000, "Stream": []}],
    }])
}

/// A title's full detail row: the library row (a film's carries its playable stand-in) plus people.
pub fn detail_row(t: &Title, shelf: Shelf<'_>, art: &ArtMap) -> Value {
    let mut row = title_row(t, shelf, art);
    insert(&mut row, "Role", (!t.cast.is_empty()).then(|| tags(&t.cast)));
    insert(&mut row, "Director", (!t.directors.is_empty()).then(|| tags(&t.directors)));
    insert(&mut row, "Writer", (!t.writers.is_empty()).then(|| tags(&t.writers)));
    row
}

/// The season rows of a show.
pub fn season_rows(t: &Title, shelf: Shelf<'_>, art: &ArtMap) -> Vec<Value> {
    let show_rk = t.rk.encode();
    let poster = t.poster.as_deref().map(|u| art.path(u));
    t.seasons()
        .into_iter()
        .map(|n| {
            let rk = Rk::Season { stype: t.stype.clone(), id: t.id.clone(), season: n }.encode();
            let count = t.episodes(n).len();
            let title = if n == 0 { "Specials".to_string() } else { format!("Season {n}") };
            let mut row = json!({
                "ratingKey": rk, "key": format!("/library/metadata/{rk}"), "type": "season",
                "title": title, "index": n, "parentRatingKey": show_rk,
                "parentKey": format!("/library/metadata/{show_rk}"), "parentTitle": t.name,
                "librarySectionID": shelf.section, "librarySectionTitle": shelf.section_title,
                "leafCount": count, "viewedLeafCount": 0,
            });
            insert(&mut row, "thumb", poster.clone().map(Value::String));
            insert(&mut row, "parentThumb", poster.clone().map(Value::String));
            row
        })
        .collect()
}

/// One episode row of a show.
pub fn episode_row(t: &Title, e: &Episode, shelf: Shelf<'_>, art: &ArtMap, playable: bool) -> Value {
    let show_rk = t.rk.encode();
    let season_rk = Rk::Season { stype: t.stype.clone(), id: t.id.clone(), season: e.season }.encode();
    let rk = Rk::Episode { stype: t.stype.clone(), id: t.id.clone(), video: e.video.clone() }.encode();
    let title = e.name.clone().unwrap_or_else(|| format!("Episode {}", e.episode));
    let season_title = if e.season == 0 { "Specials".to_string() } else { format!("Season {}", e.season) };
    let mut row = json!({
        "ratingKey": rk, "key": format!("/library/metadata/{rk}"), "type": "episode",
        "title": title, "index": e.episode, "parentIndex": e.season,
        "parentRatingKey": season_rk, "parentKey": format!("/library/metadata/{season_rk}"),
        "parentTitle": season_title,
        "grandparentRatingKey": show_rk, "grandparentKey": format!("/library/metadata/{show_rk}"),
        "grandparentTitle": t.name,
        "librarySectionID": shelf.section, "librarySectionTitle": shelf.section_title,
    });
    insert(&mut row, "summary", e.overview.as_ref().map(|s| json!(s)));
    insert(&mut row, "originallyAvailableAt", e.released.as_ref().map(|s| json!(s)));
    insert(&mut row, "year", e.released.as_deref().and_then(year_in).map(|y| json!(y)));
    insert(&mut row, "thumb", e.thumbnail.as_deref().or(t.background.as_deref()).map(|u| json!(art.path(u))));
    insert(&mut row, "grandparentThumb", t.poster.as_deref().map(|u| json!(art.path(u))));
    insert(&mut row, "grandparentArt", t.background.as_deref().map(|u| json!(art.path(u))));
    insert(&mut row, "art", t.background.as_deref().map(|u| json!(art.path(u))));
    insert(&mut row, "duration", t.minutes.map(|m| json!(m * 60_000)));
    if playable {
        insert(&mut row, "Media", Some(placeholder_media(&rk, t.minutes)));
    }
    row
}

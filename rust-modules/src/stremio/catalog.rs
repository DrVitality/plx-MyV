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
//! as `/stremio/art/{hash}`, a short stand-in [`ArtMap`] resolves back to the URL when the image
//! is fetched.

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

/// Short stand-ins for artwork URLs. Bounded: past [`ArtMap::LIMIT`] it starts over, and a stand-in
/// it forgot is a missing image until the row naming it is fetched again.
#[derive(Default)]
pub struct ArtMap {
    urls: Mutex<HashMap<String, String>>,
}

impl ArtMap {
    const LIMIT: usize = 20_000;

    /// The app-facing path for `url`, remembered so [`ArtMap::url`] can resolve it.
    pub fn path(&self, url: &str) -> String {
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

    pub fn url(&self, key: &str) -> Option<String> {
        self.urls.lock().unwrap_or_else(|e| e.into_inner()).get(key).cloned()
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

fn tags(names: &[String]) -> Value {
    Value::Array(names.iter().take(20).map(|n| json!({"tag": n})).collect())
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
    insert(&mut row, "rating", t.rating.map(|r| json!(r)));
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
    row
}

/// The playable stand-in a film or episode carries until a stream is chosen: one version whose
/// part is the bridge's play route. The stream behind it is picked when it is opened.
pub fn placeholder_media(rk: &str, minutes: Option<i64>) -> Value {
    let part = part_id(rk);
    json!([{
        "id": part, "container": "mkv", "videoCodec": "h264", "audioCodec": "aac",
        "videoResolution": "1080", "duration": minutes.unwrap_or(0) * 60_000,
        "Part": [{"id": part, "key": format!("/stremio/play/{rk}"), "container": "mkv",
                  "duration": minutes.unwrap_or(0) * 60_000, "Stream": []}],
    }])
}

/// A title's full detail row: the library row plus people, and for a film its playable stand-in.
pub fn detail_row(t: &Title, shelf: Shelf<'_>, art: &ArtMap) -> Value {
    let mut row = title_row(t, shelf, art);
    insert(&mut row, "Role", (!t.cast.is_empty()).then(|| tags(&t.cast)));
    insert(&mut row, "Director", (!t.directors.is_empty()).then(|| tags(&t.directors)));
    insert(&mut row, "Writer", (!t.writers.is_empty()).then(|| tags(&t.writers)));
    if matches!(t.rk, Rk::Item { kind: Kind::Movie, .. }) {
        insert(&mut row, "Media", Some(placeholder_media(&t.rk.encode(), t.minutes)));
    }
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

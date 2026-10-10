//! Playback: which stream a film or episode plays, the version the player configures itself from,
//! the bytes, the play queue, track choices, subtitles, and watch progress back to Stremio.
//!
//! **Resolving.** The first time an item is asked for as a single item (its page, or the player
//! fetching what it is about to play), its streams are ranked (`super::super::streams`), the best
//! ready one is chosen, and the first bytes of that file are read for its tracks
//! (`super::super::probe`). The result is kept for a few hours, so the page, the player's
//! decision, its play queue and every seek all agree on one file. The part URL the player opens,
//! `/library/parts/{id}/{rk}/file.mkv` (`catalog::part_key`), answers with a redirect to that stream; the player's own transport follows
//! it (and the debrid service's next redirect) over HTTPS.
//!
//! **Choosing another stream.** `/stremio/choose/{rk}?index=N` (the stream list's order) replaces
//! the choice; the next resolve starts from it.
//!
//! **Progress.** `/:/timeline` reports become Stremio library writes — at most one a minute while
//! playing, and one on every pause and stop — so Continue Watching here and in Stremio's own apps
//! are the same list. Finishing (90 %) or `/:/scrobble` marks the item watched.

use super::super::catalog::{self, Rk, Title};
use super::super::http::{Request, Response};
use super::super::library::Lib;
use super::super::media::{self, ExternalSub};
use super::super::probe::{self, Parse, Probe};
use super::super::streams::{best, Facts, Stream};
use super::{container, Bridge};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a resolved choice is reused. Debrid links stay valid for hours; a fresh resolve after
/// that picks up a changed cache.
const RESOLVED_TTL: Duration = Duration::from_secs(3 * 3600);
/// The first window read for a file's tracks, and the follow-up window when they lie further in.
const PROBE_HEAD: u64 = 2 * 1024 * 1024;
const PROBE_MORE: u64 = 8 * 1024 * 1024;
/// How often progress is written to Stremio while playing.
const PROGRESS_EVERY: Duration = Duration::from_secs(60);
/// The share of a title past which it counts as watched.
const WATCHED_AT: f64 = 0.9;
/// How many episodes either side of the one playing a play queue carries — the whole show for
/// almost every series, and a bounded window for a long-running anime.
const QUEUE_AROUND: usize = 150;

/// One item's chosen stream and the version built from it.
pub(super) struct Resolved {
    pub streams: Vec<(Stream, Facts, i64)>,
    pub chosen: Option<usize>,
    /// The `Media` array the page and the player read; empty when nothing can play.
    pub media: Value,
    at: Instant,
}

/// Everything playback keeps between requests.
pub(super) struct PlayState {
    resolved: Mutex<HashMap<String, Arc<Resolved>>>,
    /// A viewer's stream choice, by item: an index into the ranked list.
    choice: Mutex<HashMap<String, usize>>,
    /// Part id → item, so a track choice (`/library/parts/{id}`) finds its item.
    parts: Mutex<HashMap<i64, String>>,
    /// Part id → (audio stream id, subtitle stream id); 0 for subtitles means off.
    tracks: Mutex<HashMap<i64, (Option<i64>, Option<i64>)>>,
    /// External subtitle id → its URL.
    pub(super) subs: Mutex<HashMap<i64, String>>,
    /// Item → when progress was last written.
    written: Mutex<HashMap<String, Instant>>,
    /// The last play queue built, for `/playQueues/{id}`.
    queue: Mutex<Option<Value>>,
    /// Subtitle search candidates: id → (item, language, URL). Replaced by each search of the item.
    pub(super) candidates: Mutex<HashMap<i64, (String, String, String)>>,
    /// The next candidate id: far from every part-derived stream id.
    pub(super) next_candidate: std::sync::atomic::AtomicI64,
    /// Subtitles installed from a search, by item, for the rest of the run.
    pub(super) installed: Mutex<HashMap<String, Vec<ExternalSub>>>,
}

impl Default for PlayState {
    fn default() -> Self {
        PlayState {
            resolved: Mutex::default(),
            choice: Mutex::default(),
            parts: Mutex::default(),
            tracks: Mutex::default(),
            subs: Mutex::default(),
            written: Mutex::default(),
            queue: Mutex::default(),
            candidates: Mutex::default(),
            next_candidate: std::sync::atomic::AtomicI64::new(9_000_000_000),
            installed: Mutex::default(),
        }
    }
}

/// Streams tried at most before the first pick stands.
const FALLBACK_TRIES: usize = 3;

/// Could this stream start at once, as a fallback? Cached or direct, and not a camera copy.
fn fallback_ok(f: &Facts) -> bool {
    f.ready != super::super::streams::Ready::Uncached && !f.cam && !f.av1
}

/// The stream most like `m` among those that could start at once: the same release group and
/// resolution, else the same resolution from the same addon.
pub(super) fn like_last(m: &super::config::StreamMemory, streams: &[(Stream, Facts, i64)]) -> Option<usize> {
    let group_of = |s: &Stream| super::config::release_group(s.filename.as_deref().unwrap_or(s.detail.as_str()));
    let ok: Vec<usize> = (0..streams.len()).filter(|i| fallback_ok(&streams[*i].1)).collect();
    if !m.group.is_empty() {
        if let Some(i) = ok.iter().copied().find(|i| {
            streams[*i].1.resolution == m.resolution && group_of(&streams[*i].0).eq_ignore_ascii_case(&m.group)
        }) {
            return Some(i);
        }
    }
    ok.into_iter().find(|i| streams[*i].1.resolution == m.resolution && streams[*i].0.addon == m.addon)
}

/// The stand-in final credits marker for an episode `duration_ms` long: its last 3 %, at least
/// 45 seconds and at most two and a half minutes.
pub(super) fn credits_marker(duration_ms: i64) -> Value {
    let span = (duration_ms * 3 / 100).clamp(45_000, 150_000).min(duration_ms / 2);
    json!({"type": "credits", "startTimeOffset": duration_ms - span, "endTimeOffset": duration_ms, "final": true})
}

/// "Now", in the ISO form Stremio's library stores.
pub(super) fn now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, m, d) = plx_net::net::civil_date(secs);
    let day = secs.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.000Z", day / 3600, day % 3600 / 60, day % 60)
}

/// The item a `/library/metadata/{rk}` path (or a `server://…/library/metadata/{rk}` URI) names.
fn rk_in(path: &str) -> Option<&str> {
    path.rsplit_once("/library/metadata/").map(|(_, rk)| rk.split(['/', '?', '&']).next().unwrap_or(rk))
}

/// Applies a stored track choice to a version's streams.
fn apply_tracks(media: &mut Value, audio: Option<i64>, sub: Option<i64>) {
    let Some(streams) = media.pointer_mut("/0/Part/0/Stream").and_then(Value::as_array_mut) else { return };
    let mut audio_codec = None;
    for s in streams.iter_mut() {
        let id = s.get("id").and_then(Value::as_i64).unwrap_or(-1);
        match s.get("streamType").and_then(Value::as_i64) {
            Some(2) if audio.is_some() => {
                let on = Some(id) == audio;
                if on {
                    audio_codec = s.get("codec").cloned();
                }
                s["selected"] = json!(on);
            }
            Some(3) if sub.is_some() => s["selected"] = json!(Some(id) == sub),
            _ => {}
        }
    }
    if let Some(codec) = audio_codec {
        media[0]["audioCodec"] = codec;
    }
}

impl Bridge {
    /// Reads a file's tracks from its first bytes, following the parser to where they are.
    /// `Err` when the link answered nothing at all (dead, refused, not cached after all): the
    /// stream cannot play, which is what makes the next one worth trying. `Ok(None)` is a file
    /// that answered but whose tracks could not be read, which still plays.
    fn probe_url(&self, url: &str) -> Result<Option<Probe>, ()> {
        let head = self.stremio.fetch_range(url, 0, PROBE_HEAD).map_err(|_| ())?;
        if head.is_empty() {
            return Err(());
        }
        Ok(match probe::parse(&head, None) {
            Parse::Done(p) => Some(p),
            Parse::Need(at) => {
                let more = self.stremio.fetch_range(url, at, PROBE_MORE).ok();
                match more.map(|more| probe::parse(&head, Some((at, &more)))) {
                    Some(Parse::Done(p)) => Some(p),
                    _ => None,
                }
            }
            Parse::Unknown => None,
        })
    }

    /// The resolved choice for a film or episode, from the memo when fresh.
    pub(super) fn resolved(&self, token: &str, rk: &Rk, rk_s: &str, minutes: Option<i64>) -> Option<Arc<Resolved>> {
        if matches!(rk, Rk::Season { .. }) || matches!(rk, Rk::Item { kind: catalog::Kind::Show, .. }) {
            return None;
        }
        {
            let memo = self.play.resolved.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(r) = memo.get(rk_s).filter(|r| r.at.elapsed() < RESOLVED_TTL) {
                return Some(r.clone());
            }
        }
        let streams = self.ranked_streams(token, rk).ok()?;
        let first = self.chosen_index(rk_s, &streams);
        // The first pick, then the next streams that could start at once, until one answers: a
        // dead or refused link moves on rather than failing the play. When none answers the
        // first pick stands, and the player reports what it finds.
        let mut chosen = first;
        let mut probed = None;
        let candidates = first.into_iter().chain(
            (0..streams.len()).filter(|i| Some(*i) != first && fallback_ok(&streams[*i].1)));
        for (tried, i) in candidates.enumerate() {
            if tried >= FALLBACK_TRIES {
                break;
            }
            let Some(url) = streams[i].0.url.as_deref() else { continue };
            match self.probe_url(url) {
                Ok(p) => {
                    if tried > 0 {
                        plx_base::eventlog::log(&format!("stremio: stream {} answered after {tried} that did not", i));
                    }
                    chosen = Some(i);
                    probed = p;
                    break;
                }
                Err(()) => plx_base::eventlog::log(&format!("stremio: stream {i} did not answer; trying the next")),
            }
        }
        if let (Some(i), Rk::Episode { id, .. }) = (chosen, rk) {
            self.remember_stream(id, &streams[i].0, &streams[i].1);
        }
        let part = catalog::part_id(rk_s);
        let media = match chosen.map(|i| &streams[i]) {
            Some((s, f, _)) => {
                let (stype, video) = match rk {
                    Rk::Item { stype, id, .. } => (stype.as_str(), id.as_str()),
                    Rk::Episode { stype, video, .. } => (stype.as_str(), video.as_str()),
                    Rk::Season { .. } => return None,
                };
                match probed {
                    Some(p) => {
                        // outside subtitles only for a file with none in the viewer's language
                        let wanted = self.subtitle_language();
                        let has_own = p.tracks.iter().any(|t| t.kind == super::super::probe::TrackKind::Subtitle
                            && super::subs::two_letter(&t.language).as_deref() == Some(wanted.as_str()));
                        let ext = if has_own { Vec::new() } else { self.external_subs(token, stype, video, part) };
                        plx_base::eventlog::log(&format!("stremio: playing {} ({}) — {} tracks read",
                            s.addon, f.resolution, p.tracks.len()));
                        media::media_from_probe(rk_s, part, &p, s.size, &ext)
                    }
                    None => {
                        plx_base::eventlog::log(&format!(
                            "stremio: playing {} ({}) — tracks unreadable, using the release name", s.addon, f.resolution));
                        let ext = self.external_subs(token, stype, video, part);
                        media::media_from_facts(rk_s, part, f, &s.detail, minutes, s.size, &ext)
                    }
                }
            }
            None => json!([]),
        };
        let mut media = media;
        // subtitles installed from a search, kept across re-reads of the item
        let installed = self.play.installed.lock().unwrap_or_else(|e| e.into_inner()).get(rk_s).cloned().unwrap_or_default();
        for sub in &installed {
            media::add_external(&mut media, sub);
        }
        if let Some((a, sub)) = self.play.tracks.lock().unwrap_or_else(|e| e.into_inner()).get(&part).copied() {
            apply_tracks(&mut media, a, sub);
        }
        self.play.parts.lock().unwrap_or_else(|e| e.into_inner()).insert(part, rk_s.to_string());
        let r = Arc::new(Resolved { streams, chosen, media, at: Instant::now() });
        let mut memo = self.play.resolved.lock().unwrap_or_else(|e| e.into_inner());
        if memo.len() >= 64 {
            memo.retain(|_, r| r.at.elapsed() < RESOLVED_TTL);
            if memo.len() >= 64 {
                memo.clear();
            }
        }
        memo.insert(rk_s.to_string(), r.clone());
        Some(r)
    }

    /// Which of `streams` plays for `rk_s`: the viewer's choice while it names one of them; for
    /// an episode, the stream most like the one the show last played from (same release group
    /// and resolution, else same resolution and addon); else the best.
    pub(super) fn chosen_index(&self, rk_s: &str, streams: &[(Stream, Facts, i64)]) -> Option<usize> {
        let wanted = self.play.choice.lock().unwrap_or_else(|e| e.into_inner()).get(rk_s).copied();
        if let Some(i) = wanted.filter(|i| *i < streams.len()) {
            return Some(i);
        }
        if let Some(Rk::Episode { id, .. }) = Rk::decode(rk_s) {
            let memory = self.with_config(|c| c.shows.get(&id).cloned());
            if let Some(i) = memory.and_then(|m| like_last(&m, streams)) {
                return Some(i);
            }
        }
        best(streams)
    }

    /// Keep what a show's episode is playing from, for the next one (written only on a change).
    fn remember_stream(&self, show_id: &str, s: &Stream, f: &Facts) {
        let name = s.filename.as_deref().unwrap_or(s.detail.as_str());
        let memory = super::config::StreamMemory {
            group: super::config::release_group(name),
            resolution: f.resolution,
            addon: s.addon.clone(),
        };
        if self.with_config(|c| c.shows.get(show_id) == Some(&memory)) {
            return;
        }
        self.change_config(|c| {
            // bounded: the shows watched most recently are the ones worth remembering
            if c.shows.len() >= 200 && !c.shows.contains_key(show_id) {
                if let Some(k) = c.shows.keys().next().cloned() {
                    c.shows.remove(&k);
                }
            }
            c.shows.insert(show_id.to_string(), memory);
        });
    }

    /// Add an installed subtitle to the item's resolved version in place, so the next read shows
    /// it without resolving the stream again.
    pub(super) fn add_installed_to_memo(&self, rk_s: &str, sub: &ExternalSub) {
        let mut memo = self.play.resolved.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(r) = memo.get(rk_s).cloned() {
            let mut media = r.media.clone();
            media::add_external(&mut media, sub);
            memo.insert(rk_s.to_string(), Arc::new(Resolved {
                streams: r.streams.clone(), chosen: r.chosen, media, at: r.at,
            }));
        }
    }

    /// Forgets a resolved choice, so the next request resolves it again.
    fn forget(&self, rk_s: &str) {
        self.play.resolved.lock().unwrap_or_else(|e| e.into_inner()).remove(rk_s);
    }

    /// Where the account stopped in this item, in milliseconds.
    pub(super) fn resume_offset(&self, token: &str, rk: &Rk) -> Option<i64> {
        let (show_id, video) = match rk {
            Rk::Item { id, .. } => (id.as_str(), None),
            Rk::Episode { id, video, .. } => (id.as_str(), Some(video.as_str())),
            Rk::Season { .. } => return None,
        };
        let items = self.library_items(token);
        let it = items.iter().find(|it| it.get("_id").and_then(Value::as_str) == Some(show_id))?;
        if let Some(v) = video {
            if it.pointer("/state/video_id").and_then(Value::as_str) != Some(v) {
                return None;
            }
        }
        it.pointer("/state/timeOffset").and_then(Value::as_i64).filter(|o| *o > 0)
    }

    /// `/library/parts/{id}/{rk}/file.mkv`: the chosen stream, by redirect.
    pub(super) fn play(&self, token: &str, rk_s: &str) -> Response {
        let Some(rk) = Rk::decode(rk_s) else { return Response::empty(404) };
        let Some(r) = self.resolved(token, &rk, rk_s, None) else { return Response::empty(404) };
        match r.chosen.and_then(|i| r.streams[i].0.url.clone()) {
            Some(url) => Response { status: 302, content_type: "text/plain", body: Vec::new(), location: Some(url) },
            None => Response::empty(404),
        }
    }

    /// `/video/:/transcode/universal/decision`: always direct play — there is no server here to
    /// convert anything — or, when nothing can play, the refusal the player reads out.
    pub(super) fn decision(&self, req: &Request, token: &str) -> Response {
        let Some(rk_s) = req.query("path").and_then(rk_in).map(str::to_string) else { return Response::empty(400) };
        let Some(rk) = Rk::decode(&rk_s) else { return Response::empty(404) };
        let row = match self.metadata_row(token, &rk, &rk_s) {
            Ok(row) => row,
            Err(r) => return r,
        };
        let playable = row.pointer("/Media/0/Part/0/key").is_some();
        if !playable {
            return Response::json(200, &container(json!({
                "generalDecisionCode": 2000, "mdeDecisionCode": 2000, "transcodeDecisionCode": 4000,
                "generalDecisionText": "Nothing to play.",
                "transcodeDecisionText": "None of your stream addons has a cached copy of this title right now.",
                "Metadata": [],
            })));
        }
        let mut row = row;
        if let Some(part) = row.pointer_mut("/Media/0/Part/0") {
            part["decision"] = json!("directplay");
            if let Some(streams) = part.get_mut("Stream").and_then(Value::as_array_mut) {
                for s in streams {
                    s["decision"] = json!("copy");
                }
            }
        }
        Response::json(200, &container(json!({
            "generalDecisionCode": 1000, "generalDecisionText": "Direct play OK",
            "mdeDecisionCode": 1000, "Metadata": [row],
        })))
    }

    /// The single-item row for a film or episode, with its resolved version and resume point.
    pub(super) fn metadata_row(&self, token: &str, rk: &Rk, rk_s: &str) -> Result<Value, Response> {
        let t = self.rk_title_for(token, rk)?;
        let shelf = catalog::Shelf { section: Lib::of_title(&t.stype, &t.id).key(),
                                     section_title: Lib::of_title(&t.stype, &t.id).title() };
        let mut row = match rk {
            Rk::Item { .. } => catalog::detail_row(&t, shelf, &self.art),
            Rk::Episode { video, .. } => match t.videos.iter().find(|e| &e.video == video) {
                Some(e) => catalog::episode_row(&t, e, shelf, &self.art, true),
                None => return Err(Response::empty(404)),
            },
            Rk::Season { .. } => return Err(Response::empty(404)),
        };
        if let Some(r) = self.resolved(token, rk, rk_s, t.minutes) {
            if let Some(obj) = row.as_object_mut() {
                if r.media.as_array().is_some_and(|m| !m.is_empty()) {
                    // The file's own duration, when it said one, beats the listing's runtime.
                    if let Some(ms) = r.media.pointer("/0/duration").and_then(Value::as_i64).filter(|d| *d > 0) {
                        obj.insert("duration".into(), json!(ms));
                    }
                    obj.insert("Media".into(), r.media.clone());
                } else {
                    obj.remove("Media");
                }
            }
        }
        if let (Some(off), Some(obj)) = (self.resume_offset(token, rk), row.as_object_mut()) {
            obj.insert("viewOffset".into(), json!(off));
        }
        // Stremio has no credits markers. An episode's last stretch stands in for them, so the
        // player's Up Next card (which waits for a FINAL credits marker) offers the next episode.
        if let Rk::Episode { video, .. } = rk {
            if let Some(d) = row.get("duration").and_then(Value::as_i64).filter(|d| *d > 0) {
                // an anime episode's real opening and ending, when AniSkip knows them
                let skip = t.videos.iter().find(|e| &e.video == video)
                    .filter(|_| catalog::is_anime(&t.stype, &t.id))
                    .and_then(|e| self.anime_skips(&t.id, e.episode, d));
                let mut markers = skip.unwrap_or_default();
                // Up Next waits for a final credits marker: the stand-in when AniSkip had no ending
                if !markers.iter().any(|m| m.get("type").and_then(Value::as_str) == Some("credits")) {
                    markers.push(credits_marker(d));
                }
                row["Marker"] = json!(markers);
            }
        }
        Ok(row)
    }

    fn rk_title_for(&self, token: &str, rk: &Rk) -> Result<Title, Response> {
        let addons = self.addons(token).map_err(super::api_failure)?;
        let (stype, id) = rk.show();
        self.title(&addons, stype, id).ok_or_else(|| Response::empty(404))
    }

    /// `POST /playQueues?uri=…/library/metadata/{rk}`: the item, then (for an episode, when the
    /// queue is continuous) the episodes after it, so Up Next has somewhere to go.
    pub(super) fn play_queue(&self, req: &Request, token: &str) -> Response {
        let Some(rk_s) = req.query("uri").and_then(rk_in).map(str::to_string) else { return Response::empty(400) };
        let Some(rk) = Rk::decode(&rk_s) else { return Response::empty(404) };
        let row = match self.metadata_row(token, &rk, &rk_s) {
            Ok(row) => row,
            Err(r) => return r,
        };
        // A continuous queue for an episode is the SHOW, around the one playing: Up Next reads the
        // row after it, and the player's Episodes strip lists them all to switch episode or season.
        let mut rows = Vec::new();
        let mut selected = 0;
        if let (Rk::Episode { video, .. }, true) = (&rk, req.query("continuous") == Some("1")) {
            if let Ok(t) = self.rk_title_for(token, &rk) {
                let order: Vec<&catalog::Episode> = t.seasons().into_iter()
                    .filter(|s| *s != 0)
                    .flat_map(|s| t.episodes(s))
                    .collect();
                if let Some(at) = order.iter().position(|e| &e.video == video) {
                    let lib = Lib::of_title(&t.stype, &t.id);
                    let shelf = catalog::Shelf { section: lib.key(), section_title: lib.title() };
                    let from = at.saturating_sub(QUEUE_AROUND);
                    for (i, e) in order.iter().enumerate().skip(from).take(at - from + 1 + QUEUE_AROUND) {
                        if i == at {
                            selected = rows.len();
                            rows.push(row.clone());
                        } else {
                            rows.push(catalog::episode_row(&t, e, shelf, &self.art, true));
                        }
                    }
                }
            }
        }
        if rows.is_empty() {
            rows.push(row);
        }
        for (n, row) in rows.iter_mut().enumerate() {
            row["playQueueItemID"] = json!(n + 1);
        }
        let body = container(json!({
            "playQueueID": 1, "playQueueSelectedItemID": selected + 1, "playQueueSelectedItemOffset": selected,
            "playQueueTotalCount": rows.len(), "playQueueVersion": 1, "size": rows.len(), "Metadata": rows,
        }));
        *self.play.queue.lock().unwrap_or_else(|e| e.into_inner()) = Some(body.clone());
        Response::json(200, &body)
    }

    pub(super) fn play_queue_again(&self) -> Response {
        match self.play.queue.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            Some(body) => Response::json(200, &body),
            None => Response::json(200, &container(json!({}))),
        }
    }

    /// `PUT /library/parts/{id}?audioStreamID=…&subtitleStreamID=…`: a track choice, kept for the
    /// item and applied to its version from now on.
    pub(super) fn select_tracks(&self, req: &Request, part: &str) -> Response {
        let Ok(part) = part.parse::<i64>() else { return Response::empty(404) };
        let audio = req.query("audioStreamID").and_then(|v| v.parse::<i64>().ok());
        let sub = req.query("subtitleStreamID").and_then(|v| v.parse::<i64>().ok());
        {
            let mut tracks = self.play.tracks.lock().unwrap_or_else(|e| e.into_inner());
            let entry = tracks.entry(part).or_insert((None, None));
            if audio.is_some() {
                entry.0 = audio;
            }
            if sub.is_some() {
                entry.1 = sub;
            }
        }
        let item = self.play.parts.lock().unwrap_or_else(|e| e.into_inner()).get(&part).cloned();
        if let Some(rk_s) = item {
            let (a, s) = self.play.tracks.lock().unwrap_or_else(|e| e.into_inner()).get(&part).copied().unwrap_or((None, None));
            let mut memo = self.play.resolved.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(r) = memo.get(&rk_s).cloned() {
                let mut media = r.media.clone();
                apply_tracks(&mut media, a, s);
                memo.insert(rk_s, Arc::new(Resolved { streams: r.streams.clone(), chosen: r.chosen, media, at: r.at }));
            }
        }
        Response::json(200, &container(json!({})))
    }

    /// `/library/streams/{id}`: an external subtitle file.
    pub(super) fn subtitle_file(&self, id: &str) -> Response {
        let url = id.parse::<i64>().ok()
            .and_then(|id| self.play.subs.lock().unwrap_or_else(|e| e.into_inner()).get(&id).cloned());
        let Some(url) = url else { return Response::empty(404) };
        match self.stremio.fetch(&url) {
            Ok(body) => Response { status: 200, content_type: "application/x-subrip", body, location: None },
            Err(_) => Response::empty(502),
        }
    }

    /// `/stremio/choose/{rk}?index=N`: play the N-th stream of the ranked list from now on.
    pub(super) fn choose(&self, req: &Request, rk_s: &str) -> Response {
        let Some(n) = req.query("index").and_then(|v| v.parse::<usize>().ok()) else { return Response::empty(400) };
        self.play.choice.lock().unwrap_or_else(|e| e.into_inner()).insert(rk_s.to_string(), n);
        self.forget(rk_s);
        Response::json(200, &container(json!({"chosen": n})))
    }

    /// `/:/timeline`: a progress report, written to the account's Stremio library.
    pub(super) fn timeline(&self, req: &Request, token: &str) -> Response {
        let rk_s = req.query("ratingKey").map(str::to_string)
            .or_else(|| req.query("key").and_then(rk_in).map(str::to_string));
        let (Some(rk_s), Some(time)) = (rk_s, req.query("time").and_then(|t| t.parse::<i64>().ok())) else {
            return Response::json(200, &container(json!({})));
        };
        let duration = req.query("duration").and_then(|d| d.parse::<i64>().ok()).unwrap_or(0);
        let state = req.query("state").unwrap_or("playing");
        let due = {
            let mut written = self.play.written.lock().unwrap_or_else(|e| e.into_inner());
            let due = state != "playing" || written.get(&rk_s).map_or(true, |at| at.elapsed() >= PROGRESS_EVERY);
            if due {
                written.insert(rk_s.clone(), Instant::now());
            }
            due
        };
        if due {
            if let Some(rk) = Rk::decode(&rk_s) {
                let finished = state == "stopped" && duration > 0 && time as f64 >= duration as f64 * WATCHED_AT;
                self.write_progress(token, &rk, if finished { 0 } else { time }, duration, finished);
            }
        }
        Response::json(200, &container(json!({})))
    }

    /// `/:/scrobble` and `/:/unscrobble`: mark watched, or not.
    pub(super) fn scrobble(&self, req: &Request, token: &str, watched: bool) -> Response {
        if let Some(rk) = req.query("key").and_then(Rk::decode) {
            self.write_progress(token, &rk, 0, 0, watched);
        }
        Response::json(200, &container(json!({})))
    }

    /// Writes one item's progress to the account's library: its own entry when it has one, a
    /// temporary one (as Stremio's apps make for an unsaved title) when not.
    fn write_progress(&self, token: &str, rk: &Rk, offset: i64, duration: i64, watched: bool) {
        let (stype, show_id) = rk.show();
        let video = match rk {
            Rk::Episode { video, .. } => video.clone(),
            _ => show_id.to_string(),
        };
        let now = now_iso();
        let items = self.library_items(token);
        let mut item = match items.iter().find(|it| it.get("_id").and_then(Value::as_str) == Some(show_id)) {
            Some(existing) => existing.clone(),
            None => {
                let Ok(addons) = self.addons(token) else { return };
                let Some(t) = self.title(&addons, stype, show_id) else { return };
                json!({
                    "_id": show_id, "name": t.name, "type": stype, "poster": t.poster,
                    "posterShape": "poster", "removed": true, "temp": true, "_ctime": now,
                    "state": {"lastWatched": now, "timeWatched": 0, "timeOffset": 0, "overallTimeWatched": 0,
                              "timesWatched": 0, "flaggedWatched": 0, "duration": 0, "video_id": null,
                              "watched": null, "noNotif": false},
                    "behaviorHints": {"defaultVideoId": null, "featuredVideoId": null, "hasScheduledVideos": false},
                })
            }
        };
        item["_mtime"] = json!(now);
        let state = &mut item["state"];
        state["lastWatched"] = json!(now);
        state["timeOffset"] = json!(offset);
        if duration > 0 {
            state["duration"] = json!(duration);
        }
        state["video_id"] = json!(video);
        if watched {
            let times = state.get("timesWatched").and_then(Value::as_i64).unwrap_or(0);
            state["timesWatched"] = json!(times + 1);
            state["flaggedWatched"] = json!(1);
            state["timeOffset"] = json!(0);
        }
        match self.stremio.library_put(token, &[item]) {
            Ok(()) => *self.library.lock().unwrap_or_else(|e| e.into_inner()) = None,
            Err(_) => plx_base::eventlog::log("stremio: a progress write was refused"),
        }
    }
}

// ---------------------------------------------------------------- anime openings and endings

impl Bridge {
    /// The intro and credits markers of episode `episode` of the anime `id` (`kitsu:N` or
    /// `mal:N`), from AniSkip's community timestamps. `None` when the title has no MyAnimeList id
    /// or AniSkip has nothing for the episode; the stand-in credits marker then stays.
    fn anime_skips(&self, id: &str, episode: i64, duration_ms: i64) -> Option<Vec<Value>> {
        let mal = self.mal_id(id)?;
        let url = format!("https://api.aniskip.com/v2/skip-times/{mal}/{episode}?types%5B%5D=op&types%5B%5D=ed&episodeLength={}",
            duration_ms / 1000);
        let v = self.fetch_json(&url).ok()?;
        let markers = aniskip_markers(&v, duration_ms);
        (!markers.is_empty()).then_some(markers)
    }

    /// The MyAnimeList id behind an anime id: `mal:N` directly, `kitsu:N` through Kitsu's mappings.
    fn mal_id(&self, id: &str) -> Option<i64> {
        if let Some(n) = id.strip_prefix("mal:") {
            return n.split(':').next()?.parse().ok();
        }
        let kitsu = id.strip_prefix("kitsu:")?.split(':').next()?;
        let url = format!("https://kitsu.io/api/edge/anime/{kitsu}/mappings?filter%5BexternalSite%5D=myanimelist%2Fanime");
        let v = self.fetch_json(&url).ok()?;
        v.get("data")?.as_array()?.iter().find_map(|m| {
            let a = m.get("attributes")?;
            (a.get("externalSite")?.as_str()? == "myanimelist/anime")
                .then(|| a.get("externalId")?.as_str()?.parse().ok())?
        })
    }
}

/// AniSkip's answer as markers: the opening (`op`) as an intro, the ending (`ed`) as the final
/// credits — what the player's Skip button and Up Next read.
pub(super) fn aniskip_markers(v: &Value, duration_ms: i64) -> Vec<Value> {
    if v.get("found").and_then(Value::as_bool) != Some(true) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for r in v.get("results").and_then(Value::as_array).into_iter().flatten() {
        let (Some(start), Some(end)) = (r.pointer("/interval/startTime").and_then(Value::as_f64),
                                        r.pointer("/interval/endTime").and_then(Value::as_f64)) else { continue };
        let (start, end) = ((start * 1000.0) as i64, ((end * 1000.0) as i64).min(duration_ms));
        if end <= start {
            continue;
        }
        match r.get("skipType").and_then(Value::as_str) {
            Some("op") => out.push(json!({"type": "intro", "startTimeOffset": start, "endTimeOffset": end})),
            Some("ed") => out.push(json!({"type": "credits", "startTimeOffset": start, "endTimeOffset": end, "final": true})),
            _ => {}
        }
    }
    out
}

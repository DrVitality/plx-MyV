//! **Subtitles from OpenSubtitles**, for every film and episode, whether or not the account has a
//! subtitles addon installed.
//!
//! Where they come from: the account's own subtitle addons, then OpenSubtitles' free Stremio
//! addon ([`OPENSUBTITLES`]) asked directly — no account and no key, the same service Stremio's
//! apps use. Each answer is a list of `{url, lang}` (an ISO 639-2 code: `eng`, `spa`, `pob`).
//!
//! Two ways they reach the player:
//!
//! - **On their own.** A file with no subtitle in the viewer's language gets up to three outside
//!   ones (their language first, then English), the first one selected
//!   ([`Bridge::external_subs`]).
//! - **Searched.** The player's Subtitles › Search page asks the media server's search
//!   (`GET /library/metadata/{rk}/subtitles?language=xx`), lists the candidates, and installs one
//!   (`PUT …/subtitles?key=/library/streams/{id}`), then finds the new stream on its next read of
//!   the item (`docs/pms-api.md` §8, which the app's half follows). This module is the server half:
//!   a candidate is a numbered address, and installing it adds that address to the item's version
//!   as an outside subtitle, kept for the rest of the run.

use super::super::catalog::{self, Rk};
use super::super::http::{Request, Response};
use super::super::library;
use super::super::media::{self, ExternalSub};
use super::{container, Bridge};
use serde_json::{json, Value};
use std::sync::atomic::Ordering;

/// OpenSubtitles' Stremio addon: `/subtitles/{type}/{id}.json`, by IMDb id (`tt…`, and
/// `tt…:season:episode` for an episode).
pub(super) const OPENSUBTITLES: &str = "https://opensubtitles-v3.strem.io";

/// Search candidates listed at most.
const SEARCH_LIMIT: usize = 40;
/// Outside subtitles added on their own at most.
const AUTO_LIMIT: usize = 3;

/// One subtitle file a provider offered.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Offer {
    pub url: String,
    /// As the provider wrote it, lower-cased (usually ISO 639-2).
    pub lang: String,
    /// The provider's name, for the search list.
    pub source: String,
}

/// The two-letter code of a provider's language, for matching the viewer's choice.
pub(super) fn two_letter(lang: &str) -> Option<String> {
    match lang {
        // OpenSubtitles' own codes for the Portuguese and Chinese variants
        "pob" | "pb" | "por" => Some("pt".into()),
        "zht" | "zhe" | "chi" | "zho" => Some("zh".into()),
        _ => plx_data::metadata::two_letter_code(lang),
    }
}

/// Is `url` an address a television can fetch? Never a desktop streaming server's.
fn reachable(url: &str) -> bool {
    url.starts_with("http") && !url.contains("localhost") && !url.contains("127.0.0.1")
}

impl Bridge {
    /// Every subtitle offered for the film or episode `video` (an id the providers understand):
    /// the account's subtitle addons first, then OpenSubtitles when none of them is it already.
    pub(super) fn subtitle_offers(&self, token: &str, stype: &str, video: &str) -> Vec<Offer> {
        let mut sources: Vec<(String, String)> = Vec::new();
        if let Ok(addons) = self.addons(token) {
            for a in library::subtitle_providers(&addons, stype, video) {
                sources.push((a.base_url().to_string(), a.manifest.name.trim().to_string()));
            }
        }
        if video.starts_with("tt") && !sources.iter().any(|(base, _)| base == OPENSUBTITLES) {
            sources.push((OPENSUBTITLES.to_string(), "OpenSubtitles".to_string()));
        }
        let mut out: Vec<Offer> = Vec::new();
        for (base, source) in sources {
            let url = format!("{base}/subtitles/{stype}/{video}.json");
            let Ok(v) = self.fetch_json(&url) else { continue };
            for s in v.get("subtitles").and_then(Value::as_array).into_iter().flatten() {
                let (Some(u), Some(l)) = (s.get("url").and_then(Value::as_str), s.get("lang").and_then(Value::as_str)) else {
                    continue;
                };
                if reachable(u) && !out.iter().any(|o| o.url == u) {
                    out.push(Offer { url: u.to_string(), lang: l.to_ascii_lowercase(), source: source.clone() });
                }
            }
        }
        out
    }

    /// The viewer's subtitle language (two letters), as the app last set it.
    pub(super) fn subtitle_language(&self) -> String {
        let profile = self.profile.lock().unwrap_or_else(|e| e.into_inner());
        profile.get("defaultSubtitleLanguage").and_then(Value::as_str)
            .and_then(two_letter).unwrap_or_else(|| "en".into())
    }

    /// Outside subtitles for a file with none in the viewer's language: theirs first, then
    /// English, then other languages, one file per language, at most [`AUTO_LIMIT`]. The first is
    /// selected when it is in the viewer's language or English.
    pub(super) fn external_subs(&self, token: &str, stype: &str, video: &str, part: i64) -> Vec<ExternalSub> {
        let wanted = self.subtitle_language();
        let offers = self.subtitle_offers(token, stype, video);
        let rank = |o: &Offer| match two_letter(&o.lang).as_deref() {
            Some(l) if l == wanted => 0,
            Some("en") => 1,
            _ => 2,
        };
        let mut offers: Vec<&Offer> = offers.iter().collect();
        offers.sort_by_key(|o| rank(o));
        let mut seen: Vec<String> = Vec::new();
        let mut out = Vec::new();
        let mut map = self.play.subs.lock().unwrap_or_else(|e| e.into_inner());
        for o in offers {
            let key = two_letter(&o.lang).unwrap_or_else(|| o.lang.clone());
            if seen.contains(&key) || out.len() >= AUTO_LIMIT {
                continue;
            }
            let id = part * 100 + 90 + out.len() as i64;
            map.insert(id, o.url.clone());
            seen.push(key);
            out.push(ExternalSub { id, language: o.lang.clone(), selected: out.is_empty() && rank(o) < 2 });
        }
        out
    }

    /// `GET /library/metadata/{rk}/subtitles?language=xx`: the subtitle files offered for a film or
    /// episode in that language, as the media server's search lists them (`Stream[]`, each with a
    /// `/library/streams/{id}` key to install).
    pub(super) fn subtitle_search(&self, req: &Request, token: &str, rk_s: &str) -> Response {
        let Some(rk) = Rk::decode(rk_s) else { return Response::empty(404) };
        let (stype, video) = match &rk {
            Rk::Item { stype, id, kind: catalog::Kind::Movie } => (stype.clone(), id.clone()),
            Rk::Episode { stype, video, .. } => (stype.clone(), video.clone()),
            _ => return Response::empty(404),
        };
        let wanted = req.query("language").and_then(two_letter);
        let offers: Vec<Offer> = self.subtitle_offers(token, &stype, &video)
            .into_iter()
            .filter(|o| wanted.is_none() || two_letter(&o.lang) == wanted)
            .take(SEARCH_LIMIT)
            .collect();
        let mut candidates = self.play.candidates.lock().unwrap_or_else(|e| e.into_inner());
        // a new search replaces the last one's candidates (their keys are ephemeral, as on Plex)
        candidates.retain(|_, (r, _, _)| r != rk_s);
        let total = offers.len();
        let rows: Vec<Value> = offers
            .into_iter()
            .enumerate()
            .map(|(n, o)| {
                let id = self.play.next_candidate.fetch_add(1, Ordering::Relaxed);
                let name = media::language_name(&o.lang);
                let title = format!("{} · {}", o.source, n + 1);
                candidates.insert(id, (rk_s.to_string(), o.lang.clone(), o.url.clone()));
                json!({
                    "id": id, "key": format!("/library/streams/{id}"), "streamType": 3,
                    "codec": "srt", "format": "srt", "language": name, "languageCode": o.lang,
                    "providerTitle": o.source, "score": (total - n) as i64, "title": title,
                    "displayTitle": name, "extendedDisplayTitle": format!("{title} ({name} SRT)"),
                })
            })
            .collect();
        Response::json(200, &container(json!({"size": rows.len(), "Stream": rows})))
    }

    /// `PUT /library/metadata/{rk}/subtitles?key=/library/streams/{id}`: install a candidate from
    /// the last search as an outside subtitle of the item. 200 with an empty body, as the media
    /// server answers; the stream is on the item's next read.
    pub(super) fn subtitle_install(&self, req: &Request, rk_s: &str) -> Response {
        let Some(id) = req.query("key")
            .and_then(|k| k.strip_prefix("/library/streams/"))
            .and_then(|k| k.split('.').next())
            .and_then(|k| k.parse::<i64>().ok()) else { return Response::empty(400) };
        let Some((rk, lang, url)) = self.play.candidates.lock().unwrap_or_else(|e| e.into_inner()).get(&id).cloned() else {
            // a stale key: accepted and nothing happens, exactly as the media server does
            return Response::empty(200);
        };
        if rk != rk_s {
            return Response::empty(200);
        }
        self.play.subs.lock().unwrap_or_else(|e| e.into_inner()).insert(id, url);
        let sub = ExternalSub { id, language: lang, selected: false };
        {
            let mut installed = self.play.installed.lock().unwrap_or_else(|e| e.into_inner());
            let list = installed.entry(rk_s.to_string()).or_default();
            if !list.iter().any(|s| s.id == id) {
                list.push(sub.clone());
            }
        }
        self.add_installed_to_memo(rk_s, &sub);
        Response::empty(200)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opensubtitles_language_codes_fold_to_two_letters() {
        assert_eq!(two_letter("eng").as_deref(), Some("en"));
        assert_eq!(two_letter("spa").as_deref(), Some("es"));
        assert_eq!(two_letter("pob").as_deref(), Some("pt"));
        assert_eq!(two_letter("en").as_deref(), Some("en"));
    }
}

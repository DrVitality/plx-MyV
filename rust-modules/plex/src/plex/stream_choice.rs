//! The stream picker's two reads, against this fork's Stremio bridge (the only server the app
//! talks to): which streams a film or episode can play from, and "play this one from now on".
//!
//! `GET /stremio/choices/{rk}` answers in the media server's shape — one `Metadata` row per
//! stream, `index` its place in the ranked list, `title` the picker line, `summary` the addon and
//! file — with the playing stream's index in the container's `offset` (-1 for none).
//! `GET /stremio/choose/{rk}?index=N` records the choice; the next resolve of that item plays it.
//! Both go through the ordinary client, so the token rides exactly as on every other request.

use super::client::{urlenc_str, Client};
use super::models::MediaContainer;

impl Client {
    /// The streams `rating_key` can play from, best first. `None` when the bridge did not answer.
    pub fn stream_choices(&self, rating_key: &str) -> Option<MediaContainer> {
        self.get_json(&format!("/stremio/choices/{}", urlenc_str(rating_key)))
    }

    /// Saves `rating_key` (a film, or an episode's show) in the account's library, or takes it out.
    pub fn stremio_library(&self, rating_key: &str, add: bool) -> bool {
        self.stremio_get(&format!("/stremio/library/{}?add={}", urlenc_str(rating_key), u8::from(add))).is_some()
    }

    /// A bridge route's plain JSON answer (not a media-server container): Settings' Home rows and
    /// addons (`/stremio/config…`). `None` when the bridge did not answer or the body is not JSON.
    pub fn stremio_get(&self, path: &str) -> Option<serde_json::Value> {
        serde_json::from_slice(&self.get_json_body(path)?).ok()
    }

    /// Plays the `index`-th stream of the ranked list for `rating_key` from now on.
    pub fn choose_stream(&self, rating_key: &str, index: i64) -> bool {
        self.get_json(&format!("/stremio/choose/{}?index={index}", urlenc_str(rating_key))).is_some()
    }
}

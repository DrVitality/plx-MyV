//! **Which episodes the account has watched**, as Stremio keeps it: one field per show in the
//! library item, `state.watched`, shared with every Stremio app — so a tick here is a tick there.
//!
//! The field is `{anchorVideoId}:{anchorLength}:{bits}`: `bits` is a base64 (standard alphabet,
//! padded) zlib stream of a bit per video in the show's video list, bit `i` in byte `i / 8` at
//! `1 << (i % 8)`; the anchor is the last watched video and its 1-based place in that list when
//! the field was written. A list that has grown since (a new season) is lined up again by the
//! anchor: wherever it sits now, the bits are shifted so it lands there. That is stremio-core's
//! `WatchedBitField`, which this mirrors.
//!
//! **Writes are careful.** The field belongs to the account, not to this app, so a change is
//! written only when the stored field read cleanly against our video list (its anchor found
//! exactly where the field says, or no field yet) and the new field reads back to exactly the
//! bits meant. Anything else leaves the field as it was ([`Watched::read`] answers `Err`).

/// A show's watched bits over its video list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Watched {
    pub bits: Vec<bool>,
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for (k, shift) in [18u32, 12, 6, 0].into_iter().enumerate() {
            if k <= chunk.len() {
                out.push(ALPHABET[((n >> shift) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

pub(super) fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in text.bytes().filter(|c| !c.is_ascii_whitespace() && *c != b'=') {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// `(anchor, anchor length, bit bytes)` of a stored field.
fn parse(field: &str) -> Option<(&str, i64, Vec<u8>)> {
    let (rest, packed) = field.rsplit_once(':')?;
    let (anchor, length) = rest.rsplit_once(':')?;
    let length: i64 = length.parse().ok()?;
    let zipped = base64_decode(packed)?;
    let bytes = miniz_oxide::inflate::decompress_to_vec_zlib(&zipped).ok()?;
    Some((anchor, length, bytes))
}

impl Watched {
    /// Nothing watched, over `len` videos.
    pub fn none(len: usize) -> Watched {
        Watched { bits: vec![false; len] }
    }

    /// The stored field read against `ids` (the show's videos, in its meta's order). `None` or an
    /// empty field is nothing watched. `Err` when it does not read cleanly — unparseable, or its
    /// anchor not where the field says — which shows nothing and forbids a write.
    pub fn read(field: Option<&str>, ids: &[String]) -> Result<Watched, Watched> {
        let Some(field) = field.filter(|f| !f.is_empty()) else { return Ok(Watched::none(ids.len())) };
        let Some((anchor, length, bytes)) = parse(field) else { return Err(Watched::none(ids.len())) };
        let Some(at) = ids.iter().position(|id| *id == anchor) else { return Err(Watched::none(ids.len())) };
        let offset = length - at as i64 - 1;
        let stored = |i: i64| i >= 0 && (i as usize) < bytes.len() * 8 && bytes[i as usize / 8] & (1 << (i % 8)) != 0;
        let bits: Vec<bool> = (0..ids.len() as i64).map(|i| stored(i + offset)).collect();
        // read as stremio-core reads it either way, but only a field whose anchor sits where it
        // says is one this list is known to match
        if offset == 0 { Ok(Watched { bits }) } else { Err(Watched { bits }) }
    }

    /// The field for these bits over `ids`.
    pub fn write(&self, ids: &[String]) -> String {
        let mut bytes = vec![0u8; self.bits.len().div_ceil(8)];
        for (i, on) in self.bits.iter().enumerate() {
            if *on {
                bytes[i / 8] |= 1 << (i % 8);
            }
        }
        let last = self.bits.iter().rposition(|b| *b).unwrap_or(0);
        let anchor = ids.get(last).map_or("undefined", String::as_str);
        let packed = base64_encode(&miniz_oxide::deflate::compress_to_vec_zlib(&bytes, 6));
        format!("{anchor}:{}:{packed}", last + 1)
    }

    /// The field for these bits, only when it reads back to exactly them.
    pub fn write_checked(&self, ids: &[String]) -> Option<String> {
        let field = self.write(ids);
        match Watched::read(Some(&field), ids) {
            Ok(back) if back == *self => Some(field),
            // all-unwatched writes an anchor of the first video at length 1, which reads back
            _ => None,
        }
    }

    pub fn is(&self, ids: &[String], id: &str) -> bool {
        ids.iter().position(|v| v == id).is_some_and(|i| self.bits.get(i).copied().unwrap_or(false))
    }

    pub fn set(&mut self, ids: &[String], id: &str, on: bool) {
        if let Some(i) = ids.iter().position(|v| v == id) {
            if let Some(b) = self.bits.get_mut(i) {
                *b = on;
            }
        }
    }
}

use super::super::catalog::{self, Rk, Title};
use super::Bridge;
use serde_json::{json, Value};

impl Bridge {
    /// The show's video ids, in its meta's order (the list the field's bits are over).
    fn video_ids(t: &Title) -> Vec<String> {
        t.videos.iter().map(|e| e.video.clone()).collect()
    }

    /// What the account has watched of the show `t`, as far as its field reads.
    pub(super) fn watched_of(&self, token: &str, t: &Title) -> (Vec<String>, Watched) {
        let ids = Self::video_ids(t);
        let items = self.library_items(token);
        let field = items.iter()
            .find(|it| it.get("_id").and_then(Value::as_str) == Some(t.id.as_str()))
            .and_then(|it| it.pointer("/state/watched").and_then(Value::as_str));
        let w = Watched::read(field, &ids).unwrap_or_else(|shifted| shifted);
        (ids, w)
    }

    /// Ticks on a show's rows: an episode's `viewCount`, a season's and the show's
    /// `viewedLeafCount`.
    pub(super) fn mark_watched_rows(&self, token: &str, t: &Title, rows: &mut [Value]) {
        if t.videos.is_empty() {
            return;
        }
        let (ids, w) = self.watched_of(token, t);
        for row in rows.iter_mut() {
            match row.get("type").and_then(Value::as_str) {
                Some("episode") => {
                    let rk = row.get("ratingKey").and_then(Value::as_str).and_then(Rk::decode);
                    if let Some(Rk::Episode { video, .. }) = rk {
                        if w.is(&ids, &video) {
                            row["viewCount"] = json!(1);
                        }
                    }
                }
                Some("season") => {
                    let Some(season) = row.get("index").and_then(Value::as_i64) else { continue };
                    let seen = t.episodes(season).iter().filter(|e| w.is(&ids, &e.video)).count();
                    row["viewedLeafCount"] = json!(seen);
                }
                Some("show") => {
                    row["viewedLeafCount"] = json!(w.bits.iter().filter(|b| **b).count());
                }
                _ => {}
            }
        }
    }

    /// The watched field after marking the videos `rk` covers (an episode, a season, a whole
    /// show) watched or not, starting from `stored`. `None` — leave the field alone — for a film,
    /// a show no metadata provider has, or a stored field that does not read cleanly.
    pub(super) fn watched_field_after(&self, token: &str, rk: &Rk, stored: Option<&str>, on: bool) -> Option<String> {
        if matches!(rk, Rk::Item { kind: catalog::Kind::Movie, .. }) {
            return None;
        }
        let (stype, id) = rk.show();
        let addons = self.addons(token).ok()?;
        let t = self.title(&addons, stype, id)?;
        let ids = Self::video_ids(&t);
        let targets: Vec<String> = match rk {
            Rk::Episode { video, .. } => vec![video.clone()],
            Rk::Season { season, .. } => t.episodes(*season).iter().map(|e| e.video.clone()).collect(),
            Rk::Item { .. } => ids.clone(),
        };
        let Ok(mut w) = Watched::read(stored, &ids) else {
            plx_base::eventlog::log("stremio: the watched field did not read cleanly; left as it was");
            return None;
        };
        for v in &targets {
            w.set(&ids, v, on);
        }
        let field = w.write_checked(&ids);
        if field.is_none() {
            plx_base::eventlog::log("stremio: a watched field did not read back; left as it was");
        }
        field
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(n: usize) -> Vec<String> {
        (1..=n).map(|e| format!("tt1:1:{e}")).collect()
    }

    #[test]
    fn base64_round_trips_every_tail_length() {
        for n in 0..10u8 {
            let bytes: Vec<u8> = (0..n).map(|b| b.wrapping_mul(37)).collect();
            assert_eq!(base64_decode(&base64_encode(&bytes)).unwrap(), bytes);
        }
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"Ma"), "TWE=");
    }

    #[test]
    fn a_field_reads_back_the_bits_it_was_written_from() {
        let list = ids(12);
        let mut w = Watched::none(12);
        w.set(&list, "tt1:1:1", true);
        w.set(&list, "tt1:1:3", true);
        w.set(&list, "tt1:1:10", true);
        let field = w.write_checked(&list).expect("reads back");
        assert!(field.starts_with("tt1:1:10:10:"), "{field}");
        let back = Watched::read(Some(&field), &list).unwrap();
        assert!(back.is(&list, "tt1:1:3") && !back.is(&list, "tt1:1:2") && back.is(&list, "tt1:1:10"));
    }

    #[test]
    fn a_grown_list_is_lined_up_by_the_anchor_but_not_trusted_for_writing() {
        let old = ids(3);
        let mut w = Watched::none(3);
        w.set(&old, "tt1:1:2", true);
        let field = w.write(&old);
        // a special was added in front: the anchor moved one place
        let mut grown = vec!["tt1:0:1".to_string()];
        grown.extend(old.clone());
        let read = Watched::read(Some(&field), &grown);
        let shifted = read.clone().unwrap_err();
        assert!(shifted.is(&grown, "tt1:1:2") && !shifted.is(&grown, "tt1:1:1"));
        // appended episodes keep the anchor in place, which reads cleanly
        let mut appended = old.clone();
        appended.push("tt1:1:4".into());
        assert!(Watched::read(Some(&field), &appended).unwrap().is(&appended, "tt1:1:2"));
    }

    #[test]
    fn a_broken_or_foreign_field_forbids_a_write() {
        let list = ids(3);
        assert!(Watched::read(Some("garbage"), &list).is_err());
        assert!(Watched::read(Some("tt9:1:1:1:eJxjAAAAAQAB"), &list).is_err(), "an anchor not in the list");
        assert_eq!(Watched::read(None, &list).unwrap(), Watched::none(3));
    }

    #[test]
    fn a_field_written_by_stremio_reads() {
        // stremio-core's own encoding of [true, false, true] over three videos: zlib of [0b101]
        let list = ids(3);
        let field = format!("tt1:1:3:3:{}", base64_encode(&miniz_oxide::deflate::compress_to_vec_zlib(&[5], 9)));
        let w = Watched::read(Some(&field), &list).unwrap();
        assert_eq!(w.bits, [true, false, true]);
    }
}

//! A chosen stream as the media server describes a playable version: `Media` → `Part` → `Stream[]`,
//! with the audio and subtitle tracks chosen for this viewer.
//!
//! The tracks come from the file itself ([`super::probe`]); when the file could not be read, from
//! what the stream's release name says ([`super::streams::Facts`]), which is enough to start but
//! not to trust — so a read file always wins.
//!
//! **The choice.** Audio: an English track in a format this television plays (AAC, Dolby Digital,
//! Dolby Digital Plus, DTS), the file's default first, more channels next; failing English, the
//! default playable track. Subtitles: English, text before images, full before forced — chosen
//! even when the audio is English, as this fork's owner asked. `Media.audioCodec` names the chosen
//! track's codec, because that is what the player configures the decoder with.

use super::probe::{Probe, Track, TrackKind};
use super::streams::Facts;
use serde_json::{json, Value};

/// Audio formats this television plays without a server to convert them.
const PLAYABLE_AUDIO: &[&str] = &["aac", "ac3", "eac3", "dts", "mp3"];
/// Subtitle formats the app draws itself.
const TEXT_SUBS: &[&str] = &["srt", "ass", "webvtt", "mov_text"];

/// An ISO 639-2 code as a language name, for the track menu.
pub fn language_name(code: &str) -> String {
    let name = match code {
        "eng" | "en" => "English",
        "jpn" | "ja" => "Japanese",
        "fre" | "fra" | "fr" => "French",
        "ger" | "deu" | "de" => "German",
        "spa" | "es" => "Spanish",
        "ita" | "it" => "Italian",
        "por" | "pt" => "Portuguese",
        "rus" | "ru" => "Russian",
        "kor" | "ko" => "Korean",
        "chi" | "zho" | "zh" => "Chinese",
        "hin" | "hi" => "Hindi",
        "ara" | "ar" => "Arabic",
        "pol" | "pl" => "Polish",
        "tur" | "tr" => "Turkish",
        "dut" | "nld" | "nl" => "Dutch",
        "swe" | "sv" => "Swedish",
        "" => "Unknown",
        other => return other.to_string(),
    };
    name.to_string()
}

fn is_english(t: &Track) -> bool {
    matches!(t.language.as_str(), "eng" | "en")
}

/// The index (into `tracks`) of the audio track to play.
pub fn pick_audio(tracks: &[Track]) -> Option<usize> {
    let audio = |i: &usize| tracks[*i].kind == TrackKind::Audio;
    let playable = |i: &usize| PLAYABLE_AUDIO.contains(&tracks[*i].codec.as_str());
    let rank = |i: &usize| (tracks[*i].default, tracks[*i].channels);
    let all: Vec<usize> = (0..tracks.len()).filter(audio).collect();
    let english: Vec<usize> = all.iter().copied().filter(|i| playable(i) && is_english(&tracks[*i])).collect();
    let pick_best = |set: Vec<usize>| set.into_iter().max_by_key(|i| rank(i));
    pick_best(english)
        .or_else(|| pick_best(all.iter().copied().filter(playable).collect()))
        .or_else(|| all.first().copied())
}

/// The index (into `tracks`) of the subtitle track to show, when there is an English one.
pub fn pick_subtitle(tracks: &[Track]) -> Option<usize> {
    (0..tracks.len())
        .filter(|i| tracks[*i].kind == TrackKind::Subtitle && is_english(&tracks[*i]))
        .max_by_key(|i| {
            let t = &tracks[*i];
            let sdh = t.name.to_ascii_lowercase().contains("sdh");
            (TEXT_SUBS.contains(&t.codec.as_str()), !t.forced, !sdh)
        })
}

fn resolution_label(height: u32) -> &'static str {
    match height {
        h if h >= 1600 => "4k",
        h if h >= 900 => "1080",
        h if h >= 650 => "720",
        h if h >= 450 => "480",
        _ => "sd",
    }
}

/// An external subtitle the bridge serves at `/library/streams/{id}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalSub {
    pub id: i64,
    pub language: String,
}

/// The version for a read file. `part_id` names it in `/library/parts/{id}`; `rk` is the item; the
/// external subtitles are offered only when the file has no English subtitle of its own.
pub fn media_from_probe(rk: &str, part_id: i64, p: &Probe, size: Option<u64>, external: &[ExternalSub]) -> Value {
    let audio_pick = pick_audio(&p.tracks);
    let sub_pick = pick_subtitle(&p.tracks);
    let video = p.tracks.iter().find(|t| t.kind == TrackKind::Video);
    let mut streams = Vec::new();
    for (i, t) in p.tracks.iter().enumerate() {
        let id = part_id * 100 + i as i64 + 1;
        let lang = language_name(&t.language);
        let s = match t.kind {
            TrackKind::Video => json!({
                "id": id, "streamType": 1, "codec": t.codec, "index": i, "width": t.width,
                "height": t.height, "default": true,
                // A Dolby Vision file is shown as such but not declared as one: the player would
                // then decide how to present it from a profile this reading does not know.
                "displayTitle": format!("{} ({}{})", resolution_label(t.height).to_uppercase(),
                                        t.codec.to_uppercase(), if t.dovi { " DV" } else { "" }),
            }),
            TrackKind::Audio => json!({
                "id": id, "streamType": 2, "codec": t.codec, "index": i, "channels": t.channels,
                "language": lang, "languageCode": t.language, "title": t.name,
                "default": t.default, "selected": audio_pick == Some(i),
                "displayTitle": format!("{lang} ({} {}ch)", t.codec.to_uppercase(), t.channels),
            }),
            TrackKind::Subtitle => json!({
                "id": id, "streamType": 3, "codec": t.codec, "index": i,
                "language": lang, "languageCode": t.language, "title": t.name,
                "default": t.default, "forced": t.forced, "selected": sub_pick == Some(i),
                "displayTitle": format!("{lang}{} ({})", if t.forced { " Forced" } else { "" }, t.codec.to_uppercase()),
            }),
        };
        streams.push(s);
    }
    if sub_pick.is_none() {
        for (n, e) in external.iter().enumerate() {
            let lang = language_name(&e.language);
            streams.push(json!({
                "id": e.id, "streamType": 3, "codec": "srt", "external": true,
                "key": format!("/library/streams/{}", e.id), "language": lang,
                "languageCode": e.language, "selected": n == 0 && matches!(e.language.as_str(), "eng" | "en"),
                "displayTitle": format!("{lang} (SRT External)"),
            }));
        }
    }
    let (w, h) = video.map(|v| (v.width, v.height)).unwrap_or((0, 0));
    let audio = audio_pick.map(|i| &p.tracks[i]);
    let bitrate = match (size, p.duration_ms) {
        (Some(bytes), ms) if ms > 0 => (bytes * 8 / ms) as i64,
        _ => 0,
    };
    json!([{
        "id": part_id, "duration": p.duration_ms, "bitrate": bitrate, "width": w, "height": h,
        "aspectRatio": if h > 0 { f64::from(w) / f64::from(h) } else { 0.0 },
        "audioChannels": audio.map_or(0, |a| a.channels), "audioCodec": audio.map_or("", |a| a.codec.as_str()),
        "videoCodec": video.map_or("", |v| v.codec.as_str()), "videoResolution": resolution_label(h),
        "container": p.container,
        "Part": [{
            "id": part_id, "key": super::catalog::part_key(part_id, rk, p.container), "duration": p.duration_ms,
            "size": size.unwrap_or(0), "container": p.container, "Stream": streams,
        }],
    }])
}

/// The version for a file that could not be read, from what its release name says. Better than
/// nothing and worse than reading it: the audio codec is the release name's word for it.
pub fn media_from_facts(rk: &str, part_id: i64, f: &Facts, detail: &str, minutes: Option<i64>, size: Option<u64>) -> Value {
    let words = detail.to_ascii_uppercase();
    let has = |x: &str| words.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| w == x);
    let acodec = if has("DDP") || has("EAC3") || words.contains("DD+") || has("DDP5") || has("DDP2") {
        "eac3"
    } else if has("AAC") || has("AAC2") {
        "aac"
    } else if has("DTS") {
        "dts"
    } else {
        "ac3"
    };
    let vcodec = if f.hevc { "hevc" } else { "h264" };
    let h: u32 = match f.resolution {
        0 => 1080,
        r => r,
    };
    let w = h * 16 / 9;
    let ms = minutes.unwrap_or(0) * 60_000;
    let probe = Probe {
        container: "mkv",
        duration_ms: u64::try_from(ms).unwrap_or(0),
        tracks: vec![
            Track { kind: TrackKind::Video, codec: vcodec.into(), language: String::new(), name: String::new(),
                    default: true, forced: false, width: w, height: h, channels: 0, dovi: false },
            Track { kind: TrackKind::Audio, codec: acodec.into(), language: "eng".into(), name: String::new(),
                    default: true, forced: false, width: 0, height: 0, channels: 6, dovi: false },
        ],
    };
    media_from_probe(rk, part_id, &probe, size, &[])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(kind: TrackKind, codec: &str, lang: &str) -> Track {
        Track { kind, codec: codec.into(), language: lang.into(), name: String::new(), default: false,
                forced: false, width: 0, height: 0, channels: 6, dovi: false }
    }

    #[test]
    fn english_playable_audio_wins_over_the_default_lossless_track() {
        let mut truehd = t(TrackKind::Audio, "truehd", "eng");
        truehd.default = true;
        let tracks = vec![t(TrackKind::Video, "hevc", ""), truehd, t(TrackKind::Audio, "ac3", "fre"),
                          t(TrackKind::Audio, "eac3", "eng")];
        assert_eq!(pick_audio(&tracks), Some(3));
        // No English: the default playable one; nothing playable: the first.
        let jp = vec![t(TrackKind::Audio, "aac", "jpn")];
        assert_eq!(pick_audio(&jp), Some(0));
        let lossless = vec![t(TrackKind::Audio, "truehd", "eng")];
        assert_eq!(pick_audio(&lossless), Some(0));
    }

    #[test]
    fn english_text_subtitles_beat_images_and_forced_ones() {
        let mut forced = t(TrackKind::Subtitle, "srt", "eng");
        forced.forced = true;
        let tracks = vec![t(TrackKind::Subtitle, "pgs", "eng"), forced, t(TrackKind::Subtitle, "srt", "eng"),
                          t(TrackKind::Subtitle, "srt", "spa")];
        assert_eq!(pick_subtitle(&tracks), Some(2));
        assert_eq!(pick_subtitle(&[t(TrackKind::Subtitle, "srt", "spa")]), None);
    }

    #[test]
    fn the_version_names_the_chosen_tracks_codec_and_offers_external_subs_only_when_needed() {
        let mut v = t(TrackKind::Video, "hevc", "");
        v.width = 3840;
        v.height = 2160;
        let probe = Probe { container: "mkv", duration_ms: 3_600_000,
            tracks: vec![v, t(TrackKind::Audio, "truehd", "eng"), t(TrackKind::Audio, "eac3", "eng")] };
        let ext = [ExternalSub { id: 9001, language: "eng".into() }];
        let m = media_from_probe("mstt1", 77, &probe, Some(18_000_000_000), &ext);
        assert_eq!(m[0]["audioCodec"], "eac3");
        assert_eq!(m[0]["videoCodec"], "hevc");
        assert_eq!(m[0]["videoResolution"], "4k");
        assert_eq!(m[0]["bitrate"], 40_000);
        let streams = m[0]["Part"][0]["Stream"].as_array().unwrap();
        assert_eq!(streams[2]["selected"], true);
        assert_eq!(streams[3]["key"], "/library/streams/9001");
        assert_eq!(streams[3]["selected"], true);
        assert_eq!(m[0]["Part"][0]["key"], "/library/parts/77/mstt1/file.mkv");
    }
}

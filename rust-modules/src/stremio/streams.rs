//! Choosing what to play: every stream provider's answers for a title, read and ranked, so the
//! app can start the best one without asking — and show the rest in order when asked.
//!
//! **What a stream says about itself.** Torrent addons describe each result in free text: the
//! `name` (Torrentio: `[RD+] Torrentio\n4k HDR`; Comet: `[RD⚡] Comet 2160p`) and the `title` or
//! `description` (the release name, seeders, size, source, and the audio languages as flags). The
//! release name follows the scene's conventions (`2160p`, `x265`, `DDP5.1`, `ITA`), so [`facts`]
//! reads it the way a person scanning the list would.
//!
//! **Cached.** With a debrid service, a result is playable at once only when the service already
//! holds the file. Torrentio marks those `[RD+]` and the rest `[RD download]`; Comet marks them `⚡`
//! and the rest `⬇️`. A plain `url` with neither mark (a direct-link addon) plays at once too. A
//! bare torrent (`infoHash` and no `url`) needs a torrent client this app does not have, so it is
//! never offered.
//!
//! **This television.** An LG from 2019 decodes H.264 and HEVC (with HDR10) and plays Dolby
//! Digital, Dolby Digital Plus, AAC and DTS. It does not decode AV1, cannot show Dolby Vision-only
//! video in the right colours, and has no TrueHD, FLAC or Opus — with no server to convert them,
//! a file whose only audio is one of those plays silent. [`score`] steers away from all of those.
//!
//! **The order** ([`score`]): cached first, then English audio (or several languages, which
//! nearly always include English), then what the television plays well, then resolution. The best
//! result is the one to start automatically ([`best`]); a camera recording never is.

use serde_json::Value;

/// One stream as an addon answered it, with which addon answered.
#[derive(Debug, Clone, PartialEq)]
pub struct Stream {
    pub addon: String,
    pub name: String,
    /// `title` or `description`, whichever the addon filled: the release name and details.
    pub detail: String,
    pub url: Option<String>,
    pub filename: Option<String>,
    /// `behaviorHints.videoSize`, when given.
    pub size: Option<u64>,
}

impl Stream {
    /// A stream from an addon's `streams[]` entry; `None` for one with nothing to play.
    pub fn read(addon: &str, v: &Value) -> Option<Stream> {
        let url = v.get("url").and_then(Value::as_str).filter(|u| u.starts_with("http")).map(String::from);
        // A bare torrent cannot be played here, and a YouTube id or external link is not a video.
        url.as_ref()?;
        let text = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let detail = if text("description").is_empty() { text("title") } else { text("description") };
        Some(Stream {
            addon: addon.to_string(),
            name: text("name"),
            detail,
            url,
            filename: v.pointer("/behaviorHints/filename").and_then(Value::as_str).map(String::from),
            size: v.pointer("/behaviorHints/videoSize").and_then(Value::as_u64),
        })
    }
}

/// Whether a stream can start right away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ready {
    /// The debrid service already holds it.
    Cached,
    /// The debrid service would have to fetch it first.
    Uncached,
    /// A direct link with no debrid mark.
    Direct,
}

/// How sure we are the audio is English.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum English {
    /// Another language and nothing says English.
    No,
    /// No language is named (Torrentio names none when English is the only one).
    Unknown,
    /// Several languages ("Multi", "Dual Audio"), which nearly always include English.
    Multi,
    /// English is named.
    Yes,
}

/// What a stream's text says about it.
#[derive(Debug, Clone, PartialEq)]
pub struct Facts {
    pub ready: Ready,
    pub english: English,
    /// Vertical resolution: 2160, 1080, 720, 480, or 0 when not said.
    pub resolution: u32,
    pub hevc: bool,
    pub av1: bool,
    pub hdr: bool,
    /// Dolby Vision with no HDR10 fallback named.
    pub dv_only: bool,
    pub remux: bool,
    pub cam: bool,
    /// An audio format this television plays (DD, DD+, AAC, DTS) is named.
    pub playable_audio: bool,
    /// Only an audio format it does not play (TrueHD, FLAC, Opus) is named.
    pub silent_audio: bool,
    pub size_gb: Option<f64>,
}

/// The words of a text, split on everything that is not a letter or digit, upper-cased.
fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_uppercase).collect()
}

/// The two-letter country codes of the flag emoji in `s` (regional-indicator pairs).
fn flags(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut pending: Option<char> = None;
    for c in s.chars() {
        let code = c as u32;
        if (0x1F1E6..=0x1F1FF).contains(&code) {
            let letter = char::from_u32(code - 0x1F1E6 + u32::from(b'A')).unwrap_or('?');
            match pending.take() {
                Some(first) => out.push(format!("{first}{letter}")),
                None => pending = Some(letter),
            }
        } else {
            pending = None;
        }
    }
    out
}

/// A size like `💾 12.4 GB` or `Size: 2.1 GB` in `s`, in gigabytes.
fn size_in(s: &str) -> Option<f64> {
    let w: Vec<&str> = s.split_whitespace().collect();
    w.windows(2).find_map(|p| {
        let n: f64 = p[0].trim_start_matches(|c: char| !c.is_ascii_digit()).parse().ok()?;
        match p[1].trim_end_matches(|c: char| !c.is_ascii_alphabetic()).to_ascii_uppercase().as_str() {
            "GB" | "GIB" => Some(n),
            "MB" | "MIB" => Some(n / 1024.0),
            "TB" | "TIB" => Some(n * 1024.0),
            _ => None,
        }
    })
}

const OTHER_LANGUAGES: &[&str] = &[
    "ITA", "ITALIAN", "FRENCH", "TRUEFRENCH", "VFF", "VFQ", "VF2", "VOSTFR", "GERMAN", "SPANISH",
    "LATINO", "CASTELLANO", "ESP", "HINDI", "TAMIL", "TELUGU", "RUS", "RUSSIAN", "UKR", "POLISH",
    "PL", "PORTUGUESE", "POR", "TURKISH", "ARABIC", "KOREAN", "CHINESE", "JAPANESE", "DUTCH",
    "SWEDISH", "CZECH", "HUNGARIAN", "GREEK", "THAI", "VIETNAMESE",
];

/// Reads what a stream's name, detail and file name say about it.
pub fn facts(s: &Stream) -> Facts {
    let all = format!("{}\n{}\n{}", s.name, s.detail, s.filename.as_deref().unwrap_or(""));
    let w = words(&all);
    let has = |x: &str| w.iter().any(|y| y == x);
    let any = |xs: &[&str]| xs.iter().any(|x| has(x));
    let lower = all.to_lowercase();

    let ready = if s.name.contains("download]") || s.name.contains('⬇') {
        Ready::Uncached
    } else if s.name.contains("+]") || s.name.contains('⚡') {
        Ready::Cached
    } else {
        Ready::Direct
    };

    let flags = flags(&all);
    let english_flag = flags.iter().any(|f| f == "GB" || f == "US");
    let multi = lower.contains("multi audio") || lower.contains("dual audio") || has("MULTI")
        || has("DUAL") || flags.len() > 1;
    let other = flags.iter().any(|f| f != "GB" && f != "US") || any(OTHER_LANGUAGES);
    let english = if english_flag || has("ENG") || has("ENGLISH") {
        English::Yes
    } else if multi {
        English::Multi
    } else if other {
        English::No
    } else {
        English::Unknown
    };

    let resolution = if any(&["2160P", "4K", "UHD", "2160"]) {
        2160
    } else if any(&["1080P", "1080I", "1080"]) {
        1080
    } else if any(&["720P", "720"]) {
        720
    } else if any(&["480P", "576P", "SD", "DVDRIP"]) {
        480
    } else {
        0
    };
    let hdr = any(&["HDR", "HDR10", "HDR10PLUS", "HDR10+"]) || lower.contains("hdr10");
    let dv = any(&["DV", "DOVI", "DOLBYVISION"]) || lower.contains("dolby vision");

    let playable_audio = any(&["DDP", "DD", "EAC3", "AC3", "AAC", "DTS", "DDP5", "DD5", "DD2", "DDP2", "DDP7", "EAC"])
        || lower.contains("dd+") || lower.contains("dolby digital");
    let silent = any(&["TRUEHD", "FLAC", "OPUS"]);

    Facts {
        ready,
        english,
        resolution,
        hevc: any(&["X265", "H265", "HEVC"]),
        av1: has("AV1"),
        hdr,
        dv_only: dv && !hdr,
        remux: has("REMUX") || has("BDREMUX"),
        cam: any(&["CAM", "HDCAM", "TS", "HDTS", "TELESYNC", "TC", "TELECINE", "SCR", "SCREENER"]),
        playable_audio,
        silent_audio: silent && !playable_audio,
        size_gb: s.size.map(|b| b as f64 / 1_073_741_824.0).or_else(|| size_in(&all)),
    }
}

/// How good a stream is for this television and this viewer; higher is better. See the module
/// doc for the order this encodes.
pub fn score(f: &Facts) -> i64 {
    let mut s = 0i64;
    s += match f.ready {
        Ready::Cached | Ready::Direct => 10_000,
        Ready::Uncached => 0,
    };
    s += match f.english {
        English::Yes => 4_000,
        English::Multi => 3_000,
        English::Unknown => 2_500,
        English::No => 0,
    };
    // What would not play right on this set outweighs any resolution.
    if f.cam {
        s -= 20_000;
    }
    if f.av1 {
        s -= 5_000;
    }
    if f.dv_only {
        s -= 2_000;
    }
    if f.silent_audio {
        s -= 1_500;
    }
    if f.playable_audio {
        s += 200;
    }
    s += match f.resolution {
        2160 => 1_000,
        1080 => 800,
        720 => 400,
        480 => 100,
        _ => 300,
    };
    if f.hdr && f.resolution == 2160 {
        s += 100;
    }
    // HEVC carries 4K in about half H.264's bitrate: less for the network to keep up with.
    if f.hevc && f.resolution == 2160 {
        s += 20;
    }
    // A remux is the disc's own bitrate, often more than a television's network keeps up with.
    if f.remux {
        s -= 150;
    }
    if f.size_gb.is_some_and(|g| g > 40.0) {
        s -= 200;
    }
    s
}

/// `streams` ranked best first, with what each one's text says.
pub fn rank(streams: Vec<Stream>) -> Vec<(Stream, Facts, i64)> {
    let mut ranked: Vec<(Stream, Facts, i64)> = streams
        .into_iter()
        .map(|s| {
            let f = facts(&s);
            let sc = score(&f);
            (s, f, sc)
        })
        .collect();
    // Stable: equal scores keep the addons' own order, which is already their best guess.
    ranked.sort_by(|a, b| b.2.cmp(&a.2));
    ranked
}

/// The stream to start without asking: the best one that is ready now and is not a camera copy.
pub fn best(ranked: &[(Stream, Facts, i64)]) -> Option<usize> {
    ranked.iter().position(|(_, f, _)| f.ready != Ready::Uncached && !f.cam && !f.av1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn stream(name: &str, detail: &str) -> Stream {
        Stream { addon: "t".into(), name: name.into(), detail: detail.into(),
                 url: Some("https://x".into()), filename: None, size: None }
    }

    #[test]
    fn torrentio_and_comet_cache_marks_are_read() {
        assert_eq!(facts(&stream("[RD+] Torrentio\n4k HDR", "")).ready, Ready::Cached);
        assert_eq!(facts(&stream("[RD download] Torrentio\n1080p", "")).ready, Ready::Uncached);
        assert_eq!(facts(&stream("[RD⚡] Comet 2160p", "")).ready, Ready::Cached);
        assert_eq!(facts(&stream("[RD⬇️] Comet 1080p", "")).ready, Ready::Uncached);
        assert_eq!(facts(&stream("Some Addon 1080p", "")).ready, Ready::Direct);
    }

    #[test]
    fn a_bare_torrent_is_never_offered() {
        assert!(Stream::read("Torrentio", &json!({"name": "Torrentio", "infoHash": "abc"})).is_none());
        let s = Stream::read("Comet", &json!({"name": "[RD⚡] Comet 2160p", "url": "https://c/play",
            "description": "Movie.2160p.WEB-DL.DDP5.1.x265", "behaviorHints": {"videoSize": 2147483648u64,
            "filename": "Movie.2160p.mkv"}})).expect("a playable stream");
        assert_eq!(s.size, Some(2_147_483_648));
        assert_eq!(s.filename.as_deref(), Some("Movie.2160p.mkv"));
    }

    #[test]
    fn languages_come_from_flags_and_release_words() {
        let en = |d: &str| facts(&stream("[RD+] Torrentio\n1080p", d)).english;
        assert_eq!(en("Movie.2021.1080p.WEB-DL\n👤 120 💾 4.2 GB ⚙️ YTS"), English::Unknown);
        assert_eq!(en("Movie.2021.1080p\n🇬🇧"), English::Yes);
        assert_eq!(en("Movie.2021.1080p.ITA.ENG\n"), English::Yes);
        assert_eq!(en("Movie.2021.1080p.iTALiAN\n🇮🇹"), English::No);
        assert_eq!(en("Movie.2021.1080p.MULTi\n🇫🇷 / 🇬🇧"), English::Yes);
        assert_eq!(en("Movie.2021.1080p\nMulti Audio"), English::Multi);
        assert_eq!(en("Film.2021.TRUEFRENCH.1080p"), English::No);
    }

    #[test]
    fn video_and_audio_traits_are_read_from_the_release_name() {
        let f = facts(&stream("[RD+] Torrentio\n4k DV | HDR",
            "Movie.2021.2160p.UHD.BluRay.REMUX.DV.HDR.HEVC.TrueHD.Atmos.7.1\n💾 58.3 GB"));
        assert_eq!(f.resolution, 2160);
        assert!(f.hevc && f.hdr && !f.dv_only && f.remux);
        assert!(f.silent_audio, "TrueHD alone plays silent here");
        assert_eq!(f.size_gb, Some(58.3));
        let dv = facts(&stream("[RD+] Torrentio\n4k DV", "Movie.2160p.WEB-DL.DV.H265.DDP5.1"));
        assert!(dv.dv_only && dv.playable_audio && !dv.silent_audio);
        assert!(facts(&stream("[RD+] Torrentio\nCAM", "Movie.2024.HDCAM")).cam);
        assert!(facts(&stream("[RD+] x", "Movie.1080p.WEB.AV1.Opus")).av1);
    }

    #[test]
    fn the_best_is_cached_english_and_playable_on_this_set() {
        let ranked = rank(vec![
            stream("[RD download] Torrentio\n4k", "Movie.2160p.WEB-DL.DDP5.1.x265"),
            stream("[RD+] Torrentio\n4k", "Movie.2160p.WEB-DL.ITA.DDP5.1.x265\n🇮🇹"),
            stream("[RD+] Torrentio\n4k DV", "Movie.2160p.WEB-DL.DV.H265.DDP5.1"),
            stream("[RD+] Torrentio\n1080p", "Movie.1080p.BluRay.DD5.1.x264"),
            stream("[RD⚡] Comet 2160p", "Movie.2160p.WEB-DL.HDR.DDP5.1.Atmos.x265"),
            stream("[RD+] Torrentio\nCAM", "Movie.2024.HDCAM.x264"),
        ]);
        let i = best(&ranked).expect("something to play");
        assert_eq!(ranked[i].0.detail, "Movie.2160p.WEB-DL.HDR.DDP5.1.Atmos.x265");
        let order: Vec<&str> = ranked.iter().map(|r| r.0.detail.as_str()).collect();
        assert_eq!(order[1], "Movie.1080p.BluRay.DD5.1.x264", "English 1080p beats Dolby Vision-only 4K");
        assert_eq!(*order.last().unwrap(), "Movie.2024.HDCAM.x264");
    }

    #[test]
    fn nothing_ready_means_nothing_starts_by_itself() {
        let ranked = rank(vec![stream("[RD download] Torrentio\n1080p", "Movie.1080p.x264")]);
        assert_eq!(best(&ranked), None);
    }
}

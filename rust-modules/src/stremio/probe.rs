//! What is inside a file before it plays: its container, video, audio and subtitle tracks, read
//! from the first bytes of the file.
//!
//! The app's player configures the television's decoder from the item's metadata BEFORE its
//! demuxer opens the file — the Load payload names the video codec and the audio codec, and the
//! demuxer then feeds the audio track whose codec matches (`media/src/ff.rs`,
//! `audio_stream_matching`). A Plex server knows a file's tracks because it scanned it; this
//! bridge does not, so it reads them here, from the same bytes the player will read, rather than
//! guessing from a release name. A guess that names the wrong audio codec does not fail loudly: it
//! leaves the audio stream unconfigured and the video stalls behind it.
//!
//! - **Matroska** (`.mkv`, nearly every debrid release): the `Tracks` element sits near the start,
//!   or a `SeekHead` there says where it is. [`parse_mkv`] walks the EBML tree and either answers
//!   or names the offset it needs ([`Parse::Need`]).
//! - **MP4**: the `moov` box describes the tracks; when it comes first (as streaming encodes put
//!   it) [`parse_mp4`] reads it, and when it comes after the media it names where the media ends.
//!
//! Both parsers are pure over a byte window, so they are tested here on hand-built files.

/// One track as the file describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub kind: TrackKind,
    /// The codec in the names the app and FFmpeg use: `h264`, `hevc`, `aac`, `ac3`, `eac3`,
    /// `dts`, `truehd`, `srt`, `ass`, `pgs`, … An unknown codec keeps the container's own id.
    pub codec: String,
    /// ISO 639-2 (`eng`), or empty when the file does not say.
    pub language: String,
    pub name: String,
    pub default: bool,
    pub forced: bool,
    pub width: u32,
    pub height: u32,
    pub channels: u32,
    /// Dolby Vision configuration present.
    pub dovi: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
    Subtitle,
}

/// What a file holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// `mkv` or `mp4`.
    pub container: &'static str,
    /// Milliseconds, 0 when not said.
    pub duration_ms: u64,
    pub tracks: Vec<Track>,
}

/// The outcome of parsing a window of a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parse {
    Done(Probe),
    /// The answer is at this byte offset, beyond the window given.
    Need(u64),
    /// Not a file either parser reads, or a window that ends inside the answer.
    Unknown,
}

/// Parses `buf`, the bytes of a file starting at offset 0, plus optionally `more` — bytes starting
/// at `more_at`, fetched because an earlier call answered [`Parse::Need`].
pub fn parse(buf: &[u8], more: Option<(u64, &[u8])>) -> Parse {
    if buf.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        parse_mkv(buf, more)
    } else if buf.get(4..8) == Some(b"ftyp") {
        parse_mp4(buf, more)
    } else {
        Parse::Unknown
    }
}

// ---------------------------------------------------------------- Matroska

/// An EBML variable-length integer at `buf[at..]`: (value, length). For an element id the marker
/// bit is kept (ids are spelled with it); for a size it is removed.
fn vint(buf: &[u8], at: usize, keep_marker: bool) -> Option<(u64, usize)> {
    let first = *buf.get(at)?;
    let len = first.leading_zeros() as usize + 1;
    if len > 8 {
        return None;
    }
    let mut v = if keep_marker { u64::from(first) } else { u64::from(first) & ((1u64 << (8 - len)) - 1) };
    for i in 1..len {
        v = (v << 8) | u64::from(*buf.get(at + i)?);
    }
    Some((v, len))
}

/// The "unknown size" spelling: every value bit set.
fn unknown_size(size: u64, len: usize) -> bool {
    size == (1u64 << (7 * len)) - 1
}

/// One element header: id, data start, data end (`None` when the size is unknown).
fn element(buf: &[u8], at: usize) -> Option<(u32, usize, Option<usize>)> {
    let (id, idl) = vint(buf, at, true)?;
    let (size, sl) = vint(buf, at + idl, false)?;
    let start = at + idl + sl;
    let end = if unknown_size(size, sl) { None } else { Some(start.checked_add(usize::try_from(size).ok()?)?) };
    Some((u32::try_from(id).ok()?, start, end))
}

fn uint(data: &[u8]) -> u64 {
    data.iter().take(8).fold(0u64, |v, b| (v << 8) | u64::from(*b))
}

fn float(data: &[u8]) -> f64 {
    match data.len() {
        4 => f64::from(f32::from_be_bytes([data[0], data[1], data[2], data[3]])),
        8 => f64::from_be_bytes(data.try_into().unwrap_or([0; 8])),
        _ => 0.0,
    }
}

fn text(data: &[u8]) -> String {
    String::from_utf8_lossy(data).trim_end_matches('\0').trim().to_string()
}

/// The children of the element whose data is `buf[start..end]`, as (id, data start, data end).
/// Stops at the first child that does not fit: the window ended.
fn children(buf: &[u8], start: usize, end: usize) -> Vec<(u32, usize, usize)> {
    let mut out = Vec::new();
    let mut at = start;
    while at < end {
        let Some((id, s, e)) = element(buf, at) else { break };
        let e = e.unwrap_or(end).min(end);
        if e > buf.len() {
            break;
        }
        out.push((id, s, e));
        at = e;
    }
    out
}

fn mkv_codec(id: &str) -> String {
    let c = match id {
        "V_MPEG4/ISO/AVC" => "h264",
        "V_MPEGH/ISO/HEVC" => "hevc",
        "V_AV1" => "av1",
        "V_VP9" => "vp9",
        "V_VP8" => "vp8",
        "V_MPEG2" => "mpeg2video",
        "A_AAC" => "aac",
        "A_AC3" => "ac3",
        "A_EAC3" => "eac3",
        "A_DTS" | "A_DTS/EXPRESS" | "A_DTS/LOSSLESS" => "dts",
        "A_TRUEHD" => "truehd",
        "A_OPUS" => "opus",
        "A_FLAC" => "flac",
        "A_VORBIS" => "vorbis",
        "A_MPEG/L3" => "mp3",
        "S_TEXT/UTF8" | "S_TEXT/ASCII" => "srt",
        "S_TEXT/ASS" | "S_TEXT/SSA" | "S_ASS" | "S_SSA" => "ass",
        "S_TEXT/WEBVTT" => "webvtt",
        "S_HDMV/PGS" => "pgs",
        "S_VOBSUB" => "vobsub",
        _ if id.starts_with("A_AAC") => "aac",
        _ => return id.to_ascii_lowercase(),
    };
    c.to_string()
}

fn mkv_track(buf: &[u8], start: usize, end: usize) -> Option<Track> {
    let mut kind = None;
    let mut t = Track { kind: TrackKind::Video, codec: String::new(), language: String::new(),
        name: String::new(), default: true, forced: false, width: 0, height: 0, channels: 0, dovi: false };
    let mut language_ietf = String::new();
    for (id, s, e) in children(buf, start, end) {
        let d = &buf[s..e];
        match id {
            0x83 => {
                kind = match uint(d) {
                    1 => Some(TrackKind::Video),
                    2 => Some(TrackKind::Audio),
                    17 => Some(TrackKind::Subtitle),
                    _ => None,
                }
            }
            0x86 => t.codec = mkv_codec(&text(d)),
            0x22B59C => t.language = text(d),
            0x22B59D => language_ietf = text(d),
            0x536E => t.name = text(d),
            0x88 => t.default = uint(d) != 0,
            0x55AA => t.forced = uint(d) != 0,
            0xE0 => {
                for (vid, vs, ve) in children(buf, s, e) {
                    match vid {
                        0xB0 => t.width = uint(&buf[vs..ve]) as u32,
                        0xBA => t.height = uint(&buf[vs..ve]) as u32,
                        _ => {}
                    }
                }
            }
            0xE1 => {
                for (aid, as_, ae) in children(buf, s, e) {
                    if aid == 0x9F {
                        t.channels = uint(&buf[as_..ae]) as u32;
                    }
                }
            }
            // BlockAdditionMapping: a Dolby Vision configuration record rides here.
            0x41E4 => {
                for (bid, bs, be) in children(buf, s, e) {
                    if bid == 0x41E7 && matches!(uint(&buf[bs..be]), 0x6476_6343 | 0x6476_7643) {
                        t.dovi = true;
                    }
                }
            }
            _ => {}
        }
    }
    t.kind = kind?;
    if t.language.is_empty() || t.language == "und" {
        // The IETF tag's primary subtag, as the three-letter code the rest of the app reads.
        t.language = match language_ietf.split('-').next().unwrap_or("") {
            "en" => "eng".into(),
            "ja" => "jpn".into(),
            "fr" => "fre".into(),
            "de" => "ger".into(),
            "es" => "spa".into(),
            "it" => "ita".into(),
            "" => String::new(),
            other => other.into(),
        };
    }
    if t.language == "und" {
        t.language.clear();
    }
    Some(t)
}

/// Reads a Matroska file's tracks and duration from its first bytes.
pub fn parse_mkv(buf: &[u8], more: Option<(u64, &[u8])>) -> Parse {
    // The EBML header, then the Segment.
    let Some((_, _, Some(header_end))) = element(buf, 0) else { return Parse::Unknown };
    let Some((0x1853_8067, seg_start, seg_end)) = element(buf, header_end) else { return Parse::Unknown };
    let seg_end = seg_end.unwrap_or(usize::MAX);
    let mut tracks = None;
    let mut duration_ms = 0u64;
    let mut tracks_at: Option<u64> = None;
    for (id, s, e) in children(buf, seg_start, seg_end.min(buf.len())) {
        match id {
            0x1549_A966 => {
                let mut scale = 1_000_000.0;
                let mut dur = 0.0;
                for (iid, is, ie) in children(buf, s, e) {
                    match iid {
                        0x2A_D7B1 => scale = uint(&buf[is..ie]) as f64,
                        0x4489 => dur = float(&buf[is..ie]),
                        _ => {}
                    }
                }
                duration_ms = (dur * scale / 1_000_000.0) as u64;
            }
            0x1654_AE6B => {
                tracks = Some(children(buf, s, e).into_iter()
                    .filter(|(tid, _, _)| *tid == 0xAE)
                    .filter_map(|(_, ts, te)| mkv_track(buf, ts, te))
                    .collect::<Vec<_>>());
            }
            0x114D_9B74 => {
                for (sid, ss, se) in children(buf, s, e) {
                    if sid != 0x4DBB {
                        continue;
                    }
                    let (mut which, mut pos) = (0u64, None);
                    for (kid, ks, ke) in children(buf, ss, se) {
                        match kid {
                            0x53AB => which = uint(&buf[ks..ke]),
                            0x53AC => pos = Some(uint(&buf[ks..ke])),
                            _ => {}
                        }
                    }
                    if which == 0x1654_AE6B {
                        tracks_at = pos.map(|p| seg_start as u64 + p);
                    }
                }
            }
            // The media begins: everything that describes it came before.
            0x1F43_B675 => break,
            _ => {}
        }
    }
    if tracks.is_none() {
        if let Some((at, bytes)) = more {
            if let Some(want) = tracks_at.filter(|w| *w >= at) {
                let off = (want - at) as usize;
                if let Some((0x1654_AE6B, s, Some(e))) = element(bytes, off) {
                    if e <= bytes.len() {
                        tracks = Some(children(bytes, s, e).into_iter()
                            .filter(|(tid, _, _)| *tid == 0xAE)
                            .filter_map(|(_, ts, te)| mkv_track(bytes, ts, te))
                            .collect());
                    }
                }
            }
        }
    }
    match (tracks, tracks_at) {
        (Some(tracks), _) => Parse::Done(Probe { container: "mkv", duration_ms, tracks }),
        (None, Some(at)) if more.is_none() => Parse::Need(at),
        _ => Parse::Unknown,
    }
}

// ---------------------------------------------------------------- MP4

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// The boxes in `b[start..end]`, as (type, data start, data end). Stops where the window ends.
fn boxes(b: &[u8], start: usize, end: usize) -> Vec<([u8; 4], usize, usize)> {
    let mut out = Vec::new();
    let mut at = start;
    while at + 8 <= end {
        let Some(size32) = u32_at(b, at) else { break };
        let Some(kind) = b.get(at + 4..at + 8).and_then(|k| <[u8; 4]>::try_from(k).ok()) else { break };
        let (size, header) = match size32 {
            1 => match u64_at(b, at + 8) {
                Some(s) => (s as usize, 16),
                None => break,
            },
            0 => (end - at, 8),
            s => (s as usize, 8),
        };
        if size < header || at + size > end {
            out.push((kind, at + header, end));
            break;
        }
        out.push((kind, at + header, at + size));
        at += size;
    }
    out
}

fn find(b: &[u8], start: usize, end: usize, kind: &[u8; 4]) -> Option<(usize, usize)> {
    boxes(b, start, end).into_iter().find(|(k, _, _)| k == kind).map(|(_, s, e)| (s, e))
}

fn mp4_codec(fourcc: &[u8; 4]) -> String {
    match fourcc {
        b"avc1" | b"avc3" => "h264".into(),
        b"hvc1" | b"hev1" => "hevc".into(),
        b"dvh1" | b"dvhe" => "hevc".into(),
        b"av01" => "av1".into(),
        b"vp09" => "vp9".into(),
        b"mp4a" => "aac".into(),
        b"ac-3" => "ac3".into(),
        b"ec-3" => "eac3".into(),
        b"dtsc" | b"dtsh" | b"dtsl" => "dts".into(),
        b"Opus" => "opus".into(),
        b"fLaC" => "flac".into(),
        b"tx3g" => "mov_text".into(),
        b"wvtt" => "webvtt".into(),
        other => String::from_utf8_lossy(other).trim().to_string(),
    }
}

fn mp4_track(b: &[u8], s: usize, e: usize) -> Option<Track> {
    let (mdia_s, mdia_e) = find(b, s, e, b"mdia")?;
    let (hdlr_s, _) = find(b, mdia_s, mdia_e, b"hdlr")?;
    let kind = match b.get(hdlr_s + 8..hdlr_s + 12)? {
        b"vide" => TrackKind::Video,
        b"soun" => TrackKind::Audio,
        b"sbtl" | b"subt" | b"text" => TrackKind::Subtitle,
        _ => return None,
    };
    // mdhd: version, then (32- or 64-bit times), then the packed ISO 639-2 language.
    let language = find(b, mdia_s, mdia_e, b"mdhd").and_then(|(ms, _)| {
        let at = if *b.get(ms)? == 1 { ms + 32 } else { ms + 20 };
        let packed = u16::from_be_bytes(b.get(at..at + 2)?.try_into().ok()?);
        let c = |shift: u16| (((packed >> shift) & 0x1F) as u8 + 0x60) as char;
        let lang: String = [c(10), c(5), c(0)].iter().collect();
        (lang != "und").then_some(lang)
    }).unwrap_or_default();
    let (minf_s, minf_e) = find(b, mdia_s, mdia_e, b"minf")?;
    let (stbl_s, stbl_e) = find(b, minf_s, minf_e, b"stbl")?;
    let (stsd_s, stsd_e) = find(b, stbl_s, stbl_e, b"stsd")?;
    // stsd: version+flags, entry count, then the first sample entry.
    let entries = boxes(b, stsd_s + 8, stsd_e);
    let (fourcc, es, ee) = *entries.first()?;
    let mut t = Track { kind, codec: mp4_codec(&fourcc), language, name: String::new(), default: true,
        forced: false, width: 0, height: 0, channels: 0, dovi: matches!(&fourcc, b"dvh1" | b"dvhe") };
    match kind {
        TrackKind::Video => {
            // Visual sample entry: width and height at +24 and +26 of its data.
            t.width = u32::from(u16::from_be_bytes(b.get(es + 24..es + 26)?.try_into().ok()?));
            t.height = u32::from(u16::from_be_bytes(b.get(es + 26..es + 28)?.try_into().ok()?));
            t.dovi |= boxes(b, (es + 78).min(ee), ee).iter().any(|(k, _, _)| k == b"dvcC" || k == b"dvvC");
        }
        TrackKind::Audio => {
            t.channels = u32::from(u16::from_be_bytes(b.get(es + 16..es + 18)?.try_into().ok()?));
        }
        TrackKind::Subtitle => {}
    }
    Some(t)
}

/// Reads an MP4 file's tracks and duration from its first bytes.
pub fn parse_mp4(buf: &[u8], more: Option<(u64, &[u8])>) -> Parse {
    let read = |b: &[u8], s: usize, e: usize| -> Probe {
        let duration_ms = find(b, s, e, b"mvhd").and_then(|(ms, _)| {
            let v1 = *b.get(ms)? == 1;
            let (scale_at, dur) = if v1 { (ms + 20, u64_at(b, ms + 24)?) } else { (ms + 12, u64::from(u32_at(b, ms + 16)?)) };
            let scale = u64::from(u32_at(b, scale_at)?).max(1);
            Some(dur * 1000 / scale)
        }).unwrap_or(0);
        let tracks = boxes(b, s, e).into_iter()
            .filter(|(k, _, _)| k == b"trak")
            .filter_map(|(_, ts, te)| mp4_track(b, ts, te))
            .collect();
        Probe { container: "mp4", duration_ms, tracks }
    };
    if let Some((_, bytes)) = more {
        // `bytes` begins at the `moov` an earlier window named. A `moov` larger than the window
        // still yields the tracks that fit, which is most of them: `stsd` comes early in each.
        return match boxes(bytes, 0, bytes.len()).iter().find(|(k, _, _)| k == b"moov") {
            Some((_, s, e)) => Parse::Done(read(bytes, *s, *e)),
            None => Parse::Unknown,
        };
    }
    // Walk the top-level boxes by their declared sizes.
    let mut at = 0u64;
    loop {
        let Ok(i) = usize::try_from(at) else { return Parse::Unknown };
        if i + 8 > buf.len() {
            return Parse::Unknown;
        }
        let (size, header) = match u32_at(buf, i) {
            Some(1) => match u64_at(buf, i + 8) {
                Some(s) => (s, 16u64),
                None => return Parse::Unknown,
            },
            Some(0) | None => return Parse::Unknown,
            Some(s) => (u64::from(s), 8u64),
        };
        if size < header {
            return Parse::Unknown;
        }
        let end = at.saturating_add(size);
        if &buf[i + 4..i + 8] == b"moov" {
            // The whole `moov` is here, or the window has to start at it.
            return if end <= buf.len() as u64 {
                Parse::Done(read(buf, i + header as usize, end as usize))
            } else {
                Parse::Need(at)
            };
        }
        if end > buf.len() as u64 {
            // The media comes first: the next box, where `moov` should be, starts beyond the window.
            return Parse::Need(end);
        }
        at = end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An EBML element with a one-byte-length size (data under 127 bytes) or an eight-byte one.
    fn el(id: u32, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let idb = id.to_be_bytes();
        let skip = idb.iter().take_while(|b| **b == 0).count();
        out.extend_from_slice(&idb[skip..]);
        if data.len() < 127 {
            out.push(0x80 | data.len() as u8);
        } else {
            out.push(0x01);
            out.extend_from_slice(&(data.len() as u64).to_be_bytes()[1..]);
        }
        out.extend_from_slice(data);
        out
    }

    fn cat(parts: &[Vec<u8>]) -> Vec<u8> {
        parts.concat()
    }

    fn track(number: u8, kind: u8, codec: &str, lang: &str, extra: Vec<u8>) -> Vec<u8> {
        el(0xAE, &cat(&[el(0xD7, &[number]), el(0x83, &[kind]), el(0x86, codec.as_bytes()),
                        el(0x22B59C, lang.as_bytes()), extra]))
    }

    fn mkv(segment_children: Vec<u8>) -> Vec<u8> {
        cat(&[el(0x1A45DFA3, &el(0x4282, b"matroska")), el(0x18538067, &segment_children)])
    }

    fn sample_tracks() -> Vec<u8> {
        el(0x1654AE6B, &cat(&[
            track(1, 1, "V_MPEGH/ISO/HEVC", "und",
                  el(0xE0, &cat(&[el(0xB0, &[0x0F, 0x00]), el(0xBA, &[0x08, 0x70])]))),
            track(2, 2, "A_TRUEHD", "eng", el(0xE1, &el(0x9F, &[8]))),
            track(3, 2, "A_AC3", "eng", el(0xE1, &el(0x9F, &[6]))),
            track(4, 17, "S_TEXT/UTF8", "eng", el(0x55AA, &[1])),
            track(5, 17, "S_HDMV/PGS", "fre", Vec::new()),
        ]))
    }

    #[test]
    fn a_matroska_files_tracks_and_duration_are_read() {
        let info = el(0x1549A966, &cat(&[el(0x2AD7B1, &[0x0F, 0x42, 0x40]),
                                         el(0x4489, &7_200_000f64.to_be_bytes())]));
        let file = mkv(cat(&[info, sample_tracks(), el(0x1F43B675, &[0; 16])]));
        let Parse::Done(p) = parse(&file, None) else { panic!("parsed") };
        assert_eq!(p.container, "mkv");
        assert_eq!(p.duration_ms, 7_200_000);
        let summary: Vec<(TrackKind, &str, &str)> =
            p.tracks.iter().map(|t| (t.kind, t.codec.as_str(), t.language.as_str())).collect();
        assert_eq!(summary, vec![
            (TrackKind::Video, "hevc", ""),
            (TrackKind::Audio, "truehd", "eng"),
            (TrackKind::Audio, "ac3", "eng"),
            (TrackKind::Subtitle, "srt", "eng"),
            (TrackKind::Subtitle, "pgs", "fre"),
        ]);
        assert_eq!((p.tracks[0].width, p.tracks[0].height), (3840, 2160));
        assert_eq!(p.tracks[2].channels, 6);
        assert!(p.tracks[3].forced);
    }

    #[test]
    fn tracks_past_the_window_are_found_through_the_seek_head() {
        // A SeekHead naming Tracks at segment offset 1000, then media; Tracks itself is elsewhere.
        let seek = el(0x114D9B74, &el(0x4DBB, &cat(&[el(0x53AB, &[0x16, 0x54, 0xAE, 0x6B]),
                                                       el(0x53AC, &[0x03, 0xE8])])));
        let file = mkv(cat(&[seek, el(0x1F43B675, &[0; 16])]));
        let Parse::Need(at) = parse(&file, None) else { panic!("needs more") };
        assert!(at > 1000);
        let tail = sample_tracks();
        let Parse::Done(p) = parse(&file, Some((at, &tail))) else { panic!("parsed with more") };
        assert_eq!(p.tracks.len(), 5);
    }

    #[test]
    fn something_else_is_unknown() {
        assert_eq!(parse(b"<html>not a video</html>", None), Parse::Unknown);
    }

    fn bx(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = ((data.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        out
    }

    fn mp4_trak(handler: &[u8; 4], lang: &str, entry: Vec<u8>) -> Vec<u8> {
        let l = lang.as_bytes();
        let packed = ((u16::from(l[0]) - 0x60) << 10) | ((u16::from(l[1]) - 0x60) << 5) | (u16::from(l[2]) - 0x60);
        let mut mdhd = vec![0u8; 20];
        mdhd.extend_from_slice(&packed.to_be_bytes());
        mdhd.extend_from_slice(&[0, 0]);
        let mut hdlr = vec![0u8; 8];
        hdlr.extend_from_slice(handler);
        hdlr.extend_from_slice(&[0; 12]);
        let mut stsd = vec![0, 0, 0, 0, 0, 0, 0, 1];
        stsd.extend_from_slice(&entry);
        bx(b"trak", &bx(b"mdia", &cat(&[bx(b"mdhd", &mdhd), bx(b"hdlr", &hdlr),
            bx(b"minf", &bx(b"stbl", &bx(b"stsd", &stsd)))])))
    }

    #[test]
    fn an_mp4_with_its_moov_first_is_read() {
        let mut video = vec![0u8; 24];
        video.extend_from_slice(&1920u16.to_be_bytes());
        video.extend_from_slice(&1080u16.to_be_bytes());
        video.extend_from_slice(&[0; 50]);
        let mut audio = vec![0u8; 16];
        audio.extend_from_slice(&2u16.to_be_bytes());
        audio.extend_from_slice(&[0; 10]);
        let mut mvhd = vec![0u8; 12];
        mvhd.extend_from_slice(&1000u32.to_be_bytes());
        mvhd.extend_from_slice(&5_400_000u32.to_be_bytes());
        let file = cat(&[bx(b"ftyp", b"isom\0\0\0\0"), bx(b"moov", &cat(&[bx(b"mvhd", &mvhd),
            mp4_trak(b"vide", "und", bx(b"avc1", &video)), mp4_trak(b"soun", "eng", bx(b"mp4a", &audio))])),
            bx(b"mdat", &[0; 32])]);
        let Parse::Done(p) = parse(&file, None) else { panic!("parsed") };
        assert_eq!(p.container, "mp4");
        assert_eq!(p.duration_ms, 5_400_000);
        assert_eq!(p.tracks[0].codec, "h264");
        assert_eq!((p.tracks[0].width, p.tracks[0].height), (1920, 1080));
        assert_eq!((p.tracks[1].codec.as_str(), p.tracks[1].language.as_str(), p.tracks[1].channels), ("aac", "eng", 2));
    }
}

#[cfg(test)]
mod mp4_tail_tests {
    use super::*;

    #[test]
    fn an_mp4_whose_moov_comes_last_names_where_it_starts() {
        // ftyp (16 bytes), then an mdat declaring 1 MB that the window does not hold.
        let mut file = vec![0, 0, 0, 16];
        file.extend_from_slice(b"ftypisom\0\0\0\0");
        file.extend_from_slice(&(1_048_576u32 + 8).to_be_bytes());
        file.extend_from_slice(b"mdat");
        file.extend_from_slice(&[0; 64]);
        assert_eq!(parse(&file, None), Parse::Need(16 + 1_048_576 + 8));
    }
}

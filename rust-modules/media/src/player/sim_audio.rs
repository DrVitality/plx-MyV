//! **The simulator's sound** (armed with the picture, `plxnative-simvideo`): the audio access
//! units the clock sink already accepts are ALSO piped to a system `ffplay` child process, which
//! decodes and plays them on the computer's speakers — the sound half of [`super::sim_video`].
//!
//! **Why it can be this small.** The demuxer hands the sink self-framing audio: AC-3, E-AC-3,
//! ADTS-framed AAC or a DTS core (`player/CLAUDE.md`, "Pipeline"). Each is a raw stream `ffplay`
//! reads from a pipe with nothing but its format name, so no container, no timestamps and no
//! decoder live in this process — exactly the reasoning `sim_video` gives for its child.
//!
//! **Sync, roughly.** A raw stream carries no clock, so the writer thread holds each access unit
//! until the sink's clock is within [`LEAD_NS`] of its PTS: the child is never more than that far
//! ahead of the picture, a paused player stops feeding it (the child drains what it holds and goes
//! quiet), and a seek or a stop kills it so nothing stale plays on. It is a desk convenience, not a
//! measurement: lip sync is within the lead and the output device's own latency, nothing better.
//!
//! **Not the television.** `hostsim`-only, like `sim_video`; the TV's audio is the pipeline's.

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Mutex, OnceLock};

/// How far ahead of the clock audio is handed to the child.
const LEAD_NS: i64 = 250_000_000;

/// Armed with the picture: the same trigger, read once.
fn armed() -> bool {
    static ONCE: OnceLock<bool> = OnceLock::new();
    *ONCE.get_or_init(|| plx_base::devtrig::flag("simvideo"))
}

/// One playing child and the thread feeding it.
struct Session {
    child: Child,
    feed: Sender<(Vec<u8>, i64)>,
}

impl Drop for Session {
    fn drop(&mut self) {
        // what the child still holds belongs to a timeline that has ended
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

static SESSION: Mutex<Option<Session>> = Mutex::new(None);
/// Bumped on every reset, so a feeder from an older session stops.
static GENERATION: AtomicU64 = AtomicU64::new(0);
/// The generation whose child could not start (`u64::MAX` = none), so feeding stops trying.
static DEAD: AtomicU64 = AtomicU64::new(u64::MAX);
/// The raw format `ffplay` reads, from the Load payload; `None` = no audio this session.
static FORMAT: Mutex<Option<&'static str>> = Mutex::new(None);
static CLOCK: OnceLock<fn() -> i64> = OnceLock::new();

/// The raw-stream format for the payload's audio codec (`engine.rs`: "AC3", "AC3 PLUS", "AAC",
/// "DTS"); `None` for a video-only Load.
fn format_of(payload: &str) -> Option<&'static str> {
    let at = payload.find(r#""audio":""#)? + r#""audio":""#.len();
    let codec = &payload[at..at + payload[at..].find('"')?];
    match codec {
        "AC3" => Some("ac3"),
        "AC3 PLUS" => Some("eac3"),
        "AAC" => Some("aac"),
        "DTS" => Some("dts"),
        _ => None,
    }
}

/// A Load: learn the codec, drop whatever was playing.
pub fn load(payload: &str, clock: fn() -> i64) {
    if !armed() {
        return;
    }
    let _ = CLOCK.set(clock);
    *FORMAT.lock().unwrap_or_else(|e| e.into_inner()) = format_of(payload);
    reset();
}

/// A flush or a stop: the fed timeline restarts, or ends.
pub fn reset() {
    if !armed() {
        return;
    }
    GENERATION.fetch_add(1, Relaxed);
    *SESSION.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// One audio access unit at `pts` in the fed timeline. Never blocks the feeder.
pub fn feed(au: &[u8], pts: i64) {
    if !armed() || DEAD.load(Relaxed) == GENERATION.load(Relaxed) {
        return;
    }
    let Some(format) = *FORMAT.lock().unwrap_or_else(|e| e.into_inner()) else { return };
    let mut guard = SESSION.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = spawn(format);
    }
    let Some(session) = guard.as_ref() else { return };
    if session.feed.send((au.to_vec(), pts)).is_err() {
        *guard = None;
    }
}

fn ffplay_path() -> std::ffi::OsString {
    std::env::var_os("PLXNATIVE_SIM_FFPLAY").unwrap_or_else(|| "ffplay".into())
}

fn spawn(format: &'static str) -> Option<Session> {
    let generation = GENERATION.load(Relaxed);
    let spawned = Command::new(ffplay_path())
        .args(["-hide_banner", "-loglevel", "error", "-nodisp", "-autoexit", "-fflags", "nobuffer", "-f", format])
        .args(["-i", "pipe:0"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            DEAD.store(generation, Relaxed);
            plx_base::eventlog::log(&format!("simaudio: could not start ffplay ({e}); no sound this session"));
            return None;
        }
    };
    let mut stdin = child.stdin.take()?;
    let (tx, rx) = channel::<(Vec<u8>, i64)>();
    let writer = std::thread::Builder::new().name("simaudio-in".into()).spawn(move || {
        let mut first = true;
        for (au, pts) in rx {
            // hold the unit until the clock is within the lead of it: a paused player waits here
            while CLOCK.get().is_some_and(|clock| clock() + LEAD_NS < pts) {
                if GENERATION.load(Relaxed) != generation {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            if GENERATION.load(Relaxed) != generation || stdin.write_all(&au).is_err() {
                return;
            }
            if first {
                first = false;
                plx_base::eventlog::log(&format!("simaudio: playing {format} through ffplay"));
            }
        }
    });
    if writer.is_err() {
        plx_base::eventlog::log("simaudio: could not spawn the feed thread; no sound this session");
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    Some(Session { child, feed: tx })
}

#[cfg(test)]
mod tests {
    use super::format_of;

    #[test]
    fn the_payload_s_audio_codec_names_the_raw_format() {
        let av = r#"{"contents":{"codec":{"video":"H264","audio":"AC3 PLUS"}}}"#;
        assert_eq!(format_of(av), Some("eac3"));
        assert_eq!(format_of(&av.replace("AC3 PLUS", "AAC")), Some("aac"));
        assert_eq!(format_of(&av.replace("AC3 PLUS", "AC3")), Some("ac3"));
        assert_eq!(format_of(&av.replace("AC3 PLUS", "DTS")), Some("dts"));
        assert_eq!(format_of(r#"{"codec":{"video":"H264"}}"#), None, "a video-only Load has no sound");
    }
}

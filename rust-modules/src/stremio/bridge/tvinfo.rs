//! **About this television, for Settings › Addons & Home**: whether a newer build of this app is
//! published, and how long the set's Developer Mode session has left.
//!
//! - **Updates.** This build's commit (`PLX_BUILD_SHA`, short) against the newest commit of the
//!   branch it is built from, read from GitHub's public Atom feed of that branch ([`UPDATE_FEED`]:
//!   no key, no account). Equal is up to date; anything else is "a newer build is published".
//! - **Developer Mode.** A Developer Mode install stops launching when the set's session runs out
//!   (LG ends it after up to 1000 hours unless it is extended). The session token is the file
//!   [`DEVMODE_TOKEN`] and LG's check endpoint ([`DEVMODE_CHECK`]) answers the time left as
//!   `errorMsg: "HHH:MM:SS"`. Both are what the community's Developer Mode tools use
//!   (webosbrew/dev-manager-desktop#256); neither is documented by LG, and whether a jailed app may
//!   read the token file is not verified on this set — an unreadable token is simply "unknown".
//!   The token is never logged or sent anywhere but LG's own endpoint.
//!
//! Both answers are kept for an hour; a failed read is "unknown", never an error.

use super::Bridge;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// The branch this fork's builds come from, as a public Atom feed of its commits.
const UPDATE_FEED: &str = "https://github.com/DrVitality/plx-MyV/commits/stremio-bridge.atom";
/// Where webOS keeps the Developer Mode session token.
const DEVMODE_TOKEN: &str = "/var/luna/preferences/devmode_enabled";
/// LG's session check: `?sessionToken=…` → `{"result":"success","errorMsg":"HHH:MM:SS"}`.
const DEVMODE_CHECK: &str = "https://developer.lge.com/secure/CheckDevModeSession.dev?sessionToken=";
/// How long one answer is kept.
const TV_INFO_TTL: Duration = Duration::from_secs(3600);
/// Below this many hours left, the first Home of a run warns with a toast.
pub(super) const DEVMODE_WARN_HOURS: i64 = 72;

/// The newest commit id in a GitHub commits Atom feed.
pub(super) fn newest_commit(feed: &str) -> Option<&str> {
    let at = feed.find("Grit::Commit/")? + "Grit::Commit/".len();
    let rest = &feed[at..];
    let end = rest.find(|c: char| !c.is_ascii_hexdigit())?;
    (end >= 7).then(|| &rest[..end])
}

/// Hours left in an `errorMsg` of `"HHH:MM:SS"`.
pub(super) fn hours_left(v: &Value) -> Option<i64> {
    if v.get("result").and_then(Value::as_str) != Some("success") {
        return None;
    }
    let msg = v.get("errorMsg").and_then(Value::as_str)?;
    let mut parts = msg.split(':');
    let h: i64 = parts.next()?.trim().parse().ok()?;
    let m: i64 = parts.next()?.trim().parse().ok()?;
    parts.next()?;
    (h >= 0 && (0..60).contains(&m)).then_some(h)
}

impl Bridge {
    /// `{"update": "current" | "available" | "unknown", "devmode_hours": n | null}`, from the
    /// hour-old answers when fresh.
    pub(super) fn tv_info(&self) -> Value {
        {
            let memo = self.tv_info.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((_, v)) = memo.as_ref().filter(|(at, _)| at.elapsed() < TV_INFO_TTL) {
                return v.clone();
            }
        }
        let update = match self.stremio.fetch(UPDATE_FEED).ok().and_then(|b| String::from_utf8(b).ok()) {
            Some(feed) => match (newest_commit(&feed), env!("PLX_BUILD_SHA")) {
                (_, "unknown") | (None, _) => "unknown",
                (Some(newest), ours) if newest.starts_with(ours) => "current",
                _ => "available",
            },
            None => "unknown",
        };
        let hours = if cfg!(test) {
            None
        } else {
            std::fs::read_to_string(DEVMODE_TOKEN).ok()
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty() && t.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)))
                .and_then(|t| self.stremio.fetch(&format!("{DEVMODE_CHECK}{t}")).ok())
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .and_then(|v| hours_left(&v))
        };
        let v = json!({"update": update, "devmode_hours": hours});
        *self.tv_info.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), v.clone()));
        v
    }

    /// Once a run, in the background: warn when Developer Mode is about to end.
    pub(super) fn devmode_reminder(&self) {
        if self.devmode_warned.swap(true, std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let Some(me) = self.me.get().and_then(std::sync::Weak::upgrade) else { return };
        let _ = plx_base::task::spawn("stremio-devmode", move || {
            let hours = me.tv_info().get("devmode_hours").and_then(Value::as_i64);
            if let Some(h) = hours.filter(|h| *h < DEVMODE_WARN_HOURS) {
                plx_base::eventlog::log(&format!("stremio: Developer Mode has {h} h left"));
                let _ = plx_platform::tv::toast::toast(&plx_platform::i18n::msg::settings_stremio_devmode_toast(&h.to_string()));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_newest_commit_is_read_from_the_feed() {
        let feed = "<feed><entry><id>tag:github.com,2008:Grit::Commit/36e8974e48c8b1a7220509a4629e59112bf25899</id></entry>\
                    <entry><id>tag:github.com,2008:Grit::Commit/07c8f96aaaa</id></entry></feed>";
        assert_eq!(newest_commit(feed), Some("36e8974e48c8b1a7220509a4629e59112bf25899"));
        assert_eq!(newest_commit("<feed/>"), None);
    }

    #[test]
    fn the_time_left_is_read_in_hours() {
        assert_eq!(hours_left(&json!({"result": "success", "errorCode": "200", "errorMsg": "999:58:40"})), Some(999));
        assert_eq!(hours_left(&json!({"result": "success", "errorMsg": "12:05:00"})), Some(12));
        assert_eq!(hours_left(&json!({"result": "fail", "errorMsg": "Invalid token"})), None);
        assert_eq!(hours_left(&json!({"result": "success", "errorMsg": "soon"})), None);
    }
}

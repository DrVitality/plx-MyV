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
/// Real-Debrid's account record: `?auth_token=` the key (documented as an alternative to the
/// bearer header), `premium` = seconds of premium left.
const DEBRID_USER: &str = "https://api.real-debrid.com/rest/1.0/user?auth_token=";
/// Below this many days of premium left, the first Home of a run warns with a toast.
const DEBRID_WARN_DAYS: i64 = 7;

/// The Real-Debrid key a stream addon is configured with: Torrentio's `realdebrid=KEY` option in
/// its address, or a configuration segment that is base64 JSON naming `realdebrid` and its
/// `debridApiKey` (Comet's shape). Never logged.
pub(super) fn debrid_key(addons: &[super::super::api::Addon]) -> Option<String> {
    let ok = |k: &str| (16..=128).contains(&k.len()) && k.chars().all(|c| c.is_ascii_alphanumeric());
    for a in addons {
        let url = a.transport_url.replace("%7C", "|").replace("%7c", "|");
        if let Some(at) = url.find("realdebrid=") {
            let key: String = url[at + "realdebrid=".len()..].chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
            if ok(&key) {
                return Some(key);
            }
        }
        for seg in url.split('/') {
            let Some(bytes) = super::watched::base64_decode(seg).filter(|b| b.first() == Some(&b'{')) else { continue };
            let Ok(v) = serde_json::from_slice::<Value>(&bytes) else { continue };
            let service = v.get("debridService").or_else(|| v.get("debrid_service")).and_then(Value::as_str).unwrap_or("");
            let key = v.get("debridApiKey").or_else(|| v.get("debrid_api_key")).and_then(Value::as_str).unwrap_or("");
            if service.eq_ignore_ascii_case("realdebrid") && ok(key) {
                return Some(key.to_string());
            }
        }
    }
    None
}

/// Whole days of premium left in Real-Debrid's `/user` answer (0 on a free account).
pub(super) fn debrid_days_left(v: &Value) -> Option<i64> {
    let premium = v.get("premium").and_then(Value::as_i64)?;
    Some(premium.max(0) / 86_400)
}

/// "A, B +3" from new episode names: the first two, and how many more.
pub(super) fn announcement(names: &[String]) -> Option<String> {
    match names {
        [] => None,
        [a] => Some(a.clone()),
        [a, b] => Some(format!("{a}, {b}")),
        [a, b, rest @ ..] => Some(format!("{a}, {b} +{}", rest.len())),
    }
}

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
    pub(super) fn tv_info(&self, token: &str) -> Value {
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
        // Real-Debrid's premium days, with the key the account's stream addon is configured with
        let debrid_days = self.addons(token).ok()
            .and_then(|addons| debrid_key(&addons))
            .and_then(|key| self.stremio.fetch(&format!("{DEBRID_USER}{key}")).ok())
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| debrid_days_left(&v));
        let v = json!({"update": update, "devmode_hours": hours, "debrid_days": debrid_days});
        *self.tv_info.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), v.clone()));
        v
    }

    /// Once a run, in the background: warn when Developer Mode or Real-Debrid is about to end,
    /// and announce the episodes of saved shows released since the last announcement.
    pub(super) fn start_reminders(&self, token: &str) {
        if self.devmode_warned.swap(true, std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let Some(me) = self.me.get().and_then(std::sync::Weak::upgrade) else { return };
        let token = token.to_string();
        let _ = plx_base::task::spawn("stremio-reminders", move || {
            let info = me.tv_info(&token);
            if let Some(h) = info.get("devmode_hours").and_then(Value::as_i64).filter(|h| *h < DEVMODE_WARN_HOURS) {
                plx_base::eventlog::log(&format!("stremio: Developer Mode has {h} h left"));
                let _ = plx_platform::tv::toast::toast(&plx_platform::i18n::msg::settings_stremio_devmode_toast(&h.to_string()));
            }
            if let Some(d) = info.get("debrid_days").and_then(Value::as_i64).filter(|d| *d < DEBRID_WARN_DAYS) {
                plx_base::eventlog::log(&format!("stremio: Real-Debrid premium has {d} days left"));
                let _ = plx_platform::tv::toast::toast(&plx_platform::i18n::msg::settings_stremio_debrid_toast(&d.to_string()));
            }
            if let Some(line) = me.new_episode_announcement(&token) {
                let _ = plx_platform::tv::toast::toast(&plx_platform::i18n::msg::settings_stremio_new_episodes_toast(&line));
            }
        });
    }

    /// The episodes of saved shows released since the last announcement, as one line
    /// ("Severance S2·E4, Andor S2·E1 +2"), recording today as announced. `None` on the first run
    /// (nothing is announced then: everything would be new) or when nothing is new.
    pub(super) fn new_episode_announcement(&self, token: &str) -> Option<String> {
        let today = super::content::iso_day_pub(0);
        let since = self.with_config(|c| c.announced_through.clone());
        if since == today {
            return None;
        }
        self.change_config(|c| c.announced_through = today.clone());
        if since.is_empty() {
            return None;
        }
        let addons = self.addons(token).ok()?;
        let fresh: Vec<String> = self.new_episode_rows(token, &addons).iter()
            .filter(|r| r.get("originallyAvailableAt").and_then(Value::as_str).is_some_and(|d| d > since.as_str()))
            .filter_map(|r| Some(format!("{} S{}·E{}", r.get("grandparentTitle")?.as_str()?,
                r.get("parentIndex")?.as_i64()?, r.get("index")?.as_i64()?)))
            .collect();
        announcement(&fresh)
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
    fn the_debrid_key_is_found_in_torrentio_and_comet_addresses_and_days_are_whole() {
        let addon = |url: &str| -> super::super::super::api::Addon {
            serde_json::from_value(json!({"transportUrl": url, "manifest": {"id": "x", "name": "X"}})).unwrap()
        };
        let key = "ABCDEFGHIJKLMNOPQRSTUVWXYZ012345";
        let torrentio = addon(&format!("https://torrentio.strem.fun/sort=qualitysize%7Crealdebrid={key}/manifest.json"));
        assert_eq!(debrid_key(&[torrentio]).as_deref(), Some(key));
        let config = format!(r#"{{"debridService":"realdebrid","debridApiKey":"{key}"}}"#);
        let b64: String = {
            const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            config.as_bytes().chunks(3).flat_map(|c| {
                let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
                (0..4).map(move |k| if k <= c.len() { A[((n >> (18 - 6 * k)) & 63) as usize] as char } else { '=' })
            }).collect()
        };
        let comet = addon(&format!("https://comet.example/{b64}/manifest.json"));
        assert_eq!(debrid_key(&[comet]).as_deref(), Some(key));
        assert_eq!(debrid_key(&[addon("https://v3-cinemeta.strem.io/manifest.json")]), None);
        assert_eq!(debrid_days_left(&json!({"premium": 86_400 * 12 + 5})), Some(12));
        assert_eq!(announcement(&["A S1·E2".into(), "B S2·E1".into(), "C".into(), "D".into()]).as_deref(), Some("A S1·E2, B S2·E1 +2"));
    }

    #[test]
    fn the_time_left_is_read_in_hours() {
        assert_eq!(hours_left(&json!({"result": "success", "errorCode": "200", "errorMsg": "999:58:40"})), Some(999));
        assert_eq!(hours_left(&json!({"result": "success", "errorMsg": "12:05:00"})), Some(12));
        assert_eq!(hours_left(&json!({"result": "fail", "errorMsg": "Invalid token"})), None);
        assert_eq!(hours_left(&json!({"result": "success", "errorMsg": "soon"})), None);
    }
}

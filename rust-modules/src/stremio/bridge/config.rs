//! **What the viewer has told the bridge**, kept in one small file beside the session: which rows
//! Home shows and in what order, which addons are used for streams and for catalogs, and, per
//! show, the kind of stream last played (so the next episode picks the same release).
//!
//! Stremio keeps none of this per device, so it is the bridge's own, like the playback
//! preferences it already holds. A missing or unreadable file is the defaults; a write that fails
//! keeps the change for this run and is logged.
//!
//! **Home rows.** Every row Home can show has a stable id: the three of the bridge's own
//! (`continue`, `new_episodes`, `library`) and one per catalog (`addon|type|catalog`). The
//! stored list is the viewer's order and switches; a row it does not name (an addon installed
//! since) takes its default place and default switch after them. [`Config::home_rows`] is that
//! merge, and a move or a switch writes the merged list back whole, so the stored order is always
//! the order the viewer saw.

use super::super::api::{Addon, Resource};
use super::super::library::{Lib, Shelf};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// How many catalogs of each library Home shows by default.
pub(super) const DEFAULT_HOME_PER_LIB: usize = 2;

pub(super) const ROW_CONTINUE: &str = "continue";
pub(super) const ROW_NEW_EPISODES: &str = "new_episodes";
pub(super) const ROW_LIBRARY: &str = "library";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub(super) struct RowPref {
    pub id: String,
    pub shown: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub(super) struct AddonPref {
    pub id: String,
    #[serde(default = "yes")]
    pub streams: bool,
    #[serde(default = "yes")]
    pub catalogs: bool,
}

fn yes() -> bool {
    true
}

/// The kind of stream a show was last played from.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct StreamMemory {
    /// The release group ("NTb", "SubsPlease"), empty when the name carried none.
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub resolution: u32,
    #[serde(default)]
    pub addon: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub(super) struct Config {
    #[serde(default)]
    pub home: Vec<RowPref>,
    #[serde(default)]
    pub addons: Vec<AddonPref>,
    #[serde(default)]
    pub shows: HashMap<String, StreamMemory>,
}

/// One row Home can show, resolved against the installed addons.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum HomeRow {
    Continue,
    NewEpisodes,
    Library,
    /// A catalog: its index in the bridge's shelf list.
    Shelf(usize),
}

/// An addon's stable id: its manifest id, else its address.
pub(super) fn addon_id(a: &Addon) -> String {
    if a.manifest.id.is_empty() { a.transport_url.clone() } else { a.manifest.id.clone() }
}

/// A catalog's stable id.
pub(super) fn shelf_id(addons: &[Addon], s: Shelf) -> String {
    let c = &addons[s.addon].manifest.catalogs[s.catalog];
    format!("{}|{}|{}", addon_id(&addons[s.addon]), c.kind, c.id)
}

impl Config {
    /// Is `a` used for `r` (streams or catalogs)? Addons are on until switched off.
    pub fn addon_on(&self, a: &Addon, r: Resource) -> bool {
        let id = addon_id(a);
        match self.addons.iter().find(|p| p.id == id) {
            Some(p) => match r {
                Resource::Stream => p.streams,
                Resource::Catalog => p.catalogs,
                _ => true,
            },
            None => true,
        }
    }

    pub fn set_addon(&mut self, id: &str, r: Resource, on: bool) {
        let at = match self.addons.iter().position(|p| p.id == id) {
            Some(i) => i,
            None => {
                self.addons.push(AddonPref { id: id.to_string(), streams: true, catalogs: true });
                self.addons.len() - 1
            }
        };
        match r {
            Resource::Stream => self.addons[at].streams = on,
            Resource::Catalog => self.addons[at].catalogs = on,
            _ => {}
        }
    }

    /// Home's rows in the viewer's order, each with its id and switch (`shelves` is the bridge's
    /// shelf list, already narrowed to the catalogs in use).
    pub fn home_rows(&self, addons: &[Addon], shelves: &[Shelf]) -> Vec<(String, HomeRow, bool)> {
        let mut defaults: Vec<(String, HomeRow, bool)> = vec![
            (ROW_CONTINUE.into(), HomeRow::Continue, true),
            (ROW_NEW_EPISODES.into(), HomeRow::NewEpisodes, true),
        ];
        // the first catalogs of each library, in library order, as Home always showed them
        let mut rest = Vec::new();
        for lib in Lib::ALL {
            for (k, (n, s)) in shelves.iter().copied().enumerate().filter(|(_, s)| s.lib == lib).enumerate() {
                let row = (shelf_id(addons, s), HomeRow::Shelf(n), true);
                if k < DEFAULT_HOME_PER_LIB {
                    defaults.push(row);
                } else {
                    rest.push((row.0, row.1, false));
                }
            }
        }
        defaults.push((ROW_LIBRARY.into(), HomeRow::Library, true));
        defaults.extend(rest);
        let mut out = Vec::with_capacity(defaults.len());
        for p in &self.home {
            if let Some(i) = defaults.iter().position(|(id, _, _)| *id == p.id) {
                let (id, row, _) = defaults.remove(i);
                out.push((id, row, p.shown));
            }
        }
        out.extend(defaults);
        out
    }

    /// Switch row `id` on or off.
    pub fn show_row(&mut self, addons: &[Addon], shelves: &[Shelf], id: &str, shown: bool) {
        let mut rows = self.home_rows(addons, shelves);
        if let Some(r) = rows.iter_mut().find(|(rid, _, _)| rid == id) {
            r.2 = shown;
        }
        self.home = rows.into_iter().map(|(id, _, shown)| RowPref { id, shown }).collect();
    }

    /// Move row `id` to place `to` (clamped).
    pub fn move_row(&mut self, addons: &[Addon], shelves: &[Shelf], id: &str, to: usize) {
        let mut rows = self.home_rows(addons, shelves);
        if let Some(from) = rows.iter().position(|(rid, _, _)| rid == id) {
            let row = rows.remove(from);
            rows.insert(to.min(rows.len()), row);
        }
        self.home = rows.into_iter().map(|(id, _, shown)| RowPref { id, shown }).collect();
    }
}

/// Where the file lives; `None` (tests) keeps everything in memory.
pub(super) fn path() -> Option<PathBuf> {
    if cfg!(test) {
        None
    } else {
        Some(plx_base::paths::persistent_state_root().join("stremio-bridge.json"))
    }
}

pub(super) fn load(path: Option<&PathBuf>) -> Config {
    path.and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub(super) fn save(path: Option<&PathBuf>, config: &Config) {
    let Some(path) = path else { return };
    let Ok(body) = serde_json::to_vec_pretty(config) else { return };
    let tmp = path.with_extension("json.tmp");
    let ok = path.parent().map_or(true, |d| std::fs::create_dir_all(d).is_ok())
        && std::fs::write(&tmp, &body).is_ok()
        && std::fs::rename(&tmp, path).is_ok();
    if !ok {
        plx_base::eventlog::log("stremio: the bridge's settings could not be saved; they hold for this run");
    }
}

/// The release group in a file or release name: the `-GROUP` before the extension
/// ("Show.S01E02.1080p.WEB.h264-NTb.mkv"), or a leading `[Group]` (anime: "[SubsPlease] Show - 02").
pub(super) fn release_group(name: &str) -> String {
    let name = name.lines().next().unwrap_or(name).trim();
    if let Some(rest) = name.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            let g = rest[..end].trim();
            if !g.is_empty() && g.len() <= 32 {
                return g.to_string();
            }
        }
    }
    let stem = match name.rsplit_once('.') {
        Some((stem, ext)) if ext.len() <= 4 && ext.chars().all(|c| c.is_ascii_alphanumeric()) => stem,
        _ => name,
    };
    match stem.rsplit_once('-') {
        Some((_, g)) if !g.is_empty() && g.len() <= 20 && g.chars().all(|c| c.is_ascii_alphanumeric()) => g.to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_group_is_read_from_either_naming_style() {
        assert_eq!(release_group("Show.S01E02.1080p.WEB.h264-NTb.mkv"), "NTb");
        assert_eq!(release_group("[SubsPlease] Frieren - 02 (1080p) [ABCD1234].mkv"), "SubsPlease");
        assert_eq!(release_group("Show S01E02 1080p WEB"), "");
        assert_eq!(release_group("Film.2019.2160p.UHD.BluRay.x265-TERMiNAL\n👤 40"), "TERMiNAL");
    }

    #[test]
    fn addons_are_on_until_switched_off() {
        let mut c = Config::default();
        let a: Addon = serde_json::from_value(serde_json::json!({
            "transportUrl": "https://t.example/manifest.json", "manifest": {"id": "org.t", "name": "T"}})).unwrap();
        assert!(c.addon_on(&a, Resource::Stream));
        c.set_addon("org.t", Resource::Stream, false);
        assert!(!c.addon_on(&a, Resource::Stream));
        assert!(c.addon_on(&a, Resource::Catalog));
    }
}

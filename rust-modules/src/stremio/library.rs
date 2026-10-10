//! How the account's addons become the app's three libraries.
//!
//! The app browses LIBRARIES: each has a grid of titles and, above it, rows ("shelves") of titles,
//! and the top bar has a tab per kind of library. Stremio has no libraries, only catalogs, so this
//! is the arrangement Stremio's own Discover page implies: one library per kind — **Movies**,
//! **TV Shows** and **Anime** — whose shelves are every catalog of that kind the installed addons
//! offer, in the account's addon order, and whose grid is the first of them.
//!
//! Addons are told apart by what they declare ([`Resource`]): only catalogs (`catalog`) make
//! shelves, only metadata providers (`meta`) are asked for a title's details, and only stream
//! providers (`stream`) for where to play it — so a torrent addon's own "Real-Debrid downloads"
//! catalog is the only way it can appear here, and Cinemeta is never asked for a stream.

use super::api::{Addon, Catalog, Resource};

/// One of the app's libraries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lib {
    Movies,
    Shows,
    Anime,
}

impl Lib {
    pub const ALL: [Lib; 3] = [Lib::Movies, Lib::Shows, Lib::Anime];

    /// The library's section key, fixed so a remembered library stays the same library.
    pub fn key(self) -> i64 {
        match self {
            Lib::Movies => 1,
            Lib::Shows => 2,
            Lib::Anime => 3,
        }
    }

    pub fn from_key(key: &str) -> Option<Lib> {
        Lib::ALL.into_iter().find(|l| l.key().to_string() == key)
    }

    pub fn title(self) -> &'static str {
        match self {
            Lib::Movies => "Movies",
            Lib::Shows => "TV Shows",
            Lib::Anime => "Anime",
        }
    }

    /// The media server's library type. Anime is shows (and the occasional film) with episodes,
    /// so it is browsed the way shows are.
    pub fn wire(self) -> &'static str {
        match self {
            Lib::Movies => "movie",
            Lib::Shows | Lib::Anime => "show",
        }
    }

    /// The library a title of Stremio type `stype` and id `id` belongs to.
    pub fn of_title(stype: &str, id: &str) -> Lib {
        if super::catalog::is_anime(stype, id) {
            Lib::Anime
        } else if stype == "movie" {
            Lib::Movies
        } else {
            Lib::Shows
        }
    }
}

/// One catalog placed in a library: which addon, which of its catalogs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shelf {
    pub addon: usize,
    pub catalog: usize,
    pub lib: Lib,
}

/// Is this catalog anime? Its type says so, or every id prefix its addon understands is an anime
/// database's, or its own name or id does.
fn anime_catalog(addon: &Addon, c: &Catalog) -> bool {
    let anime_ids = addon.manifest.id_prefixes.as_ref().is_some_and(|p| {
        !p.is_empty() && p.iter().all(|x| super::catalog::is_anime("", x))
    });
    c.kind == "anime"
        || anime_ids
        || c.id.to_ascii_lowercase().contains("anime")
        || c.title().to_ascii_lowercase().contains("anime")
}

/// Every browsable catalog, placed in its library, in the account's addon order. A catalog of a
/// kind the app has no screen for (channels, live TV) is left out.
pub fn shelves(addons: &[Addon]) -> Vec<Shelf> {
    let mut out = Vec::new();
    for (a, addon) in addons.iter().enumerate() {
        if !addon.has(Resource::Catalog) {
            continue;
        }
        for (c, catalog) in addon.manifest.catalogs.iter().enumerate() {
            if !catalog.browsable() {
                continue;
            }
            let lib = if anime_catalog(addon, catalog) {
                Lib::Anime
            } else {
                match catalog.kind.as_str() {
                    "movie" => Lib::Movies,
                    "series" => Lib::Shows,
                    _ => continue,
                }
            };
            out.push(Shelf { addon: a, catalog: c, lib });
        }
    }
    out
}

/// The libraries that have at least one shelf, in tab order.
pub fn libraries(shelves: &[Shelf]) -> Vec<Lib> {
    Lib::ALL.into_iter().filter(|l| shelves.iter().any(|s| s.lib == *l)).collect()
}

/// The catalogs that take a search query, for one library kind: `movie` or `series` catalogs, and
/// anime ones under `series`.
pub fn search_catalogs(addons: &[Addon], stype: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for (a, addon) in addons.iter().enumerate() {
        if !addon.has(Resource::Catalog) {
            continue;
        }
        for (c, catalog) in addon.manifest.catalogs.iter().enumerate() {
            if catalog.searchable() && catalog.kind == stype {
                out.push((a, c));
            }
        }
    }
    out
}

/// The addons that can describe a title, in the account's order: metadata providers whose types
/// and id prefixes cover it.
pub fn meta_providers<'a>(addons: &'a [Addon], stype: &'a str, id: &'a str) -> impl Iterator<Item = &'a Addon> {
    addons.iter().filter(move |a| a.provides(Resource::Meta, stype, id))
}

/// The addons that can play a title, in the account's order.
pub fn stream_providers<'a>(addons: &'a [Addon], stype: &'a str, id: &'a str) -> impl Iterator<Item = &'a Addon> {
    addons.iter().filter(move |a| a.provides(Resource::Stream, stype, id))
}

/// A one-line account of what each installed addon was classified as, for the event log.
pub fn summary(addons: &[Addon]) -> String {
    let count = |r: Resource| addons.iter().filter(|a| a.has(r)).count();
    format!(
        "{} addons: {} catalog, {} metadata, {} stream, {} subtitle providers",
        addons.len(),
        count(Resource::Catalog),
        count(Resource::Meta),
        count(Resource::Stream),
        count(Resource::Subtitles)
    )
}

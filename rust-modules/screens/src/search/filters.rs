//! **Search's browse filters** — what to show when nothing has been typed, and what a typed search
//! is narrowed to: a TYPE (all, films, shows, anime), an ORDER (recommended for you, popular, top
//! rated, newest) and a GENRE.
//!
//! They ride the store's ONE query, so the page needs no second channel to its source: the field's
//! text, then [`MARK`] and the three choices (`dune::f|movie|popular|`). The page keeps the text in
//! its draft and the choices here; the source (the Stremio bridge's `/hubs/search`) reads both
//! halves back. With no text the page is BROWSING: the default choices are the titles recommended
//! from what the viewer has watched.
//!
//! On screen they are rows of pills under the recents — the genre row wrapped onto lines of
//! [`PER_LINE`] — each line its own focus row, so LEFT/RIGHT walk a line and UP/DOWN move between
//! lines the way they move between any two rows of the page.

use plx_ui::consts::MARGIN_X;
use plx_ui::Rect;

/// What separates the field's text from the choices in the store's query.
pub(super) const MARK: &str = "::f";

/// The genres offered, in the canonical spelling a catalog filters by (Cinemeta's); index 0 of
/// the genre rows is "All genres", so genre `g` (1-based) is `GENRES[g - 1]`.
pub(super) const GENRES: [&str; 17] = [
    "Action", "Adventure", "Animation", "Comedy", "Crime", "Documentary", "Drama", "Family",
    "Fantasy", "History", "Horror", "Mystery", "Romance", "Sci-Fi", "Thriller", "War", "Western",
];
const TYPE_WORDS: [&str; 4] = ["all", "movie", "show", "anime"];
const SORT_WORDS: [&str; 4] = ["foryou", "popular", "rated", "new"];

/// Pills on one line of a row.
pub(super) const PER_LINE: usize = 7;
/// One pill's size and the air between two.
pub(super) const PILL_W: f32 = 206.0;
pub(super) const PILL_H: f32 = 54.0;
pub(super) const PILL_GAP: f32 = 14.0;
/// A line's height, pill and air.
pub(super) const LINE_H: f32 = PILL_H + 18.0;
/// The column the row labels stand in, left of the pills.
pub(super) const LABEL_W: f32 = 168.0;

/// The focus element of pill `i` on line `line` — far above anything the page interns.
pub(super) fn elem(line: usize, i: usize) -> u32 {
    ELEM_BASE + (line as u32) * 256 + i as u32
}
const ELEM_BASE: u32 = 0x5000_0000;

/// The `(line, pill)` an element is, when it is a filter pill.
pub(super) fn of_elem(e: u32) -> Option<(usize, usize)> {
    let rel = e.checked_sub(ELEM_BASE)?;
    let (line, i) = ((rel / 256) as usize, (rel % 256) as usize);
    (line < lines() && i < len(line)).then_some((line, i))
}

/// How many lines the filters take: type, order, then the genres.
pub(super) fn lines() -> usize {
    2 + (GENRES.len() + 1).div_ceil(PER_LINE)
}

/// Pills on line `line`.
pub(super) fn len(line: usize) -> usize {
    match line {
        0 => TYPE_WORDS.len(),
        1 => SORT_WORDS.len(),
        _ => {
            let first = (line - 2) * PER_LINE;
            (GENRES.len() + 1).saturating_sub(first).min(PER_LINE)
        }
    }
}

/// The text the pill shows.
pub(super) fn label(line: usize, i: usize) -> &'static std::ffi::CStr {
    use plx_platform::i18n::msg as m;
    match line {
        0 => [m::browse_search_filter_all_c, m::browse_search_filter_movies_c, m::browse_search_filter_shows_c,
            m::browse_search_filter_anime_c][i.min(3)](),
        1 => [m::browse_search_filter_for_you_c, m::browse_search_filter_popular_c, m::browse_search_filter_top_rated_c,
            m::browse_search_filter_newest_c][i.min(3)](),
        _ => {
            let g = (line - 2) * PER_LINE + i;
            const NAMES: [fn() -> &'static std::ffi::CStr; 18] = [
                m::browse_search_filter_any_genre_c, m::browse_search_genre_action_c, m::browse_search_genre_adventure_c,
                m::browse_search_genre_animation_c, m::browse_search_genre_comedy_c, m::browse_search_genre_crime_c,
                m::browse_search_genre_documentary_c, m::browse_search_genre_drama_c, m::browse_search_genre_family_c,
                m::browse_search_genre_fantasy_c, m::browse_search_genre_history_c, m::browse_search_genre_horror_c,
                m::browse_search_genre_mystery_c, m::browse_search_genre_romance_c, m::browse_search_genre_sci_fi_c,
                m::browse_search_genre_thriller_c, m::browse_search_genre_war_c, m::browse_search_genre_western_c,
            ];
            NAMES[g.min(NAMES.len() - 1)]()
        }
    }
}

/// The label standing before line `line`, if it starts a row (genre lines after the first have
/// none: they continue the GENRE row).
pub(super) fn row_label(line: usize) -> Option<&'static std::ffi::CStr> {
    use plx_platform::i18n::msg as m;
    match line {
        0 => Some(m::browse_search_filter_type_c()),
        1 => Some(m::browse_search_filter_sort_c()),
        2 => Some(m::browse_search_filter_genre_c()),
        _ => None,
    }
}

/// Pill `i`'s rect on a line whose top is `y`.
pub(super) fn pill(i: usize, y: f32) -> Rect {
    Rect::new(MARGIN_X + LABEL_W + i as f32 * (PILL_W + PILL_GAP), y + (LINE_H - PILL_H) * 0.5, PILL_W, PILL_H)
}

/// The viewer's choices.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Filters {
    kind: usize,
    sort: usize,
    /// 0 = all genres, else `GENRES[genre - 1]`.
    genre: usize,
}

impl Filters {
    /// The pill chosen on line `line`, if one of its pills is.
    pub(super) fn chosen(&self, line: usize) -> Option<usize> {
        match line {
            0 => Some(self.kind),
            1 => Some(self.sort),
            _ => {
                let first = (line - 2) * PER_LINE;
                (self.genre >= first && self.genre < first + PER_LINE).then(|| self.genre - first)
            }
        }
    }

    /// Choose pill `i` of line `line`.
    pub(super) fn pick(&mut self, line: usize, i: usize) {
        if i >= len(line) {
            return;
        }
        match line {
            0 => self.kind = i,
            1 => self.sort = i,
            _ => self.genre = (line - 2) * PER_LINE + i,
        }
    }

    /// The store query for the field's `text` under these choices.
    pub(super) fn compose(&self, text: &str) -> String {
        let genre = if self.genre == 0 { "" } else { GENRES[self.genre - 1] };
        format!("{text}{MARK}|{}|{}|{genre}", TYPE_WORDS[self.kind], SORT_WORDS[self.sort])
    }
}

/// The field's own text inside a store query (everything before [`MARK`]).
pub(super) fn user_part(query: &str) -> &str {
    query.find(MARK).map_or(query, |i| &query[..i])
}

/// Is `query` a browse — choices and no text?
pub(super) fn browsing(query: &str) -> bool {
    query.contains(MARK) && user_part(query).trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_choices_ride_the_query_and_the_text_reads_back_whole() {
        let mut f = Filters::default();
        assert_eq!(f.compose(""), "::f|all|foryou|");
        assert!(browsing(&f.compose("")));
        f.pick(0, 1);
        f.pick(1, 2);
        f.pick(2, 1); // Action
        assert_eq!(f.compose("dune "), "dune ::f|movie|rated|Action");
        assert_eq!(user_part("dune ::f|movie|rated|Action"), "dune ");
        assert!(!browsing("dune ::f|movie|rated|Action"));
        assert_eq!(user_part("plain"), "plain");
        f.pick(4, 3); // the third genre line's fourth pill: genre 17, Western
        assert_eq!(f.compose(""), "::f|movie|rated|Western");
        assert_eq!(f.chosen(4), Some(3));
        assert_eq!(f.chosen(2), None);
    }

    #[test]
    fn every_pill_has_an_element_that_names_it_back() {
        assert_eq!(lines(), 5);
        let total: usize = (2..lines()).map(len).sum();
        assert_eq!(total, GENRES.len() + 1, "every genre and All");
        for line in 0..lines() {
            for i in 0..len(line) {
                assert_eq!(of_elem(elem(line, i)), Some((line, i)));
            }
            assert_eq!(of_elem(elem(line, len(line))), None);
        }
        assert_eq!(of_elem(10), None);
    }
}

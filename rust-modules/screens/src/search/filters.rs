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
//! On screen they are ONE line under the field: three dropdown buttons, each naming its filter and
//! what it is set to ("Genre: Comedy ▾"). Pressing one opens its options in a panel hung under it
//! (an out-of-flow overlay of the page's stack), focus inside; picking an option, or BACK, closes
//! it. The genres are laid out in columns; the two short lists in one.

use plx_ui::consts::MARGIN_X;
use plx_ui::Rect;

/// What separates the field's text from the choices in the store's query.
pub(super) const MARK: &str = "::f";

/// The genres offered, in the canonical spelling a catalog filters by (Cinemeta's); option 0 of
/// the genre dropdown is "All genres", so option `g` (1-based) is `GENRES[g - 1]`.
pub(super) const GENRES: [&str; 17] = [
    "Action", "Adventure", "Animation", "Comedy", "Crime", "Documentary", "Drama", "Family",
    "Fantasy", "History", "Horror", "Mystery", "Romance", "Sci-Fi", "Thriller", "War", "Western",
];
const TYPE_WORDS: [&str; 4] = ["all", "movie", "show", "anime"];
const SORT_WORDS: [&str; 4] = ["foryou", "popular", "rated", "new"];

/// The dropdowns: type, order, genre.
pub(super) const DROPS: usize = 3;

/// The bar's buttons: their height and the air between two.
pub(super) const PILL_H: f32 = 56.0;
pub(super) const PILL_GAP: f32 = 18.0;
/// The buttons' and the options' type size.
pub(super) const PILL_SIZE: i32 = plx_ui::theme::size::LABEL;
/// The bar's line: a button and the air around it.
pub(super) const LINE_H: f32 = PILL_H + 24.0;
/// Kept for the page's flow: the bar is one line with no panel of its own.
pub(super) const PANEL_V: f32 = 0.0;

/// One option's cell in an open dropdown, the panel's inset, and how far under its button the
/// panel hangs.
pub(super) const OPT_W: f32 = 300.0;
pub(super) const OPT_H: f32 = 60.0;
pub(super) const DROP_PAD: f32 = 14.0;
pub(super) const DROP_GAP: f32 = 12.0;

const ELEM_BASE: u32 = 0x5000_0000;

/// The focus element of the bar's button `i` (`line` is always 0: the bar is the one line).
pub(super) fn elem(line: usize, i: usize) -> u32 {
    ELEM_BASE + (line as u32) * 256 + i as u32
}

/// The bar button an element is, as `(0, button)`.
pub(super) fn of_elem(e: u32) -> Option<(usize, usize)> {
    let rel = e.checked_sub(ELEM_BASE)?;
    (rel < DROPS as u32).then_some((0, rel as usize))
}

/// The focus element of option `i` in dropdown `k`.
pub(super) fn opt_elem(k: usize, i: usize) -> u32 {
    ELEM_BASE + 256 * (k as u32 + 1) + i as u32
}

/// The `(dropdown, option)` an element is, when it is an option.
pub(super) fn of_opt(e: u32) -> Option<(usize, usize)> {
    let rel = e.checked_sub(ELEM_BASE + 256)?;
    let (k, i) = ((rel / 256) as usize, (rel % 256) as usize);
    (k < DROPS && i < options(k)).then_some((k, i))
}

/// How many lines the filters take: the bar.
pub(super) fn lines() -> usize {
    1
}

/// Buttons on line `line`.
pub(super) fn len(_line: usize) -> usize {
    DROPS
}

/// Options in dropdown `k`.
pub(super) fn options(k: usize) -> usize {
    match k {
        0 => TYPE_WORDS.len(),
        1 => SORT_WORDS.len(),
        _ => GENRES.len() + 1,
    }
}

/// Columns dropdown `k` lays its options out in.
pub(super) fn cols(k: usize) -> usize {
    if k == 2 { 3 } else { 1 }
}

/// Option `i` of dropdown `k`, as shown.
pub(super) fn label(k: usize, i: usize) -> &'static std::ffi::CStr {
    use plx_platform::i18n::msg as m;
    match k {
        0 => [m::browse_search_filter_all_c, m::browse_search_filter_movies_c, m::browse_search_filter_shows_c,
            m::browse_search_filter_anime_c][i.min(3)](),
        1 => [m::browse_search_filter_for_you_c, m::browse_search_filter_popular_c, m::browse_search_filter_top_rated_c,
            m::browse_search_filter_newest_c][i.min(3)](),
        _ => {
            const NAMES: [fn() -> &'static std::ffi::CStr; 18] = [
                m::browse_search_filter_any_genre_c, m::browse_search_genre_action_c, m::browse_search_genre_adventure_c,
                m::browse_search_genre_animation_c, m::browse_search_genre_comedy_c, m::browse_search_genre_crime_c,
                m::browse_search_genre_documentary_c, m::browse_search_genre_drama_c, m::browse_search_genre_family_c,
                m::browse_search_genre_fantasy_c, m::browse_search_genre_history_c, m::browse_search_genre_horror_c,
                m::browse_search_genre_mystery_c, m::browse_search_genre_romance_c, m::browse_search_genre_sci_fi_c,
                m::browse_search_genre_thriller_c, m::browse_search_genre_war_c, m::browse_search_genre_western_c,
            ];
            NAMES[i.min(NAMES.len() - 1)]()
        }
    }
}

/// Dropdown `k`'s name.
pub(super) fn drop_name(k: usize) -> &'static std::ffi::CStr {
    use plx_platform::i18n::msg as m;
    match k {
        0 => m::browse_search_filter_type_button_c(),
        1 => m::browse_search_filter_sort_button_c(),
        _ => m::browse_search_filter_genre_button_c(),
    }
}

/// What button `k` reads: its filter and what it is set to ("Genre: Comedy").
pub(super) fn button_text(k: usize, f: &Filters) -> std::ffi::CString {
    let text = format!("{}: {}", drop_name(k).to_string_lossy(), label(k, f.chosen_in(k)).to_string_lossy());
    std::ffi::CString::new(text).unwrap_or_default()
}

/// Button `k`'s width: its text and the chevron after it.
pub(super) fn button_w(k: usize, f: &Filters, measure: &dyn plx_machine::machine::Measure) -> f32 {
    plx_ui::widgets::Button::pill_w_measured(&button_text(k, f), PILL_SIZE, false, true, measure)
}

/// Button `i`'s rect on the bar whose line top is `y`: the buttons laid end to end from the margin.
pub(super) fn pill(i: usize, y: f32, f: &Filters, measure: &dyn plx_machine::machine::Measure) -> Rect {
    let x = (0..i).fold(MARGIN_X, |x, j| x + button_w(j, f, measure) + PILL_GAP);
    Rect::new(x, y + (LINE_H - PILL_H) * 0.5, button_w(i, f, measure), PILL_H)
}

/// Dropdown `k`'s panel, hung under its button `button`.
pub(super) fn drop_panel(k: usize, button: Rect) -> Rect {
    let rows = options(k).div_ceil(cols(k));
    Rect::new(button.x, button.y + button.h + DROP_GAP,
        cols(k) as f32 * OPT_W + 2.0 * DROP_PAD, rows as f32 * OPT_H + 2.0 * DROP_PAD)
}

/// Option `i`'s row inside dropdown `k`'s `panel`, row by row across its columns.
pub(super) fn opt_rect(k: usize, i: usize, panel: Rect) -> Rect {
    let (row, col) = (i / cols(k), i % cols(k));
    Rect::new(panel.x + DROP_PAD + col as f32 * OPT_W, panel.y + DROP_PAD + row as f32 * OPT_H, OPT_W, OPT_H)
}

/// The option a remote step from option `at` of dropdown `k` lands on, if there is one.
pub(super) fn opt_step(k: usize, at: usize, dir: plx_ui::screen::Dir) -> Option<usize> {
    use plx_ui::screen::Dir;
    let c = cols(k);
    let to = match dir {
        Dir::Up => at.checked_sub(c)?,
        Dir::Down => at + c,
        Dir::Left if at % c > 0 => at - 1,
        Dir::Right if at % c + 1 < c => at + 1,
        _ => return None,
    };
    (to < options(k)).then_some(to)
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
    /// The option chosen in dropdown `k`.
    pub(super) fn chosen_in(&self, k: usize) -> usize {
        match k {
            0 => self.kind,
            1 => self.sort,
            _ => self.genre,
        }
    }

    /// Choose option `i` of dropdown `k`.
    pub(super) fn pick_in(&mut self, k: usize, i: usize) {
        if i >= options(k) {
            return;
        }
        match k {
            0 => self.kind = i,
            1 => self.sort = i,
            _ => self.genre = i,
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
    use plx_ui::screen::Dir;

    #[test]
    fn the_choices_ride_the_query_and_the_text_reads_back_whole() {
        let mut f = Filters::default();
        assert_eq!(f.compose(""), "::f|all|foryou|");
        assert!(browsing(&f.compose("")));
        f.pick_in(0, 1);
        f.pick_in(1, 2);
        f.pick_in(2, 1); // Action
        assert_eq!(f.compose("dune "), "dune ::f|movie|rated|Action");
        assert_eq!(user_part("dune ::f|movie|rated|Action"), "dune ");
        assert!(!browsing("dune ::f|movie|rated|Action"));
        assert_eq!(user_part("plain"), "plain");
        f.pick_in(2, 17);
        assert_eq!(f.compose(""), "::f|movie|rated|Western");
        assert_eq!(f.chosen_in(2), 17);
        f.pick_in(2, 18);
        assert_eq!(f.chosen_in(2), 17, "past the last option changes nothing");
    }

    #[test]
    fn every_button_and_option_has_an_element_that_names_it_back() {
        assert_eq!(lines(), 1);
        for i in 0..DROPS {
            assert_eq!(of_elem(elem(0, i)), Some((0, i)));
            assert_eq!(of_opt(elem(0, i)), None);
        }
        assert_eq!(of_elem(elem(0, DROPS)), None);
        for k in 0..DROPS {
            for i in 0..options(k) {
                assert_eq!(of_opt(opt_elem(k, i)), Some((k, i)));
                assert_eq!(of_elem(opt_elem(k, i)), None);
            }
            assert_eq!(of_opt(opt_elem(k, options(k))), None);
        }
        assert_eq!(options(2), GENRES.len() + 1, "every genre and All");
        assert_eq!(of_elem(10), None);
    }

    #[test]
    fn a_dropdown_is_walked_by_its_rows_and_columns() {
        // the genres: three columns
        assert_eq!(opt_step(2, 0, Dir::Right), Some(1));
        assert_eq!(opt_step(2, 2, Dir::Right), None, "the end of a row");
        assert_eq!(opt_step(2, 1, Dir::Down), Some(4));
        assert_eq!(opt_step(2, 1, Dir::Up), None);
        assert_eq!(opt_step(2, 16, Dir::Down), None, "no cell under the last row's short end");
        // a short list: one column
        assert_eq!(opt_step(0, 0, Dir::Down), Some(1));
        assert_eq!(opt_step(0, 3, Dir::Down), None);
        assert_eq!(opt_step(0, 1, Dir::Right), None);
        let panel = drop_panel(2, Rect::new(100.0, 400.0, 200.0, PILL_H));
        let last = opt_rect(2, options(2) - 1, panel);
        assert!(last.y + last.h <= panel.y + panel.h && last.x + last.w <= panel.x + panel.w);
        assert!(panel.y > 400.0 + PILL_H, "the panel hangs under its button");
    }
}

//! In-player Episodes strip: the show's episodes as a row of stills over the transport, opened
//! from the HUD's Episodes tab. LEFT/RIGHT pick an episode, UP/DOWN jump to the previous / next
//! season (DOWN past the last one returns to the HUD tabs), OK plays the picked episode, BACK closes. Card layout and motion mirror
//! [`crate::chapters_panel`], so the two strips read as one family.
//!
//! **Data is the play queue, never a fetch.** Every playback of an episode creates a continuous
//! PlayQueue, and the server answers it with the show's episodes around the one playing
//! (`plx_media::route::with_queue`). The strip reads those rows as they are, so it opens at once,
//! knows which row is playing by the queue's own identity rule (`plex::queue_index_of`), and
//! playing another one is exactly what Up Next does with the row after it
//! (`plx_media::route::up_next_from_row`).
//!
//! **It owns its keys** the way the Subtitle Timing capsule does: there is one row and the season
//! jump is not a focus move the input engine could express, so the panel publishes no focus group
//! and moves its own cursor.
use plx_ui::consts::{MARGIN_X, SCR_W};
use plx_ui::theme;
use plx_ui::widgets::CARD_FOCUS_SCALE;
use plx_ui::{Rect, Spring};
use std::ffi::CString;

const EP_W: f32 = 288.0;
const EP_H: f32 = 162.0; // 16:9 still
const EP_GAP: f32 = 24.0;
const EP_TOP: f32 = 684.0; // the Chapters strip's band, so the two never fight over the screen
const EP_RAD: f32 = 10.0;
const EP_FOCUS_RING_W: f32 = 2.5;
/// The season heading's baseline band, above the stills.
const HEAD_Y: f32 = EP_TOP - 64.0;

/// One episode row of the queue, as the strip reads it.
struct Row<'a> {
    /// Index into the queue's own rows (what `up_next_from_row` is handed).
    queue: usize,
    row: &'a plx_plex::plex::QueueRow,
}

fn episode_rows(rows: &[plx_plex::plex::QueueRow]) -> Vec<Row<'_>> {
    rows.iter()
        .enumerate()
        .filter(|(_, r)| r.kind == "episode" && !r.rk.is_empty())
        .map(|(queue, row)| Row { queue, row })
        .collect()
}

/// The strip's position among the episode rows of the one playing, if the queue says.
fn playing_index(ps: &plx_media::route::PlaybackSession) -> Option<usize> {
    let item_id = plx_media::route::pq_item_id(ps).parse().unwrap_or(0);
    let rk = plx_media::route::cur_rk(ps);
    let sid = plx_media::route::cur_sid(ps);
    plx_media::route::with_queue(ps, |rows| {
        let at = plx_plex::plex::queue_index_of(rows, item_id, sid, &rk)?;
        episode_rows(rows).iter().position(|r| r.queue == at)
    })
}

/// Does the playing item have episodes to switch between — the Episodes tab's condition.
pub fn has_episodes(ps: &plx_media::route::PlaybackSession) -> bool {
    plx_media::route::with_queue(ps, |rows| episode_rows(rows).len() > 1)
}

/// The index to land on when jumping `dir` seasons (−1 or +1) from `from`: the first episode of
/// the nearest season in that direction, or `None` at either end.
fn season_jump(seasons: &[i64], from: usize, dir: i32) -> Option<usize> {
    let here = *seasons.get(from)?;
    if dir < 0 {
        // The previous season's first episode: find where it starts.
        let prev = seasons[..from].iter().rev().find(|s| **s != here).copied()?;
        seasons.iter().position(|s| *s == prev)
    } else {
        seasons.iter().skip(from).position(|s| *s != here).map(|i| from + i)
    }
}

fn scroll_target(sel: usize) -> f32 {
    // pin the focused card to the 2nd slot, as the Chapters strip and the episode picker do
    if sel > 1 { (sel as f32 - 1.0) * (EP_W + EP_GAP) } else { 0.0 }
}

/// What a press did to the strip.
pub enum EpisodesKey {
    /// The cursor moved (or stayed at an end): keep the strip up.
    Moved,
    /// DOWN with no later season: the press leaves the strip for the HUD's tabs, as DOWN off the
    /// Chapters strip does.
    Below,
    /// OK: play this.
    Play(plx_media::route::UpNext),
    /// Nothing to act on.
    Inert,
}

/// The strip's state, owned by the overlay that mounts it.
pub struct EpisodesState {
    sel: usize,
    scroll: Spring,
    scale: Spring,
}

impl EpisodesState {
    /// Opens on the episode that is playing.
    pub fn new(ps: &plx_media::route::PlaybackSession) -> Self {
        let sel = playing_index(ps).unwrap_or(0);
        EpisodesState { sel, scroll: Spring::at(scroll_target(sel)), scale: Spring::at(1.0) }
    }

    /// The highlighted episode, for the focus probe.
    pub fn sel(&self) -> i32 {
        self.sel as i32
    }

    fn set_sel(&mut self, i: usize) {
        if i != self.sel {
            self.scale.jump(1.0);
        }
        self.sel = i;
    }

    /// LEFT/RIGHT by one episode, UP/DOWN by a season (`dx`/`dy` are −1, 0 or +1).
    pub fn step(&mut self, ps: &plx_media::route::PlaybackSession, dx: i32, dy: i32) -> EpisodesKey {
        let seasons: Vec<i64> = plx_media::route::with_queue(ps, |rows| {
            episode_rows(rows).iter().map(|r| r.row.season).collect()
        });
        if seasons.is_empty() {
            return EpisodesKey::Inert;
        }
        let at = self.sel.min(seasons.len() - 1);
        let next = if dy != 0 {
            match season_jump(&seasons, at, dy) {
                Some(i) => i,
                None if dy > 0 => return EpisodesKey::Below,
                None => at,
            }
        } else if dx < 0 {
            at.saturating_sub(1)
        } else {
            (at + 1).min(seasons.len() - 1)
        };
        self.set_sel(next);
        EpisodesKey::Moved
    }

    /// OK on the highlighted episode.
    pub fn on_ok(&self, ps: &plx_media::route::PlaybackSession) -> EpisodesKey {
        if playing_index(ps) == Some(self.sel) {
            // The one already playing: nothing to start.
            return EpisodesKey::Inert;
        }
        let chosen = plx_media::route::with_queue(ps, |rows| {
            let eps = episode_rows(rows);
            eps.get(self.sel).and_then(|r| plx_media::route::up_next_from_row(r.row))
        });
        match chosen {
            Some(u) => EpisodesKey::Play(u),
            None => EpisodesKey::Inert,
        }
    }

    pub fn update(&mut self, dt: f32, ps: &plx_media::route::PlaybackSession) {
        let n = plx_media::route::with_queue(ps, |rows| episode_rows(rows).len());
        // A new play replaces the queue; re-clamp rather than chase a slot that no longer exists.
        self.sel = self.sel.min(n.saturating_sub(1));
        let target = scroll_target(self.sel);
        self.scroll.step(target, 220.0, dt);
        plx_ui::anim::probe("episodes.scroll", self.scroll.pos, self.scroll.vel, target, dt);
        self.scale.step(CARD_FOCUS_SCALE, 300.0, dt);
        plx_ui::anim::probe("episodes.scale", self.scale.pos, self.scale.vel, CARD_FOCUS_SCALE, dt);
    }

    pub fn draw(&mut self, ps: &plx_media::route::PlaybackSession, appear: f32, measure: &dyn plx_machine::machine::Measure) {
        let playing = playing_index(ps);
        let scroll = self.scroll.pos;
        let (sel, scale) = (self.sel, self.scale.pos);
        let root = plx_ui::Painter::root()
            .alpha(appear)
            .translate(0.0, plx_ui::popover::Popover::RISE * (1.0 - appear));
        plx_media::route::with_queue(ps, |rows| {
            let eps = episode_rows(rows);
            let Some(focused) = eps.get(sel) else { return };
            // The season heading, and the two keys that change it.
            let season = focused.row.season;
            let heading = if season == 0 {
                plx_platform::i18n::msg::widgets_player_specials().to_string()
            } else {
                plx_platform::i18n::msg::browse_detail_season_number(season)
            };
            if let Ok(h) = CString::new(heading) {
                root.text(h.as_ptr(), MARGIN_X, HEAD_Y, theme::size::TITLE, theme::TEXT_PRIMARY, 0, 1);
            }
            if let Ok(hint) = CString::new(plx_platform::i18n::msg::widgets_player_season_hint()) {
                root.text(hint.as_ptr(), SCR_W - MARGIN_X, HEAD_Y + 6.0, theme::size::CAPTION,
                          theme::TEXT_SECONDARY, 2, 0);
            }
            let p = root.translate(-scroll, 0.0);
            for (i, ep) in eps.iter().enumerate() {
                let x = MARGIN_X + i as f32 * (EP_W + EP_GAP);
                if !plx_ui::on_axis(x - scroll, EP_W, SCR_W, 0.0) {
                    continue;
                }
                let is_focused = i == sel;
                let card = Rect::new(x, EP_TOP, EP_W, EP_H);
                plx_ui::widgets::draw_card(p, card, plx_media::route::item_sid(ep.row.sid).raw(), &ep.row.thumb, (480, 270), EP_RAD,
                                           is_focused, scale);
                if is_focused {
                    p.rring(card.scaled(scale), EP_RAD, EP_FOCUS_RING_W, theme::CONTROL_RIM_FOCUS_UNKEYED);
                }
                let ty = EP_TOP + EP_H + 26.0;
                let name = plx_ui::fmt::episode_kicker(ep.row.season, ep.row.index, &ep.row.title);
                let colour = if is_focused { theme::TEXT_PRIMARY } else { theme::TEXT_SECONDARY };
                if let Ok(tc) = CString::new(plx_gfx::text::elide_by(&name, EP_W, false, |t| {
                    measure.width_str(t, theme::size::LABEL, true)
                })) {
                    p.text(tc.as_ptr(), x, ty, theme::size::LABEL, colour, 0, 1);
                }
                if playing == Some(i) {
                    if let Ok(now) = CString::new(plx_platform::i18n::msg::widgets_player_now_playing()) {
                        p.text(now.as_ptr(), x, ty + 34.0, theme::size::CAPTION, theme::TEXT_SECONDARY, 0, 0);
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn up_and_down_jump_to_the_first_episode_of_the_neighbouring_season() {
        // S1E1 S1E2 S1E3 S2E1 S2E2 S3E1
        let seasons = [1, 1, 1, 2, 2, 3];
        assert_eq!(season_jump(&seasons, 1, 1), Some(3), "down from S1E2 lands on S2E1");
        assert_eq!(season_jump(&seasons, 4, 1), Some(5));
        assert_eq!(season_jump(&seasons, 5, 1), None, "no season after the last");
        assert_eq!(season_jump(&seasons, 4, -1), Some(0), "up from S2E2 lands on S1E1");
        assert_eq!(season_jump(&seasons, 5, -1), Some(3));
        assert_eq!(season_jump(&seasons, 2, -1), None, "no season before the first");
    }

    #[test]
    fn the_focused_card_is_pinned_to_the_second_slot() {
        assert_eq!(scroll_target(0), 0.0);
        assert_eq!(scroll_target(1), 0.0);
        assert_eq!(scroll_target(3), 2.0 * (EP_W + EP_GAP));
    }
}

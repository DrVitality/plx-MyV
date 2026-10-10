//! **The details bubble** — what a card is, shown beside it once focus has rested on it.
//!
//! A shelf or grid card shows its artwork and, when focused, its title. Resting on it a moment
//! longer ([`DWELL_MS`]) opens a panel beside the card with the title, a facts line (year, length
//! or seasons, rating) and as much of the synopsis as fits, so the viewer can decide without
//! leaving the shelf; OK on the card then plays it (the card-press preference) and holding OK opens
//! its menu, which is what the bubble's last line says.
//!
//! **It belongs to the card, not to a screen.** [`Shelf`](super::Shelf) and
//! [`Grid`](super::Grid) each own one [`PeekState`] and draw the bubble right after the focused
//! card, so every card surface gets it from one implementation. What it says comes from
//! [`CardSource::peek`](super::CardSource::peek), which a screen fills from its own rows (the
//! library may not name an application type); a source that answers `None` (people, collections,
//! a screen that has not opted in) shows nothing.
//!
//! **Geometry.** The panel is the card's own height and stands beside it — to the right, or to
//! the left when the right would leave the safe margin — so it covers only neighbours in the same
//! row, which the focused card is already drawn over; it never reaches into the row below.
//!
//! **Motion.** The dwell is real time from the frame tick, and while it runs the state reports
//! itself as moving (exactly what a spring does) so the frame gate keeps ticking until the bubble
//! appears; the appearance is a spring. A move to another card closes it at once and starts the
//! dwell again, so walking a shelf never flashes bubbles. At rest it reports nothing.

use crate::consts::{MARGIN_X, SCR_W};
use crate::label::HAlign;
use crate::text_view::TextView;
use crate::{theme, Painter, Rect, Spring};
use plx_machine::machine::Measure;

/// How long focus rests on a card before its bubble opens, in ms. Long enough that a shelf walked
/// at remote speed (a press every 200-400 ms) never opens one; short enough to feel like a hover.
pub const DWELL_MS: f32 = 650.0;
/// The bubble's width at 1080p: two posters' worth, enough for a readable synopsis line.
pub const W: f32 = 460.0;
/// The appearance spring's stiffness.
const K_APPEAR: f32 = 260.0;
/// How far the bubble slides in from the card's side as it appears.
const RISE: f32 = 14.0;

/// What a card's bubble says: plain strings the screen fills from its row.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Peek {
    pub title: String,
    /// One line: year · length or seasons · rating, already joined. May be empty.
    pub facts: String,
    pub summary: String,
    /// The line at the foot (how to act on the card). May be empty.
    pub hint: String,
}

/// The dwell and the appearance of the one bubble a section can show.
#[derive(Clone, Copy, Default)]
pub struct PeekState {
    /// The card index the dwell is counting for.
    at: Option<usize>,
    waited_ms: f32,
    appear: Spring,
}

impl PeekState {
    pub const fn new() -> Self {
        Self { at: None, waited_ms: 0.0, appear: Spring::at(0.0) }
    }

    /// One frame: `focus` is the section's focused card (None when focus is elsewhere or the
    /// section is dormant).
    pub fn tick(&mut self, focus: Option<usize>, dt: f32) {
        if focus != self.at {
            self.at = focus;
            self.waited_ms = 0.0;
            self.appear.jump(0.0);
        }
        if focus.is_some() && self.waited_ms < DWELL_MS {
            self.waited_ms = (self.waited_ms + dt * 1000.0).min(DWELL_MS);
            // the dwell is motion the gate must keep ticking for, as a spring's travel is
            plx_machine::idle::note_spring(self.waited_ms, DWELL_MS, 0.0);
        }
        let target = if focus.is_some() && self.waited_ms >= DWELL_MS { 1.0 } else { 0.0 };
        if self.appear.pos != target || self.appear.vel != 0.0 {
            self.appear.step(target, K_APPEAR, dt);
            if plx_machine::idle::settled(self.appear.pos, target, self.appear.vel) {
                self.appear.jump(target);
            }
        }
    }

    /// How far the bubble is shown, 0 to 1.
    pub fn alpha(&self) -> f32 {
        self.appear.pos.clamp(0.0, 1.0)
    }

    /// Whether nothing here needs another frame.
    #[cfg(any(test, feature = "test-support"))]
    pub fn at_rest(&self) -> bool {
        let target = if self.at.is_some() && self.waited_ms >= DWELL_MS { 1.0 } else { 0.0 };
        (self.at.is_none() || self.waited_ms >= DWELL_MS) && self.appear.pos == target && self.appear.vel == 0.0
    }
}

/// Where the bubble for `card` (in `p`'s coordinates) stands: beside it on the right, or on the
/// left when the right would cross the safe margin.
pub fn rect_for(p: Painter, card: Rect) -> Rect {
    let gap = theme::space::SM;
    let right = card.x + card.w + gap;
    let x = if right + p.dx() + W <= SCR_W - MARGIN_X { right } else { card.x - gap - W };
    Rect::new(x, card.y, W, card.h)
}

/// Draw `peek` beside `card` at appearance `alpha`.
pub fn draw(p: Painter, card: Rect, peek: &Peek, alpha: f32, measure: &dyn Measure) {
    if alpha <= 0.01 || peek.title.is_empty() {
        return;
    }
    let r = rect_for(p, card);
    let side = if r.x > card.x { -1.0 } else { 1.0 };
    let p = p.alpha(alpha).translate(side * RISE * (1.0 - alpha), 0.0);
    crate::widgets::panel_ground(p, r, theme::shape::CORNER, None);
    let pad = theme::space::MD;
    let inner = Rect::new(r.x + pad, r.y + pad, r.w - 2.0 * pad, r.h - 2.0 * pad);
    let mut y = inner.y;
    let bottom = inner.y + inner.h;

    // the foot first, so the synopsis knows what room it has
    // (app text: it wraps to a second line rather than eliding, in every shipped language)
    let hint = (!peek.hint.is_empty()).then(|| {
        TextView::new(&peek.hint, theme::size::CAPTION, theme::TEXT_TERTIARY)
            .with_measure(measure)
            .max_lines(2)
    });
    let hint_body = hint.as_ref().map_or(0.0, |h| h.measure_h(inner.w));
    let hint_h = if hint.is_some() { hint_body + theme::space::XS } else { 0.0 };

    let title = TextView::new(&peek.title, theme::size::HEADLINE, theme::TEXT_PRIMARY)
        .bold()
        .with_measure(measure)
        .h(HAlign::Left)
        .max_lines(2);
    y += title.draw(p, Rect::new(inner.x, y, inner.w, bottom - y));

    if !peek.facts.is_empty() {
        y += theme::space::XS;
        let facts = TextView::new(&peek.facts, theme::size::CAPTION, theme::TEXT_SECONDARY)
            .with_measure(measure)
            .max_lines(1);
        y += facts.draw(p, Rect::new(inner.x, y, inner.w, bottom - y));
    }

    if !peek.summary.is_empty() {
        y += theme::space::SM;
        let body = TextView::new(&peek.summary, theme::size::LABEL, theme::TEXT_SECONDARY).with_measure(measure);
        let room = bottom - hint_h - y;
        let lines = (room / body.line_h()).floor();
        if lines >= 1.0 {
            body.max_lines(lines as usize).draw(p, Rect::new(inner.x, y, inner.w, room));
        }
    }

    if let Some(hint) = hint {
        hint.draw(p, Rect::new(inner.x, bottom - hint_body, inner.w, hint_body));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bubble_opens_only_after_focus_rests_and_closes_at_once_on_a_move() {
        let mut s = PeekState::new();
        let dt = 1.0 / 60.0;
        // walking a shelf at remote speed never opens one
        for i in 0..10 {
            for _ in 0..12 {
                s.tick(Some(i), dt);
            }
            assert_eq!(s.alpha(), 0.0, "card {i}");
        }
        // resting opens it, and it settles exactly
        for _ in 0..120 {
            s.tick(Some(10), dt);
        }
        assert_eq!(s.alpha(), 1.0);
        assert!(s.at_rest(), "an open bubble at rest asks for no frame");
        // a move closes it on the same frame
        s.tick(Some(11), dt);
        assert_eq!(s.alpha(), 0.0);
        // focus leaving the section closes it too and rests
        s.tick(None, dt);
        assert_eq!(s.alpha(), 0.0);
        assert!(s.at_rest());
    }

    #[test]
    fn the_bubble_stands_beside_the_card_and_flips_at_the_right_margin() {
        let p = Painter::root();
        let near_left = Rect::new(MARGIN_X, 300.0, 220.0, 330.0);
        let r = rect_for(p, near_left);
        assert!(r.x >= near_left.x + near_left.w, "to the right");
        assert_eq!((r.y, r.h), (near_left.y, near_left.h), "the card's own band");
        let near_right = Rect::new(SCR_W - MARGIN_X - 220.0, 300.0, 220.0, 330.0);
        let r = rect_for(p, near_right);
        assert!(r.x + r.w <= near_right.x, "flipped to the left");
    }
}

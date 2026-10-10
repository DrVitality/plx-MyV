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
//! **Its two buttons.** The foot of the bubble is a Play button and a Details button. The one OK
//! performs on the card (the card-press preference) is lit in the accent, so the remote's answer
//! is the button that looks pressed; holding OK still opens the card's menu, which the line under
//! them says. With a pointer (the Magic Remote, the simulator's mouse) both are clickable: the
//! bubble publishes where it drew them ([`hit`]) and the application performs a click there
//! itself, and a pointer resting over the bubble ([`covers`]) does not hand focus to the cards the
//! bubble is drawn over — otherwise reaching for a button would close the bubble on the way.
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

/// How long focus rests on a card before its bubble opens by default, in ms. Long enough that a
/// shelf walked at remote speed (a press every 200-400 ms) never opens one; short enough to feel
/// like a hover. The viewer can make it quicker, slower or off (`theme::appearance::Peek`).
pub const DWELL_MS: f32 = 650.0;
/// The bubble's width at 1080p: two posters' worth, enough for a readable synopsis line.
pub const W: f32 = 460.0;
/// The appearance spring's stiffness.
const K_APPEAR: f32 = 260.0;
/// How far the bubble slides in from the card's side as it appears.
const RISE: f32 = 14.0;
/// The panel's radius: drawn in full (the bubble's painter is [`Painter::rounded`]).
const PANEL_RAD: f32 = 24.0;

/// The height of the bubble's two buttons.
const BTN_H: f32 = 52.0;
/// The buttons' radius: half their height, a capsule (drawn in full, like the panel).
const BTN_RAD: f32 = 26.0;
/// The size of the glyph inside a button.
const BTN_ICON: f32 = 22.0;

/// What a card's bubble says: plain strings the screen fills from its row.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Peek {
    pub title: String,
    /// One line: year · length or seasons · rating, already joined. May be empty.
    pub facts: String,
    pub summary: String,
    /// The line at the foot (how to act on the card). May be empty.
    pub hint: String,
    /// The Play button's label; no buttons are drawn when it or [`details`](Self::details) is
    /// empty.
    pub play: String,
    /// The Details button's label.
    pub details: String,
    /// Whether OK on the card plays it (so Play is the lit button) rather than opening its page.
    pub ok_plays: bool,
    /// The card's own key, handed back with a click ([`hit`]) so the application acts on the item
    /// the bubble was drawn for. Opaque here.
    pub key: String,
}

/// One of the bubble's buttons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeekButton {
    Play,
    Details,
}

/// Where the open bubble was last drawn, in screen space, for the pointer.
#[derive(Clone, Debug, Default)]
struct Drawn {
    /// The bubble, widened over the gap to its card so a pointer crossing it stays "on" it.
    cover: Option<Rect>,
    play: Option<Rect>,
    details: Option<Rect>,
    key: String,
    /// The button under the pointer, lit on the next draw.
    hover: Option<PeekButton>,
}

static DRAWN: std::sync::Mutex<Drawn> = std::sync::Mutex::new(Drawn {
    cover: None, play: None, details: None, key: String::new(), hover: None,
});

fn drawn() -> std::sync::MutexGuard<'static, Drawn> {
    DRAWN.lock().unwrap_or_else(|e| e.into_inner())
}

/// Forget where a bubble was drawn — called once before a frame's page is drawn, so a bubble that
/// closed takes its buttons with it. A frame that is not redrawn keeps the last picture's.
pub fn begin_frame() {
    let mut d = drawn();
    d.cover = None;
    d.play = None;
    d.details = None;
}

/// The bubble button under the screen point `(x, y)`, with the key of the card it belongs to.
pub fn hit(x: f32, y: f32) -> Option<(PeekButton, String)> {
    let d = drawn();
    let inside = |r: Option<Rect>| r.is_some_and(|r| x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h);
    if inside(d.play) {
        Some((PeekButton::Play, d.key.clone()))
    } else if inside(d.details) {
        Some((PeekButton::Details, d.key.clone()))
    } else {
        None
    }
}

/// Is the screen point `(x, y)` over the open bubble (or the gap to its card)? A pointer there must
/// not move focus to the cards under it. Also records which button it is over, for the hover look.
pub fn covers(x: f32, y: f32) -> bool {
    let inside = |r: Option<Rect>| r.is_some_and(|r| x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h);
    let mut d = drawn();
    let hover = if inside(d.play) {
        Some(PeekButton::Play)
    } else if inside(d.details) {
        Some(PeekButton::Details)
    } else {
        None
    };
    if hover != d.hover {
        d.hover = hover;
        plx_machine::idle::invalidate();
    }
    inside(d.cover)
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
        self.tick_with(focus, theme::appearance::peek_dwell_ms(), dt);
    }

    /// [`tick`](Self::tick) under an explicit dwell (`None`: the bubble is switched off).
    pub fn tick_with(&mut self, focus: Option<usize>, dwell: Option<f32>, dt: f32) {
        let focus = focus.filter(|_| dwell.is_some());
        let dwell = dwell.unwrap_or(DWELL_MS);
        if focus != self.at {
            self.at = focus;
            self.waited_ms = 0.0;
            self.appear.jump(0.0);
        }
        if focus.is_some() && self.waited_ms < dwell {
            self.waited_ms = (self.waited_ms + dt * 1000.0).min(dwell);
            // the dwell is motion the gate must keep ticking for, as a spring's travel is
            plx_machine::idle::note_spring(self.waited_ms, dwell, 0.0);
        }
        let target = if focus.is_some() && self.waited_ms >= dwell { 1.0 } else { 0.0 };
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
        let dwell = theme::appearance::peek_dwell_ms().unwrap_or(DWELL_MS);
        let target = if self.at.is_some() && self.waited_ms >= dwell { 1.0 } else { 0.0 };
        (self.at.is_none() || self.waited_ms >= dwell) && self.appear.pos == target && self.appear.vel == 0.0
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
    // round by design, whatever the viewer's Corners cap: the bubble is a soft object over the shelf
    let p = p.alpha(alpha).translate(side * RISE * (1.0 - alpha), 0.0).rounded();
    let accent = theme::appearance::accent();
    p.shadow(r, PANEL_RAD, 30.0, 12.0, theme::PEEK_SHADOW);
    // the panel's dark, its top leaning toward the accent, so the bubble carries the app's colour
    let top = theme::mix(theme::appearance::remap(theme::PEEK_TOP), accent, theme::PEEK_ACCENT_W);
    p.rect_rimmed(r, PANEL_RAD, top, theme::PEEK_BOT, theme::PEEK_RIM, theme::PEEK_RIM_TOP);
    // and a pool of accent light along the top edge, fading out a third of the way down
    let glow = Rect::new(r.x, r.y, r.w, r.h * 0.38);
    p.rect(glow, PANEL_RAD, theme::with_a(accent, theme::PEEK_GLOW_A), theme::with_a(accent, 0.0), 0.0);
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
    let buttons = !peek.play.is_empty() && !peek.details.is_empty();
    let buttons_h = if buttons { BTN_H + theme::space::SM } else { 0.0 };

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
        let room = bottom - hint_h - buttons_h - y;
        let lines = (room / body.line_h()).floor();
        if lines >= 1.0 {
            body.max_lines(lines as usize).draw(p, Rect::new(inner.x, y, inner.w, room));
        }
    }

    let hover = drawn().hover;
    let mut placed = (None, None);
    if buttons {
        let top = bottom - hint_h - BTN_H;
        let w = (inner.w - theme::space::SM) * 0.5;
        let play = Rect::new(inner.x, top, w, BTN_H);
        let details = Rect::new(inner.x + w + theme::space::SM, top, w, BTN_H);
        button(p, play, &peek.play, Some(crate::icons::Icon::Play), peek.ok_plays,
            hover == Some(PeekButton::Play), measure);
        button(p, details, &peek.details, None, !peek.ok_plays,
            hover == Some(PeekButton::Details), measure);
        placed = (Some(play), Some(details));
    }

    if let Some(hint) = hint {
        hint.draw(p, Rect::new(inner.x, bottom - hint_body, inner.w, hint_body));
    }

    // where it stands on screen, for the pointer — only once it is fully there
    if alpha >= 0.99 && !p.records() {
        let screen = |q: Rect| Rect::new(q.x + p.dx(), q.y + p.dy(), q.w, q.h);
        let cover = if side < 0.0 {
            Rect::new(card.x + card.w, r.y, r.x + r.w - (card.x + card.w), r.h)
        } else {
            Rect::new(r.x, r.y, card.x - r.x, r.h)
        };
        let mut d = drawn();
        d.cover = Some(screen(cover));
        d.play = placed.0.map(screen);
        d.details = placed.1.map(screen);
        if d.key != peek.key {
            d.key = peek.key.clone();
        }
    }
}

/// One of the bubble's buttons: the lit one (what OK does) in the accent, the other a quiet face;
/// a pointer over either rims it.
fn button(p: Painter, r: Rect, label: &str, icon: Option<crate::icons::Icon>, lit: bool, hover: bool,
    measure: &dyn Measure) {
    let (fill, ink) = if lit {
        (theme::ACCENT, theme::ACCENT_INK)
    } else {
        (theme::CONTROL_IDLE_FILL_UNKEYED, theme::TEXT_PRIMARY)
    };
    let rim = if hover { theme::CONTROL_RIM_FOCUS_UNKEYED } else { theme::CONTROL_RIM_IDLE_UNKEYED };
    p.rect_rimmed(r, BTN_RAD, fill, fill, if lit && !hover { fill } else { rim }, 0.0);
    let sz = theme::size::LABEL;
    let gap = theme::space::XS;
    let room = r.w - 2.0 * theme::space::SM - icon.map_or(0.0, |_| BTN_ICON + gap);
    let run = measure.fit_line(label, room.max(0.0), sz, true);
    let tw = measure.width(&run, sz, true);
    let total = tw + icon.map_or(0.0, |_| BTN_ICON + gap);
    let mut x = r.x + (r.w - total) * 0.5;
    let cy = r.y + r.h * 0.5;
    if let Some(icon) = icon {
        crate::icons::draw(p, icon, Rect::new(x, cy - BTN_ICON * 0.5, BTN_ICON, BTN_ICON), ink);
        x += BTN_ICON + gap;
    }
    p.text(run.as_ptr(), x, plx_gfx::text::text_vcenter_y(sz, 1, cy), sz, ink, 0, 1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bubble_opens_only_after_focus_rests_and_closes_at_once_on_a_move() {
        let mut s = PeekState::new();
        let dt = 1.0 / 60.0;
        let dwell = Some(DWELL_MS);
        // walking a shelf at remote speed never opens one
        for i in 0..10 {
            for _ in 0..12 {
                s.tick_with(Some(i), dwell, dt);
            }
            assert_eq!(s.alpha(), 0.0, "card {i}");
        }
        // resting opens it, and it settles exactly
        for _ in 0..120 {
            s.tick_with(Some(10), dwell, dt);
        }
        assert_eq!(s.alpha(), 1.0);
        assert!(s.at_rest(), "an open bubble at rest asks for no frame");
        // a move closes it on the same frame
        s.tick_with(Some(11), dwell, dt);
        assert_eq!(s.alpha(), 0.0);
        // focus leaving the section closes it too and rests
        s.tick_with(None, dwell, dt);
        assert_eq!(s.alpha(), 0.0);
        assert!(s.at_rest());
        // switched off: resting never opens it
        for _ in 0..120 {
            s.tick_with(Some(3), None, dt);
        }
        assert_eq!(s.alpha(), 0.0);
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

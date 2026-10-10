//! **Settings › Addons & Home**: what the Stremio bridge shows and asks.
//!
//! Three pages of the Settings family, one type ([`StremioPage`]) in three modes:
//!
//! - **the list** (`SettingsPage::Stremio`): every row Home can show, in Home's order, with
//!   whether it is shown; then every installed addon with what it is used for. Each row opens its
//!   own page.
//! - **a Home row** (`SettingsPage::StremioRow(i)`): show it on Home or not, and move it up, down,
//!   to the top or to the bottom. The page follows the row by its id, so it stays on the row it
//!   was opened on however far it moves.
//! - **an addon** (`SettingsPage::StremioAddon(i)`): use it for streams, for catalogs (each only
//!   when the addon offers it).
//!
//! The settings are the bridge's, not the session's: every page reads them on entry and after
//! each change through [`PreferenceCmd::Stremio`], whose answer is the whole list again, and the
//! bridge tells the app to refetch Home when a change lands. Row titles and addon names are the
//! addons' own text, so they are marked server text.

use std::borrow::Cow;
use std::sync::mpsc::Receiver;

use plx_machine::machine::{
    Canon, Cx, Delivery, Effects, EntryId, FocusKey, Fx, GroupId, Handled, InputEvent, InputKind,
    InstanceId, Key, LogicalState, Machine, MachineId,
};
use plx_ui::form::{Form, FormId, FormSection, FormTable, RowKey, RowKind};
use plx_ui::frame::Budget;
use plx_ui::route_screen::RouteLayout;
use plx_ui::screen::{DrawFrame, Enter, FocusSource, FocusTarget, HitSource, RenderStrategy, Screen, ScreenEvent};
use plx_ui::table::Row;
use plx_ui::table_screen::{Header, TableScreen};
use plx_ui::Rect;

use super::family::{form_activate, form_focus, form_right_target, InnerHost, SettingsPage};
use super::registry::{word, AppFx, PreferenceCmd, StremioConfig, StremioOp, BAND};

/// Which of the three pages this is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    List,
    Row(u8),
    Addon(u8),
}

/// A row's identity on any of the three pages.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Id {
    HomeRow(u16),
    Addon(u16),
    Show,
    MoveUp,
    MoveDown,
    MoveTop,
    MoveBottom,
    Streams,
    Catalogs,
    Retry,
}

impl FormId for Id {
    fn key(&self) -> RowKey {
        RowKey(match self {
            Id::Show => 1,
            Id::MoveUp => 2,
            Id::MoveDown => 3,
            Id::MoveTop => 4,
            Id::MoveBottom => 5,
            Id::Streams => 6,
            Id::Catalogs => 7,
            Id::Retry => 8,
            Id::HomeRow(i) => 100 + u32::from(*i),
            Id::Addon(i) => 2000 + u32::from(*i),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Act {
    /// A `Nav` row's slot (opening is the row kind's job).
    Open,
    Retry,
    Show,
    Move(Place),
    Streams,
    Catalogs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Place {
    Up,
    Down,
    Top,
    Bottom,
}

struct State {
    mode: Mode,
    /// The focused row's key.
    sel: u32,
    busy: bool,
    failed: bool,
    /// The bridge's list as last read (not hashed: it is the bridge's state, not this page's).
    config: Option<StremioConfig>,
    /// A row or addon page: the id of what it was opened on, once the list has been read.
    subject: Option<String>,
}

impl LogicalState for State {
    fn write(&self, w: &mut Canon) {
        let (m, i) = match self.mode {
            Mode::List => (0, 0),
            Mode::Row(i) => (1, i),
            Mode::Addon(i) => (2, i),
        };
        w.u8(m).u8(i).u32(self.sel).bool(self.busy).bool(self.failed);
    }
    fn probe(&self, out: &mut String) {
        out.push_str(&format!("addons mode={:?} sel={} busy={} failed={}", self.mode, self.sel, self.busy, self.failed));
    }
}

pub struct StremioPage {
    entry: EntryId,
    form: FormTable<Id, Act, SettingsPage>,
    state: State,
    pending: Option<Receiver<Option<StremioConfig>>>,
    title: String,
    copy: String,
}

impl StremioPage {
    pub fn list(entry: EntryId) -> Self {
        Self::new(entry, Mode::List)
    }
    pub fn row(entry: EntryId, index: u8) -> Self {
        Self::new(entry, Mode::Row(index))
    }
    pub fn addon(entry: EntryId, index: u8) -> Self {
        Self::new(entry, Mode::Addon(index))
    }

    fn new(entry: EntryId, mode: Mode) -> Self {
        let mut page = Self {
            entry,
            form: FormTable::new(BAND),
            state: State { mode, sel: 0, busy: false, failed: false, config: None, subject: None },
            pending: None,
            title: String::new(),
            copy: String::new(),
        };
        page.rebuild();
        page
    }

    /// Ask the bridge: read, or change and read.
    fn send(&mut self, op: StremioOp, fx: &mut Effects<'_, InnerHost>) {
        if self.pending.is_some() {
            return;
        }
        let (reply, receipt) = std::sync::mpsc::channel();
        self.pending = Some(receipt);
        self.state.busy = true;
        self.state.failed = false;
        fx.push(Fx::App(AppFx::Preferences(PreferenceCmd::Stremio { op, reply })));
        self.rebuild();
    }

    /// The answer, if it has come: true when the page changed.
    fn poll(&mut self) -> bool {
        let Some(receipt) = &self.pending else { return false };
        let answer = match receipt.try_recv() {
            Ok(answer) => answer,
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => None,
        };
        self.pending = None;
        self.state.busy = false;
        self.state.failed = answer.is_none();
        if let Some(config) = answer {
            // a child page learns what it is about from the first list it reads
            if self.state.subject.is_none() {
                self.state.subject = match self.state.mode {
                    Mode::List => None,
                    Mode::Row(i) => config.rows.get(usize::from(i)).map(|r| r.id.clone()),
                    Mode::Addon(i) => config.addons.get(usize::from(i)).map(|a| a.id.clone()),
                };
            }
            self.state.config = Some(config);
        }
        self.rebuild();
        true
    }

    fn rebuild(&mut self) {
        let keep = self.form.selected_id().cloned();
        let (form, title, copy) = build(&self.state);
        self.title = title;
        self.copy = copy;
        self.form.table.compact = false;
        self.form.set(form, keep.as_ref());
        self.form.table.list_focused = true;
    }

    fn view(&self) -> TableScreen<'_> {
        let crumb = match self.state.mode {
            Mode::List => plx_platform::i18n::msg::settings_title(),
            _ => plx_platform::i18n::msg::settings_stremio_title(),
        };
        TableScreen::new(Header::new(RouteLayout::screen(), Some(crumb), &self.title, &self.copy),
            &self.form.table, GroupId(0), self.entry).keyed(&self.form)
    }

    fn focus(&self, fx: &mut Effects<'_, InnerHost>) {
        let key = self.form.key_at(self.form.table.sel.max(0) as usize).map_or(0, |k| k.0);
        fx.push(Fx::Deliver(MachineId::Instance(InstanceId(0)),
            Delivery::Screen(ScreenEvent::Enter(Enter::Fresh { focus: FocusTarget::Elem(FocusKey { entry: self.entry, elem: key }) }))));
    }

    fn activate(&mut self, key: u32, fx: &mut Effects<'_, InnerHost>) {
        let Some(act) = form_activate(&self.form, key, fx) else { return };
        if self.state.busy {
            return;
        }
        let Some(op) = op_for(&self.state, &act) else { return };
        self.send(op, fx);
    }
}

/// The change a press asks for, against the list as last read.
fn op_for(state: &State, act: &Act) -> Option<StremioOp> {
    if *act == Act::Retry {
        return Some(StremioOp::Read);
    }
    let config = state.config.as_ref()?;
    let id = state.subject.clone()?;
    match (state.mode, act) {
        (Mode::Row(_), Act::Show) => {
            let row = config.rows.iter().find(|r| r.id == id)?;
            Some(StremioOp::ShowRow { id, shown: !row.shown })
        }
        (Mode::Row(_), Act::Move(place)) => {
            let at = config.rows.iter().position(|r| r.id == id)?;
            let last = config.rows.len().saturating_sub(1);
            let to = match place {
                Place::Up => at.saturating_sub(1),
                Place::Down => (at + 1).min(last),
                Place::Top => 0,
                Place::Bottom => last,
            };
            (to != at).then_some(StremioOp::MoveRow { id, to })
        }
        (Mode::Addon(_), Act::Streams) => {
            let a = config.addons.iter().find(|a| a.id == id)?;
            Some(StremioOp::Streams { id, on: !a.streams? })
        }
        (Mode::Addon(_), Act::Catalogs) => {
            let a = config.addons.iter().find(|a| a.id == id)?;
            Some(StremioOp::Catalogs { id, on: !a.catalogs? })
        }
        _ => None,
    }
}

/// A page's rows, title and copy from its state.
fn build(state: &State) -> (Form<Id, Act, SettingsPage>, String, String) {
    use plx_platform::i18n::msg;
    let busy = state.busy;
    let retry = || FormSection::new("").item(Id::Retry, RowKind::Button, Act::Retry,
        Row::new(msg::settings_stremio_retry()));
    let Some(config) = state.config.as_ref() else {
        // placeholder-exempt: the page's copy line says the bridge has not answered yet; no skeleton is drawn
        let copy = if state.failed { msg::settings_stremio_failed() } else { msg::settings_stremio_loading() };
        let form = if state.failed { Form::new().section(retry()) } else { Form::new() };
        let title = match state.mode {
            Mode::List => msg::settings_stremio_title().to_string(),
            Mode::Row(_) => msg::settings_stremio_rows_section().to_string(),
            Mode::Addon(_) => msg::settings_stremio_addons_section().to_string(),
        };
        return (form, title, copy.to_string());
    };
    let on_home = |shown: bool| if shown { msg::settings_stremio_shown() } else { msg::settings_stremio_hidden() };
    match state.mode {
        Mode::List => {
            let mut rows = FormSection::new(msg::settings_stremio_rows_section());
            for (i, r) in config.rows.iter().enumerate().take(usize::from(u16::MAX)) {
                let i = i as u16;
                rows = rows.item(Id::HomeRow(i), RowKind::Nav(SettingsPage::StremioRow(i.min(255) as u8)), Act::Open,
                    Row::new(&r.title).server_label().detail(r.detail.clone()).server_detail()
                        .value(on_home(r.shown)).dim(busy).chevron(true));
            }
            let mut addons = FormSection::new(msg::settings_stremio_addons_section());
            for (i, a) in config.addons.iter().enumerate().take(usize::from(u16::MAX)) {
                let i = i as u16;
                let used: Vec<&str> = [(a.streams, msg::settings_stremio_streams()), (a.catalogs, msg::settings_stremio_catalogs())]
                    .into_iter()
                    .filter_map(|(on, word)| on.filter(|on| *on).map(|_| word))
                    .collect();
                let value = if a.streams.is_none() && a.catalogs.is_none() {
                    String::new()
                } else if used.is_empty() {
                    msg::settings_stremio_off().to_string()
                } else {
                    used.join(" · ")
                };
                let row = Row::new(&a.name).server_label().detail(a.detail.clone()).server_detail()
                    .value(value).dim(busy);
                let opens = a.streams.is_some() || a.catalogs.is_some();
                addons = if opens {
                    addons.item(Id::Addon(i), RowKind::Nav(SettingsPage::StremioAddon(i.min(255) as u8)), Act::Open, row.chevron(true))
                } else {
                    addons.item(Id::Addon(i), RowKind::Button, Act::Open, row)
                };
            }
            let mut form = Form::new().section(rows).section(addons);
            if state.failed {
                form = form.section(retry());
            }
            let copy = if state.failed { msg::settings_stremio_failed() } else { msg::settings_stremio_copy() };
            (form, msg::settings_stremio_title().to_string(), copy.to_string())
        }
        Mode::Row(_) => {
            let found = state.subject.as_ref()
                .and_then(|id| config.rows.iter().position(|r| &r.id == id).map(|at| (at, &config.rows[at])));
            let Some((at, r)) = found else {
                return (Form::new(), msg::settings_stremio_rows_section().to_string(), msg::settings_stremio_gone().to_string());
            };
            let last = config.rows.len().saturating_sub(1);
            let sec = FormSection::new("")
                .item(Id::Show, RowKind::Toggle, Act::Show,
                    Row::new(msg::settings_stremio_show_on_home()).toggle(r.shown).dim(busy))
                .item_if(at > 0, Id::MoveUp, RowKind::Button, Act::Move(Place::Up),
                    Row::new(msg::settings_stremio_move_up()).dim(busy))
                .item_if(at < last, Id::MoveDown, RowKind::Button, Act::Move(Place::Down),
                    Row::new(msg::settings_stremio_move_down()).dim(busy))
                .item_if(at > 0, Id::MoveTop, RowKind::Button, Act::Move(Place::Top),
                    Row::new(msg::settings_stremio_move_top()).dim(busy))
                .item_if(at < last, Id::MoveBottom, RowKind::Button, Act::Move(Place::Bottom),
                    Row::new(msg::settings_stremio_move_bottom()).dim(busy));
            let copy = format!("{} {} / {}", msg::settings_stremio_position(), at + 1, config.rows.len());
            let mut form = Form::new().section(sec);
            if state.failed {
                form = form.section(retry());
            }
            (form, r.title.clone(), if state.failed { msg::settings_stremio_failed().to_string() } else { copy })
        }
        Mode::Addon(_) => {
            let found = state.subject.as_ref().and_then(|id| config.addons.iter().find(|a| &a.id == id));
            let Some(a) = found else {
                return (Form::new(), msg::settings_stremio_addons_section().to_string(), msg::settings_stremio_gone().to_string());
            };
            let sec = FormSection::new("")
                .item_if(a.streams.is_some(), Id::Streams, RowKind::Toggle, Act::Streams,
                    Row::new(msg::settings_stremio_use_streams()).detail(msg::settings_stremio_use_streams_detail())
                        .toggle(a.streams.unwrap_or(false)).dim(busy))
                .item_if(a.catalogs.is_some(), Id::Catalogs, RowKind::Toggle, Act::Catalogs,
                    Row::new(msg::settings_stremio_use_catalogs()).detail(msg::settings_stremio_use_catalogs_detail())
                        .toggle(a.catalogs.unwrap_or(false)).dim(busy));
            let mut form = Form::new().section(sec);
            if state.failed {
                form = form.section(retry());
            }
            let copy = if state.failed { msg::settings_stremio_failed().to_string() } else { a.detail.clone() };
            (form, a.name.clone(), copy)
        }
    }
}

impl Machine<InnerHost> for StremioPage {
    type Ev = ScreenEvent<InnerHost>;
    fn step(&mut self, ev: &Self::Ev, cx: &Cx<'_, InnerHost>, fx: &mut Effects<'_, InnerHost>) -> Handled {
        match ev {
            ScreenEvent::Mount => {
                self.send(StremioOp::Read, fx);
                Handled::Yes
            }
            // back from a child page: it may have changed the list
            ScreenEvent::Enter(Enter::Restored) => {
                if self.state.config.is_some() {
                    self.send(StremioOp::Read, fx);
                }
                Handled::No
            }
            ScreenEvent::Tick(t) => {
                let had_rows = self.form.table.n_rows() > 0;
                if self.poll() {
                    if !had_rows && self.form.table.n_rows() > 0
                        && cx.focus.current.is_none_or(|k| k.entry != self.entry || self.form.index_of_key(RowKey(k.elem)).is_none()) {
                        self.focus(fx);
                    }
                    fx.invalidate(plx_machine::present::Provenance::Input);
                }
                self.form.table.update(t.dt(), RouteLayout::screen().sectioned_table().h);
                Handled::Yes
            }
            ScreenEvent::FocusMoved { to, .. } => {
                form_focus(&mut self.form, to.elem);
                self.state.sel = self.form.key_at(self.form.table.sel.max(0) as usize).map_or(0, |k| k.0);
                Handled::Yes
            }
            ScreenEvent::Activate(key) => {
                self.activate(*key, fx);
                Handled::Yes
            }
            ScreenEvent::Input(InputEvent { kind: InputKind::Key { key: Key::Right, at_edge: true, .. }, .. }) => {
                if let Some(key) = cx.focus.current.and_then(|k| form_right_target(&self.form, k.elem)) {
                    self.activate(key, fx);
                }
                Handled::Yes
            }
            _ => Handled::No,
        }
    }
}

plx_ui::focusable_via_view!(StremioPage, InnerHost, view);

impl Screen<InnerHost> for StremioPage {
    fn name(&self) -> &'static str {
        word::ADDONS
    }
    fn state(&self) -> &dyn LogicalState {
        &self.state
    }
    fn crumb(&self, _cx: &Cx<'_, InnerHost>) -> Option<Cow<'_, str>> {
        Some(Cow::Borrowed(match self.state.mode {
            Mode::List => plx_platform::i18n::msg::settings_title(),
            _ => plx_platform::i18n::msg::settings_stremio_title(),
        }))
    }
    fn prepare(&mut self, _b: &mut Budget, _cx: &Cx<'_, InnerHost>) {}
    fn draw(&mut self, f: &mut DrawFrame<'_, '_, InnerHost>) {
        plx_ui::screen::Part::<InnerHost>::draw(&mut self.view(), f, Rect::FULL);
    }
    fn render(&self) -> RenderStrategy {
        RenderStrategy::Page
    }
    fn focus_source(&self) -> FocusSource {
        FocusSource::Engine
    }
    fn hit_source(&self) -> HitSource {
        HitSource::Engine
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{StremioAddon, StremioRow};

    fn config() -> StremioConfig {
        let row = |id: &str, shown| StremioRow { id: id.into(), title: id.into(), detail: String::new(), shown };
        StremioConfig {
            rows: vec![row("continue", true), row("library", true), row("x|movie|top", false)],
            addons: vec![StremioAddon { id: "t".into(), name: "Torrentio".into(), detail: "streams".into(),
                streams: Some(true), catalogs: None }],
        }
    }

    fn state(mode: Mode, subject: Option<&str>) -> State {
        State { mode, sel: 0, busy: false, failed: false, config: Some(config()), subject: subject.map(Into::into) }
    }

    #[test]
    fn a_row_page_moves_and_switches_the_row_it_was_opened_on() {
        let s = state(Mode::Row(1), Some("library"));
        assert_eq!(op_for(&s, &Act::Show), Some(StremioOp::ShowRow { id: "library".into(), shown: false }));
        assert_eq!(op_for(&s, &Act::Move(Place::Up)), Some(StremioOp::MoveRow { id: "library".into(), to: 0 }));
        assert_eq!(op_for(&s, &Act::Move(Place::Bottom)), Some(StremioOp::MoveRow { id: "library".into(), to: 2 }));
        let top = state(Mode::Row(0), Some("continue"));
        assert_eq!(op_for(&top, &Act::Move(Place::Up)), None, "the top row has nowhere up to go");
        let (form, _, _) = build(&top);
        let mut t: FormTable<Id, Act, SettingsPage> = FormTable::new(BAND);
        t.set(form, None);
        assert!(t.index_of(&Id::MoveUp).is_none() && t.index_of(&Id::MoveDown).is_some());
    }

    #[test]
    fn an_addon_page_offers_only_the_switches_the_addon_has() {
        let s = state(Mode::Addon(0), Some("t"));
        assert_eq!(op_for(&s, &Act::Streams), Some(StremioOp::Streams { id: "t".into(), on: false }));
        assert_eq!(op_for(&s, &Act::Catalogs), None);
        let (form, title, _) = build(&s);
        assert_eq!(title, "Torrentio");
        let mut t: FormTable<Id, Act, SettingsPage> = FormTable::new(BAND);
        t.set(form, None);
        assert!(t.index_of(&Id::Streams).is_some() && t.index_of(&Id::Catalogs).is_none());
    }

    #[test]
    fn the_list_names_every_row_and_addon_and_a_failure_offers_retry() {
        let s = state(Mode::List, None);
        let (form, _, _) = build(&s);
        let mut t: FormTable<Id, Act, SettingsPage> = FormTable::new(BAND);
        t.set(form, None);
        assert!(t.index_of(&Id::HomeRow(2)).is_some() && t.index_of(&Id::Addon(0)).is_some());
        let failed = State { config: None, failed: true, ..state(Mode::List, None) };
        let (form, _, _) = build(&failed);
        t.set(form, None);
        assert!(t.index_of(&Id::Retry).is_some());
    }
}

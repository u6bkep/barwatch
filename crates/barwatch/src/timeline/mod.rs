//! The replay timeline: a component that shows a replay's statistics over game
//! time, with two positions on one axis — where the game is, and a cursor that
//! picks what every readout shows.
//!
//! Laid out like a video editor: a gutter of labels and readouts on the left,
//! tracks on a shared time axis on the right (time ruler, event lane, the lead
//! strip, one large chart, one compact row per metric), and one overlay across
//! all tracks that carries the position lines and the fog over the future, so
//! the lines run unbroken from top to bottom.
//!
//! The large chart's title is the control that picks its metric, and the rows
//! below it carry a radio mark each: one of them is what the large chart
//! shows. A third, lighter line follows the pointer across the tracks with a
//! readout of the track under it; it moves nothing.
//!
//! Rules it keeps: nothing past the game position shows while the future is
//! hidden (no line, marker, scale, total or axis length gives it away); the
//! cursor moves freely and instantly; the game only moves forward, and only
//! through an explicit jump that every viewer gets.

// Sample loops index several parallel series at once.
#![allow(clippy::needless_range_loop)]

pub mod axis;
mod data;
mod dds;
mod icons;
mod marks;
mod paint;
mod proto;

pub use data::{Data, MetricDef};
pub use icons::Icons;
pub use marks::{MarkClass, Marker};
#[cfg(test)]
use marks::{Faction, MarkKind};
pub use paint::shaders;
pub use proto::{Proto, ProtoOptions};

use axis::{clock, frac, horizon, nice_ceil, number, Eased};
use damascene_core::prelude::*;
use damascene_core::widgets::{collapsible, select, switch, toggle};
use marks::cluster;
use paint::{placed, quad, Anchor, Place};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const KEY_AREA: &str = "tl:area";
const KEY_LEAD: &str = "tl:area:lead";
const KEY_MARKER: &str = "tl:marker";
const KEY_ROW: &str = "tl:row";
const KEY_PLAYER: &str = "tl:player";
/// The large chart's title, which is also the control that picks its metric.
const KEY_METRIC: &str = "tl:metric";

const RULER_H: f32 = 30.0;
const LANE_H: f32 = 32.0;
const LEAD_H: f32 = 64.0;
const HERO_H: f32 = 196.0;
/// Air between the strip and the large chart.
const HERO_GAP_H: f32 = 10.0;
/// The line between the large chart and the rows that says what the rows' marks choose.
const LIST_HEAD_H: f32 = 24.0;
const ROW_H: f32 = 34.0;
const BADGE: f32 = 26.0;
/// Below this window height only the strip is shown.
const COMPACT_BELOW: f32 = 460.0;
/// A cursor this close to the game line or to a marker (px) snaps onto it.
const SNAP_PX: f32 = 6.0;
/// A jump counts as arrived this close to its target (servers land on keyframes).
const JUMP_TOLERANCE_S: f32 = 2.0;
/// A jump that has not arrived after this long is given up on.
const JUMP_GIVE_UP: Duration = Duration::from_secs(90);
/// A jump shorter than this is not worth interrupting everyone for.
const MIN_JUMP_S: f32 = 10.0;
/// The readout beside the hover line: its width, and how far it stands off the line.
const READOUT_W: f32 = 236.0;
const READOUT_GAP: f32 = 14.0;
/// The most lines of values a hover readout lists.
const READOUT_ROOM: usize = 6;

/// The cursor's colour, apart from every team colour and from the game's white.
fn cursor_color() -> Color {
    Color::srgb_u8(255, 204, 92)
}

fn game_color() -> Color {
    Color::srgb_u8(244, 244, 245)
}

/// The surface under the large chart and under the row it is showing: the
/// same tone in both places, so the two read as one thing.
fn chosen_surface() -> Color {
    Color::srgb_u8(21, 21, 24)
}

struct Chip {
    at: f32,
    anchor: Anchor,
    /// About how wide it draws, px, so tick labels can make room.
    width: f32,
    what: ChipKind,
}

enum ChipKind {
    Game,
    Cursor(f32),
    Jump(f32),
    Jumping(f32),
    /// The moment under the pointer.
    Hover(f32),
}

impl Chip {
    /// The pixels it covers along an axis `w` wide, flipped at the edges as
    /// [`Place::Tag`] flips it.
    fn span(&self, w: f32) -> (f32, f32) {
        let x = self.at * w;
        match self.anchor {
            Anchor::Center => (x - self.width / 2.0, x + self.width / 2.0),
            Anchor::After if x + self.width > w => (x - self.width, x),
            Anchor::After => (x, x + self.width),
            Anchor::Before if x - self.width < 0.0 => (x, x + self.width),
            Anchor::Before => (x - self.width, x),
        }
    }
}

/// A track of the timeline, as the pointer finds it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Track {
    Ruler,
    Events,
    Lead,
    Hero,
    /// The space above the large chart and the line above the rows: on the
    /// axis like the rest, with nothing of their own to read.
    Gap,
    Row(usize),
}

impl Track {
    /// From the key of a track's time area (`tl:area:…`).
    fn from_key(key: &str) -> Option<Track> {
        match key.strip_prefix(KEY_AREA)?.strip_prefix(':')? {
            "ruler" => Some(Track::Ruler),
            "events" => Some(Track::Events),
            "lead" => Some(Track::Lead),
            "hero" => Some(Track::Hero),
            "gap" | "list" => Some(Track::Gap),
            row => row.strip_prefix("row")?.parse().ok().map(Track::Row),
        }
    }
}

/// What the pointer points at on the tracks, without a click: where the hover
/// line goes and what the readout beside it says. It never moves the cursor.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Hover {
    /// The moment a click here would pick, snapped as a click is.
    t: f32,
    /// On the game line: a click would go back to following the game.
    on_game: bool,
    track: Track,
    /// The pointer's height, px below the top of the tracks.
    y: f32,
}

/// A line of the hover readout: whose value, and the value.
struct ReadoutLine {
    color: Color,
    name: String,
    value: String,
    /// The value as a number, to rank by when there are too many to list.
    v: f32,
    /// The player picked out in the large chart.
    picked: bool,
}

/// What the last frame drew, for the calls that come between frames.
#[derive(Clone, Copy, Default)]
struct Shown {
    /// The hover line was up.
    hover: bool,
    /// The large chart (and so the anchor of its menu) was there.
    hero: bool,
    tracks_h: f32,
}

/// What the timeline asks of whoever hosts it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    /// Ask the server to move every viewer to this game time.
    Jump(f32),
}

#[derive(Default)]
struct Motion {
    horizon: Option<Eased>,
    tops: HashMap<(usize, bool), Eased>,
    moving: bool,
    last: Option<Instant>,
}

pub struct Timeline {
    pub data: Data,
    icons: Icons,
    /// Where the game is, in game seconds.
    game_s: f32,
    /// What the readouts show; `None` follows the game.
    cursor: Option<f32>,
    hide_future: bool,
    per_player: bool,
    /// The metric of the large chart, as an index into `data.metrics`.
    hero: usize,
    /// A team picked out in the large chart; the others dim.
    picked: Option<usize>,
    /// The target of a jump that has been asked for and not arrived at yet.
    jump: Option<f32>,
    jump_asked: Option<Instant>,
    table_open: bool,
    /// The list of metrics under the large chart's title is open.
    metric_open: bool,
    /// Where the pointer is, in window px, as the host reports every move.
    pointer: Option<(f32, f32)>,
    /// A press on the tracks is held (the cursor is being dragged).
    pressed: bool,
    focus_request: Option<String>,
    motion: RefCell<Motion>,
    shown: Cell<Shown>,
    #[cfg(test)]
    forced_hover: Option<Hover>,
}

/// The geometry of one frame, worked out once.
struct Frame {
    /// Right edge of the axis, in game seconds.
    end_s: f32,
    /// Width of the time axis, px.
    w: f32,
    gutter: f32,
    compact: bool,
    narrow: bool,
    now: Instant,
    hover: Option<Hover>,
    /// The key under the pointer, for hover feedback the library does not draw.
    hovered: Option<String>,
}

impl Frame {
    fn is_hovered(&self, key: &str) -> bool {
        self.hovered.as_deref() == Some(key)
    }
}

impl Timeline {
    pub fn new(data: Data) -> Timeline {
        let hero = data.metric_index(data::METAL_INCOME.id).unwrap_or(0);
        Timeline {
            data,
            icons: Icons::default(),
            game_s: 0.0,
            cursor: None,
            hide_future: true,
            per_player: false,
            hero,
            picked: None,
            jump: None,
            jump_asked: None,
            table_open: false,
            metric_open: false,
            pointer: None,
            pressed: false,
            focus_request: None,
            motion: RefCell::default(),
            shown: Cell::default(),
            #[cfg(test)]
            forced_hover: None,
        }
    }

    /// The host passes on every pointer move (`None`: the pointer left the
    /// window), in the window's logical px. Returns whether a frame is needed:
    /// while the pointer is over the tracks, and once more when it has left
    /// them, so the hover line goes away.
    pub fn pointer_moved(&mut self, position: Option<(f32, f32)>, cx: &EventCx) -> bool {
        if self.pointer == position {
            return false;
        }
        self.pointer = position;
        let shown = self.shown.get();
        let tracks = cx.rect_of_key(&Self::area_key("ruler")).map(|r| Rect::new(r.x, r.y, r.w, shown.tracks_h));
        let over = position.is_some_and(|(x, y)| tracks.is_some_and(|r| r.contains(x, y)));
        // The frame asked for here takes a line that was up down again; until
        // one is drawn (a host may not be showing the timeline), ask no more.
        self.shown.set(Shown { hover: false, ..shown });
        over || shown.hover
    }

    /// Layers the host must put at the root of its view, above everything
    /// else (`overlays(main, layers)`): the list of metrics while it is open.
    /// A popover cannot be drawn from inside [`Timeline::view`].
    pub fn layers(&self) -> Vec<El> {
        if !self.metric_open || !self.shown.get().hero {
            return Vec::new();
        }
        let options = self.data.metrics.iter().enumerate().map(|(m, s)| (m, s.def.about));
        vec![select::select_menu_selected(KEY_METRIC, options, &self.hero)]
    }

    pub fn set_icons(&mut self, icons: Icons) {
        self.icons = icons;
    }

    pub fn game_s(&self) -> f32 {
        self.game_s
    }

    pub fn cursor_s(&self) -> f32 {
        self.cursor.unwrap_or(self.game_s)
    }

    pub fn jumping_to(&self) -> Option<f32> {
        self.jump
    }

    /// Moves the game position; called as the game reports it.
    pub fn set_game(&mut self, game_s: f32) {
        let before = self.game_s;
        self.game_s = game_s.clamp(0.0, self.data.duration_s);
        let over = self.game_s >= self.data.duration_s;
        // Arrived (a server may land a little short, on a keyframe), or given up on.
        let stale = self.jump_asked.is_some_and(|t| t.elapsed() > JUMP_GIVE_UP);
        if self.jump.is_some_and(|target| over || stale || self.game_s >= target.min(self.data.duration_s) - JUMP_TOLERANCE_S) {
            self.jump = None;
            self.jump_asked = None;
        }
        // The game reached or passed the cursor: carry on together. Past the
        // end of the game a cursor has nothing left to wait for.
        if self.cursor.is_some_and(|c| (c > before - 0.75 && c <= self.game_s + 0.75 && self.jump.is_none()) || (over && c > self.game_s)) {
            self.cursor = None;
        }
    }

    /// The host tells the timeline a jump will not happen (refused, or failed).
    // For the party view, once a server can refuse a jump.
    #[allow(dead_code)]
    pub fn cancel_jump(&mut self) {
        self.jump = None;
        self.jump_asked = None;
    }

    fn ask_jump(&mut self) -> Option<Action> {
        let target = self.can_jump()?;
        self.jump = Some(target);
        self.jump_asked = Some(Instant::now());
        // The axis may reach past the real end while the future is hidden; the
        // server gets no further than the end either way.
        Some(Action::Jump(target.min(self.data.duration_s)))
    }

    /// The key the host should focus (via `App::drain_focus_requests`) so the
    /// arrow keys reach the timeline after a click on it.
    pub fn take_focus_request(&mut self) -> Option<String> {
        self.focus_request.take()
    }

    /// Whether the numbers at `t` may be shown.
    pub fn revealed(&self, t: f32) -> bool {
        (!self.hide_future || t <= self.game_s) && t <= self.data.computed_s
    }

    /// How many samples may be drawn: those the game has passed, or all.
    pub fn drawable_samples(&self) -> usize {
        let limit = if self.hide_future { self.game_s } else { self.data.duration_s };
        let limit = limit.min(self.data.computed_s);
        ((limit / self.data.period_s).floor().max(0.0) as usize + 1).min(self.data.n)
    }

    /// The moment the readouts show: the cursor, or the game (held back to the
    /// last statistics worked out, when the game is ahead of them).
    pub fn readout_s(&self) -> f32 {
        self.cursor.unwrap_or(self.game_s.min(self.data.computed_s))
    }

    /// Why the numbers at `t` are not shown, if they are not.
    fn hidden_reason(&self, t: f32) -> Option<&'static str> {
        if self.game_s < self.data.period_s {
            Some("Waiting for the first numbers")
        } else if self.hide_future && t > self.game_s {
            Some("Hidden until the game gets there")
        } else if t > self.data.computed_s {
            Some("Still being worked out")
        } else {
            None
        }
    }

    /// How many samples the game has passed: the played part of a line. When
    /// the future is shown, the rest is drawn too, but quieter.
    fn played_samples(&self) -> usize {
        let limit = self.game_s.min(self.data.computed_s);
        ((limit / self.data.period_s).floor().max(0.0) as usize + 1).min(self.data.n)
    }

    /// The axis length the rules allow right now (before easing).
    pub fn horizon_s(&self) -> f32 {
        horizon(self.game_s, self.data.duration_s, self.hide_future)
    }

    /// The player picked out, while the view is by player. Checked against
    /// the data, which a host may replace under a selection.
    fn picked(&self) -> Option<usize> {
        self.picked.filter(|p| self.per_player && *p < self.data.teams.len())
    }

    fn can_jump(&self) -> Option<f32> {
        self.cursor.filter(|c| *c > self.game_s + MIN_JUMP_S && self.jump.is_none() && self.game_s < self.data.duration_s)
    }

    fn marker_visible(&self, m: &Marker) -> bool {
        !self.hide_future || m.t <= self.game_s
    }

    /// The moment a pointer at `t` on the axis means: `None` on the game line
    /// (close enough to it), else `t`, pulled onto a marker that is close.
    fn snap(&self, t: f32, end_s: f32, w: f32) -> Option<f32> {
        let px = |dt: f32| dt.abs() / end_s.max(1.0) * w;
        if px(t - self.game_s) <= SNAP_PX {
            return None;
        }
        let near = self
            .data
            .markers
            .iter()
            .filter(|m| self.marker_visible(m) && px(m.t - t) <= SNAP_PX)
            .min_by(|a, b| (a.t - t).abs().total_cmp(&(b.t - t).abs()))
            .map(|m| m.t);
        Some(near.unwrap_or(t).clamp(0.0, end_s))
    }

    /// Sets the cursor from an axis position, snapping onto the game line and
    /// onto markers that are close.
    fn point_cursor(&mut self, t: f32, end_s: f32, w: f32) {
        self.cursor = self.snap(t, end_s, w);
    }

    /// What the pointer points at: `key` is the element under it, `axis` the
    /// time axis in window px (its top is the top of the tracks). Nothing
    /// while a press is held, off the tracks, or under something that covers
    /// them (a menu, a button riding the ruler).
    fn hover_at(&self, key: Option<&str>, axis: Option<Rect>, end_s: f32) -> Option<Hover> {
        if self.pressed {
            return None;
        }
        let ((px, py), key, axis) = (self.pointer?, key?, axis?);
        let marker = key.strip_prefix(KEY_MARKER).and_then(|r| r.strip_prefix(':')).and_then(|r| r.parse::<usize>().ok());
        let (track, t) = match marker {
            // A badge: the click goes to its marker, wherever on it the pointer is.
            Some(i) => {
                let t = self.data.markers.get(i)?.t;
                (Track::Events, (t != self.game_s).then_some(t))
            }
            None => (Track::from_key(key)?, self.snap(axis::time_at(px, axis.x, axis.w, end_s), end_s, axis.w)),
        };
        Some(Hover { t: t.unwrap_or(self.game_s), on_game: t.is_none(), track, y: py - axis.y })
    }

    pub fn on_event(&mut self, event: &UiEvent, cx: &EventCx) -> Option<Action> {
        let route = event.route().unwrap_or_default().to_owned();
        // Escape at the window, or on one of ours; not one meant for another widget.
        // It closes the list of metrics first, if that is open.
        if event.kind == UiEventKind::Escape && (route.is_empty() || route.starts_with("tl:")) {
            if !std::mem::take(&mut self.metric_open) {
                self.cursor = None;
            }
            return None;
        }
        // A press held on the tracks drags the cursor: no hover line meanwhile.
        match event.kind {
            UiEventKind::PointerDown => self.pressed = route.starts_with(KEY_AREA),
            UiEventKind::PointerUp | UiEventKind::PointerCancel | UiEventKind::Click => self.pressed = false,
            _ => {}
        }
        let metrics = self.data.metrics.len();
        if select::apply_event(&mut self.hero, &mut self.metric_open, event, KEY_METRIC, |s| s.parse().ok().filter(|m| *m < metrics)) {
            return None;
        }
        let end_s = self.motion.borrow().horizon.map(|h| h.value).unwrap_or_else(|| self.horizon_s());
        if route.starts_with(KEY_AREA) {
            match event.kind {
                UiEventKind::PointerDown | UiEventKind::Drag | UiEventKind::Click => {
                    if let (Some(rect), Some(x)) = (event.target_rect().or_else(|| cx.rect_of_key(&route)), event.pointer_x()) {
                        self.point_cursor(axis::time_at(x, rect.x, rect.w, end_s), end_s, rect.w);
                    }
                    if event.kind == UiEventKind::PointerDown {
                        self.focus_request = Some(KEY_LEAD.into());
                    }
                    return None;
                }
                UiEventKind::KeyDown => {
                    self.on_key(event, end_s);
                    return None;
                }
                // Enter on the focused strip asks for the jump the cursor offers.
                UiEventKind::Activate => return self.ask_jump(),
                _ => {}
            }
        }
        if let Some(m) = route.strip_prefix(KEY_ROW).and_then(|r| r.strip_prefix(':')).and_then(|r| r.parse::<usize>().ok()) {
            if event.is_click_or_activate(&route) && m < self.data.metrics.len() {
                self.hero = m;
            } else if event.kind == UiEventKind::KeyDown {
                self.on_key(event, end_s);
            }
            return None;
        }
        if let Some(i) = route.strip_prefix(KEY_MARKER).and_then(|r| r.strip_prefix(':')).and_then(|r| r.parse::<usize>().ok()) {
            if event.is_click_or_activate(&route) {
                if let Some(m) = self.data.markers.get(i) {
                    self.cursor = (m.t != self.game_s).then_some(m.t);
                }
            }
            return None;
        }
        if let Some(t) = route.strip_prefix(KEY_PLAYER).and_then(|r| r.strip_prefix(':')).and_then(|r| r.parse::<usize>().ok()) {
            if event.is_click_or_activate(&route) {
                self.picked = if self.picked == Some(t) { None } else { Some(t) };
            }
            return None;
        }
        if event.is_click_or_activate("tl:follow") {
            self.cursor = None;
        } else if event.is_click_or_activate("tl:jump") {
            return self.ask_jump();
        }
        switch::apply_event(&mut self.hide_future, event, "tl:hide");
        collapsible::apply_event(&mut self.table_open, event, "tl:table");
        if toggle::apply_event_single(&mut self.per_player, event, "tl:group", |s| Some(s == "players")) && !self.per_player {
            self.picked = None;
        }
        None
    }

    fn on_key(&mut self, event: &UiEvent, end_s: f32) {
        let Some(press) = event.key_press.as_ref() else { return };
        let step = if press.modifiers.shift { 60.0 } else { self.data.period_s };
        let here = self.cursor_s();
        let target = match press.logical.named() {
            Some(NamedKey::ArrowLeft) => (here - step).max(0.0),
            Some(NamedKey::ArrowRight) => (here + step).min(end_s),
            Some(NamedKey::Home) => 0.0,
            Some(NamedKey::End) => {
                self.cursor = None;
                return;
            }
            _ => return,
        };
        // Stepping onto the game position goes back to following it.
        let crossed = (here - self.game_s) * (target - self.game_s) <= 0.0 && self.cursor.is_some();
        self.cursor = if crossed && (target - self.game_s).abs() < step { None } else { Some(target) };
    }

    fn frame(&self, cx: &BuildCx) -> Frame {
        let viewport_w = cx.viewport_width().unwrap_or(1180.0);
        let narrow = viewport_w < 900.0;
        let gutter = if narrow { 212.0 } else { 300.0 };
        let compact = cx.viewport_height().is_some_and(|h| h < COMPACT_BELOW);
        let w = cx
            .rect_of_key(KEY_LEAD)
            .map(|r| r.w)
            .unwrap_or(viewport_w - 2.0 * tokens::SPACE_4 - gutter - 12.0)
            .max(120.0);
        let now = Instant::now();
        let mut motion = self.motion.borrow_mut();
        motion.moving = false;
        let target = self.horizon_s();
        let mut h = motion.horizon.unwrap_or(Eased::new(target));
        let moving = h.follow(target, now);
        motion.horizon = Some(h);
        motion.moving |= moving;
        motion.last = Some(now);
        let hover = self.hover_at(cx.hovered_key(), cx.rect_of_key(&Self::area_key("ruler")), h.value);
        #[cfg(test)]
        let hover = self.forced_hover.or(hover);
        Frame { end_s: h.value, w, gutter, compact, narrow, now, hover, hovered: cx.hovered_key().map(str::to_owned) }
    }

    pub fn view(&self, cx: &BuildCx) -> El {
        let f = self.frame(cx);
        let mut tracks = vec![self.ruler(&f), self.event_lanes(&f), self.lead_strip(&f)];
        if !f.compact {
            tracks.push(self.gap(&f, HERO_GAP_H, "gap", El::new(Kind::Custom("tl_gutter"))));
            tracks.push(self.hero_chart(&f));
            let label = column([text("Shown in the large chart:").caption().muted()]).justify(Justify::End).padding(Sides { bottom: 3.0, ..Sides::zero() });
            tracks.push(self.gap(&f, LIST_HEAD_H, "list", label));
            for m in 0..self.data.metrics.len() {
                tracks.push(self.metric_row(&f, m));
            }
        }
        let tracks_h = self.tracks_height(&f);
        self.shown.set(Shown { hover: f.hover.is_some(), hero: !f.compact, tracks_h });
        let grid = stack([column(tracks).width(Size::Fill(1.0)), self.overlay(&f, tracks_h)]).width(Size::Fill(1.0));
        let mut parts = vec![self.header(&f), grid];
        if !f.compact {
            parts.push(self.table());
        }
        let root = column(parts).gap(tokens::SPACE_3).width(Size::Fill(1.0));
        let moving = self.motion.borrow().moving || self.jump.is_some();
        if moving {
            root.redraw_within(Duration::from_millis(16))
        } else {
            root
        }
    }

    /// Where a track sits among the tracks: its top and its height, px.
    fn track_span(&self, track: Track) -> (f32, f32) {
        let lanes = self.lanes_height();
        let hero_top = RULER_H + lanes + LEAD_H + HERO_GAP_H;
        match track {
            Track::Ruler => (0.0, RULER_H),
            Track::Events => (RULER_H, lanes),
            Track::Lead => (RULER_H + lanes, LEAD_H),
            Track::Gap => (hero_top - HERO_GAP_H, HERO_GAP_H),
            Track::Hero => (hero_top, self.hero_height()),
            Track::Row(m) => (hero_top + self.hero_height() + LIST_HEAD_H + m as f32 * ROW_H, ROW_H),
        }
    }

    fn tracks_height(&self, f: &Frame) -> f32 {
        let base = RULER_H + self.lanes_height() + LEAD_H;
        if f.compact {
            base
        } else {
            base + HERO_GAP_H + self.hero_height() + LIST_HEAD_H + ROW_H * self.data.metrics.len() as f32
        }
    }

    // ---- header ----------------------------------------------------------

    fn header(&self, f: &Frame) -> El {
        let d = &self.data;
        let over = self.game_s >= d.duration_s;
        let mut clock_line = vec![text(clock(self.game_s)).heading().tabular_numerals()];
        if over {
            clock_line.push(badge("Game over").muted());
        } else if !self.hide_future {
            clock_line.push(text(format!("of {}", clock(d.duration_s))).muted());
        }
        let game = column([
            text("GAME").caption().muted(),
            row(clock_line).gap(tokens::SPACE_2).align(Align::Center),
        ])
        .gap(2.0);

        let (status, detail, follow): (El, String, bool) = if let Some(target) = self.jump {
            (
                row([spinner_with_color(cursor_color()).width(Size::Fixed(14.0)).height(Size::Fixed(14.0)), text(format!("Jumping everyone to {}", clock(target)))])
                    .gap(tokens::SPACE_2)
                    .align(Align::Center),
                "The game runs ahead until it gets there".into(),
                false,
            )
        } else if let Some(c) = self.cursor {
            let delta = c - self.game_s;
            let detail = if delta >= 0.0 {
                if self.revealed(c) {
                    format!("{} ahead of the game", clock(delta))
                } else {
                    format!("{} ahead of the game · numbers hidden until it gets there", clock(delta))
                }
            } else {
                format!("{} behind the game", clock(-delta))
            };
            (
                row([status_dot(cursor_color()), text(format!("Looking at {}", clock(c))).tabular_numerals()])
                    .gap(tokens::SPACE_2)
                    .align(Align::Center),
                detail,
                true,
            )
        } else {
            (
                row([status_dot(game_color()), text(if over { "The game is over" } else { "Following the game" })])
                    .gap(tokens::SPACE_2)
                    .align(Align::Center),
                if f.compact { String::new() } else { "Click or drag the timeline to look back or ahead".into() },
                false,
            )
        };
        let mut status_col = vec![status];
        if !detail.is_empty() {
            status_col.push(text(detail).caption().muted().ellipsis());
        }
        let mut items = vec![
            game,
            column(status_col).gap(2.0).width(Size::Fill(1.0)).padding(Sides { left: tokens::SPACE_6, ..Sides::zero() }),
        ];
        if follow {
            items.push(row([button("Follow the game").key("tl:follow").secondary().small(), kbd("Esc")]).gap(tokens::SPACE_2).align(Align::Center));
        }
        if d.sides.iter().any(|s| s.teams.len() > 1) && !f.compact {
            items.push(toggle_group(
                "tl:group",
                &if self.per_player { "players" } else { "sides" },
                [("sides", "By side"), ("players", "By player")],
            ));
        }
        items.push(
            row([switch("tl:hide", self.hide_future), text("Hide what has not happened yet")])
                .gap(tokens::SPACE_2)
                .align(Align::Center),
        );
        row(items).gap(tokens::SPACE_4).align(Align::Center).height(Size::Fixed(48.0))
    }

    // ---- tracks ----------------------------------------------------------

    /// A track: its gutter on the left and its time area on the right.
    fn track(&self, f: &Frame, h: f32, gutter: El, area: El) -> El {
        self.track_inset(f, h, Sides::zero(), gutter, area)
    }

    /// A track whose gutter keeps its content `inset` from its edges: one
    /// that is drawn on a surface of its own.
    fn track_inset(&self, f: &Frame, h: f32, inset: Sides, gutter: El, area: El) -> El {
        let padding = Sides { right: tokens::SPACE_3, ..inset };
        row([gutter.width(Size::Fixed(f.gutter)).height(Size::Fixed(h)).padding(padding), area]).height(Size::Fixed(h)).width(Size::Fill(1.0))
    }

    fn area_key(id: &str) -> String {
        format!("{KEY_AREA}:{id}")
    }

    fn ruler(&self, f: &Frame) -> El {
        let mut items = Vec::new();
        // Tick labels give way to the position chips riding the same row.
        let chips: Vec<(f32, f32)> = self.chips(f).iter().map(|c| c.span(f.w)).collect();
        for tick in axis::ticks(f.end_s, f.w, &chips) {
            let at = frac(tick.t, f.end_s);
            if tick.labelled {
                items.push((
                    Place::Tag { at, w: Some(axis::TICK_LABEL_BOX), h: 16.0, top: 6.0, anchor: Anchor::Center },
                    text(clock(tick.t)).caption().muted().center_text().tabular_numerals(),
                ));
            }
            items.push((Place::Line { at, w: 1.0, top: RULER_H - 5.0, bottom: 0.0 }, quad(tokens::BORDER)));
        }
        items.push((Place::Span { from: 0.0, to: 1.0, top: RULER_H - 1.0, bottom: 0.0 }, quad(tokens::BORDER)));
        let area = placed(Some(RULER_H), 0.0, items).key(Self::area_key("ruler")).cursor(Cursor::Pointer);
        self.track(f, RULER_H, El::new(Kind::Custom("tl_gutter")), area)
    }

    /// One lane per side (up to four), else one shared lane.
    fn lanes(&self) -> usize {
        let s = self.data.sides.len();
        if s <= 4 { s.max(1) } else { 1 }
    }

    fn lanes_height(&self) -> f32 {
        self.lanes() as f32 * LANE_H + 4.0
    }

    fn event_lanes(&self, f: &Frame) -> El {
        let d = &self.data;
        let lanes = self.lanes();
        let h = self.lanes_height();
        let min_gap_s = (BADGE + 4.0) / f.w * f.end_s;
        let sample = d.markers.iter().any(|m| m.sample);
        let mut items = Vec::new();
        let mut labels = Vec::new();
        for lane in 0..lanes {
            let members: Vec<usize> = (0..d.markers.len())
                .filter(|i| {
                    let m = &d.markers[*i];
                    (lanes == 1 || d.teams[m.team].side == lane) && self.marker_visible(m) && m.t <= f.end_s
                })
                .collect();
            for c in cluster(&d.markers, &members, min_gap_s) {
                let top = lane as f32 * LANE_H + (LANE_H - BADGE) / 2.0 + 2.0;
                items.push((
                    Place::Tag { at: frac(c.t, f.end_s), w: Some(BADGE + 4.0), h: BADGE + 2.0, top, anchor: Anchor::Center },
                    self.badge(f, &c.members, c.t > self.game_s),
                ));
            }
            let (name, color) = if lanes == 1 {
                ("Events".to_owned(), tokens::MUTED_FOREGROUND)
            } else {
                (d.sides[lane].name.clone(), d.sides[lane].color)
            };
            let mut label = vec![status_dot(color), text(name).caption().muted().ellipsis().width(Size::Fill(1.0))];
            if sample && lane == 0 {
                label.push(badge("sample events").warning().key("tl:sample-note").tooltip("Made up to try the design (--sample-markers): not from this replay"));
            }
            let legend = [MarkClass::Built, MarkClass::Tech, MarkClass::Attack, MarkClass::Lost]
                .map(|c| format!("{} {}", c.sign(), c.label()))
                .join("   ");
            labels.push(
                row(label)
                    .gap(tokens::SPACE_2)
                    .align(Align::Center)
                    .height(Size::Fixed(LANE_H))
                    .key(format!("tl:lane:{lane}"))
                    .tooltip(format!("Milestones, pictured by the unit that set them off. Corner signs: {legend}")),
            );
        }
        if !d.markers.iter().any(|m| self.marker_visible(m)) && !f.compact {
            labels = vec![row([text("No events yet").caption().muted()]).height(Size::Fixed(LANE_H))];
        }
        let area = placed(Some(h), 0.0, items).key(Self::area_key("events")).cursor(Cursor::Pointer);
        self.track(f, h, column(labels).padding(Sides { top: 2.0, ..Sides::zero() }), area)
    }

    /// A marker badge in BAR's style: the unit's picture in a frame of its
    /// team's colour, a small sign for what kind of event it is, and a count
    /// when several share the spot.
    fn badge(&self, f: &Frame, members: &[usize], unplayed: bool) -> El {
        let d = &self.data;
        let lead = &d.markers[members[0]];
        let team = &d.teams[lead.team];
        let inner = match lead.picture().and_then(|p| self.icons.get(p)) {
            // On a dark backdrop, as BAR does: some pictures (the skull) are cut out.
            Some(img) => stack([image(img.clone()).image_fit(ImageFit::Cover).radius(2.0).fill_size()])
                .fill(Color::srgb_u8(20, 20, 20))
                .radius(2.0)
                .fill_size(),
            None => stack([icon(lead.kind.glyph()).icon_size(12.0).text_color(tokens::FOREGROUND)])
                .align(Align::Center)
                .justify(Justify::Center)
                .fill(Color::srgb_u8(24, 24, 27))
                .radius(2.0)
                .fill_size(),
        };
        let framed = stack([inner]).padding(1.5).fill(team.color).radius(3.5).width(Size::Fixed(BADGE)).height(Size::Fixed(BADGE));
        let mut kids = vec![framed];
        let class = lead.kind.class();
        let with_sign = class != MarkClass::Out;
        if with_sign {
            kids.push(paint::class_sign(class, 11.0));
        }
        let extra = members.len() - 1;
        if extra > 0 {
            kids.push(
                text(format!("+{extra}"))
                    .font_size(9.0)
                    .line_height(11.0)
                    .center_text()
                    .text_color(tokens::FOREGROUND)
                    .fill(Color::srgb_u8(39, 39, 42))
                    .stroke(Color::srgb_u8(9, 9, 11))
                    .radius(5.0)
                    .width(Size::Fixed(if extra > 9 { 18.0 } else { 14.0 }))
                    .height(Size::Fixed(11.0)),
            );
        }
        let mut lines: Vec<String> = members
            .iter()
            .take(6)
            .map(|i| {
                let m = &d.markers[*i];
                let mut s = format!("{}  {} — {}", clock(m.t), m.kind.label(), d.teams[m.team].name);
                if let Some(u) = &m.unit {
                    s.push_str(&format!(" ({u})"));
                }
                if let Some(note) = &m.note {
                    s.push_str(&format!("\n      {note}"));
                }
                s
            })
            .collect();
        if members.len() > 6 {
            lines.push(format!("+{} more here", members.len() - 6));
        }
        if lead.sample {
            lines.insert(0, "SAMPLE — made up to try the design, not from this replay".into());
        }
        let layout_extra = extra > 0;
        let key = format!("{KEY_MARKER}:{}", members[0]);
        // Under the pointer it steps forward: a click takes the cursor there.
        let hot = f.is_hovered(&key);
        El::new(Kind::Custom("tl_badge"))
            .width(Size::Fixed(BADGE + 4.0))
            .height(Size::Fixed(BADGE + 2.0))
            .children(kids)
            .layout(move |cx| {
                let c = cx.container;
                let mut out = vec![Rect::new(c.x + 2.0, c.y + 1.0, BADGE, BADGE)];
                if with_sign {
                    out.push(Rect::new(c.x + 2.0 + BADGE - 8.0, c.y + 1.0 + BADGE - 8.0, 11.0, 11.0));
                }
                if layout_extra {
                    out.push(Rect::new(c.x + BADGE - 8.0, c.y - 3.0, 14.0, 11.0));
                }
                out
            })
            .key(key)
            .tooltip(lines.join("\n"))
            .cursor(Cursor::Pointer)
            .opacity(if hot { 1.0 } else if unplayed { 0.45 } else { 1.0 })
            .scale(if hot { 1.18 } else { 1.0 })
            .animate(Timing::SPRING_QUICK)
            .aria_label(format!("{} at {}", lead.kind.label(), clock(lead.t)))
    }

    /// Where the samples go along the axis.
    fn x_of(&self, f: &Frame, i: usize) -> f32 {
        i as f32 * self.data.period_s / f.end_s.max(1.0) * f.w
    }

    /// The lead strip: who is ahead, by each side's trailing share of metal
    /// income, with fighting shown as light behind it.
    fn lead_strip(&self, f: &Frame) -> El {
        let d = &self.data;
        let h = LEAD_H;
        let shown = self.drawable_samples();
        let mid = h / 2.0;
        let mut items = vec![(Place::Span { from: 0.0, to: 1.0, top: 0.0, bottom: 0.0 }, quad(Color::srgb_u8(20, 20, 23)))];
        // Heat, one quad per sample step so the glow follows the data.
        let ember = Color::srgb_u8(255, 214, 170).with_alpha(0.26);
        for i in 1..shown {
            let (a, b) = (d.heat[i - 1], d.heat[i]);
            if a.max(b) > 0.02 {
                let from = frac((i - 1) as f32 * d.period_s, f.end_s);
                let to = frac(i as f32 * d.period_s, f.end_s);
                items.push((Place::Span { from, to, top: 0.0, bottom: 0.0 }, paint::heat(ember, a, b, 4.0)));
            }
        }
        let mut paths = Vec::new();
        let x = |i: usize| self.x_of(f, i);
        if shown >= 2 && d.sides.len() == 2 {
            // A lead of 25 points of share (62.5% against 37.5%) reaches the edge.
            // The scale is fixed so that it cannot hint at how lopsided it gets.
            let lead = |i: usize| ((d.shares[0][i] - 0.5) / 0.125).clamp(-1.0, 1.0) * (mid - 4.0);
            for (side, sign) in [(0usize, 1.0f32), (1, -1.0)] {
                let pts: Vec<(f32, f32)> = (0..shown).map(|i| (x(i), mid - (lead(i) * sign).max(0.0) * sign)).collect();
                let color = d.sides[side].color;
                let played = self.played_samples().min(pts.len());
                let (now, later) = (&pts[..played], &pts[played.saturating_sub(1)..]);
                if later.len() >= 2 {
                    if let Some(p) = paint::area(later, mid, VectorColor::Solid(color.with_alpha(0.18))) {
                        paths.push(p);
                    }
                }
                if let Some(p) = paint::area(now, mid, VectorColor::Solid(color.with_alpha(0.5))) {
                    paths.push(p);
                }
                if let Some(p) = paint::polyline(now, color, 1.25) {
                    paths.push(p);
                }
            }
            paths.push(PathBuilder::new().move_to(0.0, mid).line_to(f.w, mid).stroke_solid(Color::srgb_u8(63, 63, 70), 1.0).build());
            // Where the lead changed hands: a diamond in the colour of the new leader.
            for (i, side) in &d.lead_changes {
                if *i < shown {
                    let (cx, r) = (x(*i), 4.0);
                    let diamond = |r: f32, c: Color| {
                        PathBuilder::new().move_to(cx, mid - r).line_to(cx + r, mid).line_to(cx, mid + r).line_to(cx - r, mid).close().fill_solid(c).build()
                    };
                    paths.push(diamond(r + 1.5, Color::srgb_u8(20, 20, 23)));
                    paths.push(diamond(r, d.sides[*side].color.lighten(0.25)));
                }
            }
        } else if shown >= 2 {
            let mut below = vec![0.0f32; d.n];
            for (s, side) in d.sides.iter().enumerate() {
                let top = below.clone();
                for i in 0..d.n {
                    below[i] += d.shares[s][i];
                }
                let mut p = PathBuilder::new().move_to(0.0, top[0] * h);
                for i in 1..shown {
                    p = p.line_to(x(i), top[i] * h);
                }
                for i in (0..shown).rev() {
                    p = p.line_to(x(i), below[i] * h);
                }
                paths.push(p.close().fill_solid(side.color.with_alpha(0.75)).build());
            }
        }
        items.push((Place::Span { from: 0.0, to: 1.0, top: 0.0, bottom: 0.0 }, vector(VectorAsset::from_paths([0.0, 0.0, f.w, h], paths))));
        let area = placed(Some(h), 0.0, items)
            .key(KEY_LEAD)
            .focusable()
            .focus_ring_inside()
            .cursor(Cursor::Pointer)
            .aria_label("Replay timeline: arrow keys move the cursor, Escape follows the game")
            .radius(tokens::RADIUS_SM)
            .clip();
        self.track(f, h, self.lead_gutter(), area)
    }

    fn lead_gutter(&self) -> El {
        let d = &self.data;
        let at = self.readout_s();
        let mut lines = vec![row([text("Who is ahead").label(), text("metal income, 75 s").caption().muted().ellipsis().width(Size::Fill(1.0))]).gap(tokens::SPACE_2).align(Align::Center)];
        if self.game_s < d.period_s {
            lines.push(text("Waiting for the first numbers").caption().muted());
        } else if let Some(why) = self.hidden_reason(at) {
            lines.push(text(why).caption().muted());
        } else {
            let i = d.sample_index(at);
            let mut order: Vec<usize> = (0..d.sides.len()).collect();
            if d.sides.len() > 2 {
                order.sort_by(|a, b| d.shares[*b][i].total_cmp(&d.shares[*a][i]));
            }
            for s in order.into_iter().take(2) {
                lines.push(
                    row([
                        status_dot(d.sides[s].color),
                        text(d.sides[s].name.clone()).caption().ellipsis().width(Size::Fill(1.0)),
                        text(format!("{:.0}%", d.shares[s][i] * 100.0)).caption().tabular_numerals(),
                    ])
                    .gap(tokens::SPACE_2)
                    .align(Align::Center),
                );
            }
        }
        column(lines).gap(2.0).justify(Justify::Center)
    }

    /// Series the large chart draws: per side, or per player.
    fn hero_series(&self) -> Vec<(Option<usize>, String, Color, Vec<f32>)> {
        let d = &self.data;
        if self.per_player {
            d.teams.iter().enumerate().map(|(t, team)| (Some(t), team.name.clone(), team.color, d.team_values(self.hero, t).to_vec())).collect()
        } else {
            d.sides.iter().enumerate().map(|(s, side)| (None, side.name.clone(), side.color, d.side_values(self.hero, s))).collect()
        }
    }

    fn hero_height(&self) -> f32 {
        // Room for the ranked list: seven players and a line of advice.
        if self.per_player { HERO_H + 60.0 } else { HERO_H }
    }

    /// The scale a chart uses, glided toward the top of what it may draw, and
    /// the round value it is gliding to.
    fn eased_top(&self, f: &Frame, metric: usize, hero: bool, peak: f32) -> (f32, f32) {
        let target = nice_ceil(peak * 1.04);
        let mut motion = self.motion.borrow_mut();
        let e = motion.tops.entry((metric, hero)).or_insert(Eased::new(target));
        let moving = e.follow(target, f.now);
        let v = e.value;
        motion.moving |= moving;
        (v.max(1e-3), target)
    }

    fn hero_chart(&self, f: &Frame) -> El {
        let d = &self.data;
        let h = self.hero_height();
        let def = d.metrics[self.hero].def;
        let series = self.hero_series();
        let shown = self.drawable_samples();
        let played = self.played_samples();
        let peak = series.iter().flat_map(|s| s.3[..shown].iter().copied()).fold(0.0f32, f32::max);
        let (top, settled_top) = self.eased_top(f, self.hero, true, peak);
        let (pad_top, pad_bottom) = (14.0, 4.0);
        let y = |v: f32| pad_top + (1.0 - v / top).clamp(-0.05, 1.0) * (h - pad_top - pad_bottom);
        let x = |i: usize| self.x_of(f, i);
        let mut asset = VectorAsset::from_paths([0.0, 0.0, f.w, h], Vec::new());
        let mut items = vec![];
        // Grid lines stand at round values of the scale being glided to, and
        // slide into place with it: their labels never show a number in between.
        for level in [0.0, 0.5, 1.0] {
            let value = settled_top * level;
            if value > top * 1.02 {
                continue;
            }
            let gy = y(value).round() + 0.5;
            asset.paths.push(PathBuilder::new().move_to(0.0, gy).line_to(f.w, gy).stroke_solid(Color::srgb_u8(39, 39, 42), 1.0).build());
            if level > 0.0 && shown >= 2 {
                items.push((Place::Tag { at: 0.0, w: None, h: 16.0, top: gy - 16.0, anchor: Anchor::After }, text(number(value)).caption().muted().tabular_numerals().padding(Sides { left: 2.0, ..Sides::zero() })));
            }
        }
        let many = series.len() > 4;
        let picked = self.picked();
        // Faded series first, the picked one last so it lies on top.
        let mut order: Vec<usize> = (0..series.len()).collect();
        order.sort_by_key(|k| picked.is_some_and(|p| series[*k].0 == Some(p)));
        for k in order {
            let (team, _, color, values) = &series[k];
            if shown < 2 {
                break;
            }
            let pts: Vec<(f32, f32)> = (0..shown).map(|i| (x(i), y(values[i]))).collect();
            let (now, later) = pts.split_at(played.min(pts.len()));
            let later: Vec<(f32, f32)> = now.last().into_iter().chain(later).copied().collect();
            let pts = now;
            let lit = picked.is_none_or(|p| *team == Some(p));
            if later.len() >= 2 {
                if let Some(p) = paint::polyline(&later, color.with_alpha(if lit { 0.42 } else { 0.15 }), 1.25) {
                    asset.paths.push(p);
                }
            }
            if !many && picked.is_none() {
                asset.gradients.push(paint::vertical_gradient(y(top), y(0.0), color.with_alpha(0.24), color.with_alpha(0.0)));
                if let Some(a) = paint::area(pts, y(0.0), VectorColor::Gradient(asset.gradients.len() as u32 - 1)) {
                    asset.paths.push(a);
                }
            }
            let (c, w) = match (lit, picked.is_some(), many) {
                (true, true, _) => (*color, 2.75),
                (false, _, _) => (color.with_alpha(0.28), 1.25),
                (true, false, true) => (color.with_alpha(0.85), 1.25),
                (true, false, false) => (*color, 2.0),
            };
            if let Some(p) = paint::polyline(pts, c, w) {
                asset.paths.push(p);
            }
        }
        items.insert(0, (Place::Span { from: 0.0, to: 1.0, top: 0.0, bottom: 0.0 }, vector(asset)));
        self.cursor_readout(f, &series, top, &y, &mut items, h);
        if let Some(i) = self.hover_sample(f, Track::Hero) {
            let at = frac(i as f32 * d.period_s, f.end_s);
            items.extend(series.iter().map(|s| hover_ring(at, y(s.3[i]), s.2, 4.5)));
        }
        let area = placed(Some(h), 0.0, items).key(Self::area_key("hero")).cursor(Cursor::Pointer).clip();
        // The large chart and its controls stand on one surface, the same
        // one the chosen row below is lit with.
        let inset = Sides { left: tokens::SPACE_2, top: tokens::SPACE_2, bottom: tokens::SPACE_2, ..Sides::zero() };
        self.track_inset(f, h, inset, self.hero_gutter(def, &series), area).fill(chosen_surface()).radius(tokens::RADIUS_MD)
    }

    /// The sample under the pointer on `track`, when the pointer is on that
    /// track and the numbers there may be shown.
    fn hover_sample(&self, f: &Frame, track: Track) -> Option<usize> {
        let hv = f.hover.filter(|h| h.track == track)?;
        self.hidden_reason(hv.t).is_none().then(|| self.data.sample_index(hv.t))
    }

    /// The values at the cursor, written on the cursor line itself.
    fn cursor_readout(
        &self,
        f: &Frame,
        series: &[(Option<usize>, String, Color, Vec<f32>)],
        _top: f32,
        y: &dyn Fn(f32) -> f32,
        items: &mut Vec<(Place, El)>,
        h: f32,
    ) {
        let d = &self.data;
        let at = self.readout_s();
        if !self.revealed(at) || self.game_s < d.period_s {
            return;
        }
        let i = d.sample_index(at);
        let def = d.metrics[self.hero].def;
        let picked = self.picked();
        let mut marks: Vec<(f32, Color, String)> = series
            .iter()
            .filter(|s| match picked {
                Some(p) => s.0 == Some(p),
                None => series.len() <= 4,
            })
            .map(|(_, _, color, v)| (y(v[i]), *color, def.show(v[i])))
            .collect();
        // Keep labels from overlapping: sort by height and push apart.
        marks.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut label_y: Vec<f32> = marks.iter().map(|m| m.0 - 9.0).collect();
        for k in 1..label_y.len() {
            label_y[k] = label_y[k].max(label_y[k - 1] + 18.0);
        }
        let overflow = label_y.last().map(|l| l + 18.0 - h).unwrap_or(0.0).max(0.0);
        let at_f = frac(i as f32 * d.period_s, f.end_s);
        // Chips go on the side away from the fog (or from the window edge).
        let left = frac(at, f.end_s) > 0.78 || (self.hide_future && at >= self.game_s - 1.0);
        for (k, (py, color, value)) in marks.into_iter().enumerate() {
            items.push((
                Place::Tag { at: at_f, w: Some(9.0), h: 9.0, top: py - 4.5, anchor: Anchor::Center },
                El::new(Kind::Custom("tl_dot")).fill(color).stroke(Color::srgb_u8(9, 9, 11)).stroke_width(2.0).radius(4.5),
            ));
            let chip = row([text(value).caption().semibold().tabular_numerals().text_color(tokens::FOREGROUND)])
                .padding(Sides::xy(6.0, 1.0))
                .fill(Color::srgb_u8(24, 24, 27).with_alpha(0.92))
                .stroke(color.with_alpha(0.7))
                .radius(4.0);
            let pad = if left { Sides { right: 9.0, ..Sides::zero() } } else { Sides { left: 9.0, ..Sides::zero() } };
            items.push((
                Place::Tag {
                    at: frac(at, f.end_s),
                    w: None,
                    h: 18.0,
                    top: (label_y[k] - overflow).max(0.0),
                    anchor: if left { Anchor::Before } else { Anchor::After },
                },
                row([chip]).padding(pad),
            ));
        }
    }

    fn hero_gutter(&self, def: MetricDef, series: &[(Option<usize>, String, Color, Vec<f32>)]) -> El {
        let d = &self.data;
        let at = self.readout_s();
        let visible = self.revealed(at) && self.game_s >= d.period_s;
        let i = d.sample_index(at);
        let when = if self.cursor.is_none() && self.game_s > d.computed_s {
            format!("At {} · the latest worked out so far", clock(at))
        } else {
            format!("At {}", clock(at))
        };
        // The title is the control: it names what the chart shows and opens
        // the list that changes it.
        let title = select::select_trigger(KEY_METRIC, def.about)
            .aria_expanded(self.metric_open)
            .tooltip("What the large chart shows. Click to choose another.");
        let mut lines = vec![title, text(when).caption().muted().tabular_numerals().wrap_text()];
        if !visible {
            lines.push(text(self.hidden_reason(at).unwrap_or("Waiting for the first numbers")).muted().wrap_text());
            return column(lines).gap(tokens::SPACE_1);
        }
        if self.per_player {
            // A ranked list at the cursor; a click picks a player out.
            let mut ranked: Vec<&(Option<usize>, String, Color, Vec<f32>)> = series.iter().collect();
            ranked.sort_by(|a, b| b.3[i].total_cmp(&a.3[i]));
            let room = 7;
            let picked_row = self.picked.and_then(|p| ranked.iter().position(|s| s.0 == Some(p)));
            for (rank, s) in ranked.iter().enumerate() {
                let keep = rank < room - 1 || Some(rank) == picked_row || (rank == room - 1 && picked_row.is_none_or(|p| p < room));
                if !keep {
                    continue;
                }
                let team = s.0.unwrap_or(0);
                let selected = self.picked == Some(team);
                let (name, value) = (text(s.1.clone()).caption().ellipsis().width(Size::Fill(1.0)), text(def.show(s.3[i])).caption().tabular_numerals());
                let mut r = row([
                    text(format!("{}", rank + 1)).caption().muted().tabular_numerals().width(Size::Fixed(16.0)),
                    status_dot(s.2),
                    if selected { name.semibold() } else { name },
                    if selected { value.semibold() } else { value },
                ])
                .gap(tokens::SPACE_2)
                .align(Align::Center)
                .height(Size::Fixed(20.0))
                .padding(Sides::xy(4.0, 0.0))
                .radius(3.0)
                .key(format!("{KEY_PLAYER}:{team}"))
                .focusable()
                .cursor(Cursor::Pointer)
                .tooltip(if selected {
                    format!("{} is picked out: click to let go", s.1)
                } else {
                    format!("Click to pick {} out: a thicker line here, and the rows below follow", s.1)
                });
                if selected {
                    r = r.selected();
                }
                lines.push(r);
            }
            lines.push(
                text(match self.picked() {
                    Some(p) => format!("The rows below follow {}", d.teams[p].name),
                    None => "Click a player to follow them in the rows below".into(),
                })
                .caption()
                .muted()
                .ellipsis(),
            );
        } else {
            for (_, name, color, values) in series {
                lines.push(
                    row([
                        status_dot(*color),
                        text(name.clone()).ellipsis().width(Size::Fill(1.0)),
                        text(def.show(values[i])).title().tabular_numerals(),
                    ])
                    .gap(tokens::SPACE_2)
                    .align(Align::Center),
                );
            }
            if series.len() >= 2 {
                let vals: Vec<(Color, f32)> = series.iter().map(|s| (s.2, s.3[i])).collect();
                lines.push(share_bar(&vals, 8.0).padding(Sides { top: tokens::SPACE_1, ..Sides::zero() }));
            }
        }
        column(lines).gap(tokens::SPACE_1)
    }

    /// A track with nothing on the axis: air, or a line of text in the gutter.
    /// A click on it still places the cursor, as anywhere on the axis.
    fn gap(&self, f: &Frame, h: f32, id: &str, gutter: El) -> El {
        let area = placed(Some(h), 0.0, Vec::new()).key(Self::area_key(id)).cursor(Cursor::Pointer);
        self.track(f, h, gutter, area)
    }

    /// A compact row: a mark that says whether the large chart shows this
    /// metric (a click on the row makes it so), its name and the values at the
    /// cursor in the gutter, a sparkline on the axis. One line per side, or
    /// the picked player's alone.
    fn metric_row(&self, f: &Frame, m: usize) -> El {
        let d = &self.data;
        let def = d.metrics[m].def;
        let h = ROW_H;
        let shown = self.drawable_samples();
        let picked = self.picked();
        let lines: Vec<(Color, Vec<f32>)> = match picked {
            Some(p) => vec![(d.teams[p].color, d.team_values(m, p).to_vec())],
            None => (0..d.sides.len()).map(|s| (d.sides[s].color, d.side_values(m, s))).collect(),
        };
        let peak = lines.iter().flat_map(|l| l.1[..shown].iter().copied()).fold(0.0f32, f32::max);
        let (top, _) = self.eased_top(f, m, false, peak);
        let (pt, pb) = (5.0, 4.0);
        let y = |v: f32| pt + (1.0 - v / top).clamp(-0.05, 1.0) * (h - pt - pb);
        let mut paths = vec![PathBuilder::new().move_to(0.0, h - 0.5).line_to(f.w, h - 0.5).stroke_solid(Color::srgb_u8(32, 32, 36), 1.0).build()];
        let mut items = Vec::new();
        if shown >= 2 {
            for (color, v) in &lines {
                let pts: Vec<(f32, f32)> = (0..shown).map(|i| (self.x_of(f, i), y(v[i]))).collect();
                let played = self.played_samples().min(pts.len());
                if let Some(p) = paint::polyline(&pts[played.saturating_sub(1)..], color.with_alpha(0.4), 1.0) {
                    paths.push(p);
                }
                if let Some(p) = paint::polyline(&pts[..played], *color, 1.5) {
                    paths.push(p);
                }
            }
        }
        items.push((Place::Span { from: 0.0, to: 1.0, top: 0.0, bottom: 0.0 }, vector(VectorAsset::from_paths([0.0, 0.0, f.w, h], paths))));
        let at = self.readout_s();
        let visible = self.revealed(at) && self.game_s >= d.period_s;
        let i = d.sample_index(at);
        if visible {
            let at_f = frac(i as f32 * d.period_s, f.end_s);
            for (color, v) in &lines {
                items.push((
                    Place::Tag { at: at_f, w: Some(7.0), h: 7.0, top: y(v[i]) - 3.5, anchor: Anchor::Center },
                    El::new(Kind::Custom("tl_dot")).fill(*color).stroke(Color::srgb_u8(9, 9, 11)).stroke_width(1.5).radius(3.5),
                ));
            }
        }
        if let Some(i) = self.hover_sample(f, Track::Row(m)) {
            let at = frac(i as f32 * d.period_s, f.end_s);
            items.extend(lines.iter().map(|(color, v)| hover_ring(at, y(v[i]), *color, 3.5)));
        }
        let area = placed(Some(h), 0.0, items).key(Self::area_key(&format!("row{m}"))).cursor(Cursor::Pointer).clip();

        let selected = m == self.hero;
        let key = format!("{KEY_ROW}:{m}");
        let hot = f.is_hovered(&key);
        let mut cells = vec![
            choice_mark(selected, hot),
            if selected || hot { text(def.label).label().ellipsis() } else { text(def.label).muted().ellipsis() }.width(Size::Fill(1.0)),
        ];
        match picked {
            // One player: the value and where it stands among all players.
            Some(_) => {
                let v = lines[0].1[i];
                let rank = 1 + (0..d.teams.len()).filter(|t| d.team_values(m, *t)[i] > v).count();
                let value = text(if visible { def.show(v) } else { "–".into() }).tabular_numerals().end_text().width(Size::Fixed(if f.narrow { 40.0 } else { 46.0 }));
                cells.push(if visible { value } else { value.muted() });
                let standing = match (visible, f.narrow) {
                    (false, _) => String::new(),
                    (true, true) => format!("#{rank}"),
                    (true, false) => format!("#{rank} of {}", d.teams.len()),
                };
                // As wide as the bar and second number it stands in for.
                cells.push(text(standing).caption().muted().tabular_numerals().width(Size::Fixed(if f.narrow { 40.0 } else { 124.0 })));
            }
            None => {
                let vals: Vec<(Color, f32)> = lines.iter().map(|(color, v)| (*color, v[i])).collect();
                cells.extend(tug(def, &vals, visible, !f.narrow));
            }
        }
        let whose = picked.map(|p| format!(" for {}", d.teams[p].name)).unwrap_or_default();
        let gutter = row(cells)
            .gap(tokens::SPACE_2)
            .align(Align::Center)
            .key(key)
            .focusable()
            .focus_ring_inside()
            .cursor(Cursor::Pointer)
            .role(Role::Radio)
            .aria_checked(selected)
            .tooltip(if selected {
                format!("{}{whose}: this is what the large chart shows", def.about)
            } else {
                format!("{}{whose}: click to show it in the large chart", def.about)
            })
            .radius(tokens::RADIUS_SM);
        // The chosen row is lit across its whole width; one under the pointer
        // lights where a click would choose it.
        let gutter = if hot && !selected { gutter.fill(chosen_surface()) } else { gutter };
        let inset = Sides { left: tokens::SPACE_2, ..Sides::zero() };
        let track = self.track_inset(f, h, inset, gutter, area).radius(tokens::RADIUS_SM);
        if selected {
            track.fill(chosen_surface())
        } else {
            track
        }
    }

    // ---- overlay across all tracks ----------------------------------------

    fn overlay(&self, f: &Frame, height: f32) -> El {
        let d = &self.data;
        let g = frac(self.game_s, f.end_s);
        let mut items: Vec<(Place, El)> = Vec::new();
        let fog_top = RULER_H;
        if g < 1.0 {
            if self.hide_future {
                items.push((Place::Span { from: g, to: 1.0, top: fog_top, bottom: 0.0 }, paint::fog(Color::srgb_u8(30, 30, 34).with_alpha(0.94), 22.0)));
                let fog_w = (1.0 - g) * f.w;
                // Not where the cursor's line would run through the words.
                let middle = (g + 1.0) / 2.0 * f.w;
                let struck = self.cursor.is_some_and(|c| (frac(c, f.end_s) * f.w - middle).abs() < 52.0);
                if fog_w > 140.0 && self.jump.is_none() && !struck {
                    items.push((
                        Place::Tag { at: (g + 1.0) / 2.0, w: None, h: 16.0, top: fog_top + self.lanes_height() + LEAD_H / 2.0 - 8.0, anchor: Anchor::Center },
                        text("Not played yet").caption().muted(),
                    ));
                }
            } else {
                items.push((Place::Span { from: g, to: 1.0, top: fog_top, bottom: 0.0 }, quad(tokens::BACKGROUND.with_alpha(0.3))));
            }
        }
        // Statistics still being worked out between what is done and the game.
        if d.computed_s < self.game_s {
            let from = frac(d.computed_s, f.end_s);
            items.push((Place::Span { from, to: g, top: fog_top, bottom: 0.0 }, skeleton().radius(0.0)));
            if (g - from) * f.w > 120.0 {
                items.push((
                    Place::Tag { at: (from + g) / 2.0, w: None, h: 16.0, top: fog_top + self.lanes_height() + LEAD_H / 2.0 - 8.0, anchor: Anchor::Center },
                    text("Working out the numbers…").caption().muted(),
                ));
            }
        }
        if let Some(target) = self.jump {
            let to = frac(target, f.end_s);
            items.push((Place::Span { from: g, to, top: fog_top, bottom: 0.0 }, paint::sweep(cursor_color().with_alpha(0.035))));
            items.push((Place::Line { at: to, w: 9.0, top: RULER_H - 4.0, bottom: 0.0 }, paint::glow_line(cursor_color(), 1.5, 3.0, 0.25, 4.0)));
        }
        // The hover line: thin and plain, under the two position lines. On
        // the game line or on the cursor it has nothing of its own to add.
        if let Some(hv) = f.hover.filter(|h| !h.on_game && Some(h.t) != self.cursor) {
            items.push((Place::Line { at: frac(hv.t, f.end_s), w: 1.0, top: RULER_H - 4.0, bottom: 0.0 }, quad(tokens::FOREGROUND.with_alpha(0.42))));
        }
        items.push((Place::Line { at: g, w: 16.0, top: RULER_H - 4.0, bottom: 0.0 }, paint::glow_line(game_color(), 2.0, 4.0, 0.12, 0.0)));
        if let Some(c) = self.cursor.filter(|c| Some(*c) != self.jump) {
            items.push((Place::Line { at: frac(c, f.end_s), w: 12.0, top: RULER_H - 4.0, bottom: 0.0 }, paint::glow_line(cursor_color(), 2.0, 3.0, 0.12, 0.0)));
        }
        self.position_chips(f, &mut items);
        if let Some((hv, (el, h))) = f.hover.and_then(|hv| Some((hv, self.hover_readout(hv)?))) {
            let x = frac(hv.t, f.end_s) * f.w;
            // Beside the line, on the side with room, level with the pointer.
            let left = x + READOUT_GAP + READOUT_W > f.w;
            let pad = if left { Sides { right: READOUT_GAP, ..Sides::zero() } } else { Sides { left: READOUT_GAP, ..Sides::zero() } };
            // Level with the pointer on the large chart; under a low track
            // (over it at the bottom), so the track's own line stays in view.
            let top = match hv.track {
                Track::Hero => hv.y - h / 2.0,
                track => {
                    let (top, tall) = self.track_span(track);
                    let under = top + tall + 4.0;
                    if under + h <= height - 2.0 { under } else { top - h - 4.0 }
                }
            };
            let top = top.min(height - h - 2.0).max(RULER_H + 2.0);
            items.push((
                Place::Tag { at: frac(hv.t, f.end_s), w: Some(READOUT_W + READOUT_GAP), h, top, anchor: if left { Anchor::Before } else { Anchor::After } },
                row([el]).padding(pad),
            ));
        }
        placed(Some(height), f.gutter, items)
    }

    /// The readout beside the hover line: the moment, what the track under
    /// the pointer shows, and its values there. With its height, px. Tracks
    /// without numbers (the ruler, the events) have none.
    fn hover_readout(&self, hv: Hover) -> Option<(El, f32)> {
        let d = &self.data;
        let i = d.sample_index(hv.t);
        let picked = self.picked();
        let mut note = None;
        let line = |color: Color, name: &str, value: String, v: f32, picked: bool| ReadoutLine { color, name: name.to_owned(), value, v, picked };
        let (title, mut lines): (&str, Vec<ReadoutLine>) = match hv.track {
            Track::Ruler | Track::Events | Track::Gap => return None,
            Track::Lead => (
                "Share of metal income",
                d.sides.iter().enumerate().map(|(s, side)| line(side.color, &side.name, format!("{:.0}%", d.shares[s][i] * 100.0), d.shares[s][i], false)).collect(),
            ),
            Track::Hero => {
                let def = d.metrics[self.hero].def;
                let series = self.hero_series();
                (def.about, series.iter().map(|(team, name, color, v)| line(*color, name, def.show(v[i]), v[i], team.is_some() && *team == picked)).collect())
            }
            Track::Row(m) => {
                let def = d.metrics.get(m)?.def;
                let lines = match picked {
                    Some(p) => {
                        let v = d.team_values(m, p)[i];
                        let rank = 1 + (0..d.teams.len()).filter(|t| d.team_values(m, *t)[i] > v).count();
                        note = Some(format!("#{rank} of {} players", d.teams.len()));
                        vec![line(d.teams[p].color, &d.teams[p].name, def.show(v), v, false)]
                    }
                    None => d
                        .sides
                        .iter()
                        .enumerate()
                        .map(|(s, side)| {
                            let v = d.side_values(m, s)[i];
                            line(side.color, &side.name, def.show(v), v, false)
                        })
                        .collect(),
                };
                (def.about, lines)
            }
        };
        if let Some(more) = shortlist(&mut lines, READOUT_ROOM, |l| l.v, |l| l.picked) {
            note = Some(format!("and {more} more"));
        }
        let small = |s: String| text(s).caption().muted().ellipsis().height(Size::Fixed(18.0));
        let mut body: Vec<El> = vec![row([
            text(clock(hv.t)).caption().semibold().tabular_numerals(),
            text(title).caption().muted().ellipsis().width(Size::Fill(1.0)),
        ])
        .gap(6.0)
        .align(Align::Center)
        .height(Size::Fixed(18.0))];
        match self.hidden_reason(hv.t) {
            Some(why) => body.push(small(why.into())),
            None => {
                for l in lines {
                    let name = text(l.name).caption().ellipsis().width(Size::Fill(1.0));
                    body.push(
                        row([status_dot(l.color), if l.picked { name.semibold() } else { name }, text(l.value).caption().semibold().tabular_numerals()])
                            .gap(tokens::SPACE_2)
                            .align(Align::Center)
                            .height(Size::Fixed(18.0)),
                    );
                }
                body.extend(note.map(small));
            }
        }
        let h = 12.0 + body.len() as f32 * 18.0 + (body.len() - 1) as f32 * 2.0;
        let el = column(body)
            .gap(2.0)
            .padding(Sides::xy(8.0, 6.0))
            .fill(tokens::POPOVER)
            .stroke(Color::srgb_u8(63, 63, 70))
            .radius(6.0)
            .shadow(tokens::SHADOW_MD)
            .width(Size::Fixed(READOUT_W))
            .height(Size::Fixed(h));
        Some((el, h))
    }

    /// Time chips on the ruler, riding the position lines: the game's in white,
    /// the cursor's in amber, and the jump button at the cursor itself.
    /// Which chips ride the ruler, where, and how wide they are about to be.
    fn chips(&self, f: &Frame) -> Vec<Chip> {
        let g = frac(self.game_s, f.end_s);
        let ahead = self.cursor.is_some_and(|c| c > self.game_s);
        let game_anchor = match (self.cursor.is_some() || self.jump.is_some(), ahead || self.jump.is_some()) {
            (false, _) => Anchor::Center,
            (true, true) => Anchor::Before,
            (true, false) => Anchor::After,
        };
        // About how wide a time chip draws: "25:00", or "1:02:05" past the hour.
        let chip_w = |t: f32| if t >= 3600.0 { 66.0 } else { 50.0 };
        let mut out = vec![Chip { at: g, anchor: game_anchor, width: chip_w(self.game_s), what: ChipKind::Game }];
        if let Some(target) = self.jump {
            out.push(Chip { at: frac(target, f.end_s), anchor: Anchor::After, width: 128.0, what: ChipKind::Jumping(target) });
        }
        // While a jump is under way the cursor may still look elsewhere.
        if let Some(c) = self.cursor.filter(|c| Some(*c) != self.jump) {
            let at = frac(c, f.end_s);
            if self.can_jump().is_some() {
                out.push(Chip { at, anchor: Anchor::After, width: 176.0, what: ChipKind::Jump(c) });
            } else {
                out.push(Chip { at, anchor: if ahead { Anchor::After } else { Anchor::Before }, width: chip_w(c), what: ChipKind::Cursor(c) });
            }
        }
        // The moment under the pointer, wherever it does not sit on another chip.
        if let Some(hv) = f.hover.filter(|h| !h.on_game && Some(h.t) != self.cursor) {
            let taken: Vec<(f32, f32)> = out.iter().map(|c| c.span(f.w)).collect();
            let chip = [Anchor::Center, Anchor::After, Anchor::Before]
                .into_iter()
                .map(|anchor| Chip { at: frac(hv.t, f.end_s), anchor, width: chip_w(hv.t), what: ChipKind::Hover(hv.t) })
                .find(|c| {
                    let (a, b) = c.span(f.w);
                    a >= 0.0 && b <= f.w && taken.iter().all(|(from, to)| b + 4.0 <= *from || a - 4.0 >= *to)
                });
            out.extend(chip);
        }
        out
    }

    /// Time chips on the ruler, riding the position lines: the game's in white,
    /// the cursor's in amber, and the jump button at the cursor itself.
    fn position_chips(&self, f: &Frame, items: &mut Vec<(Place, El)>) {
        let dark = Color::srgb_u8(9, 9, 11);
        let chip = |label: String, fill: Color| {
            text(label).caption().semibold().tabular_numerals().text_color(dark).padding(Sides::xy(7.0, 2.0)).fill(fill).radius(4.0)
        };
        for c in self.chips(f) {
            let place = |h: f32, top: f32| Place::Tag { at: c.at, w: None, h, top, anchor: c.anchor };
            let el = match c.what {
                ChipKind::Game => (place(20.0, 4.0), chip(clock(self.game_s), game_color())),
                ChipKind::Cursor(t) => (place(20.0, 4.0), chip(clock(t), cursor_color())),
                ChipKind::Hover(t) => (
                    place(20.0, 4.0),
                    text(clock(t))
                        .caption()
                        .semibold()
                        .tabular_numerals()
                        .text_color(tokens::FOREGROUND)
                        .padding(Sides::xy(7.0, 2.0))
                        .fill(Color::srgb_u8(39, 39, 42))
                        .stroke(Color::srgb_u8(82, 82, 91))
                        .radius(4.0),
                ),
                ChipKind::Jumping(t) => (place(20.0, 4.0), chip(format!("Jumping to {}…", clock(t)), cursor_color())),
                ChipKind::Jump(t) => (
                    place(22.0, 3.0),
                    row([
                        icon("fast-forward").icon_size(12.0).text_color(dark),
                        text(format!("Jump everyone to {}", clock(t))).caption().semibold().text_color(dark),
                    ])
                    .gap(tokens::SPACE_1)
                    .align(Align::Center)
                    .padding(Sides::xy(8.0, 0.0))
                    .height(Size::Fixed(22.0))
                    // Under the pointer it brightens and gains a rim: this one acts on everyone.
                    .fill(if f.is_hovered("tl:jump") { Color::srgb_u8(255, 226, 150) } else { cursor_color() })
                    .stroke(if f.is_hovered("tl:jump") { tokens::FOREGROUND } else { cursor_color() })
                    .radius(4.0)
                    .key("tl:jump")
                    .focusable()
                    .role(Role::Button)
                    .cursor(Cursor::Pointer)
                    .tooltip("Moves the game forward for every viewer (Enter)"),
                ),
            };
            items.push(el);
        }
    }

    // ---- the numbers as a table ----------------------------------------------

    fn table(&self) -> El {
        let d = &self.data;
        let at = self.readout_s();
        let visible = self.revealed(at) && self.game_s >= d.period_s;
        let i = d.sample_index(at);
        let cell_w = Size::Fixed(76.0);
        let mut header: Vec<El> = vec![text("").width(Size::Fill(1.0))];
        header.extend(d.metrics.iter().map(|m| text(m.def.label).caption().muted().end_text().width(cell_w)));
        let mut rows = vec![row(header).align(Align::Center)];
        if !visible {
            rows.push(text(self.hidden_reason(at).unwrap_or("No numbers here")).muted());
        } else {
            let line = |name: String, color: Color, indent: f32, value: &dyn Fn(usize) -> f32, strong: bool| {
                let label = text(name).ellipsis();
                let mut cells: Vec<El> = vec![row([status_dot(color), if strong { label.label() } else { label }])
                    .gap(tokens::SPACE_2)
                    .align(Align::Center)
                    .padding(Sides { left: indent, ..Sides::zero() })
                    .width(Size::Fill(1.0))];
                cells.extend(d.metrics.iter().enumerate().map(|(m, s)| text(s.def.show(value(m))).tabular_numerals().end_text().width(cell_w)));
                row(cells).align(Align::Center)
            };
            for (s, side) in d.sides.iter().enumerate() {
                rows.push(line(side.name.clone(), side.color, 0.0, &|m| d.side_values(m, s)[i], side.teams.len() > 1));
                if side.teams.len() > 1 {
                    for t in &side.teams {
                        rows.push(line(d.teams[*t].name.clone(), d.teams[*t].color, tokens::SPACE_4, &|m| d.team_values(m, *t)[i], false));
                    }
                }
            }
        }
        collapsible("tl:table", format!("All numbers at {}", clock(at)), self.table_open, [column(rows).gap(tokens::SPACE_1)])
    }
}

/// Cuts a list too long to show down to `room` entries: the highest by
/// `value`, and the one `picked` out whatever its place (last, if it is not
/// among them). Returns how many were left out, or `None` when all fit, in
/// which case the list keeps its order.
fn shortlist<T>(lines: &mut Vec<T>, room: usize, value: impl Fn(&T) -> f32, picked: impl Fn(&T) -> bool) -> Option<usize> {
    if lines.len() <= room {
        return None;
    }
    let total = lines.len();
    lines.sort_by(|a, b| value(b).total_cmp(&value(a)));
    let tail = lines.iter().position(picked).filter(|k| *k >= room).map(|k| lines.remove(k));
    lines.truncate(room - tail.is_some() as usize);
    lines.extend(tail);
    Some(total - lines.len())
}

/// A ring on a line at the moment under the pointer: hollow, where the
/// cursor's dots are solid.
fn hover_ring(at: f32, y: f32, color: Color, r: f32) -> (Place, El) {
    (
        Place::Tag { at, w: Some(2.0 * r), h: 2.0 * r, top: y - r, anchor: Anchor::Center },
        El::new(Kind::Custom("tl_ring")).fill(Color::srgb_u8(9, 9, 11)).stroke(color).stroke_width(1.5).radius(r),
    )
}

/// The mark at the left of a row, drawn as a radio button because that is
/// what it is: one row is shown in the large chart, and a click chooses it.
fn choice_mark(selected: bool, hot: bool) -> El {
    let (ring, dot) = match (selected, hot) {
        (true, _) => (tokens::FOREGROUND, tokens::FOREGROUND),
        (false, true) => (tokens::FOREGROUND.with_alpha(0.85), tokens::FOREGROUND.with_alpha(0.4)),
        (false, false) => (tokens::MUTED_FOREGROUND.with_alpha(0.6), Color::srgb_u8a(0, 0, 0, 0)),
    };
    stack([El::new(Kind::Custom("tl_choice_dot")).fill(dot).width(Size::Fixed(6.0)).height(Size::Fixed(6.0)).radius(3.0)])
        .align(Align::Center)
        .justify(Justify::Center)
        .stroke(ring)
        .stroke_width(1.5)
        .radius(7.0)
        .width(Size::Fixed(14.0))
        .height(Size::Fixed(14.0))
}

/// Shares of a whole as one bar, a 2 px gap between segments.
fn share_bar(vals: &[(Color, f32)], h: f32) -> El {
    let total: f32 = vals.iter().map(|v| v.1.max(0.0)).sum();
    let segments: Vec<El> = if total <= 0.0 {
        vec![quad(Color::srgb_u8(39, 39, 42)).width(Size::Fill(1.0)).height(Size::Fixed(h)).radius(h / 2.0)]
    } else {
        vals.iter()
            .filter(|v| v.1 > 0.0)
            .map(|(c, v)| quad(*c).width(Size::Fill((v / total).max(0.001))).height(Size::Fixed(h)).radius(h / 2.0))
            .collect()
    };
    row(segments).gap(2.0).height(Size::Fixed(h)).width(Size::Fill(1.0))
}

/// Tug of war: left value, share bar, right value (or a bar and the leader's
/// value when there are more than two sides).
fn tug(def: MetricDef, vals: &[(Color, f32)], visible: bool, with_bar: bool) -> Vec<El> {
    // Without the bar the gutter is the narrow one: the numbers close up too.
    let value_w = if with_bar { 46.0 } else { 40.0 };
    let value = |v: f32, end: bool| {
        let t = text(if visible { def.show(v) } else { "–".into() }).tabular_numerals().width(Size::Fixed(value_w));
        let t = if end { t.end_text() } else { t };
        if visible { t } else { t.muted() }
    };
    let bar = |vals: &[(Color, f32)]| {
        if visible { share_bar(vals, 6.0) } else { share_bar(&[], 6.0) }.width(Size::Fixed(if with_bar { 70.0 } else { 0.0 }))
    };
    match vals {
        [a, b] => {
            let mut out = vec![value(a.1, true)];
            if with_bar {
                out.push(bar(vals));
            }
            out.push(value(b.1, false));
            out
        }
        _ => {
            let lead = vals.iter().map(|v| v.1).fold(0.0f32, f32::max);
            let mut out = Vec::new();
            if with_bar {
                out.push(bar(vals));
            }
            out.push(value(lead, true));
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::data::tests::two_sided;
    use super::*;

    fn timeline() -> Timeline {
        let mut t = Timeline::new(two_sided([&[1.0; 160], &[2.0; 160]]));
        t.set_game(600.0);
        t
    }

    #[test]
    fn the_future_is_hidden_by_default() {
        let t = timeline();
        assert!(t.revealed(600.0));
        assert!(!t.revealed(601.0));
        assert_eq!(t.drawable_samples(), 41);
        assert!(t.horizon_s() < t.data.duration_s);
    }

    #[test]
    fn showing_the_future_reveals_everything() {
        let mut t = timeline();
        t.hide_future = false;
        assert!(t.revealed(2000.0));
        assert_eq!(t.drawable_samples(), t.data.n);
        assert_eq!(t.horizon_s(), t.data.duration_s);
    }

    #[test]
    fn a_cursor_near_the_game_follows_it() {
        let mut t = timeline();
        let (end, w) = (1200.0, 1200.0);
        t.point_cursor(603.0, end, w);
        assert_eq!(t.cursor, None, "3 px away snaps onto the game");
        t.point_cursor(700.0, end, w);
        assert_eq!(t.cursor, Some(700.0));
    }

    #[test]
    fn jumps_only_forward_and_one_at_a_time() {
        let mut t = timeline();
        t.cursor = Some(300.0);
        assert_eq!(t.can_jump(), None);
        t.cursor = Some(605.0);
        assert_eq!(t.can_jump(), None, "too short to bother everyone");
        t.cursor = Some(900.0);
        assert_eq!(t.can_jump(), Some(900.0));
        t.jump = Some(900.0);
        assert_eq!(t.can_jump(), None);
        t.set_game(899.8);
        assert_eq!(t.jump, None, "arrived");
        assert_eq!(t.cursor, None, "and follows the game again");
    }

    #[test]
    fn arrow_keys_step_the_cursor_and_return_to_the_game() {
        let mut t = timeline();
        let key = |k: NamedKey| UiEvent::synthetic_key_down(KEY_LEAD, LogicalKey::Named(k));
        t.on_key(&key(NamedKey::ArrowLeft), 1200.0);
        assert_eq!(t.cursor, Some(585.0));
        t.on_key(&key(NamedKey::ArrowRight), 1200.0);
        assert_eq!(t.cursor, None, "back on the game line");
        t.on_key(&key(NamedKey::Home), 1200.0);
        assert_eq!(t.cursor, Some(0.0));
        t.on_key(&key(NamedKey::End), 1200.0);
        assert_eq!(t.cursor, None);
    }

    #[test]
    fn a_game_that_steps_over_the_cursor_takes_it_along() {
        let mut t = timeline();
        t.cursor = Some(610.0);
        t.set_game(640.0);
        assert_eq!(t.cursor, None);
    }

    #[test]
    fn a_jump_that_lands_a_little_short_has_arrived() {
        let mut t = timeline();
        t.cursor = Some(900.0);
        assert_eq!(t.ask_jump(), Some(Action::Jump(900.0)));
        t.set_game(898.5);
        assert_eq!(t.jump, None);
    }

    #[test]
    fn a_jump_past_the_end_asks_for_the_end() {
        let mut t = timeline();
        let end = t.data.duration_s;
        t.cursor = Some(end + 100.0);
        assert_eq!(t.ask_jump(), Some(Action::Jump(end)));
        t.set_game(end);
        assert_eq!((t.jump, t.cursor), (None, None), "game over: nothing left to wait for");
    }

    #[test]
    fn data_from_any_source_is_made_safe_to_index() {
        let teams = (0..2)
            .map(|s| data::Team { name: String::new(), color: tokens::FOREGROUND, faction: Faction::Armada, is_ai: false, side: s })
            .collect();
        let short = data::Series { def: data::METAL_INCOME, values: vec![vec![1.0, 2.0]] };
        let stray = Marker { t: 5.0, kind: MarkKind::Radar, team: 9, unit: None, note: None, sample: false };
        let d = Data::assemble(60.0, 15.0, 4, teams, vec![short], vec![stray]);
        assert_eq!(d.metrics[0].values, vec![vec![1.0, 2.0, 2.0, 2.0], vec![0.0; 4]]);
        assert!(d.markers.is_empty());
    }

    /// Two sides: one of two players, one of `others`. Every metric of player
    /// `k` is `k + 1` throughout, so player `others + 1` leads everything.
    fn teams_of(others: usize) -> Timeline {
        let teams: Vec<data::Team> = (0..others + 2)
            .map(|k| data::Team { name: format!("p{k}"), color: tokens::FOREGROUND, faction: Faction::Armada, is_ai: k >= 2, side: (k >= 2) as usize })
            .collect();
        let metrics = data::REPLAY_METRICS
            .iter()
            .map(|def| data::Series { def: *def, values: (0..teams.len()).map(|k| vec![k as f32 + 1.0; 160]).collect() })
            .collect();
        let mut t = Timeline::new(Data::assemble(2400.0, 15.0, 160, teams, metrics, Vec::new()));
        t.set_game(600.0);
        t
    }

    /// The time axis as the tests lay it out: 1200 px for 1200 s from x = 300,
    /// the tracks starting at y = 100.
    const AXIS: Rect = Rect::new(300.0, 100.0, 1200.0, 30.0);

    fn hover(t: &Timeline, key: &str) -> Option<Hover> {
        t.hover_at(Some(key), Some(AXIS), 1200.0)
    }

    #[test]
    fn the_pointer_over_a_track_is_a_moment_and_a_track() {
        let mut t = timeline();
        assert_eq!(hover(&t, "tl:area:hero"), None, "no pointer yet");
        t.pointer = Some((700.0, 250.0));
        assert_eq!(hover(&t, "tl:area:hero"), Some(Hover { t: 400.0, on_game: false, track: Track::Hero, y: 150.0 }));
        assert_eq!(hover(&t, "tl:area:row3").map(|h| h.track), Some(Track::Row(3)));
        assert_eq!(hover(&t, "tl:area:lead").map(|h| h.track), Some(Track::Lead));
        assert_eq!(hover(&t, "tl:area:list").map(|h| h.track), Some(Track::Gap));
        // Off the tracks, or under something that covers them.
        assert_eq!(hover(&t, "tl:row:3"), None);
        assert_eq!(hover(&t, "tl:jump"), None);
        assert_eq!(hover(&t, "tl:metric:option:2"), None);
        assert_eq!(t.hover_at(None, Some(AXIS), 1200.0), None);
        assert_eq!(t.cursor, None, "pointing moves nothing");
    }

    #[test]
    fn hover_snaps_where_a_click_would() {
        let mut t = timeline();
        t.data.markers.push(Marker { t: 300.0, kind: MarkKind::Fusion, team: 0, unit: None, note: None, sample: false });
        // 3 px from the game line: a click there follows the game.
        t.pointer = Some((903.0, 250.0));
        assert_eq!(hover(&t, "tl:area:hero"), Some(Hover { t: 600.0, on_game: true, track: Track::Hero, y: 150.0 }));
        // 4 px from a marker: onto the marker.
        t.pointer = Some((604.0, 250.0));
        assert_eq!(hover(&t, "tl:area:lead").map(|h| h.t), Some(300.0));
        // On a marker's badge: the marker's moment, wherever on the badge.
        t.pointer = Some((612.0, 140.0));
        assert_eq!(hover(&t, "tl:marker:0"), Some(Hover { t: 300.0, on_game: false, track: Track::Events, y: 40.0 }));
        // What a click does at the same places.
        t.point_cursor(603.0, 1200.0, 1200.0);
        assert_eq!(t.cursor, None);
        t.point_cursor(304.0, 1200.0, 1200.0);
        assert_eq!(t.cursor, Some(300.0));
    }

    #[test]
    fn no_hover_while_the_cursor_is_dragged() {
        let mut t = timeline();
        let cx = EventCx::new();
        t.pointer = Some((700.0, 250.0));
        let mut down = UiEvent::synthetic_click("tl:area:hero");
        down.kind = UiEventKind::PointerDown;
        t.on_event(&down, &cx);
        assert_eq!(hover(&t, "tl:area:hero"), None);
        let mut up = UiEvent::synthetic_click("tl:area:hero");
        up.kind = UiEventKind::PointerUp;
        t.on_event(&up, &cx);
        assert!(hover(&t, "tl:area:hero").is_some());
        // A press elsewhere is not a drag of the cursor.
        let mut down = UiEvent::synthetic_click("tl:row:2");
        down.kind = UiEventKind::PointerDown;
        t.on_event(&down, &cx);
        assert!(hover(&t, "tl:area:hero").is_some());
    }

    #[test]
    fn pointer_moves_ask_for_a_frame_only_when_one_is_due() {
        let mut t = timeline();
        // No layout yet (nothing drawn): the pointer is over nothing of ours.
        let cx = EventCx::new();
        assert!(!t.pointer_moved(Some((700.0, 250.0)), &cx));
        assert_eq!(t.pointer, Some((700.0, 250.0)));
        // The hover line is up: the next move must redraw, even one that leaves.
        t.shown.set(Shown { hover: true, hero: true, tracks_h: 600.0 });
        assert!(t.pointer_moved(None, &cx));
        assert_eq!(t.pointer, None);
        // The same report twice changes nothing.
        assert!(!t.pointer_moved(None, &cx));
    }

    #[test]
    fn the_hover_chip_keeps_clear_of_the_other_chips() {
        let mut t = timeline();
        let frame = |hover| Frame { end_s: 1200.0, w: 1200.0, gutter: 300.0, compact: false, narrow: false, now: Instant::now(), hover, hovered: None };
        let at = |time: f32| Some(Hover { t: time, on_game: false, track: Track::Hero, y: 100.0 });
        let hover_chip = |t: &Timeline, time: f32| t.chips(&frame(at(time))).into_iter().find(|c| matches!(c.what, ChipKind::Hover(_))).map(|c| c.anchor);
        // Far from everything: centred on its line.
        assert_eq!(hover_chip(&t, 200.0), Some(Anchor::Center));
        // Just left of the game's chip (centred on 600, 50 wide): it steps aside.
        assert_eq!(hover_chip(&t, 560.0), Some(Anchor::Before));
        assert_eq!(hover_chip(&t, 640.0), Some(Anchor::After));
        // At the left edge it cannot be centred.
        assert_eq!(hover_chip(&t, 10.0), Some(Anchor::After));
        // Under the jump button (176 px right of the cursor) there is no room at all.
        t.cursor = Some(900.0);
        assert_eq!(hover_chip(&t, 960.0), None);
        // No chip of its own on the cursor or on the game line.
        assert_eq!(hover_chip(&t, 900.0), None);
        let on_game = Some(Hover { t: 600.0, on_game: true, track: Track::Hero, y: 100.0 });
        assert!(!t.chips(&frame(on_game)).iter().any(|c| matches!(c.what, ChipKind::Hover(_))));
        // Whatever is on the ruler, no two chips overlap.
        for time in [0.0, 300.0, 580.0, 620.0, 880.0, 1000.0, 1190.0] {
            let spans: Vec<(f32, f32)> = t.chips(&frame(at(time))).iter().map(|c| c.span(1200.0)).collect();
            for (i, a) in spans.iter().enumerate() {
                for b in &spans[i + 1..] {
                    assert!(a.1 <= b.0 || b.1 <= a.0, "chips overlap with the pointer at {time}: {spans:?}");
                }
            }
        }
    }

    #[test]
    fn a_long_list_is_cut_to_the_highest_and_the_picked() {
        let list = |picked: Option<usize>| -> (Vec<usize>, Option<usize>) {
            let mut lines: Vec<(usize, f32)> = (0..10).map(|k| (k, k as f32)).collect();
            let more = shortlist(&mut lines, 4, |l| l.1, |l| Some(l.0) == picked);
            (lines.into_iter().map(|l| l.0).collect(), more)
        };
        assert_eq!(list(None), (vec![9, 8, 7, 6], Some(6)));
        assert_eq!(list(Some(8)), (vec![9, 8, 7, 6], Some(6)), "already among the highest");
        assert_eq!(list(Some(2)), (vec![9, 8, 7, 2], Some(6)), "kept, in the last place");
        // A list that fits keeps its order.
        let mut short = vec![(0, 1.0), (1, 5.0)];
        assert_eq!(shortlist(&mut short, 4, |l: &(usize, f32)| l.1, |_| false), None);
        assert_eq!(short, vec![(0, 1.0), (1, 5.0)]);
    }

    #[test]
    fn the_title_menu_and_the_rows_choose_the_large_chart() {
        let mut t = timeline();
        let cx = EventCx::new();
        assert_eq!(t.hero, 0);
        assert!(t.layers().is_empty());
        // The title opens the list (once the large chart has been drawn).
        t.shown.set(Shown { hover: false, hero: true, tracks_h: 600.0 });
        t.on_event(&UiEvent::synthetic_click(KEY_METRIC), &cx);
        assert!(t.metric_open);
        assert_eq!(t.layers().len(), 1);
        // A choice from it closes it.
        t.on_event(&UiEvent::synthetic_click("tl:metric:option:4"), &cx);
        assert_eq!((t.hero, t.metric_open), (4, false));
        // A row does the same without the list.
        t.on_event(&UiEvent::synthetic_click("tl:row:2"), &cx);
        assert_eq!(t.hero, 2);
        // A choice that does not exist is ignored.
        t.on_event(&UiEvent::synthetic_click(KEY_METRIC), &cx);
        t.on_event(&UiEvent::synthetic_click("tl:metric:option:99"), &cx);
        assert_eq!(t.hero, 2);
        // A click beside the open list closes it.
        t.on_event(&UiEvent::synthetic_click("tl:metric:dismiss"), &cx);
        assert!(!t.metric_open);
        // No list where there is no large chart to hang it from (a short window).
        t.metric_open = true;
        t.shown.set(Shown { hover: false, hero: false, tracks_h: 200.0 });
        assert!(t.layers().is_empty());
    }

    #[test]
    fn escape_closes_the_list_before_it_lets_go_of_the_cursor() {
        let mut t = timeline();
        let cx = EventCx::new();
        t.cursor = Some(300.0);
        t.metric_open = true;
        let mut escape = UiEvent::synthetic_click(KEY_METRIC);
        escape.kind = UiEventKind::Escape;
        t.on_event(&escape, &cx);
        assert_eq!((t.metric_open, t.cursor), (false, Some(300.0)));
        t.on_event(&escape, &cx);
        assert_eq!(t.cursor, None);
    }

    #[test]
    fn tracks_stack_up_to_the_height_the_overlay_spans() {
        let mut t = teams_of(16);
        let frame = |compact| Frame { end_s: 1200.0, w: 1200.0, gutter: 300.0, compact, narrow: false, now: Instant::now(), hover: None, hovered: None };
        for per_player in [false, true] {
            t.per_player = per_player;
            let last = t.data.metrics.len() - 1;
            let (top, h) = t.track_span(Track::Row(last));
            assert_eq!(top + h, t.tracks_height(&frame(false)), "per player: {per_player}");
            let (top, h) = t.track_span(Track::Lead);
            assert_eq!(top + h, t.tracks_height(&frame(true)));
            let (gap_top, gap_h) = t.track_span(Track::Gap);
            assert_eq!(gap_top + gap_h, t.track_span(Track::Hero).0);
        }
    }

    #[test]
    fn markers_ahead_of_the_game_stay_hidden() {
        let mut t = timeline();
        t.data.markers.push(Marker { t: 900.0, kind: MarkKind::Fusion, team: 0, unit: None, note: None, sample: false });
        assert!(!t.marker_visible(&t.data.markers[0]));
        t.hide_future = false;
        assert!(t.marker_visible(&t.data.markers[0]));
    }
}

#[cfg(test)]
mod view_tests {
    use super::data::tests::two_sided;
    use super::*;

    /// Builds the whole view headlessly in several states and runs damascene's
    /// lint over it: catches panics in the builders and layout slips.
    #[test]
    fn view_builds_and_lints_in_every_state() {
        let theme = Theme::default();
        let income: Vec<f32> = (0..160).map(|i| i as f32).collect();
        let mut t = Timeline::new(two_sided([&income, &[5.0; 160]]));
        t.data.markers.push(Marker { t: 300.0, kind: MarkKind::Fusion, team: 0, unit: None, note: None, sample: true });
        let states: Vec<fn(&mut Timeline)> = vec![
            |t| t.set_game(0.0),
            |t| t.set_game(700.0),
            |t| t.cursor = Some(300.0),
            |t| t.cursor = Some(1100.0),
            |t| t.jump = Some(1100.0),
            |t| t.hide_future = false,
            |t| t.per_player = true,
            |t| t.picked = Some(1),
            |t| t.forced_hover = Some(Hover { t: 450.0, on_game: false, track: Track::Hero, y: 300.0 }),
            |t| t.forced_hover = Some(Hover { t: 450.0, on_game: false, track: Track::Row(8), y: 700.0 }),
            |t| t.forced_hover = Some(Hover { t: 1500.0, on_game: false, track: Track::Row(0), y: 500.0 }),
            |t| t.hide_future = true,
            |t| t.forced_hover = Some(Hover { t: 700.0, on_game: true, track: Track::Lead, y: 120.0 }),
            |t| t.forced_hover = Some(Hover { t: 20.0, on_game: false, track: Track::Ruler, y: 10.0 }),
            |t| t.metric_open = true,
            |t| t.data.computed_s = 400.0,
            |t| t.set_game(t.data.duration_s),
        ];
        for (k, state) in states.iter().enumerate() {
            state(&mut t);
            for (w, h) in [(1180.0, 900.0), (760.0, 700.0), (760.0, 400.0)] {
                let cx = BuildCx::new(&theme).with_viewport(w, h);
                let view = t.view(&cx);
                let mut root = overlays(view, t.layers().into_iter().map(Some));
                let bundle = damascene_core::render_bundle_themed(&mut root, Rect::new(0.0, 0.0, w, h), &theme);
                let serious: Vec<_> = bundle
                    .lint
                    .findings
                    .iter()
                    .filter(|f| {
                    matches!(f.kind, FindingKind::TextOverflow | FindingKind::DeadTooltip | FindingKind::DuplicateId)
                        || (f.kind == FindingKind::Overflow && f.message.contains("text cell"))
                })
                    .collect();
                assert!(serious.is_empty(), "state {k} at {w}x{h}:\n{serious:#?}");
            }
        }
    }
}

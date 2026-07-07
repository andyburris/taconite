// taconite::layer — reactive layer wrappers.
//
// The model: a layer reads what it needs out of a `State` *at paint time* and
// never owns a copy (so it's as cheap for a big struct as a small one). All the
// unsafe/Box/RefCell/raw-pointer plumbing lives here; a layer author writes only
// safe Rust — closures over a borrowed value and a safe drawing surface (`GContext`).
//
// `Draw` reads its `State<T>` each paint and hands the draw closure a borrowed `&T`.
// `Text` is the one wrapper that owns a copy, because pebble's C `TextLayer` stores
// the text pointer rather than redrawing each frame.

use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::ffi::CString;

use pebble::layer::{AsLayer, MenuCellLayer, MenuLayerRef, TextLayer, TypedMenuCallbacks};
use pebble::system::fonts::GFont;
use pebble::types::{GColor, GRect, GTextAlignment, MenuIndex, MenuRowAlign};
use pebble::window::Window;
use pebble::{GContext, RawLayer};

use core::cell::RefCell;

use crate::{State, MutableState};

// ── Draw — the safe primitive (reference model) ──────────────────────────────────

type Painter = Box<dyn Fn(&mut GContext, GRect)>;

fn run_painter(gctx: &mut GContext, painter: &mut Painter, frame: GRect) {
    (painter)(gctx, frame);
}

/// A layer whose contents are drawn from a `State<T>`, read at paint time.
pub struct Draw {
    inner:    pebble::layer::DrawLayer<Painter>,
    _painter: Box<Painter>,
}

impl Draw {
    pub fn new<T: 'static>(
        bounds: GRect,
        data: impl Into<State<T>>,
        draw: impl Fn(&mut GContext, &T, GRect) + 'static,
    ) -> Self {
        let data = data.into();
        // Erase T: the painter captures the State handle + the user closure and
        // reads the value (scoped) at paint time.
        let painter: Painter = Box::new(move |canvas, frame| {
            data.with(|t| draw(canvas, t, frame));
        });
        // The painter lives on the heap; moving the Box into the struct keeps its
        // address, so the pointer handed to pebble's DrawLayer stays valid.
        let mut boxed = Box::new(painter);
        let ptr: *mut Painter = &mut *boxed;
        let inner = pebble::layer::DrawLayer::new(bounds, ptr, run_painter);
        Draw { inner, _painter: boxed }
    }

    pub fn render(&self) {
        self.inner.mark_dirty();
    }
}

impl AsLayer for Draw {
    fn as_raw(&self) -> *mut RawLayer {
        self.inner.as_raw()
    }
}

// ── Text — the one owned wrapper (pebble TextLayer stores the pointer) ───────────

/// A text layer backed by pebble's `TextLayer`. taconite owns the `CString` the C
/// layer points at; the content is a `State<CString>` read (and copied into the
/// owned buffer) at render time. Pass `State::fixed(..)` for static text,
/// `State::from(state, |s| &s.field)` to mirror a field, or `State::computed(..)`
/// for derived text (e.g. `pbl_format!`).
pub struct Text {
    inner:   TextLayer,
    text:    RefCell<CString>,
    content: State<CString>,
}

impl Text {
    pub fn new(bounds: GRect, content: impl Into<State<CString>>) -> Self {
        let me = Text {
            inner:   TextLayer::new(bounds),
            text:    RefCell::new(c"".to_owned()),
            content: content.into(),
        };
        me.render();
        me
    }

    pub fn render(&self) {
        // Copy the current value into the owned buffer (the C layer keeps the
        // pointer), then point the C layer at it.
        self.content.with(|c| *self.text.borrow_mut() = c.clone());
        self.inner.set_text(self.text.borrow().as_c_str());
    }

    pub fn set_font(&self, font: GFont) { self.inner.set_font(font); }
    pub fn set_text_color(&self, color: GColor) { self.inner.set_text_color(color); }
    pub fn set_background_color(&self, color: GColor) { self.inner.set_background_color(color); }
    pub fn set_text_alignment(&self, alignment: GTextAlignment) { self.inner.set_text_alignment(alignment); }
}

impl AsLayer for Text {
    fn as_raw(&self) -> *mut RawLayer {
        self.inner.as_raw()
    }
}

// ── Menu ─────────────────────────────────────────────────────────────────────────
//
// Three flavors, by how the menu is bound to state — all share the same read/layout/
// paint callbacks (`&R`); they differ only in what the *input* callbacks
// (`select_click`/`select_long_click`/`selection_changed`) receive:
//
//   • `Menu::new`      — read-only `State<R>`. Input cbs get `&R` (navigate/read).
//   • `Menu::stateful` — one `MutableState<S>`, read+written. Read cbs `&S`; input
//     cbs `&MutableState<S>`. (read == write — same cell, no conflict.)
//   • `Menu::split`    — read `State<R>` + write `MutableState<W>`. Read cbs `&R`;
//     input cbs `&State<R>` (a *scoped* read handle) + `&MutableState<W>`. Works even
//     when `R` is a projection of `W`'s cell, because the input cbs read through the
//     handle (`r.with(..)`) and release before `update` — no held borrow.

// The 10 read/layout/paint callbacks, identical across all flavors (R = read data).
struct ReadCallbacks<R> {
    get_num_sections:      Option<Box<dyn Fn(MenuLayerRef, &R) -> u16>>,
    get_num_rows:          Option<Box<dyn Fn(MenuLayerRef, &R, u16) -> u16>>,
    get_cell_height:       Option<Box<dyn Fn(MenuLayerRef, &R, &MenuIndex) -> i16>>,
    get_header_height:     Option<Box<dyn Fn(MenuLayerRef, &R, u16) -> i16>>,
    get_separator_height:  Option<Box<dyn Fn(MenuLayerRef, &R, &MenuIndex) -> i16>>,
    draw_row:              Option<Box<dyn Fn(MenuLayerRef, &mut GContext, &MenuCellLayer, &MenuIndex, &R)>>,
    draw_header:           Option<Box<dyn Fn(MenuLayerRef, &mut GContext, &MenuCellLayer, u16, &R)>>,
    draw_separator:        Option<Box<dyn Fn(MenuLayerRef, &mut GContext, &MenuCellLayer, &MenuIndex, &R)>>,
    draw_background:       Option<Box<dyn Fn(MenuLayerRef, &mut GContext, &MenuCellLayer, bool, &R)>>,
    selection_will_change: Option<Box<dyn Fn(MenuLayerRef, &R, &mut MenuIndex, MenuIndex)>>,
}

// The 3 input callbacks, one shape per flavor (the only thing that differs).
struct ReadOnlyWrites<R> {
    select_click:      Option<Box<dyn Fn(MenuLayerRef, &R, &MenuIndex)>>,
    select_long_click: Option<Box<dyn Fn(MenuLayerRef, &R, &MenuIndex)>>,
    selection_changed: Option<Box<dyn Fn(MenuLayerRef, &R, MenuIndex, MenuIndex)>>,
}
struct StatefulWrites<W> {
    select_click:      Option<Box<dyn Fn(MenuLayerRef, &MutableState<W>, &MenuIndex)>>,
    select_long_click: Option<Box<dyn Fn(MenuLayerRef, &MutableState<W>, &MenuIndex)>>,
    selection_changed: Option<Box<dyn Fn(MenuLayerRef, &MutableState<W>, MenuIndex, MenuIndex)>>,
}
struct SplitWrites<R, W> {
    select_click:      Option<Box<dyn Fn(MenuLayerRef, &State<R>, &MutableState<W>, &MenuIndex)>>,
    select_long_click: Option<Box<dyn Fn(MenuLayerRef, &State<R>, &MutableState<W>, &MenuIndex)>>,
    selection_changed: Option<Box<dyn Fn(MenuLayerRef, &State<R>, &MutableState<W>, MenuIndex, MenuIndex)>>,
}

enum WriteMode<R, W> {
    ReadOnly(ReadOnlyWrites<R>),
    Stateful(StatefulWrites<W>),
    Split(SplitWrites<R, W>),
}

impl<R, W> WriteMode<R, W> {
    fn has_select_click(&self) -> bool {
        match self {
            WriteMode::ReadOnly(w) => w.select_click.is_some(),
            WriteMode::Stateful(w) => w.select_click.is_some(),
            WriteMode::Split(w)    => w.select_click.is_some(),
        }
    }
    fn has_select_long_click(&self) -> bool {
        match self {
            WriteMode::ReadOnly(w) => w.select_long_click.is_some(),
            WriteMode::Stateful(w) => w.select_long_click.is_some(),
            WriteMode::Split(w)    => w.select_long_click.is_some(),
        }
    }
    fn has_selection_changed(&self) -> bool {
        match self {
            WriteMode::ReadOnly(w) => w.selection_changed.is_some(),
            WriteMode::Stateful(w) => w.selection_changed.is_some(),
            WriteMode::Split(w)    => w.selection_changed.is_some(),
        }
    }
}

struct MenuCtx<R, W> {
    state:    State<R>,
    writable: Option<MutableState<W>>,   // Some for stateful/split, None for new
    read:     ReadCallbacks<R>,
    write:    WriteMode<R, W>,
}

// ── Public callback structs (flat; one per flavor) ───────────────────────────────

/// Callbacks for a read-only `Menu::new`. Every callback receives `&R`, plus a
/// `MenuLayerRef`. Input callbacks (`select_click`, …) read `&R` to navigate; they
/// have no menu-managed writable state (use `stateful`/`split` to update state).
pub struct MenuCallbacks<R> {
    pub get_num_sections:      Option<Box<dyn Fn(MenuLayerRef, &R) -> u16>>,
    pub get_num_rows:          Option<Box<dyn Fn(MenuLayerRef, &R, u16) -> u16>>,
    pub get_cell_height:       Option<Box<dyn Fn(MenuLayerRef, &R, &MenuIndex) -> i16>>,
    pub get_header_height:     Option<Box<dyn Fn(MenuLayerRef, &R, u16) -> i16>>,
    pub get_separator_height:  Option<Box<dyn Fn(MenuLayerRef, &R, &MenuIndex) -> i16>>,
    pub draw_row:              Option<Box<dyn Fn(MenuLayerRef, &mut GContext, &MenuCellLayer, &MenuIndex, &R)>>,
    pub draw_header:           Option<Box<dyn Fn(MenuLayerRef, &mut GContext, &MenuCellLayer, u16, &R)>>,
    pub draw_separator:        Option<Box<dyn Fn(MenuLayerRef, &mut GContext, &MenuCellLayer, &MenuIndex, &R)>>,
    pub draw_background:       Option<Box<dyn Fn(MenuLayerRef, &mut GContext, &MenuCellLayer, bool, &R)>>,
    /// Mutate the `&mut MenuIndex` (the *next* index) to redirect the selection.
    pub selection_will_change: Option<Box<dyn Fn(MenuLayerRef, &R, &mut MenuIndex, MenuIndex)>>,
    pub select_click:          Option<Box<dyn Fn(MenuLayerRef, &R, &MenuIndex)>>,
    pub select_long_click:     Option<Box<dyn Fn(MenuLayerRef, &R, &MenuIndex)>>,
    /// Args are `(old_index, new_index)`.
    pub selection_changed:     Option<Box<dyn Fn(MenuLayerRef, &R, MenuIndex, MenuIndex)>>,
}

/// Callbacks for `Menu::stateful` — the menu reads and writes one `MutableState<S>`.
/// Read/paint callbacks get `&S`; input callbacks get `&MutableState<S>` and
/// `update` it. (Read and write are the same cell; no conflict, since input cbs hold
/// no read borrow and fire at a different time than paint.)
pub struct StatefulMenuCallbacks<S> {
    pub get_num_sections:      Option<Box<dyn Fn(MenuLayerRef, &S) -> u16>>,
    pub get_num_rows:          Option<Box<dyn Fn(MenuLayerRef, &S, u16) -> u16>>,
    pub get_cell_height:       Option<Box<dyn Fn(MenuLayerRef, &S, &MenuIndex) -> i16>>,
    pub get_header_height:     Option<Box<dyn Fn(MenuLayerRef, &S, u16) -> i16>>,
    pub get_separator_height:  Option<Box<dyn Fn(MenuLayerRef, &S, &MenuIndex) -> i16>>,
    pub draw_row:              Option<Box<dyn Fn(MenuLayerRef, &mut GContext, &MenuCellLayer, &MenuIndex, &S)>>,
    pub draw_header:           Option<Box<dyn Fn(MenuLayerRef, &mut GContext, &MenuCellLayer, u16, &S)>>,
    pub draw_separator:        Option<Box<dyn Fn(MenuLayerRef, &mut GContext, &MenuCellLayer, &MenuIndex, &S)>>,
    pub draw_background:       Option<Box<dyn Fn(MenuLayerRef, &mut GContext, &MenuCellLayer, bool, &S)>>,
    pub selection_will_change: Option<Box<dyn Fn(MenuLayerRef, &S, &mut MenuIndex, MenuIndex)>>,
    pub select_click:          Option<Box<dyn Fn(MenuLayerRef, &MutableState<S>, &MenuIndex)>>,
    pub select_long_click:     Option<Box<dyn Fn(MenuLayerRef, &MutableState<S>, &MenuIndex)>>,
    pub selection_changed:     Option<Box<dyn Fn(MenuLayerRef, &MutableState<S>, MenuIndex, MenuIndex)>>,
}

/// Callbacks for `Menu::split` — read from a `State<R>`, write to a separate
/// `MutableState<W>`. Read/paint callbacks get `&R`; input callbacks get a *scoped*
/// read handle `&State<R>` (read via `r.with(|d| …)`) plus `&MutableState<W>`.
/// Because the read is scoped (released before `update`), `R` may even be a
/// projection of `W`'s own cell, e.g.
/// `split(state.as_state().map(|s, read| read(&s.items)), state)`.
pub struct SplitMenuCallbacks<R, W> {
    pub get_num_sections:      Option<Box<dyn Fn(MenuLayerRef, &R) -> u16>>,
    pub get_num_rows:          Option<Box<dyn Fn(MenuLayerRef, &R, u16) -> u16>>,
    pub get_cell_height:       Option<Box<dyn Fn(MenuLayerRef, &R, &MenuIndex) -> i16>>,
    pub get_header_height:     Option<Box<dyn Fn(MenuLayerRef, &R, u16) -> i16>>,
    pub get_separator_height:  Option<Box<dyn Fn(MenuLayerRef, &R, &MenuIndex) -> i16>>,
    pub draw_row:              Option<Box<dyn Fn(MenuLayerRef, &mut GContext, &MenuCellLayer, &MenuIndex, &R)>>,
    pub draw_header:           Option<Box<dyn Fn(MenuLayerRef, &mut GContext, &MenuCellLayer, u16, &R)>>,
    pub draw_separator:        Option<Box<dyn Fn(MenuLayerRef, &mut GContext, &MenuCellLayer, &MenuIndex, &R)>>,
    pub draw_background:       Option<Box<dyn Fn(MenuLayerRef, &mut GContext, &MenuCellLayer, bool, &R)>>,
    pub selection_will_change: Option<Box<dyn Fn(MenuLayerRef, &R, &mut MenuIndex, MenuIndex)>>,
    pub select_click:          Option<Box<dyn Fn(MenuLayerRef, &State<R>, &MutableState<W>, &MenuIndex)>>,
    pub select_long_click:     Option<Box<dyn Fn(MenuLayerRef, &State<R>, &MutableState<W>, &MenuIndex)>>,
    pub selection_changed:     Option<Box<dyn Fn(MenuLayerRef, &State<R>, &MutableState<W>, MenuIndex, MenuIndex)>>,
}

impl<R> Default for MenuCallbacks<R> {
    fn default() -> Self {
        MenuCallbacks {
            get_num_sections: None, get_num_rows: None, get_cell_height: None, get_header_height: None,
            get_separator_height: None, draw_row: None, draw_header: None, draw_separator: None,
            draw_background: None, selection_will_change: None,
            select_click: None, select_long_click: None, selection_changed: None,
        }
    }
}
impl<S> Default for StatefulMenuCallbacks<S> {
    fn default() -> Self {
        StatefulMenuCallbacks {
            get_num_sections: None, get_num_rows: None, get_cell_height: None, get_header_height: None,
            get_separator_height: None, draw_row: None, draw_header: None, draw_separator: None,
            draw_background: None, selection_will_change: None,
            select_click: None, select_long_click: None, selection_changed: None,
        }
    }
}
impl<R, W> Default for SplitMenuCallbacks<R, W> {
    fn default() -> Self {
        SplitMenuCallbacks {
            get_num_sections: None, get_num_rows: None, get_cell_height: None, get_header_height: None,
            get_separator_height: None, draw_row: None, draw_header: None, draw_separator: None,
            draw_background: None, selection_will_change: None,
            select_click: None, select_long_click: None, selection_changed: None,
        }
    }
}

pub struct Menu<R: 'static, W: 'static = ()> {
    inner: pebble::layer::MenuLayer<MenuCtx<R, W>>,
    _ctx:  Box<MenuCtx<R, W>>,
}

impl<R: 'static> Menu<R, ()> {
    /// A read-only menu over `data`. Input callbacks read `&R` (navigate); no
    /// menu-managed writable state.
    pub fn new(frame: GRect, data: impl Into<State<R>>, cbs: MenuCallbacks<R>) -> Self {
        let MenuCallbacks {
            get_num_sections, get_num_rows, get_cell_height, get_header_height, get_separator_height,
            draw_row, draw_header, draw_separator, draw_background, selection_will_change,
            select_click, select_long_click, selection_changed,
        } = cbs;
        let read = ReadCallbacks {
            get_num_sections, get_num_rows, get_cell_height, get_header_height, get_separator_height,
            draw_row, draw_header, draw_separator, draw_background, selection_will_change,
        };
        let write = WriteMode::ReadOnly(ReadOnlyWrites { select_click, select_long_click, selection_changed });
        Menu::build(frame, data.into(), None, read, write)
    }
}

impl<S: 'static> Menu<S, S> {
    /// A menu that reads and writes one `MutableState<S>`. Input callbacks get
    /// `&MutableState<S>` to `update`.
    pub fn stateful(frame: GRect, state: MutableState<S>, cbs: StatefulMenuCallbacks<S>) -> Self {
        let StatefulMenuCallbacks {
            get_num_sections, get_num_rows, get_cell_height, get_header_height, get_separator_height,
            draw_row, draw_header, draw_separator, draw_background, selection_will_change,
            select_click, select_long_click, selection_changed,
        } = cbs;
        let read = ReadCallbacks {
            get_num_sections, get_num_rows, get_cell_height, get_header_height, get_separator_height,
            draw_row, draw_header, draw_separator, draw_background, selection_will_change,
        };
        let write = WriteMode::Stateful(StatefulWrites { select_click, select_long_click, selection_changed });
        let data = state.as_state();
        Menu::build(frame, data, Some(state), read, write)
    }
}

impl<R: 'static, W: 'static> Menu<R, W> {
    /// A menu that reads `data` and writes a separate `state`. Input callbacks get a
    /// scoped read handle `&State<R>` plus `&MutableState<W>`; `R` may be a projection
    /// of `W`'s cell.
    pub fn split(frame: GRect, data: impl Into<State<R>>, state: MutableState<W>, cbs: SplitMenuCallbacks<R, W>) -> Self {
        let SplitMenuCallbacks {
            get_num_sections, get_num_rows, get_cell_height, get_header_height, get_separator_height,
            draw_row, draw_header, draw_separator, draw_background, selection_will_change,
            select_click, select_long_click, selection_changed,
        } = cbs;
        let read = ReadCallbacks {
            get_num_sections, get_num_rows, get_cell_height, get_header_height, get_separator_height,
            draw_row, draw_header, draw_separator, draw_background, selection_will_change,
        };
        let write = WriteMode::Split(SplitWrites { select_click, select_long_click, selection_changed });
        Menu::build(frame, data.into(), Some(state), read, write)
    }

    fn build(frame: GRect, state: State<R>, writable: Option<MutableState<W>>, read: ReadCallbacks<R>, write: WriteMode<R, W>) -> Self {
        let mut mc = Box::new(MenuCtx { state, writable, read, write });
        let typed = TypedMenuCallbacks::<MenuCtx<R, W>> {
            get_num_sections:      mc.read.get_num_sections     .as_ref().map(|_| t_num_sections::<R, W>        as fn(MenuLayerRef, &MenuCtx<R, W>) -> u16),
            get_num_rows:          mc.read.get_num_rows         .as_ref().map(|_| t_num_rows::<R, W>            as fn(MenuLayerRef, &MenuCtx<R, W>, u16) -> u16),
            get_cell_height:       mc.read.get_cell_height      .as_ref().map(|_| t_cell_height::<R, W>         as fn(MenuLayerRef, &MenuCtx<R, W>, &MenuIndex) -> i16),
            get_header_height:     mc.read.get_header_height    .as_ref().map(|_| t_header_height::<R, W>       as fn(MenuLayerRef, &MenuCtx<R, W>, u16) -> i16),
            get_separator_height:  mc.read.get_separator_height .as_ref().map(|_| t_separator_height::<R, W>    as fn(MenuLayerRef, &MenuCtx<R, W>, &MenuIndex) -> i16),
            draw_row:              mc.read.draw_row             .as_ref().map(|_| t_draw_row::<R, W>            as fn(MenuLayerRef, &mut GContext, &MenuCellLayer, &MenuIndex, &MenuCtx<R, W>)),
            draw_header:           mc.read.draw_header          .as_ref().map(|_| t_draw_header::<R, W>         as fn(MenuLayerRef, &mut GContext, &MenuCellLayer, u16, &MenuCtx<R, W>)),
            draw_separator:        mc.read.draw_separator       .as_ref().map(|_| t_draw_separator::<R, W>      as fn(MenuLayerRef, &mut GContext, &MenuCellLayer, &MenuIndex, &MenuCtx<R, W>)),
            draw_background:       mc.read.draw_background       .as_ref().map(|_| t_draw_background::<R, W>     as fn(MenuLayerRef, &mut GContext, &MenuCellLayer, bool, &MenuCtx<R, W>)),
            selection_will_change: mc.read.selection_will_change.as_ref().map(|_| t_selection_will_change::<R, W> as fn(MenuLayerRef, &MenuCtx<R, W>, &mut MenuIndex, MenuIndex)),
            select_click:          mc.write.has_select_click()     .then(|| t_select_click::<R, W>      as fn(MenuLayerRef, &MenuCtx<R, W>, &MenuIndex)),
            select_long_click:     mc.write.has_select_long_click().then(|| t_select_long_click::<R, W> as fn(MenuLayerRef, &MenuCtx<R, W>, &MenuIndex)),
            selection_changed:     mc.write.has_selection_changed().then(|| t_selection_changed::<R, W> as fn(MenuLayerRef, &MenuCtx<R, W>, MenuIndex, MenuIndex)),
        };
        let ptr: *mut MenuCtx<R, W> = &mut *mc;
        let inner = pebble::layer::MenuLayer::new(frame, ptr, typed);
        Menu { inner, _ctx: mc }
    }

    pub fn render(&self) { self.inner.reload_data(); }
    pub fn set_click_config_onto_window(&self, window: &Window) {
        self.inner.set_click_config_onto_window(window);
    }
    pub fn set_highlight_colors(&self, background: GColor, foreground: GColor) {
        self.inner.set_highlight_colors(background, foreground);
    }
    pub fn set_normal_colors(&self, background: GColor, foreground: GColor) {
        self.inner.set_normal_colors(background, foreground);
    }

    pub fn get_selected_index(&self) -> MenuIndex { self.inner.get_selected_index() }
    pub fn is_index_selected(&self, index: &MenuIndex) -> bool { self.inner.is_index_selected(index) }
    pub fn get_center_focused(&self) -> bool { self.inner.get_center_focused() }
    pub fn set_center_focused(&self, center_focused: bool) { self.inner.set_center_focused(center_focused); }
    pub fn set_selected_index(&self, index: MenuIndex, scroll_align: MenuRowAlign, animated: bool) {
        self.inner.set_selected_index(index, scroll_align, animated);
    }
    pub fn set_selected_next(&self, up: bool, scroll_align: MenuRowAlign, animated: bool) {
        self.inner.set_selected_next(up, scroll_align, animated);
    }
    pub fn pad_bottom_enable(&self, enable: bool) { self.inner.pad_bottom_enable(enable); }
}

impl<R: 'static, W: 'static> AsLayer for Menu<R, W> {
    fn as_raw(&self) -> *mut RawLayer {
        self.inner.as_raw()
    }
}

// `expect`: a write callback fired but no writable state is present (only happens if
// a `Menu::new` somehow had a write trampoline registered — it can't).
fn writable<R, W>(mc: &MenuCtx<R, W>) -> &MutableState<W> {
    mc.writable.as_ref().expect("taconite: menu write callback without a writable state")
}

// Read trampolines: borrow `&R` (scoped) via `mc.state.with(..)` and forward.
fn t_num_sections<R: 'static, W: 'static>(menu: MenuLayerRef, mc: &MenuCtx<R, W>) -> u16 {
    mc.state.with(|r| (mc.read.get_num_sections.as_ref().unwrap())(menu, r))
}
fn t_num_rows<R: 'static, W: 'static>(menu: MenuLayerRef, mc: &MenuCtx<R, W>, section: u16) -> u16 {
    mc.state.with(|r| (mc.read.get_num_rows.as_ref().unwrap())(menu, r, section))
}
fn t_cell_height<R: 'static, W: 'static>(menu: MenuLayerRef, mc: &MenuCtx<R, W>, idx: &MenuIndex) -> i16 {
    mc.state.with(|r| (mc.read.get_cell_height.as_ref().unwrap())(menu, r, idx))
}
fn t_header_height<R: 'static, W: 'static>(menu: MenuLayerRef, mc: &MenuCtx<R, W>, section: u16) -> i16 {
    mc.state.with(|r| (mc.read.get_header_height.as_ref().unwrap())(menu, r, section))
}
fn t_separator_height<R: 'static, W: 'static>(menu: MenuLayerRef, mc: &MenuCtx<R, W>, idx: &MenuIndex) -> i16 {
    mc.state.with(|r| (mc.read.get_separator_height.as_ref().unwrap())(menu, r, idx))
}
fn t_draw_row<R: 'static, W: 'static>(menu: MenuLayerRef, g: &mut GContext, cell: &MenuCellLayer, idx: &MenuIndex, mc: &MenuCtx<R, W>) {
    mc.state.with(|r| (mc.read.draw_row.as_ref().unwrap())(menu, g, cell, idx, r))
}
fn t_draw_header<R: 'static, W: 'static>(menu: MenuLayerRef, g: &mut GContext, cell: &MenuCellLayer, section: u16, mc: &MenuCtx<R, W>) {
    mc.state.with(|r| (mc.read.draw_header.as_ref().unwrap())(menu, g, cell, section, r))
}
fn t_draw_separator<R: 'static, W: 'static>(menu: MenuLayerRef, g: &mut GContext, cell: &MenuCellLayer, idx: &MenuIndex, mc: &MenuCtx<R, W>) {
    mc.state.with(|r| (mc.read.draw_separator.as_ref().unwrap())(menu, g, cell, idx, r))
}
fn t_draw_background<R: 'static, W: 'static>(menu: MenuLayerRef, g: &mut GContext, cell: &MenuCellLayer, highlighted: bool, mc: &MenuCtx<R, W>) {
    mc.state.with(|r| (mc.read.draw_background.as_ref().unwrap())(menu, g, cell, highlighted, r))
}
fn t_selection_will_change<R: 'static, W: 'static>(menu: MenuLayerRef, mc: &MenuCtx<R, W>, next_idx: &mut MenuIndex, current_idx: MenuIndex) {
    mc.state.with(|r| (mc.read.selection_will_change.as_ref().unwrap())(menu, r, next_idx, current_idx))
}

// Write trampolines: dispatch on the flavor. `new` holds `&R` (read-only); `stateful`
// passes `&MutableState`; `split` passes the read *handle* `&State<R>` + the writable
// (no held borrow, so an `update` on the same cell as `R` is fine).
fn t_select_click<R: 'static, W: 'static>(menu: MenuLayerRef, mc: &MenuCtx<R, W>, idx: &MenuIndex) {
    match &mc.write {
        WriteMode::ReadOnly(w) => mc.state.with(|r| (w.select_click.as_ref().unwrap())(menu, r, idx)),
        WriteMode::Stateful(w) => (w.select_click.as_ref().unwrap())(menu, writable(mc), idx),
        WriteMode::Split(w)    => (w.select_click.as_ref().unwrap())(menu, &mc.state, writable(mc), idx),
    }
}
fn t_select_long_click<R: 'static, W: 'static>(menu: MenuLayerRef, mc: &MenuCtx<R, W>, idx: &MenuIndex) {
    match &mc.write {
        WriteMode::ReadOnly(w) => mc.state.with(|r| (w.select_long_click.as_ref().unwrap())(menu, r, idx)),
        WriteMode::Stateful(w) => (w.select_long_click.as_ref().unwrap())(menu, writable(mc), idx),
        WriteMode::Split(w)    => (w.select_long_click.as_ref().unwrap())(menu, &mc.state, writable(mc), idx),
    }
}
// pebble-rust forwards `(new_index, old_index)`; the public order is `(old, new)`.
fn t_selection_changed<R: 'static, W: 'static>(menu: MenuLayerRef, mc: &MenuCtx<R, W>, new_idx: MenuIndex, old_idx: MenuIndex) {
    match &mc.write {
        WriteMode::ReadOnly(w) => mc.state.with(|r| (w.selection_changed.as_ref().unwrap())(menu, r, old_idx, new_idx)),
        WriteMode::Stateful(w) => (w.selection_changed.as_ref().unwrap())(menu, writable(mc), old_idx, new_idx),
        WriteMode::Split(w)    => (w.selection_changed.as_ref().unwrap())(menu, &mc.state, writable(mc), old_idx, new_idx),
    }
}

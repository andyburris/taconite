// taconite::layer — reactive layer wrappers.
//
// The model: a layer reads what it needs out of shared state *at paint time* and
// never owns a copy (so it's as cheap for a big struct as a small one). All the
// unsafe/Box/RefCell/raw-pointer plumbing lives here; a layer author writes only
// safe Rust — closures over `&S` and a safe drawing surface (`GContext`).
//
// Everything is built on `Draw`, which captures `ctx.shared()` once and hands the
// draw closure a borrowed `&S` each paint. `Text` is the one wrapper that owns a
// copy, because pebble's C `TextLayer` stores the text pointer rather than
// redrawing each frame.

use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::ffi::CString;
use alloc::rc::Rc;
use core::ffi::CStr;

use pebble::layer::{AsLayer, MenuCellLayer, MenuLayerRef, TextLayer, TypedMenuCallbacks};
use pebble::system::fonts::GFont;
use pebble::types::{GColor, GRect, GTextAlignment, MenuIndex, MenuRowAlign};
use pebble::window::Window;
use pebble::{GContext, RawLayer};

use core::cell::RefCell;

use crate::State;

// ── Draw — the safe primitive (reference model) ──────────────────────────────────

type Painter = Box<dyn Fn(&mut GContext, GRect)>;

fn run_painter(gctx: &mut GContext, painter: &mut Painter, frame: GRect) {
    (painter)(gctx, frame);
}

/// A layer whose contents are drawn from a borrowed `&S` at paint time.
pub struct Draw {
    inner:    pebble::layer::DrawLayer<Painter>,
    _painter: Box<Painter>,
}

impl Draw {
    pub fn new<S: 'static>(
        bounds: GRect,
        state: &State<S>,
        draw: impl Fn(&mut GContext, &S, GRect) + 'static,
    ) -> Self {
        let state = state.clone();
        // Erase S: the painter captures the State handle + the user closure and
        // reads the state at paint time.
        let painter: Painter = Box::new(move |canvas, frame| {
            draw(canvas, &state.read(), frame);
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

/// A text layer backed by pebble's `TextLayer`. taconite owns the `CString` the
/// C layer points at; the author supplies a selector that borrows from state.
pub struct Text {
    inner:  TextLayer,
    text:   RefCell<CString>,
    select: Box<dyn Fn() -> CString>,
}

impl Text {
    pub fn new<S: 'static>(
        bounds: GRect,
        state: &State<S>,
        f: impl Fn(&S) -> &CStr + 'static,
    ) -> Self {
        let state = state.clone();
        let select: Box<dyn Fn() -> CString> = Box::new(move || f(&state.read()).to_owned());
        let me = Text {
            inner:  TextLayer::new(bounds),
            text:   RefCell::new(c"".to_owned()),
            select,
        };
        me.render();
        me
    }

    /// Like `new`, but the selector returns the owned `CString` directly — use it
    /// when the text is *computed* (e.g. `pbl_format!(…)`) rather than a borrow of
    /// an existing field. The lower-level primitive behind `from_build`.
    pub fn computed<S: 'static>(
        bounds: GRect,
        state: &State<S>,
        f: impl Fn(&S) -> CString + 'static,
    ) -> Self {
        let state = state.clone();
        let select: Box<dyn Fn() -> CString> = Box::new(move || f(&state.read()));
        let me = Text {
            inner:  TextLayer::new(bounds),
            text:   RefCell::new(c"".to_owned()),
            select,
        };
        me.render();
        me
    }

    /// A Text child driven by a composing custom layer's shared `build` callback.
    /// `extract` pulls this layer's string out of the (ephemeral) data struct; the
    /// continuation-passing dance is handled internally so the author writes a
    /// one-liner: `Text::from_build(rect, ctx, build.clone(), |d| d.label.clone())`.
    ///
    /// NOTE: still some boilerplate (the `Rc<dyn Fn…>` + `.clone()` per child); the
    /// eventual `custom_layer!` macro is intended to generate this wiring.
    pub fn from_build<S: 'static, D: 'static>(
        bounds: GRect,
        state: &State<S>,
        build: Rc<dyn Fn(&S, &mut dyn FnMut(&D))>,
        extract: impl Fn(&D) -> CString + 'static,
    ) -> Self {
        Text::computed(bounds, state, move |s| {
            let mut out = c"".to_owned();
            build(s, &mut |d| out = extract(d));
            out
        })
    }

    pub fn render(&self) {
        *self.text.borrow_mut() = (self.select)();
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

// ── Menu — generic over the screen state ─────────────────────────────────────────

/// Reactive menu callbacks, mirroring pebble's full menu callback set.
///
/// Read-only callbacks (layout/paint) receive `&S`, the live screen state
/// borrowed for the call. Writable callbacks (user input) receive `&State<S>`, so
/// they can `state.update(..)` and repaint sibling layers. Every callback also
/// gets a `MenuLayerRef` to query the menu itself (selected index, etc.).
///
/// Callbacks are boxed closures (not `fn` pointers) so they can capture — e.g. a
/// shared `Rc<build>` when this menu is one child of a composing custom layer.
/// Wrap each at the call site: `Some(Box::new(|menu, s, section| …))`.
pub struct MenuCallbacks<S> {
    // ── read-only (layout / paint): borrowed `&S` ──
    pub get_num_sections:      Option<Box<dyn Fn(MenuLayerRef, &S) -> u16>>,
    pub get_num_rows:          Option<Box<dyn Fn(MenuLayerRef, &S, u16) -> u16>>,
    pub get_cell_height:       Option<Box<dyn Fn(MenuLayerRef, &S, &MenuIndex) -> i16>>,
    pub get_header_height:     Option<Box<dyn Fn(MenuLayerRef, &S, u16) -> i16>>,
    pub get_separator_height:  Option<Box<dyn Fn(MenuLayerRef, &S, &MenuIndex) -> i16>>,
    pub draw_row:              Option<Box<dyn Fn(MenuLayerRef, &mut GContext, &MenuCellLayer, &MenuIndex, &S)>>,
    pub draw_header:           Option<Box<dyn Fn(MenuLayerRef, &mut GContext, &MenuCellLayer, u16, &S)>>,
    pub draw_separator:        Option<Box<dyn Fn(MenuLayerRef, &mut GContext, &MenuCellLayer, &MenuIndex, &S)>>,
    pub draw_background:       Option<Box<dyn Fn(MenuLayerRef, &mut GContext, &MenuCellLayer, bool, &S)>>,
    /// Fires just before the selection moves; mutate the `&mut MenuIndex` (the
    /// *next* index) to redirect it. Read-only on state.
    pub selection_will_change: Option<Box<dyn Fn(MenuLayerRef, &S, &mut MenuIndex, MenuIndex)>>,
    // ── writable (user input): `&State<S>`, call `.update(..)` ──
    pub select_click:          Option<Box<dyn Fn(MenuLayerRef, &State<S>, &MenuIndex)>>,
    pub select_long_click:     Option<Box<dyn Fn(MenuLayerRef, &State<S>, &MenuIndex)>>,
    /// Args are `(old_index, new_index)`.
    pub selection_changed:     Option<Box<dyn Fn(MenuLayerRef, &State<S>, MenuIndex, MenuIndex)>>,
}

impl<S> Default for MenuCallbacks<S> {
    fn default() -> Self {
        MenuCallbacks {
            get_num_sections:      None,
            get_num_rows:          None,
            get_cell_height:       None,
            get_header_height:     None,
            get_separator_height:  None,
            draw_row:              None,
            draw_header:           None,
            draw_separator:        None,
            draw_background:       None,
            selection_will_change: None,
            select_click:          None,
            select_long_click:     None,
            selection_changed:     None,
        }
    }
}

struct MenuCtx<S> {
    state: State<S>,
    cbs:   MenuCallbacks<S>,
}

pub struct Menu<S: 'static> {
    inner: pebble::layer::MenuLayer<MenuCtx<S>>,
    _ctx:  Box<MenuCtx<S>>,
}

impl<S: 'static> Menu<S> {
    pub fn new(frame: GRect, state: &State<S>, cbs: MenuCallbacks<S>) -> Self {
        let mut mc = Box::new(MenuCtx { state: state.clone(), cbs });
        let typed = TypedMenuCallbacks::<MenuCtx<S>> {
            get_num_sections:      mc.cbs.get_num_sections     .as_ref().map(|_| t_num_sections::<S>        as fn(MenuLayerRef, &MenuCtx<S>) -> u16),
            get_num_rows:          mc.cbs.get_num_rows         .as_ref().map(|_| t_num_rows::<S>            as fn(MenuLayerRef, &MenuCtx<S>, u16) -> u16),
            get_cell_height:       mc.cbs.get_cell_height      .as_ref().map(|_| t_cell_height::<S>         as fn(MenuLayerRef, &MenuCtx<S>, &MenuIndex) -> i16),
            get_header_height:     mc.cbs.get_header_height    .as_ref().map(|_| t_header_height::<S>       as fn(MenuLayerRef, &MenuCtx<S>, u16) -> i16),
            get_separator_height:  mc.cbs.get_separator_height .as_ref().map(|_| t_separator_height::<S>    as fn(MenuLayerRef, &MenuCtx<S>, &MenuIndex) -> i16),
            draw_row:              mc.cbs.draw_row             .as_ref().map(|_| t_draw_row::<S>            as fn(MenuLayerRef, &mut GContext, &MenuCellLayer, &MenuIndex, &MenuCtx<S>)),
            draw_header:           mc.cbs.draw_header          .as_ref().map(|_| t_draw_header::<S>         as fn(MenuLayerRef, &mut GContext, &MenuCellLayer, u16, &MenuCtx<S>)),
            draw_separator:        mc.cbs.draw_separator       .as_ref().map(|_| t_draw_separator::<S>      as fn(MenuLayerRef, &mut GContext, &MenuCellLayer, &MenuIndex, &MenuCtx<S>)),
            draw_background:       mc.cbs.draw_background       .as_ref().map(|_| t_draw_background::<S>     as fn(MenuLayerRef, &mut GContext, &MenuCellLayer, bool, &MenuCtx<S>)),
            selection_will_change: mc.cbs.selection_will_change.as_ref().map(|_| t_selection_will_change::<S> as fn(MenuLayerRef, &MenuCtx<S>, &mut MenuIndex, MenuIndex)),
            select_click:          mc.cbs.select_click         .as_ref().map(|_| t_select_click::<S>        as fn(MenuLayerRef, &MenuCtx<S>, &MenuIndex)),
            select_long_click:     mc.cbs.select_long_click    .as_ref().map(|_| t_select_long_click::<S>   as fn(MenuLayerRef, &MenuCtx<S>, &MenuIndex)),
            selection_changed:     mc.cbs.selection_changed    .as_ref().map(|_| t_selection_changed::<S>   as fn(MenuLayerRef, &MenuCtx<S>, MenuIndex, MenuIndex)),
        };
        let ptr: *mut MenuCtx<S> = &mut *mc;
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

impl<S: 'static> AsLayer for Menu<S> {
    fn as_raw(&self) -> *mut RawLayer {
        self.inner.as_raw()
    }
}

// Read-only trampolines borrow `&S` for the call via `mc.state.read()`.
fn t_num_sections<S: 'static>(menu: MenuLayerRef, mc: &MenuCtx<S>) -> u16 {
    (mc.cbs.get_num_sections.as_ref().unwrap())(menu, &mc.state.read())
}
fn t_num_rows<S: 'static>(menu: MenuLayerRef, mc: &MenuCtx<S>, section: u16) -> u16 {
    (mc.cbs.get_num_rows.as_ref().unwrap())(menu, &mc.state.read(), section)
}
fn t_cell_height<S: 'static>(menu: MenuLayerRef, mc: &MenuCtx<S>, idx: &MenuIndex) -> i16 {
    (mc.cbs.get_cell_height.as_ref().unwrap())(menu, &mc.state.read(), idx)
}
fn t_header_height<S: 'static>(menu: MenuLayerRef, mc: &MenuCtx<S>, section: u16) -> i16 {
    (mc.cbs.get_header_height.as_ref().unwrap())(menu, &mc.state.read(), section)
}
fn t_separator_height<S: 'static>(menu: MenuLayerRef, mc: &MenuCtx<S>, idx: &MenuIndex) -> i16 {
    (mc.cbs.get_separator_height.as_ref().unwrap())(menu, &mc.state.read(), idx)
}
fn t_draw_row<S: 'static>(menu: MenuLayerRef, g: &mut GContext, cell: &MenuCellLayer, idx: &MenuIndex, mc: &MenuCtx<S>) {
    (mc.cbs.draw_row.as_ref().unwrap())(menu, g, cell, idx, &mc.state.read())
}
fn t_draw_header<S: 'static>(menu: MenuLayerRef, g: &mut GContext, cell: &MenuCellLayer, section: u16, mc: &MenuCtx<S>) {
    (mc.cbs.draw_header.as_ref().unwrap())(menu, g, cell, section, &mc.state.read())
}
fn t_draw_separator<S: 'static>(menu: MenuLayerRef, g: &mut GContext, cell: &MenuCellLayer, idx: &MenuIndex, mc: &MenuCtx<S>) {
    (mc.cbs.draw_separator.as_ref().unwrap())(menu, g, cell, idx, &mc.state.read())
}
fn t_draw_background<S: 'static>(menu: MenuLayerRef, g: &mut GContext, cell: &MenuCellLayer, highlighted: bool, mc: &MenuCtx<S>) {
    (mc.cbs.draw_background.as_ref().unwrap())(menu, g, cell, highlighted, &mc.state.read())
}
fn t_selection_will_change<S: 'static>(menu: MenuLayerRef, mc: &MenuCtx<S>, next_idx: &mut MenuIndex, current_idx: MenuIndex) {
    (mc.cbs.selection_will_change.as_ref().unwrap())(menu, &mc.state.read(), next_idx, current_idx)
}

// Writable trampolines hand the user the `State` handle (call `.update(..)`).
fn t_select_click<S: 'static>(menu: MenuLayerRef, mc: &MenuCtx<S>, idx: &MenuIndex) {
    (mc.cbs.select_click.as_ref().unwrap())(menu, &mc.state, idx)
}
fn t_select_long_click<S: 'static>(menu: MenuLayerRef, mc: &MenuCtx<S>, idx: &MenuIndex) {
    (mc.cbs.select_long_click.as_ref().unwrap())(menu, &mc.state, idx)
}
// pebble-rust forwards `(new_index, old_index)`; the public order is `(old, new)`.
fn t_selection_changed<S: 'static>(menu: MenuLayerRef, mc: &MenuCtx<S>, new_idx: MenuIndex, old_idx: MenuIndex) {
    (mc.cbs.selection_changed.as_ref().unwrap())(menu, &mc.state, old_idx, new_idx)
}

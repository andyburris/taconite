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
use core::ffi::CStr;

use pebble::layer::{ILayer, TextLayer, TypedMenuCallbacks};
use pebble::system::fonts::Font;
use pebble::types::{GColor, GRect, GTextAlignment, MenuIndex};
use pebble::window::Window;
use pebble::{GContext, RawLayer};

use crate::cell::ReCell;
use crate::{ScreenCtx, Shared};

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
        ctx: &ScreenCtx<S>,
        draw: impl Fn(&mut GContext, &S, GRect) + 'static,
    ) -> Self {
        let shared: Shared<S> = ctx.shared();
        // Erase S: the painter captures Shared<S> + the user closure and borrows
        // the state at paint time.
        let painter: Painter = Box::new(move |canvas, frame| {
            draw(canvas, &shared.borrow(), frame);
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

impl ILayer for Draw {
    fn get_internal(&self) -> *mut RawLayer {
        self.inner.get_internal()
    }
}

// ── Text — the one owned wrapper (pebble TextLayer stores the pointer) ───────────

/// A text layer backed by pebble's `TextLayer`. taconite owns the `CString` the
/// C layer points at; the author supplies a selector that borrows from state.
pub struct Text {
    inner:  TextLayer,
    text:   ReCell<CString>,
    select: Box<dyn Fn() -> CString>,
}

impl Text {
    pub fn new<S: 'static>(
        bounds: GRect,
        ctx: &ScreenCtx<S>,
        f: impl Fn(&S) -> &CStr + 'static,
    ) -> Self {
        let shared: Shared<S> = ctx.shared();
        let select: Box<dyn Fn() -> CString> = Box::new(move || f(&shared.borrow()).to_owned());
        let me = Text {
            inner:  TextLayer::new(bounds),
            text:   ReCell::new(c"".to_owned()),
            select,
        };
        me.render();
        me
    }

    pub fn render(&self) {
        *self.text.borrow_mut() = (self.select)();
        self.inner.set_text(self.text.borrow().as_c_str());
    }

    pub fn set_font(&self, font: Font) { self.inner.set_font(font); }
    pub fn set_text_color(&self, color: GColor) { self.inner.set_text_color(color); }
    pub fn set_background_color(&self, color: GColor) { self.inner.set_background_color(color); }
    pub fn set_text_alignment(&self, alignment: GTextAlignment) { self.inner.set_text_alignment(alignment); }
}

impl ILayer for Text {
    fn get_internal(&self) -> *mut RawLayer {
        self.inner.get_internal()
    }
}

// ── Menu — generic over the screen state ─────────────────────────────────────────

/// Reactive menu callbacks. Each receives `&S` (the live screen state) instead of
/// a raw context. Mirrors the subset of pebble's menu callbacks most apps use.
pub struct MenuCallbacks<S> {
    pub get_num_sections:  Option<fn(&S) -> u16>,
    pub get_num_rows:      Option<fn(&S, u16) -> u16>,
    pub get_cell_height:   Option<fn(&S, &MenuIndex) -> i16>,
    pub get_header_height: Option<fn(&S, u16) -> i16>,
    pub draw_row:          Option<fn(*mut GContext, *const RawLayer, &MenuIndex, &S)>,
    pub draw_header:       Option<fn(*mut GContext, *const RawLayer, u16, &S)>,
    pub select_click:      Option<fn(&S, &MenuIndex)>,
}

impl<S> Default for MenuCallbacks<S> {
    fn default() -> Self {
        MenuCallbacks {
            get_num_sections:  None,
            get_num_rows:      None,
            get_cell_height:   None,
            get_header_height: None,
            draw_row:          None,
            draw_header:       None,
            select_click:      None,
        }
    }
}

struct MenuCtx<S> {
    shared: Shared<S>,
    cbs:    MenuCallbacks<S>,
}

pub struct Menu<S: 'static> {
    inner: pebble::layer::MenuLayer<MenuCtx<S>>,
    _ctx:  Box<MenuCtx<S>>,
}

impl<S: 'static> Menu<S> {
    pub fn new(frame: GRect, ctx: &ScreenCtx<S>, cbs: MenuCallbacks<S>) -> Self {
        let mut mc = Box::new(MenuCtx { shared: ctx.shared(), cbs });
        let typed = TypedMenuCallbacks::<MenuCtx<S>> {
            get_num_sections:  mc.cbs.get_num_sections .map(|_| t_num_sections::<S>  as fn(&MenuCtx<S>) -> u16),
            get_num_rows:      mc.cbs.get_num_rows     .map(|_| t_num_rows::<S>      as fn(&MenuCtx<S>, u16) -> u16),
            get_cell_height:   mc.cbs.get_cell_height  .map(|_| t_cell_height::<S>   as fn(&MenuCtx<S>, &MenuIndex) -> i16),
            get_header_height: mc.cbs.get_header_height.map(|_| t_header_height::<S> as fn(&MenuCtx<S>, u16) -> i16),
            draw_row:          mc.cbs.draw_row         .map(|_| t_draw_row::<S>      as fn(*mut GContext, *const RawLayer, &MenuIndex, &MenuCtx<S>)),
            draw_header:       mc.cbs.draw_header      .map(|_| t_draw_header::<S>   as fn(*mut GContext, *const RawLayer, u16, &MenuCtx<S>)),
            select_click:      mc.cbs.select_click     .map(|_| t_select_click::<S>  as fn(&MenuCtx<S>, &MenuIndex)),
            ..TypedMenuCallbacks::default()
        };
        let ptr: *mut MenuCtx<S> = &mut *mc;
        let inner = pebble::layer::MenuLayer::new(frame, ptr, typed);
        Menu { inner, _ctx: mc }
    }

    pub fn render(&self) { self.inner.reload_data(); }
    pub fn set_click_config_onto_window(&self, window: &Window) {
        self.inner.set_click_config_onto_window(window);
    }
}

impl<S: 'static> ILayer for Menu<S> {
    fn get_internal(&self) -> *mut RawLayer {
        self.inner.get_internal()
    }
}

fn t_num_sections<S>(mc: &MenuCtx<S>) -> u16 {
    (mc.cbs.get_num_sections.unwrap())(&mc.shared.borrow())
}
fn t_num_rows<S>(mc: &MenuCtx<S>, section: u16) -> u16 {
    (mc.cbs.get_num_rows.unwrap())(&mc.shared.borrow(), section)
}
fn t_cell_height<S>(mc: &MenuCtx<S>, idx: &MenuIndex) -> i16 {
    (mc.cbs.get_cell_height.unwrap())(&mc.shared.borrow(), idx)
}
fn t_header_height<S>(mc: &MenuCtx<S>, section: u16) -> i16 {
    (mc.cbs.get_header_height.unwrap())(&mc.shared.borrow(), section)
}
fn t_draw_row<S>(g: *mut GContext, cell: *const RawLayer, idx: &MenuIndex, mc: &MenuCtx<S>) {
    (mc.cbs.draw_row.unwrap())(g, cell, idx, &mc.shared.borrow())
}
fn t_draw_header<S>(g: *mut GContext, cell: *const RawLayer, section: u16, mc: &MenuCtx<S>) {
    (mc.cbs.draw_header.unwrap())(g, cell, section, &mc.shared.borrow())
}
fn t_select_click<S>(mc: &MenuCtx<S>, idx: &MenuIndex) {
    (mc.cbs.select_click.unwrap())(&mc.shared.borrow(), idx)
}

// taconite::layer — reactive layer wrappers.
//
// The model: a layer reads what it needs out of a `State` *at paint time* and never
// owns a copy (so it's as cheap for a big struct as a small one). All the
// unsafe/Box/RefCell plumbing lives in pebble-rust's layer wrappers; a layer author
// writes only safe Rust — closures over a borrowed value and a safe drawing surface.

use alloc::boxed::Box;
use alloc::ffi::CString;
use alloc::rc::Rc;
use core::cell::RefCell;
use core::ffi::CStr;

use pebble::content_indicator::{ContentIndicator, ContentIndicatorDirection};
use pebble::layer::{AsLayer, Layer, MenuCellLayer, MenuLayerRef, TypedMenuCallbacks};
use pebble::system::fonts::{FontKey, GFont};
use pebble::types::{GAlign, GColor, GCornerMask, GRect, GTextAlignment, GTextOverflowMode, MenuIndex, MenuRowAlign};
use pebble::window::Window;
use pebble::{GContext, RawLayer};

use crate::state::Snap;
use crate::State;

// ── Draw — the safe primitive (reference model) ──────────────────────────────────

type Painter = Box<dyn Fn(&mut GContext, GRect)>;

fn run_painter(gctx: &mut GContext, painter: &mut Painter, frame: GRect) {
    (painter)(gctx, frame);
}

/// A layer whose contents are drawn from a `State<T>`, read at paint time.
pub struct Draw {
    inner: pebble::layer::DrawLayer<Painter>,
}

impl Draw {
    pub fn new<T: 'static>(
        bounds: GRect,
        data: impl Into<State<T>>,
        draw: impl Fn(&mut GContext, &T, GRect) + 'static,
    ) -> Self {
        let data = data.into();
        // Erase T: the painter captures the State handle + the user closure and reads
        // the value (scoped) at paint time. `DrawLayer` owns the painter box.
        let painter: Painter = Box::new(move |canvas, frame| {
            data.with(|t| draw(canvas, t, frame));
        });
        Draw { inner: pebble::layer::DrawLayer::new(bounds, painter, run_painter) }
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

// ── Text — a pull layer over `graphics_draw_text` (no owned buffer) ──────────────
//
// Unlike the C `TextLayer` (which stores a pointer and so needs an owned buffer),
// this draws with `graphics_draw_text` inside its update proc — reading the content
// straight through the `State` chain at paint time. No copy, no stored pointer.

/// Anything a `Text` can render: a reactive string, a snapshot, or a literal. One
/// `Text::new` accepts them all.
pub trait TextContent {
    fn read(&self, f: &mut dyn FnMut(&CStr));
}
impl TextContent for State<CString> {
    fn read(&self, f: &mut dyn FnMut(&CStr)) { self.with(|c| f(c.as_c_str())); }
}
impl TextContent for State<Snap<CString>> {
    fn read(&self, f: &mut dyn FnMut(&CStr)) { self.with(|s| f(s.as_c_str())); }
}
impl TextContent for Snap<CString> {
    fn read(&self, f: &mut dyn FnMut(&CStr)) { f(self.as_c_str()); }
}
impl TextContent for &'static CStr {
    fn read(&self, f: &mut dyn FnMut(&CStr)) { f(self); }
}

struct TextStyle {
    font:             GFont,
    text_color:       GColor,
    background_color: Option<GColor>,
    alignment:        GTextAlignment,
    overflow:         GTextOverflowMode,
}

struct TextCtx {
    content: Box<dyn TextContent>,
    style:   Rc<RefCell<TextStyle>>,
}

fn text_draw(g: &mut GContext, ctx: &mut TextCtx, frame: GRect) {
    let style = ctx.style.borrow();
    if let Some(bg) = style.background_color {
        g.set_fill_color(bg);
        g.fill_rect(frame, 0, GCornerMask::GCornerNone);
    }
    g.set_text_color(style.text_color);
    ctx.content.read(&mut |text| {
        g.draw_text(text, &style.font, frame, style.overflow, style.alignment);
    });
}

/// A text layer that draws its content each paint by reading it through the `State`
/// chain — accepts `State<CString>`, `State<Snap<CString>>`, a `Snap<CString>`, or a
/// `&'static CStr` literal.
pub struct Text {
    inner: pebble::layer::DrawLayer<TextCtx>,
    style: Rc<RefCell<TextStyle>>,
}

impl Text {
    pub fn new(bounds: GRect, content: impl TextContent + 'static) -> Self {
        let style = Rc::new(RefCell::new(TextStyle {
            font:             GFont::get_system(FontKey::GOTHIC_24),
            text_color:       GColor::Black,
            background_color: None,
            alignment:        GTextAlignment::Left,
            overflow:         GTextOverflowMode::TrailingEllipsis,
        }));
        let ctx = TextCtx { content: Box::new(content), style: style.clone() };
        Text { inner: pebble::layer::DrawLayer::new(bounds, ctx, text_draw), style }
    }

    /// Repaint (reads current content). Like `Draw::render`, it just marks dirty.
    pub fn render(&self) {
        self.inner.mark_dirty();
    }

    pub fn set_font(&self, font: GFont) {
        self.style.borrow_mut().font = font;
        self.render();
    }
    pub fn set_text_color(&self, color: GColor) {
        self.style.borrow_mut().text_color = color;
        self.render();
    }
    pub fn set_background_color(&self, color: GColor) {
        self.style.borrow_mut().background_color = Some(color);
        self.render();
    }
    pub fn set_text_alignment(&self, alignment: GTextAlignment) {
        self.style.borrow_mut().alignment = alignment;
        self.render();
    }
    pub fn set_overflow(&self, overflow: GTextOverflowMode) {
        self.style.borrow_mut().overflow = overflow;
        self.render();
    }
}

impl AsLayer for Text {
    fn as_raw(&self) -> *mut RawLayer {
        self.inner.as_raw()
    }
}

// ── Menu ─────────────────────────────────────────────────────────────────────────
//
// One `Menu<R>` over a `State<R>`. The read/layout/paint callbacks get `&R` (read at
// paint time). The three input callbacks — `select_click`, `select_long_click`,
// `selection_changed` — take no state: they *capture* whatever handles they need
// (e.g. `let sel = state.clone(); move |_, idx| sel.update(..)`), which holds no read
// borrow, so a same-cell `update` is fine. To read from two cells at once, feed the
// menu a `State::combine(..)` of them.

/// Callbacks for a `Menu<R>`. Read/layout/paint callbacks receive `&R`; input
/// callbacks capture their own state handles.
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
    pub select_click:          Option<Box<dyn Fn(MenuLayerRef, &MenuIndex)>>,
    pub select_long_click:     Option<Box<dyn Fn(MenuLayerRef, &MenuIndex)>>,
    /// Args are `(old_index, new_index)`.
    pub selection_changed:     Option<Box<dyn Fn(MenuLayerRef, MenuIndex, MenuIndex)>>,
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

struct MenuCtx<R> {
    state: State<R>,
    cbs:   MenuCallbacks<R>,
}

pub struct Menu<R: 'static> {
    inner: pebble::layer::MenuLayer<MenuCtx<R>>,
}

impl<R: 'static> Menu<R> {
    pub fn new(frame: GRect, data: impl Into<State<R>>, cbs: MenuCallbacks<R>) -> Self {
        let mc = MenuCtx { state: data.into(), cbs };
        let typed = TypedMenuCallbacks::<MenuCtx<R>> {
            get_num_sections:      mc.cbs.get_num_sections     .as_ref().map(|_| t_num_sections::<R>        as fn(MenuLayerRef, &MenuCtx<R>) -> u16),
            get_num_rows:          mc.cbs.get_num_rows         .as_ref().map(|_| t_num_rows::<R>            as fn(MenuLayerRef, &MenuCtx<R>, u16) -> u16),
            get_cell_height:       mc.cbs.get_cell_height      .as_ref().map(|_| t_cell_height::<R>         as fn(MenuLayerRef, &MenuCtx<R>, &MenuIndex) -> i16),
            get_header_height:     mc.cbs.get_header_height    .as_ref().map(|_| t_header_height::<R>       as fn(MenuLayerRef, &MenuCtx<R>, u16) -> i16),
            get_separator_height:  mc.cbs.get_separator_height .as_ref().map(|_| t_separator_height::<R>    as fn(MenuLayerRef, &MenuCtx<R>, &MenuIndex) -> i16),
            draw_row:              mc.cbs.draw_row             .as_ref().map(|_| t_draw_row::<R>            as fn(MenuLayerRef, &mut GContext, &MenuCellLayer, &MenuIndex, &MenuCtx<R>)),
            draw_header:           mc.cbs.draw_header          .as_ref().map(|_| t_draw_header::<R>         as fn(MenuLayerRef, &mut GContext, &MenuCellLayer, u16, &MenuCtx<R>)),
            draw_separator:        mc.cbs.draw_separator       .as_ref().map(|_| t_draw_separator::<R>      as fn(MenuLayerRef, &mut GContext, &MenuCellLayer, &MenuIndex, &MenuCtx<R>)),
            draw_background:       mc.cbs.draw_background      .as_ref().map(|_| t_draw_background::<R>     as fn(MenuLayerRef, &mut GContext, &MenuCellLayer, bool, &MenuCtx<R>)),
            selection_will_change: mc.cbs.selection_will_change.as_ref().map(|_| t_selection_will_change::<R> as fn(MenuLayerRef, &MenuCtx<R>, &mut MenuIndex, MenuIndex)),
            select_click:          mc.cbs.select_click         .as_ref().map(|_| t_select_click::<R>        as fn(MenuLayerRef, &MenuCtx<R>, &MenuIndex)),
            select_long_click:     mc.cbs.select_long_click    .as_ref().map(|_| t_select_long_click::<R>   as fn(MenuLayerRef, &MenuCtx<R>, &MenuIndex)),
            selection_changed:     mc.cbs.selection_changed    .as_ref().map(|_| t_selection_changed::<R>   as fn(MenuLayerRef, &MenuCtx<R>, MenuIndex, MenuIndex)),
        };
        Menu { inner: pebble::layer::MenuLayer::new(frame, mc, typed) }
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

impl<R: 'static> AsLayer for Menu<R> {
    fn as_raw(&self) -> *mut RawLayer {
        self.inner.as_raw()
    }
}

// Read/layout/paint trampolines: borrow `&R` (scoped) via `mc.state.with(..)`.
fn t_num_sections<R: 'static>(menu: MenuLayerRef, mc: &MenuCtx<R>) -> u16 {
    mc.state.with(|r| (mc.cbs.get_num_sections.as_ref().unwrap())(menu, r))
}
fn t_num_rows<R: 'static>(menu: MenuLayerRef, mc: &MenuCtx<R>, section: u16) -> u16 {
    mc.state.with(|r| (mc.cbs.get_num_rows.as_ref().unwrap())(menu, r, section))
}
fn t_cell_height<R: 'static>(menu: MenuLayerRef, mc: &MenuCtx<R>, idx: &MenuIndex) -> i16 {
    mc.state.with(|r| (mc.cbs.get_cell_height.as_ref().unwrap())(menu, r, idx))
}
fn t_header_height<R: 'static>(menu: MenuLayerRef, mc: &MenuCtx<R>, section: u16) -> i16 {
    mc.state.with(|r| (mc.cbs.get_header_height.as_ref().unwrap())(menu, r, section))
}
fn t_separator_height<R: 'static>(menu: MenuLayerRef, mc: &MenuCtx<R>, idx: &MenuIndex) -> i16 {
    mc.state.with(|r| (mc.cbs.get_separator_height.as_ref().unwrap())(menu, r, idx))
}
fn t_draw_row<R: 'static>(menu: MenuLayerRef, g: &mut GContext, cell: &MenuCellLayer, idx: &MenuIndex, mc: &MenuCtx<R>) {
    mc.state.with(|r| (mc.cbs.draw_row.as_ref().unwrap())(menu, g, cell, idx, r))
}
fn t_draw_header<R: 'static>(menu: MenuLayerRef, g: &mut GContext, cell: &MenuCellLayer, section: u16, mc: &MenuCtx<R>) {
    mc.state.with(|r| (mc.cbs.draw_header.as_ref().unwrap())(menu, g, cell, section, r))
}
fn t_draw_separator<R: 'static>(menu: MenuLayerRef, g: &mut GContext, cell: &MenuCellLayer, idx: &MenuIndex, mc: &MenuCtx<R>) {
    mc.state.with(|r| (mc.cbs.draw_separator.as_ref().unwrap())(menu, g, cell, idx, r))
}
fn t_draw_background<R: 'static>(menu: MenuLayerRef, g: &mut GContext, cell: &MenuCellLayer, highlighted: bool, mc: &MenuCtx<R>) {
    mc.state.with(|r| (mc.cbs.draw_background.as_ref().unwrap())(menu, g, cell, highlighted, r))
}
fn t_selection_will_change<R: 'static>(menu: MenuLayerRef, mc: &MenuCtx<R>, next_idx: &mut MenuIndex, current_idx: MenuIndex) {
    mc.state.with(|r| (mc.cbs.selection_will_change.as_ref().unwrap())(menu, r, next_idx, current_idx))
}

// Input trampolines: call the captured closure directly — no read borrow held, so a
// same-cell `update` inside the closure is fine.
fn t_select_click<R: 'static>(menu: MenuLayerRef, mc: &MenuCtx<R>, idx: &MenuIndex) {
    (mc.cbs.select_click.as_ref().unwrap())(menu, idx)
}
fn t_select_long_click<R: 'static>(menu: MenuLayerRef, mc: &MenuCtx<R>, idx: &MenuIndex) {
    (mc.cbs.select_long_click.as_ref().unwrap())(menu, idx)
}
// pebble-rust forwards `(new_index, old_index)`; the public order is `(old, new)`.
fn t_selection_changed<R: 'static>(menu: MenuLayerRef, mc: &MenuCtx<R>, new_idx: MenuIndex, old_idx: MenuIndex) {
    (mc.cbs.selection_changed.as_ref().unwrap())(menu, old_idx, new_idx)
}

// ── ContentIndicatorLayer ────────────────────────────────────────────────────────
//
// Wraps a (non-layer) `ContentIndicator` so it can be treated as one layer. A root
// layer hosts two thin strips (top/bottom); a single indicator paints an arrow into
// each, driven by a `State<bool>` per direction (read at `render`).

const CONTENT_INDICATOR_STRIP_HEIGHT: i16 = 18;

pub struct ContentIndicatorLayer {
    root:       Layer,
    _up_host:   Layer,
    _down_host: Layer,
    indicator:  ContentIndicator,
    up:         State<bool>,
    down:       State<bool>,
}

impl ContentIndicatorLayer {
    pub fn new(window_bounds: GRect, up: impl Into<State<bool>>, down: impl Into<State<bool>>) -> Self {
        let root = Layer::new(window_bounds);
        let w = window_bounds.size.w;
        let up_host = Layer::new(GRect::new(0, 0, w, CONTENT_INDICATOR_STRIP_HEIGHT));
        let down_host = Layer::new(GRect::new(0, window_bounds.size.h - CONTENT_INDICATOR_STRIP_HEIGHT, w, CONTENT_INDICATOR_STRIP_HEIGHT));
        root.add_child(&up_host);
        root.add_child(&down_host);

        let indicator = ContentIndicator::new();
        indicator.configure_direction(ContentIndicatorDirection::Up, &up_host, false, GAlign::Center, GColor::Black, GColor::Clear);
        indicator.configure_direction(ContentIndicatorDirection::Down, &down_host, false, GAlign::Center, GColor::Black, GColor::Clear);

        let me = ContentIndicatorLayer {
            root, _up_host: up_host, _down_host: down_host, indicator,
            up: up.into(), down: down.into(),
        };
        me.render();
        me
    }

    pub fn render(&self) {
        let a = self.up.with(|&a| a);
        let b = self.down.with(|&b| b);
        self.indicator.set_content_available(ContentIndicatorDirection::Up, a);
        self.indicator.set_content_available(ContentIndicatorDirection::Down, b);
    }
}

impl AsLayer for ContentIndicatorLayer {
    fn as_raw(&self) -> *mut RawLayer {
        self.root.as_raw()
    }
}

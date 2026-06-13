#![no_std]
#![no_builtins]

extern crate alloc;
extern crate pebble_rust as pebble;

// Taconite — reactive screen framework for Pebble apps built with pebble-rust.
//
// Each screen is a zero-size type implementing `ScreenFns`.  At runtime, state
// lives in a single `Rc<RefCell<State>>` owned by a typed `ScreenCtx<State>`,
// and layers read it (by reference, at paint time) through that context.  A
// small router table maps window IDs to bundles so AppMessages can be delivered
// to the correct screen even when it is not at the front of the stack.

pub mod cell;
pub mod layer;

use alloc::boxed::Box;
use alloc::rc::Rc;
use core::cell::Cell;
use cell::ReCell;

use pebble::{window, WindowPtr};
use pebble::app_message::{AppMessage, AppMessageDict, Dictionary};
use pebble::types::{DictPtr, VoidPtr};
use pebble::layer::Layer;
use pebble::click::{ClickCallbacks, WindowClickHandler};

/// State shared between a screen and its layers. Cloning is a cheap refcount bump.
pub type Shared<S> = Rc<ReCell<S>>;

/// Message keys reserved by taconite. Include all of these in your app's
/// `messageKeys` in `package.json` with the exact numeric values shown.
///
/// - `WindowId`  = 0x5441434F (1413563215) — `"taconite_window_id"`
/// - `ItemIndex` = 0x54414350 (1413563216) — `"taconite_item_index"`
/// - `ItemTotal` = 0x54414351 (1413563217) — `"taconite_item_total"`
///
/// A phone-side message with `WindowId = 0` is the phone-ready sentinel that
/// triggers `on_messaging_initialized` on all active screens.
pub enum TaconiteMessageKey {
    WindowId  = 0x5441_434F,
    ItemIndex = 0x5441_4350,
    ItemTotal = 0x5441_4351,
}

// ── ScreenCtx — typed, handed to every screen method ───────────────────────────

/// The per-screen context. Holds the shared state and the window's layers, and
/// is what every `ScreenFns` method receives. App code sees only `&S` / `&mut S`
/// through it — `Rc`/`RefCell`/raw pointers never surface.
pub struct ScreenCtx<S> {
    state:         Shared<S>,
    pub window_id: u8,
    root:          Layer,
    window:        window::Window,
    layers_ptr:    Cell<*const ()>,                                   // type-erased Layers, set after build
    view_fn:       fn(&S, *const ()),                                 // monomorphized -> Sc::view
    click:         ReCell<Option<WindowClickHandler<ScreenCtx<S>>>>, // kept alive for the screen's life
}

impl<S> ScreenCtx<S> {
    /// A cheap shared handle to the state, for layer wrappers.
    pub fn shared(&self) -> Shared<S> { self.state.clone() }

    pub fn root(&self) -> &Layer { &self.root }
    pub fn window(&self) -> &window::Window { &self.window }

    /// Read the state.
    pub fn with<R>(&self, f: impl FnOnce(&S) -> R) -> R {
        f(&self.state.borrow())
    }

    /// The window's layers, if they have been built yet.
    pub fn layers<L>(&self) -> Option<&L> {
        let lp = self.layers_ptr.get();
        if lp.is_null() { None } else { Some(unsafe { &*(lp as *const L) }) }
    }

    /// Mutate the state with `f`; if `f` returns `true`, re-render via `view`.
    /// The `bool` gates re-render so a screen doesn't flicker through partially
    /// assembled state (e.g. a list arriving over several AppMessages).
    pub fn update(&self, f: impl FnOnce(&mut S) -> bool) {
        let rerender = f(&mut self.state.borrow_mut());   // borrow_mut dropped at the `;`
        if rerender {
            let lp = self.layers_ptr.get();
            if !lp.is_null() {
                (self.view_fn)(&self.state.borrow(), lp);  // safe to borrow() now
            }
        }
    }

    /// Wire up click handlers for this screen. Call from `configure_clicks`.
    /// Callbacks receive `&mut ScreenCtx<S>` and typically call `ctx.update(...)`.
    pub fn set_clicks(&self, callbacks: ClickCallbacks<ScreenCtx<S>>) -> WindowClickHandler<ScreenCtx<S>> {
        let ptr = self as *const ScreenCtx<S> as *mut ScreenCtx<S>;
        self.window.set_click_handlers(ptr, callbacks)
    }
}

// ── Public trait ───────────────────────────────────────────────────────────────

pub trait ScreenFns {
    type State: 'static;
    type Layers;

    /// Create all layers, add them to the window's root layer, and return the
    /// `Layers` struct. Build layers with `taconite::layer::*` constructors,
    /// passing `ctx` so they can read state at paint time.
    fn create_window(ctx: &ScreenCtx<Self::State>) -> Self::Layers;

    /// Push current state into the layers. Called after every `ctx.update(...)`
    /// that returns `true`, and once on load.
    fn view(state: &Self::State, layers: &Self::Layers);

    /// Called when an AppMessage arrives for this screen's window ID.
    fn on_message(ctx: &ScreenCtx<Self::State>, dict: &AppMessageDict);

    fn on_create(_ctx: &ScreenCtx<Self::State>) {}
    /// Called once the phone signals it is ready (via the `taconite_window_id: 0`
    /// sentinel) — or immediately on load if the phone was already ready. Use it
    /// for sending subscribe/init messages to the phone.
    fn on_messaging_initialized(_ctx: &ScreenCtx<Self::State>) {}
    fn on_appear(_ctx: &ScreenCtx<Self::State>) {}
    fn on_disappear(_ctx: &ScreenCtx<Self::State>) {}
    fn on_drop(_ctx: &ScreenCtx<Self::State>) {}
    fn configure_clicks(_ctx: &ScreenCtx<Self::State>) -> Option<WindowClickHandler<ScreenCtx<Self::State>>> { None }
}

// ── Internal bundle stored in window user data (type-erased over the screen) ────

struct ScreenBundle {
    ctx_ptr:                  *mut (),   // Box<ScreenCtx<Sc::State>>
    window_id:                u8,
    on_load:                  fn(*mut (), WindowPtr),
    on_message:               fn(*mut (), &AppMessageDict),
    on_messaging_initialized: fn(*mut ()),
    on_appear:                fn(*mut ()),
    on_disappear:             fn(*mut ()),
    on_drop:                  fn(*mut ()),
    configure_clicks:         fn(*mut ()),
}

// ── Router ──────────────────────────────────────────────────────────────────────

struct RouterEntry {
    window_id:  u8,
    bundle_ptr: *mut ScreenBundle,
}

static mut ROUTER: alloc::vec::Vec<RouterEntry> = alloc::vec::Vec::new();
static mut WINDOW_ID_COUNTER: u8 = 0;
static mut MESSAGING_INITIALIZED: bool = false;

fn next_window_id() -> u8 {
    unsafe {
        WINDOW_ID_COUNTER = WINDOW_ID_COUNTER.wrapping_add(1);
        WINDOW_ID_COUNTER
    }
}

fn router_register(window_id: u8, bundle_ptr: *mut ScreenBundle) {
    unsafe {
        (*core::ptr::addr_of_mut!(ROUTER)).push(RouterEntry { window_id, bundle_ptr });
    }
}

fn router_unregister(window_id: u8) {
    unsafe {
        (*core::ptr::addr_of_mut!(ROUTER)).retain(|e| e.window_id != window_id);
    }
}

// ── Public API ──────────────────────────────────────────────────────────────────

/// Push a new screen onto the Pebble window stack with the given initial state.
pub fn push_screen<Sc: ScreenFns>(initial_state: Sc::State, animate: bool) {
    let window_id = next_window_id();
    let win  = window::Window::new();
    let root = win.get_root_layer();

    let ctx = Box::new(ScreenCtx::<Sc::State> {
        state:      Rc::new(ReCell::new(initial_state)),
        window_id,
        root,
        window:     window::Window::from_raw(win.raw()),
        layers_ptr: Cell::new(core::ptr::null()),
        view_fn:    view_trampoline::<Sc>,
        click:      ReCell::new(None),
    });
    let ctx_ptr = Box::into_raw(ctx) as *mut ();

    let bundle = Box::new(ScreenBundle {
        ctx_ptr,
        window_id,
        on_load:                  on_load_trampoline::<Sc>,
        on_message:               on_message_trampoline::<Sc>,
        on_messaging_initialized: on_messaging_initialized_trampoline::<Sc>,
        on_appear:                on_appear_trampoline::<Sc>,
        on_disappear:             on_disappear_trampoline::<Sc>,
        on_drop:                  on_drop_trampoline::<Sc>,
        configure_clicks:         configure_clicks_trampoline::<Sc>,
    });
    let bundle_ptr = Box::into_raw(bundle);

    router_register(window_id, bundle_ptr);

    win.set_user_data(bundle_ptr);
    win.set_handlers(pebble::window::WindowHandlers {
        load:      pebble_ui_load,
        unload:    pebble_ui_unload,
        appear:    pebble_ui_appear,
        disappear: pebble_ui_disappear,
    });
    win.push(animate);
}

/// Global AppMessage inbox handler — register this with `AppMessage::register_inbox`.
///
/// A message with `taconite_window_id: 0` is the phone-ready sentinel: it sets the
/// `MESSAGING_INITIALIZED` flag and calls `on_messaging_initialized` on every active
/// screen. All other messages are routed to the screen whose window ID matches.
pub extern "C" fn message_received(dict_ptr: DictPtr, _ctx: VoidPtr) {
    let dict = AppMessageDict::from_raw(dict_ptr);
    let window_id = match dict.find_i32(TaconiteMessageKey::WindowId as u32) {
        Some(id) => id as u8,
        None => return,
    };

    if window_id == 0 {
        unsafe {
            MESSAGING_INITIALIZED = true;
            for e in &mut *core::ptr::addr_of_mut!(ROUTER) {
                let bundle = &mut *e.bundle_ptr;
                (bundle.on_messaging_initialized)(bundle.ctx_ptr);
            }
        }
        return;
    }

    unsafe {
        for e in &mut *core::ptr::addr_of_mut!(ROUTER) {
            if e.window_id == window_id {
                let bundle = &mut *e.bundle_ptr;
                (bundle.on_message)(bundle.ctx_ptr, &dict);
                return;
            }
        }
    }
}

/// Receive a paged list over AppMessage.
///
/// Builds `vec` incrementally as chunks arrive. Returns `true` when the final
/// item has been received (or when the list is empty) — return that value from
/// your `ctx.update(...)` closure so the screen re-renders only once complete.
pub fn handle_list_message<T>(
    vec: &mut alloc::vec::Vec<Option<T>>,
    dict: &AppMessageDict,
    parse_item: impl Fn(&AppMessageDict) -> T,
) -> bool {
    let index = dict.find_i32(TaconiteMessageKey::ItemIndex as u32);
    let total = dict.find_i32(TaconiteMessageKey::ItemTotal as u32).unwrap_or(0);

    if total == 0 || index.is_none() || (index.unwrap() >= total) {
        vec.clear();
        return true;
    }

    let index = index.unwrap();
    let item = parse_item(dict);

    if index == 0 {
        vec.clear();
        vec.resize_with(total as usize, || None);
    }
    if let Some(slot) = vec.get_mut(index as usize) {
        *slot = Some(item);
    }

    index + 1 >= total
}

/// Send a small AppMessage from watch to phone (key-value pairs of i32).
pub fn send_message(key_values: &[(u32, i32)]) {
    let mut dict = Dictionary::new();
    AppMessage::init_write(&mut dict);
    for &(key, val) in key_values {
        dict.write_int(key, val);
    }
    AppMessage::send();
}

// ── Non-generic Pebble window callbacks ──────────────────────────────────────────

extern "C" fn pebble_ui_load(window_ptr: WindowPtr) {
    let win    = window::Window::from_raw(window_ptr);
    let bundle = unsafe { &mut *win.get_user_data::<ScreenBundle>() };
    (bundle.on_load)(bundle.ctx_ptr, window_ptr);
    (bundle.configure_clicks)(bundle.ctx_ptr);
}

extern "C" fn pebble_ui_unload(window_ptr: WindowPtr) {
    let win        = window::Window::from_raw(window_ptr);
    let bundle_ptr = win.get_user_data::<ScreenBundle>();
    let bundle     = unsafe { &mut *bundle_ptr };

    router_unregister(bundle.window_id);
    (bundle.on_drop)(bundle.ctx_ptr);   // runs on_drop, frees layers + the ScreenCtx box
    unsafe { drop(Box::from_raw(bundle_ptr)); }
}

extern "C" fn pebble_ui_appear(window_ptr: WindowPtr) {
    let win    = window::Window::from_raw(window_ptr);
    let bundle = unsafe { &mut *win.get_user_data::<ScreenBundle>() };
    (bundle.on_appear)(bundle.ctx_ptr);
}

extern "C" fn pebble_ui_disappear(window_ptr: WindowPtr) {
    let win    = window::Window::from_raw(window_ptr);
    let bundle = unsafe { &mut *win.get_user_data::<ScreenBundle>() };
    (bundle.on_disappear)(bundle.ctx_ptr);
}

// ── Type-erased trampolines (monomorphized per screen type) ──────────────────────

fn ctx_ref<'a, Sc: ScreenFns>(ctx_ptr: *mut ()) -> &'a ScreenCtx<Sc::State> {
    unsafe { &*(ctx_ptr as *const ScreenCtx<Sc::State>) }
}

fn view_trampoline<Sc: ScreenFns>(state: &Sc::State, layers_ptr: *const ()) {
    let layers = unsafe { &*(layers_ptr as *const Sc::Layers) };
    Sc::view(state, layers);
}

fn on_load_trampoline<Sc: ScreenFns>(ctx_ptr: *mut (), _window_ptr: WindowPtr) {
    let ctx = ctx_ref::<Sc>(ctx_ptr);
    let layers = Sc::create_window(ctx);
    ctx.layers_ptr.set(Box::into_raw(Box::new(layers)) as *const ());
    // initial render
    Sc::view(&ctx.state.borrow(), unsafe { &*(ctx.layers_ptr.get() as *const Sc::Layers) });
    Sc::on_create(ctx);
    if unsafe { *core::ptr::addr_of!(MESSAGING_INITIALIZED) } {
        Sc::on_messaging_initialized(ctx);
    }
}

fn on_message_trampoline<Sc: ScreenFns>(ctx_ptr: *mut (), dict: &AppMessageDict) {
    Sc::on_message(ctx_ref::<Sc>(ctx_ptr), dict);
}

fn on_messaging_initialized_trampoline<Sc: ScreenFns>(ctx_ptr: *mut ()) {
    Sc::on_messaging_initialized(ctx_ref::<Sc>(ctx_ptr));
}

fn on_appear_trampoline<Sc: ScreenFns>(ctx_ptr: *mut ()) {
    Sc::on_appear(ctx_ref::<Sc>(ctx_ptr));
}

fn on_disappear_trampoline<Sc: ScreenFns>(ctx_ptr: *mut ()) {
    Sc::on_disappear(ctx_ref::<Sc>(ctx_ptr));
}

fn configure_clicks_trampoline<Sc: ScreenFns>(ctx_ptr: *mut ()) {
    let ctx = ctx_ref::<Sc>(ctx_ptr);
    let handler = Sc::configure_clicks(ctx);
    *ctx.click.borrow_mut() = handler;
}

fn on_drop_trampoline<Sc: ScreenFns>(ctx_ptr: *mut ()) {
    let ctx = ctx_ref::<Sc>(ctx_ptr);
    Sc::on_drop(ctx);
    let lp = ctx.layers_ptr.get();
    if !lp.is_null() {
        unsafe { drop(Box::from_raw(lp as *mut Sc::Layers)); }
        ctx.layers_ptr.set(core::ptr::null());
    }
    // free the ScreenCtx box itself (drops state Rc + click handler)
    unsafe { drop(Box::from_raw(ctx_ptr as *mut ScreenCtx<Sc::State>)); }
}

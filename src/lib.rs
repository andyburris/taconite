#![no_std]
#![no_builtins]
#![feature(associated_type_defaults)]

extern crate alloc;
#[macro_use]
extern crate pebble_rust as pebble;

// Taconite — reactive screen framework for Pebble apps built with pebble-rust.
//
// Each screen is a zero-size type implementing `ScreenFns`.  At runtime, state
// lives in a single `Rc<RefCell<State>>` owned by a typed `ScreenCtx<State>`,
// and layers read it (by reference, at paint time) through that context.  A
// small router table maps window IDs to bundles so AppMessages can be delivered
// to the correct screen even when it is not at the front of the stack.

pub mod layer;
pub mod state;

pub use state::State;

use alloc::boxed::Box;
use alloc::rc::Rc;
use core::cell::{Cell, RefCell};
use core::ops::Deref;

use pebble::{window, WindowPtr};
use pebble::app_message::{AppMessage, AppMessageDict, Dictionary};
use pebble::types::{DictPtr, VoidPtr};
use pebble::layer::Layer;
use pebble::click::{ClickCallbacks, WindowClickHandler};

/// State shared between a screen and its layers. Cloning is a cheap refcount bump.
pub type Shared<S> = Rc<RefCell<S>>;

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
    WindowId = 0x5441_434F,
    WindowType = 0x5441_4350,
    ItemIndex = 0x5441_4351,
    ItemTotal = 0x5441_4352,
    SubscriptionEvent = 0x5441_4353,
}

pub enum SubscriptionEvent {
    Subscribe = 0,
    Unsubscribe = 1,
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
    click:         RefCell<Option<WindowClickHandler<ScreenCtx<S>>>>, // kept alive for the screen's life
    handle:        RefCell<Option<State<S>>>,                         // cached root `State`, built on first `state()`
}

impl<S: 'static> ScreenCtx<S> {
    /// A cheap shared handle to the state, for layer wrappers.
    #[deprecated(note = "use `state()`; layers should take a `State<S>`")]
    pub fn shared(&self) -> Shared<S> { self.state.clone() }

    pub fn root(&self) -> &Layer { &self.root }
    pub fn window(&self) -> &window::Window { &self.window }

    /// The reactive handle to this screen's state — the currency every layer and
    /// writable callback takes. Reads at paint time, `update`s with a repaint, and
    /// `split_field`s into sub-states. Built once and cached (cloning is cheap).
    pub fn state(&self) -> State<S> {
        let mut cached = self.handle.borrow_mut();
        if let Some(handle) = cached.as_ref() {
            return handle.clone();
        }
        // `rerender` re-runs `view` against current state. It reads `layers_ptr`
        // *lazily* (via a raw pointer into this boxed, stable `ScreenCtx`), so it
        // works even though `layers_ptr` is still null while `create_window` runs;
        // and it captures only the state cell (no `Rc` back to the ctx → no cycle).
        let cell      = self.state.clone();
        let cell_view = cell.clone();
        let view_fn   = self.view_fn;
        let lp: *const Cell<*const ()> = &self.layers_ptr;
        let rerender: Rc<dyn Fn()> = Rc::new(move || {
            let layers = unsafe { (*lp).get() };
            if !layers.is_null() {
                view_fn(&cell_view.borrow(), layers);
            }
        });
        let handle = State::root(cell, rerender);
        *cached = Some(handle.clone());
        handle
    }

    /// Read the state.
    #[deprecated(note = "use `state().with(..)` or `state().read()`")]
    pub fn with<R>(&self, f: impl FnOnce(&S) -> R) -> R {
        f(&self.state.borrow())
    }

    /// The window's layers, if they have been built yet.
    pub fn layers<L>(&self) -> Option<&L> {
        let lp = self.layers_ptr.get();
        if lp.is_null() { None } else { Some(unsafe { &*(lp as *const L) }) }
    }

    /// Mutate the state with `f` and re-render — a thin delegator to
    /// `state().update(..)`, so there's a single update+repaint path.
    ///
    /// (Data arriving over several AppMessages should accumulate via
    /// `ScreenMessageCtx::update_temp` and land in one `commit`, so the rendered
    /// state is never caught half-assembled — see `ScreenMessageCtx`.)
    #[deprecated(note = "use `state().update(..)`")]
    pub fn update(&self, f: impl FnOnce(&mut S)) {
        self.state().update(f);
    }

    /// Wire up click handlers for this screen. Call from `configure_clicks`.
    /// Callbacks receive `&mut ScreenCtx<S>` and typically call `ctx.update(...)`.
    pub fn set_clicks(&self, callbacks: ClickCallbacks<ScreenCtx<S>>) -> WindowClickHandler<ScreenCtx<S>> {
        let ptr = self as *const ScreenCtx<S> as *mut ScreenCtx<S>;
        self.window.set_click_handlers(ptr, callbacks)
    }
}

// ── ScreenMessageCtx — handed to on_message only ───────────────────────────────

/// The context `on_message` receives. Derefs to `ScreenCtx<S>` for all the usual
/// reads/updates, and adds a staging buffer (`TempState`) that nothing draws from,
/// so data arriving over several AppMessages is assembled off-screen and applied in
/// one atomic `commit` — the rendered `State` is never caught half-built.
pub struct ScreenMessageCtx<'a, S, T> {
    ctx:  &'a ScreenCtx<S>,
    temp: &'a RefCell<T>,
}

impl<S, T> Deref for ScreenMessageCtx<'_, S, T> {
    type Target = ScreenCtx<S>;
    fn deref(&self) -> &ScreenCtx<S> { self.ctx }
}

impl<S, T: Default> ScreenMessageCtx<'_, S, T> {
    /// Accumulate into the off-screen staging buffer. Never re-renders (nothing
    /// draws from `TempState`). Returns `f`'s value — e.g. the completeness `bool`
    /// from `handle_list_message`, so you can decide when to `commit`.
    ///
    /// (Cleanup-pending, like `update`/`with`: the intended end state is `temp`
    /// becoming a `State<T>` and this a `State::mutate`. Not attributed
    /// `#[deprecated]` yet because no `State`-based replacement exists — temp is
    /// still a plain `RefCell`. `commit` stays first-class regardless.)
    pub fn update_temp<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        f(&mut self.temp.borrow_mut())
    }

    /// Atomically fold the staging buffer into the rendered `State`, then re-render
    /// once. `f` gets `&mut T` so you can `mem::take` / `Option::take` buffers
    /// straight into `State` (no clone). The buffer is reset to `T::default()`
    /// afterwards. Returns `f`'s value.
    pub fn commit<R>(&self, f: impl FnOnce(&mut S, &mut T) -> R) -> R {
        let r = f(&mut self.ctx.state.borrow_mut(), &mut self.temp.borrow_mut()); // both borrows dropped at `;`
        *self.temp.borrow_mut() = T::default();
        let lp = self.ctx.layers_ptr.get();
        if !lp.is_null() {
            (self.ctx.view_fn)(&self.ctx.state.borrow(), lp);
        }
        r
    }
}

// ── Public trait ───────────────────────────────────────────────────────────────

pub trait ScreenFns {
    type State: 'static;
    /// Off-screen staging buffer for multi-message loads (default `()` — opt in only
    /// when a screen assembles data over several AppMessages). See `ScreenMessageCtx`.
    type TempState: Default + 'static = ();
    type Layers;

    /// Create all layers, add them to the window's root layer, and return the
    /// `Layers` struct. Build layers with `taconite::layer::*` constructors,
    /// passing `ctx` so they can read state at paint time.
    fn create_window(ctx: &ScreenCtx<Self::State>) -> Self::Layers;

    /// Push current state into the layers. Called after every `ctx.update(...)`
    /// that returns `true`, and once on load.
    fn view(state: &Self::State, layers: &Self::Layers);

    /// Called when an AppMessage arrives for this screen's window ID. The context
    /// derefs to `ScreenCtx` and adds `update_temp` / `commit` for staged loads.
    fn on_message(ctx: &ScreenMessageCtx<Self::State, Self::TempState>, dict: &AppMessageDict);

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
    temp_ptr:                 *mut (),   // Box<RefCell<Sc::TempState>>
    window_id:                u8,
    on_load:                  fn(*mut (), WindowPtr),
    on_message:               fn(*mut (), *mut (), &AppMessageDict),
    on_messaging_initialized: fn(*mut ()),
    on_appear:                fn(*mut ()),
    on_disappear:             fn(*mut ()),
    on_drop:                  fn(*mut (), *mut ()),
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
        state:      Rc::new(RefCell::new(initial_state)),
        window_id,
        root,
        window:     window::Window::from_raw(win.raw()),
        layers_ptr: Cell::new(core::ptr::null()),
        view_fn:    view_trampoline::<Sc>,
        click:      RefCell::new(None),
        handle:     RefCell::new(None),
    });
    let ctx_ptr = Box::into_raw(ctx) as *mut ();

    let temp_ptr = Box::into_raw(Box::new(RefCell::new(Sc::TempState::default()))) as *mut ();

    let bundle = Box::new(ScreenBundle {
        ctx_ptr,
        temp_ptr,
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
                (bundle.on_message)(bundle.ctx_ptr, bundle.temp_ptr, &dict);
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

    vec.iter().all(|o| o.is_some())
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
    (bundle.on_drop)(bundle.ctx_ptr, bundle.temp_ptr);   // on_drop, frees layers + ctx + temp boxes
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

fn on_message_trampoline<Sc: ScreenFns>(ctx_ptr: *mut (), temp_ptr: *mut (), dict: &AppMessageDict) {
    let ctx  = ctx_ref::<Sc>(ctx_ptr);
    let temp = unsafe { &*(temp_ptr as *const RefCell<Sc::TempState>) };
    let mctx = ScreenMessageCtx { ctx, temp };
    Sc::on_message(&mctx, dict);
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

fn on_drop_trampoline<Sc: ScreenFns>(ctx_ptr: *mut (), temp_ptr: *mut ()) {
    let ctx = ctx_ref::<Sc>(ctx_ptr);
    Sc::on_drop(ctx);
    let lp = ctx.layers_ptr.get();
    if !lp.is_null() {
        unsafe { drop(Box::from_raw(lp as *mut Sc::Layers)); }
        ctx.layers_ptr.set(core::ptr::null());
    }
    // free the ScreenCtx box (drops state Rc + click handler) and the TempState box
    unsafe { drop(Box::from_raw(ctx_ptr as *mut ScreenCtx<Sc::State>)); }
    unsafe { drop(Box::from_raw(temp_ptr as *mut RefCell<Sc::TempState>)); }
}

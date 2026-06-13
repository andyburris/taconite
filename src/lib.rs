#![no_std]
#![no_builtins]

extern crate alloc;
extern crate pebble_rust as pebble;

// Taconite — reactive screen framework for Pebble apps built with pebble-rust.
//
// Each screen is a zero-size type implementing `ScreenFns`.  At runtime, state
// and layers live in heap allocations tracked by a `ScreenBundle` stored in
// the Pebble window's user-data slot.  A small router table maps window IDs
// to bundles so AppMessages can be delivered to the correct screen even when
// it is not at the front of the stack.

use alloc::boxed::Box;
use pebble::{window, WindowPtr};
use pebble::app_message::{AppMessage, AppMessageDict, Dictionary};
use pebble::types::{DictPtr, VoidPtr};
use pebble::layer::{Layer, ILayer};
use pebble::click::WindowClickHandler;

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

// ── Public trait ─────────────────────────────────────────────────────────────

pub trait ScreenFns {
    type State;
    type Layers;

    /// Called inside Pebble's load handler. Create all layers, add them to
    /// the window's root layer, and return the `Layers` struct. The `handle`
    /// pointer can be stored for use in click-handler context pointers.
    fn create_window(window: &window::Window, handle: *mut ScreenHandle) -> Self::Layers;

    /// Pure description of the screen given the current state. Called
    /// automatically after every `handle.update(...)` call.
    fn view(state: &Self::State, layers: &Self::Layers);

    /// Called when an AppMessage arrives for this screen's window ID.
    /// Mutate state by calling `handle.update(|s| { ... })`.
    fn on_message(handle: &mut ScreenHandle, dict: &AppMessageDict);

    fn on_create(_handle: &mut ScreenHandle) {}
    /// Called after the window is fully loaded and rendered, once the phone signals
    /// it is ready to receive messages (via `taconite_window_id: 0` sentinel). If
    /// the phone is already ready when this screen is pushed, it fires immediately.
    /// Use this for sending subscribe/init messages to the phone; use `on_create`
    /// for one-time setup like timers and clock subscriptions.
    fn on_messaging_initialized(_handle: &mut ScreenHandle) {}
    fn on_appear(_handle: &mut ScreenHandle) {}
    fn on_disappear(_handle: &mut ScreenHandle) {}
    fn on_drop(_handle: &mut ScreenHandle) {}
    fn configure_clicks(_win: &window::Window, _handle: *mut ScreenHandle) -> Option<WindowClickHandler<ScreenHandle>> { None }
}

// ── ScreenHandle ─────────────────────────────────────────────────────────────

pub struct ScreenHandle {
    pub window_id:             u8,
    pub(crate) state_ptr:      *mut (),
    pub(crate) layers_ptr:     *mut (),
    pub(crate) root_layer_ptr: *mut pebble::RawLayer,
    pub(crate) window_ptr:     WindowPtr,
    view_fn: fn(*const (), *const ()),
}

impl ScreenHandle {
    pub fn state<S>(&self) -> &S {
        unsafe { &*(self.state_ptr as *const S) }
    }

    pub fn layers<L>(&self) -> Option<&L> {
        if self.layers_ptr.is_null() { None }
        else { Some(unsafe { &*(self.layers_ptr as *const L) }) }
    }

    pub fn root_layer(&self) -> Layer {
        Layer::from_raw(self.root_layer_ptr)
    }

    pub fn window(&self) -> window::Window {
        window::Window::from_raw(self.window_ptr)
    }

    /// Mutate state with `f`, then automatically re-render.
    pub fn update<S, F: FnOnce(&mut S) -> bool>(&mut self, f: F) {
        let rerender = unsafe { f(&mut *(self.state_ptr as *mut S)) };
        if rerender && !self.layers_ptr.is_null() {
            (self.view_fn)(self.state_ptr as *const (), self.layers_ptr as *const ());
        }
    }
}

// ── Internal bundle stored in window user data ────────────────────────────────

struct ScreenBundle {
    handle:                   ScreenHandle,
    window_id:                u8,
    on_load:                  fn(&mut ScreenHandle, *mut ()),
    on_message:               fn(&mut ScreenHandle, &AppMessageDict),
    on_messaging_initialized: fn(&mut ScreenHandle),
    on_appear:                fn(&mut ScreenHandle),
    on_disappear:             fn(&mut ScreenHandle),
    on_drop:                  fn(&mut ScreenHandle),
    configure_clicks:         fn(&window::Window, *mut ScreenHandle) -> Option<WindowClickHandler<ScreenHandle>>,
    click_handler:            Option<WindowClickHandler<ScreenHandle>>,
}

// ── Router ────────────────────────────────────────────────────────────────────

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

// ── Public API ────────────────────────────────────────────────────────────────

/// Push a new screen onto the Pebble window stack with the given initial state.
pub fn push_screen<S: ScreenFns>(initial_state: S::State, animate: bool) {
    let state_ptr = Box::into_raw(Box::new(initial_state)) as *mut ();
    let window_id = next_window_id();
    let win = window::Window::new();
    let root_layer_ptr = win.get_root_layer().get_internal();

    let handle = ScreenHandle {
        window_id,
        state_ptr,
        layers_ptr: core::ptr::null_mut(),
        root_layer_ptr,
        window_ptr: win.raw(),
        view_fn: view_trampoline::<S>,
    };

    let bundle = Box::new(ScreenBundle {
        handle,
        window_id,
        on_load:                  on_load_trampoline::<S>,
        on_message:               on_message_trampoline::<S>,
        on_messaging_initialized: on_messaging_initialized_trampoline::<S>,
        on_appear:                on_appear_trampoline::<S>,
        on_disappear:             on_disappear_trampoline::<S>,
        on_drop:                  on_drop_trampoline::<S>,
        configure_clicks:         configure_clicks_trampoline::<S>,
        click_handler:            None,
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
                (bundle.on_messaging_initialized)(&mut bundle.handle);
            }
        }
        return;
    }

    unsafe {
        for e in &mut *core::ptr::addr_of_mut!(ROUTER) {
            if e.window_id == window_id {
                let bundle = &mut *e.bundle_ptr;
                (bundle.on_message)(&mut bundle.handle, &dict);
                return;
            }
        }
    }
}

/// Receive a paged list over AppMessage.
///
/// Builds `vec` incrementally as chunks arrive. Returns `true` when the final
/// item has been received (or when the list is empty). Callers should re-render
/// only on `true`.
///
/// Typical usage inside `on_message`:
/// ```ignore
/// if taconite::handle_list_message(&mut state.items, dict, |d| MyItem::parse(d)) {
///     // list is complete
/// }
/// ```
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

// ── Non-generic Pebble window callbacks ───────────────────────────────────────

extern "C" fn pebble_ui_load(window_ptr: WindowPtr) {
    let win    = window::Window::from_raw(window_ptr);
    let bundle = unsafe { &mut *win.get_user_data::<ScreenBundle>() };
    (bundle.on_load)(&mut bundle.handle, window_ptr as *mut ());
    let handle_ptr: *mut ScreenHandle = &mut bundle.handle;
    bundle.click_handler = (bundle.configure_clicks)(&win, handle_ptr);
}

extern "C" fn pebble_ui_unload(window_ptr: WindowPtr) {
    let win        = window::Window::from_raw(window_ptr);
    let bundle_ptr = win.get_user_data::<ScreenBundle>();
    let bundle     = unsafe { &mut *bundle_ptr };

    router_unregister(bundle.window_id);
    (bundle.on_drop)(&mut bundle.handle);
    unsafe { drop(Box::from_raw(bundle_ptr)); }
}

extern "C" fn pebble_ui_appear(window_ptr: WindowPtr) {
    let win    = window::Window::from_raw(window_ptr);
    let bundle = unsafe { &mut *win.get_user_data::<ScreenBundle>() };
    (bundle.on_appear)(&mut bundle.handle);
}

extern "C" fn pebble_ui_disappear(window_ptr: WindowPtr) {
    let win    = window::Window::from_raw(window_ptr);
    let bundle = unsafe { &mut *win.get_user_data::<ScreenBundle>() };
    (bundle.on_disappear)(&mut bundle.handle);
}

// ── Type-erased trampolines ───────────────────────────────────────────────────

fn view_trampoline<S: ScreenFns>(state_ptr: *const (), layers_ptr: *const ()) {
    let state  = unsafe { &*(state_ptr  as *const S::State) };
    let layers = unsafe { &*(layers_ptr as *const S::Layers) };
    S::view(state, layers);
}

fn on_load_trampoline<S: ScreenFns>(handle: &mut ScreenHandle, window_ptr: *mut ()) {
    let win    = window::Window::from_raw(window_ptr as WindowPtr);
    let layers = S::create_window(&win, handle);
    handle.layers_ptr = Box::into_raw(Box::new(layers)) as *mut ();
    S::on_create(handle);
    // Initial render with default state
    (handle.view_fn)(handle.state_ptr as *const (), handle.layers_ptr as *const ());
    // If the phone is already ready, fire immediately; otherwise wait for the sentinel.
    if unsafe { *core::ptr::addr_of!(MESSAGING_INITIALIZED) } {
        S::on_messaging_initialized(handle);
    }
}

fn on_message_trampoline<S: ScreenFns>(handle: &mut ScreenHandle, dict: &AppMessageDict) {
    S::on_message(handle, dict);
}

fn on_messaging_initialized_trampoline<S: ScreenFns>(handle: &mut ScreenHandle) {
    S::on_messaging_initialized(handle);
}

fn on_appear_trampoline<S: ScreenFns>(handle: &mut ScreenHandle) {
    S::on_appear(handle);
}

fn on_disappear_trampoline<S: ScreenFns>(handle: &mut ScreenHandle) {
    S::on_disappear(handle);
}

fn configure_clicks_trampoline<S: ScreenFns>(win: &window::Window, handle: *mut ScreenHandle) -> Option<WindowClickHandler<ScreenHandle>> {
    S::configure_clicks(win, handle)
}

fn on_drop_trampoline<S: ScreenFns>(handle: &mut ScreenHandle) {
    S::on_drop(handle);
    unsafe {
        if !handle.state_ptr.is_null() {
            drop(Box::from_raw(handle.state_ptr as *mut S::State));
            handle.state_ptr = core::ptr::null_mut();
        }
        if !handle.layers_ptr.is_null() {
            drop(Box::from_raw(handle.layers_ptr as *mut S::Layers));
            handle.layers_ptr = core::ptr::null_mut();
        }
    }
}

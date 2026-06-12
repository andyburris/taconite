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

// The message key taconite reserves for routing AppMessages to the correct
// screen. Apps must include `"taconite_window_id": 0` in their package.json
// messageKeys, and the phone-side TypeScript must include this key in every
// outbound AppMessage sent to the watch.
pub const WINDOW_ID_KEY: u32 = 0;

// ── Public trait ─────────────────────────────────────────────────────────────

pub trait ScreenFns {
    type State: Default;
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
    handle:           ScreenHandle,
    window_id:        u8,
    on_load:          fn(&mut ScreenHandle, *mut ()),
    on_message:       fn(&mut ScreenHandle, &AppMessageDict),
    on_appear:        fn(&mut ScreenHandle),
    on_disappear:     fn(&mut ScreenHandle),
    on_drop:          fn(&mut ScreenHandle),
    configure_clicks: fn(&window::Window, *mut ScreenHandle) -> Option<WindowClickHandler<ScreenHandle>>,
    click_handler:    Option<WindowClickHandler<ScreenHandle>>,
}

// ── Router ────────────────────────────────────────────────────────────────────

struct RouterEntry {
    window_id:  u8,
    bundle_ptr: *mut ScreenBundle,
}

static mut ROUTER: alloc::vec::Vec<RouterEntry> = alloc::vec::Vec::new();
static mut WINDOW_ID_COUNTER: u8 = 0;

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

/// Push a new screen onto the Pebble window stack.
pub fn push_screen<S: ScreenFns>(animate: bool) {
    push_screen_with::<S>(S::State::default(), animate);
}

pub fn push_screen_with<S: ScreenFns>(initial_state: S::State, animate: bool) {
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
        on_load:          on_load_trampoline::<S>,
        on_message:       on_message_trampoline::<S>,
        on_appear:        on_appear_trampoline::<S>,
        on_disappear:     on_disappear_trampoline::<S>,
        on_drop:          on_drop_trampoline::<S>,
        configure_clicks: configure_clicks_trampoline::<S>,
        click_handler:    None,
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
/// Routes incoming messages to the screen identified by `WINDOW_ID_KEY`.
pub extern "C" fn message_received(dict_ptr: DictPtr, _ctx: VoidPtr) {
    let dict = AppMessageDict::from_raw(dict_ptr);
    let window_id = match dict.find_i32(WINDOW_ID_KEY) {
        Some(id) => id as u8,
        None => return,
    };

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
}

fn on_message_trampoline<S: ScreenFns>(handle: &mut ScreenHandle, dict: &AppMessageDict) {
    S::on_message(handle, dict);
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

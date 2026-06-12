#![crate_type = "staticlib"]
#![no_std]
#![no_builtins]

extern crate alloc;
#[macro_use]
extern crate pebble_rust as pebble;
extern crate taconite;

use pebble::app;
use pebble::app_message::{AppMessage, AppMessageDict};
use pebble::layer::{ILayer, TextLayer};
use pebble::std::ToCString;
use pebble::types::{GPoint, GRect, GSize};
use taconite::{ScreenFns, ScreenHandle};

const MESSAGE_KEY_EXAMPLE: u32 = 1768777472;

// ── Screen definition ─────────────────────────────────────────────────────────

pub struct AppMessageScreen;

pub struct AppMessageState {
    text: alloc::ffi::CString,
}

impl Default for AppMessageState {
    fn default() -> Self {
        Self { text: "Loading...".to_cstring() }
    }
}

pub struct AppMessageLayers {
    text_layer: TextLayer,
}

impl ScreenFns for AppMessageScreen {
    type State = AppMessageState;
    type Layers = AppMessageLayers;

    fn create_window(window: &pebble::window::Window, _handle: *mut ScreenHandle) -> Self::Layers {
        let root = window.get_root_layer();
        let bounds = root.get_bounds();
        let text_layer = TextLayer::new(GRect {
            origin: GPoint { x: bounds.size.w / 9, y: bounds.size.h / 2 - 20 },
            size: GSize { w: bounds.size.w, h: 20 },
        });
        root.add_child(&text_layer);
        AppMessageLayers { text_layer }
    }

    fn view(state: &Self::State, layers: &Self::Layers) {
        layers.text_layer.set_text(&state.text);
    }

    fn on_messaging_initialized(handle: &mut ScreenHandle) {
        // Send our window ID to the phone so it knows where to route replies.
        taconite::send_message(&[(taconite::WINDOW_ID_KEY, handle.window_id as i32)]);
    }

    fn on_message(handle: &mut ScreenHandle, dict: &AppMessageDict) {
        if let Some(text) = dict.find_str(MESSAGE_KEY_EXAMPLE) {
            let cstring = text.to_cstring();
            handle.update(|s: &mut AppMessageState| {
                s.text = cstring;
                true
            });
        }
    }
}

// ── App entry point ───────────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub fn main() -> isize {
    AppMessage::open(200, 200);
    AppMessage::register_inbox(taconite::message_received);

    let app = app::App::new();
    taconite::push_screen::<AppMessageScreen>(false);
    app.run_event_loop();

    pbl_log!("Exiting.");
    0
}

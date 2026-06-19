#![crate_type = "staticlib"]
#![no_std]
#![no_builtins]

extern crate alloc;
#[macro_use]
extern crate pebble_rust as pebble;
extern crate taconite;

use pebble::app;
use pebble::app_message::{AppMessage, AppMessageDict};
use pebble::layer::ILayer;
use pebble::std::ToCString;
use pebble::types::{GPoint, GRect, GSize};
use taconite::layer::Text;
use taconite::{ScreenCtx, ScreenFns, ScreenMessageCtx};

const MESSAGE_KEY_EXAMPLE: u32 = 1768777472;

// ── Screen definition ─────────────────────────────────────────────────────────

pub struct AppMessageScreen;

pub struct AppMessageState {
    text: alloc::ffi::CString,
}

pub struct AppMessageLayers {
    text_layer: Text,
}

impl ScreenFns for AppMessageScreen {
    type State = AppMessageState;
    type Layers = AppMessageLayers;

    fn create_window(ctx: &ScreenCtx<AppMessageState>) -> Self::Layers {
        let bounds = ctx.root().get_bounds();
        let text_layer = Text::new(
            GRect {
                origin: GPoint { x: bounds.size.w / 9, y: bounds.size.h / 2 - 20 },
                size: GSize { w: bounds.size.w, h: 20 },
            },
            ctx,
            |s| s.text.as_c_str(),
        );
        ctx.root().add_child(&text_layer);
        AppMessageLayers { text_layer }
    }

    fn view(_state: &Self::State, layers: &Self::Layers) {
        layers.text_layer.render();
    }

    fn on_messaging_initialized(ctx: &ScreenCtx<AppMessageState>) {
        // Send our window ID to the phone so it knows where to route replies.
        taconite::send_message(&[
            (taconite::TaconiteMessageKey::WindowId as u32, ctx.window_id as i32), 
            (taconite::TaconiteMessageKey::WindowType as u32, 0 as i32)
        ]);
    }

    fn on_message(ctx: &ScreenMessageCtx<AppMessageState, ()>, dict: &AppMessageDict) {
        if let Some(text) = dict.find_str(MESSAGE_KEY_EXAMPLE) {
            let cstring = text.to_cstring();
            ctx.update(|s| {
                s.text = cstring;
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
    taconite::push_screen::<AppMessageScreen>(AppMessageState { text: "Loading...".to_cstring() }, false);
    app.run_event_loop();

    pbl_log!("Exiting.");
    0
}

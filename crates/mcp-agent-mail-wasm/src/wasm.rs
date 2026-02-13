//! WASM-specific implementation using wasm-bindgen.
//!
//! This module provides the JavaScript-facing API for the Agent Mail TUI.

use std::{cell::RefCell, rc::Rc};

use wasm_bindgen::prelude::*;
use web_sys::{console, CanvasRenderingContext2d, HtmlCanvasElement, WebSocket};

use crate::{AppConfig, InputEvent, StateSnapshot, SyncState, WsMessage};

// ──────────────────────────────────────────────────────────────────────────────
// Initialization
// ──────────────────────────────────────────────────────────────────────────────

/// Initialize the WASM module.
///
/// Call this once before creating any `AgentMailApp` instances.
#[wasm_bindgen(start)]
pub fn wasm_init() {
    // Set up panic hook for better error messages in browser console
    #[cfg(feature = "console-panic")]
    console_error_panic_hook::set_once();

    console::log_1(&"MCP Agent Mail WASM initialized".into());
}

// ──────────────────────────────────────────────────────────────────────────────
// Main Application
// ──────────────────────────────────────────────────────────────────────────────

/// Agent Mail TUI application for the browser.
///
/// # Example
///
/// ```javascript
/// const app = new AgentMailApp('#canvas', 'ws://localhost:8765/ws');
/// await app.connect();
/// app.start();
/// ```
#[wasm_bindgen]
pub struct AgentMailApp {
    config: AppConfig,
    canvas: Option<HtmlCanvasElement>,
    ctx: Option<CanvasRenderingContext2d>,
    websocket: Option<WebSocket>,
    state: Rc<RefCell<SyncState>>,
    onopen: Option<Closure<dyn FnMut()>>,
    onmessage: Option<Closure<dyn FnMut(web_sys::MessageEvent)>>,
    onerror: Option<Closure<dyn FnMut(web_sys::ErrorEvent)>>,
    onclose: Option<Closure<dyn FnMut(web_sys::CloseEvent)>>,
}

#[wasm_bindgen]
impl AgentMailApp {
    /// Create a new Agent Mail application.
    ///
    /// # Arguments
    ///
    /// * `canvas_selector` - CSS selector for the canvas element (e.g., "#terminal")
    /// * `websocket_url` - WebSocket URL for server connection
    #[wasm_bindgen(constructor)]
    pub fn new(canvas_selector: &str, websocket_url: &str) -> Self {
        Self {
            config: AppConfig {
                canvas_selector: canvas_selector.to_string(),
                websocket_url: websocket_url.to_string(),
                ..AppConfig::default()
            },
            canvas: None,
            ctx: None,
            websocket: None,
            state: Rc::new(RefCell::new(SyncState::default())),
            onopen: None,
            onmessage: None,
            onerror: None,
            onclose: None,
        }
    }

    /// Create with full configuration.
    #[wasm_bindgen]
    pub fn from_config(config_json: &str) -> Result<AgentMailApp, JsValue> {
        let config: AppConfig = serde_json::from_str(config_json)
            .map_err(|e| JsValue::from_str(&format!("Invalid config: {e}")))?;

        Ok(Self {
            config,
            canvas: None,
            ctx: None,
            websocket: None,
            state: Rc::new(RefCell::new(SyncState::default())),
            onopen: None,
            onmessage: None,
            onerror: None,
            onclose: None,
        })
    }

    /// Initialize the canvas and rendering context.
    #[wasm_bindgen]
    pub fn init_canvas(&mut self) -> Result<(), JsValue> {
        let window = web_sys::window().ok_or("No window")?;
        let document = window.document().ok_or("No document")?;

        let element = document
            .query_selector(&self.config.canvas_selector)
            .map_err(|_| "Failed to query selector")?
            .ok_or("Canvas element not found")?;

        let canvas: HtmlCanvasElement =
            element.dyn_into().map_err(|_| "Element is not a canvas")?;

        let ctx: CanvasRenderingContext2d = canvas
            .get_context("2d")
            .map_err(|_| "Failed to get 2d context")?
            .ok_or("No 2d context")?
            .dyn_into()
            .map_err(|_| "Context is not CanvasRenderingContext2d")?;

        // Set canvas size based on terminal dimensions
        let state = self.state.borrow();
        let char_width = self.config.font_size_px as f64 * 0.6;
        let char_height = self.config.font_size_px as f64;
        canvas.set_width((state.cols as f64 * char_width) as u32);
        canvas.set_height((state.rows as f64 * char_height) as u32);

        // Configure font
        let font = format!("{}px monospace", self.config.font_size_px);
        ctx.set_font(&font);

        self.canvas = Some(canvas);
        self.ctx = Some(ctx);

        console::log_1(&"Canvas initialized".into());
        Ok(())
    }

    /// Connect to the Agent Mail server via WebSocket.
    #[wasm_bindgen]
    pub fn connect(&mut self) -> Result<(), JsValue> {
        let ws = WebSocket::new(&self.config.websocket_url)?;
        ws.set_binary_type(web_sys::BinaryType::Arraybuffer);

        // Set up event handlers
        let state_for_open = Rc::clone(&self.state);
        let onopen = Closure::<dyn FnMut()>::new(move || {
            state_for_open.borrow_mut().connected = true;
            console::log_1(&"WebSocket connected".into());
        });

        let state_for_message = Rc::clone(&self.state);
        let ws_for_message = ws.clone();
        let onmessage = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(
            move |event: web_sys::MessageEvent| {
                if let Ok(text) = event.data().dyn_into::<js_sys::JsString>() {
                    let text_str: String = text.into();
                    match serde_json::from_str::<WsMessage>(&text_str) {
                        Ok(message) => {
                            if matches!(message, WsMessage::Ping) {
                                if let Ok(pong) = serde_json::to_string(&WsMessage::Pong) {
                                    let _ = ws_for_message.send_with_str(&pong);
                                }
                            }
                            state_for_message.borrow_mut().apply_message(&message);
                        }
                        Err(error) => {
                            console::warn_1(
                                &format!("Failed to parse WebSocket payload: {error}").into(),
                            );
                        }
                    }
                }
            },
        );

        let onerror =
            Closure::<dyn FnMut(web_sys::ErrorEvent)>::new(|event: web_sys::ErrorEvent| {
                console::error_1(&format!("WebSocket error: {:?}", event.message()).into());
            });

        let state_for_close = Rc::clone(&self.state);
        let onclose =
            Closure::<dyn FnMut(web_sys::CloseEvent)>::new(move |event: web_sys::CloseEvent| {
                state_for_close.borrow_mut().connected = false;
                console::log_1(
                    &format!(
                        "WebSocket closed: code={} reason={}",
                        event.code(),
                        event.reason()
                    )
                    .into(),
                );
            });

        self.onopen = Some(onopen);
        self.onmessage = Some(onmessage);
        self.onerror = Some(onerror);
        self.onclose = Some(onclose);

        if let Some(handler) = self.onopen.as_ref() {
            ws.set_onopen(Some(handler.as_ref().unchecked_ref()));
        }
        if let Some(handler) = self.onmessage.as_ref() {
            ws.set_onmessage(Some(handler.as_ref().unchecked_ref()));
        }
        if let Some(handler) = self.onerror.as_ref() {
            ws.set_onerror(Some(handler.as_ref().unchecked_ref()));
        }
        if let Some(handler) = self.onclose.as_ref() {
            ws.set_onclose(Some(handler.as_ref().unchecked_ref()));
        }

        self.websocket = Some(ws);
        Ok(())
    }

    /// Send an input event to the server.
    #[wasm_bindgen]
    pub fn send_input(&self, key: &str, modifiers: u8) -> Result<(), JsValue> {
        let ws = self.websocket.as_ref().ok_or("Not connected")?;

        let event = WsMessage::Input(InputEvent::Key {
            key: key.to_string(),
            modifiers,
        });

        let json = serde_json::to_string(&event)
            .map_err(|e| JsValue::from_str(&format!("Serialize error: {e}")))?;

        ws.send_with_str(&json)?;
        Ok(())
    }

    /// Send a resize event to the server.
    #[wasm_bindgen]
    pub fn send_resize(&self, cols: u16, rows: u16) -> Result<(), JsValue> {
        let ws = self.websocket.as_ref().ok_or("Not connected")?;

        let event = WsMessage::Resize { cols, rows };
        let json = serde_json::to_string(&event)
            .map_err(|e| JsValue::from_str(&format!("Serialize error: {e}")))?;

        ws.send_with_str(&json)?;
        Ok(())
    }

    /// Render the current state to the canvas.
    #[wasm_bindgen]
    pub fn render(&self) -> Result<(), JsValue> {
        let ctx = self.ctx.as_ref().ok_or("Canvas not initialized")?;
        let canvas = self.canvas.as_ref().ok_or("Canvas not initialized")?;
        let state = self.state.borrow();

        // Clear canvas
        ctx.set_fill_style_str(if self.config.high_contrast {
            "#000000"
        } else {
            "#1a1a2e"
        });
        ctx.fill_rect(0.0, 0.0, canvas.width() as f64, canvas.height() as f64);

        // Render cells (placeholder - actual implementation would decode cell data)
        ctx.set_fill_style_str(if self.config.high_contrast {
            "#ffffff"
        } else {
            "#e0e0e0"
        });

        // Draw cursor
        if state.cursor_visible {
            let char_width = self.config.font_size_px as f64 * 0.6;
            let char_height = self.config.font_size_px as f64;
            let x = state.cursor_x as f64 * char_width;
            let y = state.cursor_y as f64 * char_height;

            ctx.set_fill_style_str("#00ff00");
            ctx.fill_rect(x, y, char_width, char_height);
        }

        Ok(())
    }

    /// Check if connected to the server.
    #[wasm_bindgen(getter)]
    pub fn is_connected(&self) -> bool {
        self.state.borrow().connected
    }

    /// Get current screen ID.
    #[wasm_bindgen(getter)]
    pub fn screen_id(&self) -> u8 {
        self.state.borrow().screen_id
    }

    /// Get current screen title.
    #[wasm_bindgen(getter)]
    pub fn screen_title(&self) -> String {
        self.state.borrow().screen_title.clone()
    }

    /// Get terminal columns.
    #[wasm_bindgen(getter)]
    pub fn cols(&self) -> u16 {
        self.state.borrow().cols
    }

    /// Get terminal rows.
    #[wasm_bindgen(getter)]
    pub fn rows(&self) -> u16 {
        self.state.borrow().rows
    }

    /// Get timestamp from the last server sync event (microseconds).
    #[wasm_bindgen(getter)]
    pub fn last_timestamp_us(&self) -> i64 {
        self.state.borrow().last_timestamp_us
    }

    /// Get number of state-sync messages processed.
    #[wasm_bindgen(getter)]
    pub fn messages_received(&self) -> u64 {
        self.state.borrow().messages_received
    }

    /// Disconnect from the server.
    #[wasm_bindgen]
    pub fn disconnect(&mut self) {
        if let Some(ws) = self.websocket.take() {
            ws.set_onopen(None);
            ws.set_onmessage(None);
            ws.set_onerror(None);
            ws.set_onclose(None);
            let _ = ws.close();
        }
        self.onopen = None;
        self.onmessage = None;
        self.onerror = None;
        self.onclose = None;
        self.state.borrow_mut().connected = false;
        console::log_1(&"Disconnected".into());
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Utility exports
// ──────────────────────────────────────────────────────────────────────────────

/// Get the library version.
#[wasm_bindgen]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Parse a state snapshot from JSON.
#[wasm_bindgen]
pub fn parse_snapshot(json: &str) -> Result<JsValue, JsValue> {
    let snapshot: StateSnapshot =
        serde_json::from_str(json).map_err(|e| JsValue::from_str(&format!("Parse error: {e}")))?;

    serde_wasm_bindgen::to_value(&snapshot)
        .map_err(|e| JsValue::from_str(&format!("Conversion error: {e}")))
}

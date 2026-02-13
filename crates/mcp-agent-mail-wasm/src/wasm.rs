//! WASM-specific implementation using wasm-bindgen.
//!
//! This module provides the JavaScript-facing API for the Agent Mail TUI.

use wasm_bindgen::prelude::*;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement, WebSocket, console};

use crate::{AppConfig, InputEvent, StateSnapshot, WsMessage};

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
    state: AppState,
}

/// Internal application state.
struct AppState {
    connected: bool,
    screen_id: u8,
    cols: u16,
    rows: u16,
    cursor_x: u16,
    cursor_y: u16,
    cursor_visible: bool,
    #[allow(dead_code)] // Reserved for future rendering
    cells: Vec<u32>,
    #[allow(dead_code)] // Reserved for delta tracking
    last_seq: u64,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            connected: false,
            screen_id: 1,
            cols: 80,
            rows: 24,
            cursor_x: 0,
            cursor_y: 0,
            cursor_visible: true,
            cells: vec![0; 80 * 24],
            last_seq: 0,
        }
    }
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
            state: AppState::default(),
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
            state: AppState::default(),
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
        let char_width = self.config.font_size_px as f64 * 0.6;
        let char_height = self.config.font_size_px as f64;
        canvas.set_width((self.state.cols as f64 * char_width) as u32);
        canvas.set_height((self.state.rows as f64 * char_height) as u32);

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
        let onopen = Closure::<dyn FnMut()>::new(|| {
            console::log_1(&"WebSocket connected".into());
        });
        ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));
        onopen.forget();

        let onmessage =
            Closure::<dyn FnMut(web_sys::MessageEvent)>::new(|event: web_sys::MessageEvent| {
                if let Ok(text) = event.data().dyn_into::<js_sys::JsString>() {
                    let text_str: String = text.into();
                    console::log_1(
                        &format!("Received: {}", &text_str[..text_str.len().min(100)]).into(),
                    );
                }
            });
        ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
        onmessage.forget();

        let onerror =
            Closure::<dyn FnMut(web_sys::ErrorEvent)>::new(|event: web_sys::ErrorEvent| {
                console::error_1(&format!("WebSocket error: {:?}", event.message()).into());
            });
        ws.set_onerror(Some(onerror.as_ref().unchecked_ref()));
        onerror.forget();

        let onclose =
            Closure::<dyn FnMut(web_sys::CloseEvent)>::new(|event: web_sys::CloseEvent| {
                console::log_1(
                    &format!(
                        "WebSocket closed: code={} reason={}",
                        event.code(),
                        event.reason()
                    )
                    .into(),
                );
            });
        ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));
        onclose.forget();

        self.websocket = Some(ws);
        self.state.connected = true;

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
        if self.state.cursor_visible {
            let char_width = self.config.font_size_px as f64 * 0.6;
            let char_height = self.config.font_size_px as f64;
            let x = self.state.cursor_x as f64 * char_width;
            let y = self.state.cursor_y as f64 * char_height;

            ctx.set_fill_style_str("#00ff00");
            ctx.fill_rect(x, y, char_width, char_height);
        }

        Ok(())
    }

    /// Check if connected to the server.
    #[wasm_bindgen(getter)]
    pub fn is_connected(&self) -> bool {
        self.state.connected
    }

    /// Get current screen ID.
    #[wasm_bindgen(getter)]
    pub fn screen_id(&self) -> u8 {
        self.state.screen_id
    }

    /// Get terminal columns.
    #[wasm_bindgen(getter)]
    pub fn cols(&self) -> u16 {
        self.state.cols
    }

    /// Get terminal rows.
    #[wasm_bindgen(getter)]
    pub fn rows(&self) -> u16 {
        self.state.rows
    }

    /// Disconnect from the server.
    #[wasm_bindgen]
    pub fn disconnect(&mut self) {
        if let Some(ws) = self.websocket.take() {
            let _ = ws.close();
        }
        self.state.connected = false;
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

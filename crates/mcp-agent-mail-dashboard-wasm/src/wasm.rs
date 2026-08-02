//! JavaScript bindings for the host-driven Agent Mail dashboard runner.

use js_sys::{Array, Object, Reflect, Uint32Array};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

use crate::runner_core::DashboardRunnerCore;

fn console_error(message: &str) {
    let global = js_sys::global();
    let Ok(console) = Reflect::get(&global, &"console".into()) else {
        return;
    };
    let Ok(error) = Reflect::get(&console, &"error".into()) else {
        return;
    };
    let Ok(error) = error.dyn_into::<js_sys::Function>() else {
        return;
    };
    let _ = error.call1(&console, &JsValue::from_str(message));
}

fn install_panic_hook() {
    use std::sync::Once;

    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            console_error(&format!("Agent Mail dashboard WASM panic: {info}"));
        }));
    });
}

#[wasm_bindgen]
pub struct AgentMailDashboardRunner {
    inner: DashboardRunnerCore,
}

#[wasm_bindgen(start)]
pub fn wasm_start() {
    install_panic_hook();
}

#[wasm_bindgen]
impl AgentMailDashboardRunner {
    #[wasm_bindgen(constructor)]
    pub fn new(cols: u16, rows: u16) -> Self {
        install_panic_hook();
        Self {
            inner: DashboardRunnerCore::new(cols, rows),
        }
    }

    pub fn init(&mut self) {
        self.inner.init();
    }

    #[wasm_bindgen(js_name = loadDemoPack)]
    pub fn load_demo_pack(&mut self, json: &str) -> Result<(), JsValue> {
        self.inner
            .load_demo_pack_json(json)
            .map_err(|error| JsValue::from_str(&error.to_string()))
    }

    #[wasm_bindgen(js_name = advanceTime)]
    pub fn advance_time(&mut self, dt_ms: f64) {
        self.inner.advance_time_ms(dt_ms);
    }

    #[wasm_bindgen(js_name = pushEncodedInput)]
    pub fn push_encoded_input(&mut self, json: &str) -> bool {
        self.inner.push_encoded_input(json)
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.inner.resize(cols, rows);
    }

    pub fn step(&mut self) -> JsValue {
        let result = self.inner.step();
        let object = Object::new();
        let _ = Reflect::set(&object, &"running".into(), &result.running.into());
        let _ = Reflect::set(&object, &"rendered".into(), &result.rendered.into());
        let _ = Reflect::set(
            &object,
            &"events_processed".into(),
            &result.events_processed.into(),
        );
        let _ = Reflect::set(
            &object,
            &"frame_idx".into(),
            &JsValue::from_f64(result.frame_idx as f64),
        );
        object.into()
    }

    #[wasm_bindgen(js_name = takeFlatPatches)]
    pub fn take_flat_patches(&mut self) -> JsValue {
        let patches = self.inner.take_flat_patches();
        let cells = Uint32Array::from(patches.cells.as_slice());
        let spans = Uint32Array::from(patches.spans.as_slice());
        let object = Object::new();
        let _ = Reflect::set(&object, &"cells".into(), &cells.into());
        let _ = Reflect::set(&object, &"spans".into(), &spans.into());
        object.into()
    }

    #[wasm_bindgen(js_name = takeLogs)]
    pub fn take_logs(&mut self) -> Array {
        let logs = Array::new();
        for line in self.inner.take_logs() {
            logs.push(&JsValue::from_str(&line));
        }
        logs
    }

    #[wasm_bindgen(js_name = patchHash)]
    pub fn patch_hash(&self) -> Option<String> {
        self.inner.patch_hash().map(str::to_owned)
    }

    #[wasm_bindgen(js_name = patchStats)]
    pub fn patch_stats(&self) -> JsValue {
        let Some(stats) = self.inner.patch_stats() else {
            return JsValue::NULL;
        };
        let object = Object::new();
        let _ = Reflect::set(&object, &"dirty_cells".into(), &stats.dirty_cells.into());
        let _ = Reflect::set(&object, &"patch_count".into(), &stats.patch_count.into());
        let _ = Reflect::set(
            &object,
            &"bytes_uploaded".into(),
            &JsValue::from_f64(stats.bytes_uploaded as f64),
        );
        object.into()
    }

    #[wasm_bindgen(js_name = statusJson)]
    pub fn status_json(&self) -> String {
        serde_json::to_string(&self.inner.status()).unwrap_or_else(|_| "{}".to_string())
    }

    #[wasm_bindgen(js_name = setPaused)]
    pub fn set_paused(&mut self, paused: bool) {
        self.inner.set_paused(paused);
    }

    #[wasm_bindgen(js_name = setReducedMotion)]
    pub fn set_reduced_motion(&mut self, reduced_motion: bool) {
        self.inner.set_reduced_motion(reduced_motion);
    }

    pub fn reset(&mut self) {
        self.inner.reset();
    }

    pub fn destroy(&mut self) {}
}

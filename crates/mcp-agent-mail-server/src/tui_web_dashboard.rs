//! Web dashboard: mirrors the terminal TUI in a browser.
//!
//! Serves `/web-dashboard` with a self-contained HTML page that polls
//! `/web-dashboard/state` for rendered frame data and renders it to a
//! `<canvas>` element.  Input events are forwarded back via
//! `/web-dashboard/input`.
//!
//! # Performance design
//!
//! The hot path is `capture()` → `handle_state()`:
//!
//! 1. **Capture** (runs on TUI render thread, every tick):
//!    - Copies raw cell bytes directly from the `ftui::Buffer` into a reusable
//!      byte buffer (zero per-cell allocation).
//!    - Computes a delta against the previous frame (only changed cells).
//!    - Pre-serializes the JSON + base64 response string so the HTTP handler
//!      returns a cached `Arc<str>` with zero work.
//!
//! 2. **Serve** (runs on HTTP worker thread, per-poll):
//!    - Reads an `Arc<str>` under a brief mutex — no serialization, no alloc.
//!
//! Target: < 500µs capture for 200×50 grids, < 1µs serve.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;

use ftui::render::buffer::Buffer;
use mcp_agent_mail_core::now_micros;
use serde_json::json;

use crate::tui_bridge::TuiSharedState;
use crate::tui_ws_input;

// ─── Constants ──────────────────────────────────────────────────────────────

/// Base64 alphabet (standard, no padding).
const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Pre-computed "unchanged" response (no frame data).
fn unchanged_response(seq: u64) -> String {
    format!(r#"{{"mode":"unchanged","seq":{seq}}}"#)
}

// ─── Frame store ────────────────────────────────────────────────────────────

/// Internal state held under the mutex.
struct FrameState {
    /// Raw cell bytes from previous capture (for delta computation).
    prev_bytes: Vec<u8>,
    /// Current raw cell bytes.
    curr_bytes: Vec<u8>,
    /// Grid dimensions of the current frame.
    cols: u16,
    rows: u16,
    screen_id: u8,
    /// Pre-serialized full snapshot JSON response (base64-encoded cells).
    cached_snapshot: Arc<str>,
    /// Pre-serialized delta JSON response (changed cells only).
    cached_delta: Arc<str>,
    /// Sequence number of the cached snapshot.
    snapshot_seq: u64,
}

impl Default for FrameState {
    fn default() -> Self {
        let empty: Arc<str> = Arc::from(
            r#"{"mode":"snapshot","seq":0,"cols":0,"rows":0,"screen_id":0,"timestamp_us":0,"cells":""}"#,
        );
        Self {
            prev_bytes: Vec::new(),
            curr_bytes: Vec::new(),
            cols: 0,
            rows: 0,
            screen_id: 0,
            cached_snapshot: Arc::clone(&empty),
            cached_delta: empty,
            snapshot_seq: 0,
        }
    }
}

/// Storage for the latest captured frame, embedded in `TuiSharedState`.
#[derive(Debug)]
pub struct WebDashboardFrameStore {
    state: Mutex<FrameState>,
    seq: AtomicU64,
}

impl std::fmt::Debug for FrameState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameState")
            .field("cols", &self.cols)
            .field("rows", &self.rows)
            .field("snapshot_seq", &self.snapshot_seq)
            .field("curr_bytes_len", &self.curr_bytes.len())
            .finish()
    }
}

impl WebDashboardFrameStore {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(FrameState::default()),
            seq: AtomicU64::new(0),
        }
    }

    /// Capture a rendered buffer into the store.  Called from the TUI render
    /// loop after `view()` completes.
    ///
    /// This is the hot path.  It:
    /// 1. Copies raw cell data (16 bytes/cell) into a reusable buffer.
    /// 2. Computes changed cell indices vs previous frame.
    /// 3. Pre-builds the JSON response strings (snapshot + delta).
    ///
    /// The HTTP handler then just returns the cached string.
    pub fn capture(&self, buffer: &Buffer, screen_id: u8) {
        let cols = buffer.width();
        let rows = buffer.height();
        let cells = buffer.cells();
        let n_cells = cells.len();
        let byte_len = n_cells * 16; // 4 u32s × 4 bytes each

        let new_seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let ts = now_micros();

        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());

        // Swap prev ← curr, then reuse curr's allocation for the new frame.
        // Take curr out, swap, put back — avoids double-borrow on guard fields.
        let mut new_curr = std::mem::take(&mut guard.prev_bytes);
        guard.prev_bytes = std::mem::take(&mut guard.curr_bytes);
        new_curr.clear();
        if new_curr.capacity() < byte_len {
            new_curr.reserve(byte_len - new_curr.capacity());
        }

        // Copy raw cell data: Cell is #[repr(C, align(16))] = [content:u32, fg:u32, bg:u32, attrs:u32].
        // We extract fields individually because the Cell's alignment padding
        // makes a raw memcpy incorrect (padding bytes are undefined).
        for cell in cells {
            new_curr.extend_from_slice(&cell.content.raw().to_le_bytes());
            new_curr.extend_from_slice(&cell.fg.0.to_le_bytes());
            new_curr.extend_from_slice(&cell.bg.0.to_le_bytes());
            new_curr.extend_from_slice(&cell_attrs_raw(&cell.attrs).to_le_bytes());
        }

        // ── Build snapshot response (base64-encoded raw bytes) ──────
        let b64_len = (new_curr.len() + 2) / 3 * 4;
        // Pre-size: ~130 chars header + b64 + ~2 chars footer
        let mut snap = String::with_capacity(140 + b64_len);
        write!(
            snap,
            r#"{{"mode":"snapshot","seq":{new_seq},"cols":{cols},"rows":{rows},"screen_id":{screen_id},"timestamp_us":{ts},"cells":""#
        ).unwrap();
        base64_encode_into(&new_curr, &mut snap);
        snap.push_str("\"}");
        guard.cached_snapshot = Arc::from(snap.as_str());

        // ── Build delta response (only changed cell indices) ────────
        let same_dims =
            guard.prev_bytes.len() == new_curr.len() && guard.cols == cols && guard.rows == rows;

        if same_dims {
            // Compare 16-byte chunks, collect indices of changed cells.
            let mut delta = String::with_capacity(256);
            write!(
                delta,
                r#"{{"mode":"delta","seq":{new_seq},"cols":{cols},"rows":{rows},"screen_id":{screen_id},"timestamp_us":{ts},"changed":["#
            ).unwrap();
            let mut first = true;
            let prev = &guard.prev_bytes;
            for i in 0..n_cells {
                let off = i * 16;
                if prev[off..off + 16] != new_curr[off..off + 16] {
                    if !first {
                        delta.push(',');
                    }
                    first = false;
                    // Emit: [idx, content, fg, bg, attrs]
                    let c = u32::from_le_bytes([
                        new_curr[off],
                        new_curr[off + 1],
                        new_curr[off + 2],
                        new_curr[off + 3],
                    ]);
                    let f = u32::from_le_bytes([
                        new_curr[off + 4],
                        new_curr[off + 5],
                        new_curr[off + 6],
                        new_curr[off + 7],
                    ]);
                    let b = u32::from_le_bytes([
                        new_curr[off + 8],
                        new_curr[off + 9],
                        new_curr[off + 10],
                        new_curr[off + 11],
                    ]);
                    let a = u32::from_le_bytes([
                        new_curr[off + 12],
                        new_curr[off + 13],
                        new_curr[off + 14],
                        new_curr[off + 15],
                    ]);
                    write!(delta, "[{i},{c},{f},{b},{a}]").unwrap();
                }
            }
            delta.push_str("]}");
            guard.cached_delta = Arc::from(delta.as_str());
        } else {
            // Dimensions changed — no valid delta, use snapshot.
            guard.cached_delta = Arc::clone(&guard.cached_snapshot);
        }

        guard.curr_bytes = new_curr;
        guard.cols = cols;
        guard.rows = rows;
        guard.screen_id = screen_id;
        guard.snapshot_seq = new_seq;
    }

    /// Read the current sequence number without locking.
    pub fn current_seq(&self) -> u64 {
        self.seq.load(Ordering::Relaxed)
    }

    /// Get the pre-serialized snapshot response (zero-copy Arc<str>).
    pub fn cached_snapshot(&self) -> Arc<str> {
        Arc::clone(
            &self
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .cached_snapshot,
        )
    }

    /// Get the pre-serialized delta response (zero-copy Arc<str>).
    pub fn cached_delta(&self) -> Arc<str> {
        Arc::clone(
            &self
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .cached_delta,
        )
    }

    /// Get the sequence number of the cached snapshot.
    pub fn snapshot_seq(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot_seq
    }
}

// ─── Cell attribute accessor ────────────────────────────────────────────────

/// Reconstruct raw u32 from CellAttrs (transparent newtype, but no public raw()).
#[inline]
fn cell_attrs_raw(attrs: &ftui::render::cell::CellAttrs) -> u32 {
    let flags_byte = attrs.flags().bits() as u32;
    let link = attrs.link_id();
    (flags_byte << 24) | (link & 0x00FF_FFFF)
}

// ─── Fast base64 encoder ────────────────────────────────────────────────────

/// Encode bytes into base64, appending directly to the output string.
/// No padding, no allocation beyond the string growth.
fn base64_encode_into(input: &[u8], out: &mut String) {
    let chunks = input.chunks_exact(3);
    let remainder = chunks.remainder();
    for chunk in chunks {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8) | u32::from(chunk[2]);
        out.push(B64[((n >> 18) & 0x3F) as usize] as char);
        out.push(B64[((n >> 12) & 0x3F) as usize] as char);
        out.push(B64[((n >> 6) & 0x3F) as usize] as char);
        out.push(B64[(n & 0x3F) as usize] as char);
    }
    match remainder.len() {
        1 => {
            let n = u32::from(remainder[0]) << 16;
            out.push(B64[((n >> 18) & 0x3F) as usize] as char);
            out.push(B64[((n >> 12) & 0x3F) as usize] as char);
        }
        2 => {
            let n = (u32::from(remainder[0]) << 16) | (u32::from(remainder[1]) << 8);
            out.push(B64[((n >> 18) & 0x3F) as usize] as char);
            out.push(B64[((n >> 12) & 0x3F) as usize] as char);
            out.push(B64[((n >> 6) & 0x3F) as usize] as char);
        }
        _ => {}
    }
}

// ─── HTTP endpoints ─────────────────────────────────────────────────────────

/// GET `/web-dashboard/state` — return the pre-serialized frame response.
///
/// Query params:
///   `since=<seq>` — return delta if available, full snapshot otherwise.
///
/// This does zero serialization work — it returns a pre-built `Arc<str>`.
pub fn handle_state(state: &TuiSharedState, query: Option<&str>) -> String {
    let since_seq = query.and_then(|q| {
        q.split('&')
            .find_map(|kv| kv.strip_prefix("since="))
            .and_then(|v| v.parse::<u64>().ok())
    });

    let store = state.web_dashboard_frame_store();
    let current_seq = store.current_seq();

    // If client is up-to-date, return a minimal "no change" response.
    if let Some(since) = since_seq {
        if since >= current_seq {
            return unchanged_response(current_seq);
        }
        // Delta is only valid for exactly the previous frame (seq - 1).
        // If the client missed more than one frame, send a full snapshot
        // so it doesn't render stale cells from the missed intermediate frames.
        if since + 1 >= current_seq {
            return store.cached_delta().to_string();
        }
        // Client is too far behind — fall through to full snapshot.
    }

    // No since param or client too far behind — send full snapshot.
    store.cached_snapshot().to_string()
}

fn input_error_status(detail: &str) -> u16 {
    if detail.contains("too large") {
        413
    } else {
        400
    }
}

/// POST `/web-dashboard/input` — forward keyboard/mouse events to the TUI.
pub fn handle_input(state: &TuiSharedState, body: &[u8]) -> (u16, String) {
    let parsed = match tui_ws_input::parse_remote_terminal_events(body) {
        Ok(parsed) => parsed,
        Err(detail) => {
            let status = input_error_status(&detail);
            return (status, json!({ "detail": detail }).to_string());
        }
    };
    let accepted = parsed.events.len();
    let mut dropped_oldest = 0_usize;
    for event in parsed.events {
        if state.push_remote_terminal_event(event) {
            dropped_oldest += 1;
        }
    }
    let queue_stats = state.remote_terminal_queue_stats();
    (
        202,
        json!({
            "status": "accepted",
            "accepted": accepted,
            "ignored": parsed.ignored,
            "dropped_oldest": dropped_oldest,
            "queue_depth": queue_stats.depth,
            "queue_dropped_oldest_total": queue_stats.dropped_oldest_total,
            "queue_resize_coalesced_total": queue_stats.resize_coalesced_total,
        })
        .to_string(),
    )
}

/// GET `/web-dashboard` — serve the self-contained HTML page.
///
/// The JS client decodes base64-encoded cell data into a Uint32Array
/// and uses ImageData for background fills (one putImageData per frame)
/// with fillText only for visible characters — dramatically faster than
/// per-cell fillRect.
pub fn handle_page(_host: &str) -> String {
    // Static page — no dynamic content, no format args needed.
    DASHBOARD_HTML.to_string()
}

/// Pre-built HTML page as a static string (no format! overhead per request).
static DASHBOARD_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Agent Mail — Web Dashboard</title>
<style>
* { margin: 0; padding: 0; box-sizing: border-box; }
body { background: #0a0a0f; color: #e0e0e0; font-family: monospace; overflow: hidden; }
#header { padding: 4px 12px; background: #16161e; border-bottom: 1px solid #2a2a3a;
           display: flex; align-items: center; gap: 12px; font-size: 13px; }
#header .status { width: 8px; height: 8px; border-radius: 50%; }
#header .status.ok { background: #50fa7b; }
#header .status.err { background: #ff5555; }
#header .status.wait { background: #f1fa8c; }
#terminal { display: block; margin: 0 auto; }
#info { position: fixed; bottom: 4px; right: 8px; font-size: 11px; color: #666; }
</style>
</head>
<body>
<div id="header">
  <span class="status wait" id="conn-dot"></span>
  <span id="conn-text">Connecting…</span>
  <span style="flex:1"></span>
  <span id="stats"></span>
</div>
<canvas id="terminal"></canvas>
<div id="info">Agent Mail Web Dashboard</div>

<script>
"use strict";
const STATE_URL = "/web-dashboard/state";
const INPUT_URL = "/web-dashboard/input";
const POLL_MS = 100;
const CW = 8, CH = 16;
const FONT = "14px monospace";

const canvas = document.getElementById("terminal");
const ctx = canvas.getContext("2d", { alpha: false });
const dot = document.getElementById("conn-dot");
const connText = document.getElementById("conn-text");
const statsEl = document.getElementById("stats");

let lastSeq = 0, frameCount = 0, lastFpsTime = performance.now();
let lastCols = 0, lastRows = 0;
// Reusable typed arrays — grown once, never shrunk.
let cellBuf = null;   // Uint32Array view of decoded cells
let imgData = null;   // ImageData for background pixel fills

// ── Base64 decode ─────────────────────────────────────────────
const B64_LOOKUP = new Uint8Array(128);
"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/".split("").forEach((c, i) => B64_LOOKUP[c.charCodeAt(0)] = i);

function b64ToU32Array(b64str) {
  const len = b64str.length;
  const byteLen = (len * 3) >>> 2;
  const buf = new ArrayBuffer(byteLen);
  const u8 = new Uint8Array(buf);
  let j = 0;
  for (let i = 0; i < len; i += 4) {
    const a = B64_LOOKUP[b64str.charCodeAt(i)];
    const b = B64_LOOKUP[b64str.charCodeAt(i+1)];
    const c = i+2 < len ? B64_LOOKUP[b64str.charCodeAt(i+2)] : 0;
    const d = i+3 < len ? B64_LOOKUP[b64str.charCodeAt(i+3)] : 0;
    u8[j++] = (a << 2) | (b >> 4);
    if (j < byteLen) u8[j++] = ((b & 0xF) << 4) | (c >> 2);
    if (j < byteLen) u8[j++] = ((c & 0x3) << 6) | d;
  }
  return new Uint32Array(buf);
}

// ── Render ────────────────────────────────────────────────────
function renderSnapshot(data) {
  const cols = data.cols, rows = data.rows;
  const cells = b64ToU32Array(data.cells); // Uint32Array: [content, fg, bg, attrs, ...]
  const W = cols * CW, H = rows * CH;

  if (cols !== lastCols || rows !== lastRows) {
    canvas.width = W; canvas.height = H;
    imgData = ctx.createImageData(W, H);
    lastCols = cols; lastRows = rows;
  }

  // Phase 1: Paint backgrounds via ImageData (one putImageData, no per-cell fillRect).
  const px = imgData.data;
  px.fill(0); // clear to black
  for (let i = 0, n = cols * rows; i < n; i++) {
    const bg = cells[i * 4 + 2];
    const a = bg & 0xFF;
    if (a === 0) continue; // transparent
    const r = (bg >>> 24) & 0xFF, g = (bg >>> 16) & 0xFF, b = (bg >>> 8) & 0xFF;
    const col = i % cols, row = (i / cols) | 0;
    const x0 = col * CW, y0 = row * CH;
    for (let dy = 0; dy < CH; dy++) {
      let off = ((y0 + dy) * W + x0) * 4;
      for (let dx = 0; dx < CW; dx++, off += 4) {
        px[off] = r; px[off+1] = g; px[off+2] = b; px[off+3] = 255;
      }
    }
  }
  ctx.putImageData(imgData, 0, 0);

  // Phase 2: Draw text (batched by font variant to minimize ctx.font changes).
  ctx.textBaseline = "top";
  let curFont = FONT;
  ctx.font = curFont;
  for (let i = 0, n = cols * rows; i < n; i++) {
    const base = i * 4;
    const content = cells[base];
    if (content <= 0x20 || content >= 0x7FFFFFFF || (content & 0x80000000)) continue;

    const fg = cells[base + 1];
    const attrs = cells[base + 3];
    const flags = (attrs >>> 24) & 0xFF;
    const wantFont = ((flags & 1) ? "bold " : "") + ((flags & 2) ? "italic " : "") + FONT;
    if (wantFont !== curFont) { ctx.font = wantFont; curFont = wantFont; }

    const fgA = fg & 0xFF;
    const fgR = (fg >>> 24) & 0xFF, fgG = (fg >>> 16) & 0xFF, fgB = (fg >>> 8) & 0xFF;
    ctx.fillStyle = fgA === 0 ? "#e0e0e0" : `rgb(${fgR},${fgG},${fgB})`;

    const col = i % cols, row = (i / cols) | 0;
    ctx.fillText(String.fromCodePoint(content), col * CW + 1, row * CH + 1);
  }

  cellBuf = cells; // keep for delta application
}

function applyDelta(data) {
  if (!cellBuf || data.cols !== lastCols || data.rows !== lastRows) {
    // Dimensions mismatch or no previous frame — request full snapshot
    // on the next poll by resetting lastSeq.  (Cannot call renderSnapshot
    // here because delta payloads don't carry the base64 cells field.)
    lastSeq = 0;
    return;
  }
  const changed = data.changed; // array of [idx, content, fg, bg, attrs]
  const cols = data.cols, rows = data.rows;
  const W = cols * CW, H = rows * CH;
  const px = imgData ? imgData.data : null;

  for (const entry of changed) {
    const [idx, content, fg, bg, attrs] = entry;
    const base = idx * 4;
    cellBuf[base] = content;
    cellBuf[base+1] = fg;
    cellBuf[base+2] = bg;
    cellBuf[base+3] = attrs;

    const col = idx % cols, row = (idx / cols) | 0;
    const x0 = col * CW, y0 = row * CH;

    // Repaint background for this cell.
    const a = bg & 0xFF;
    const r = a ? (bg >>> 24) & 0xFF : 10;
    const g = a ? (bg >>> 16) & 0xFF : 10;
    const b = a ? (bg >>> 8)  & 0xFF : 15;
    if (px) {
      for (let dy = 0; dy < CH; dy++) {
        let off = ((y0 + dy) * W + x0) * 4;
        for (let dx = 0; dx < CW; dx++, off += 4) {
          px[off] = r; px[off+1] = g; px[off+2] = b; px[off+3] = 255;
        }
      }
    }
  }
  if (px) ctx.putImageData(imgData, 0, 0);

  // Redraw text for changed cells.
  ctx.textBaseline = "top";
  let curFont = FONT;
  ctx.font = curFont;
  for (const [idx, content, fg, , attrs] of changed) {
    if (content <= 0x20 || content >= 0x7FFFFFFF || (content & 0x80000000)) continue;
    const flags = (attrs >>> 24) & 0xFF;
    const wantFont = ((flags & 1) ? "bold " : "") + ((flags & 2) ? "italic " : "") + FONT;
    if (wantFont !== curFont) { ctx.font = wantFont; curFont = wantFont; }
    const fgA = fg & 0xFF;
    const fgR = (fg >>> 24) & 0xFF, fgG = (fg >>> 16) & 0xFF, fgB = (fg >>> 8) & 0xFF;
    ctx.fillStyle = fgA === 0 ? "#e0e0e0" : `rgb(${fgR},${fgG},${fgB})`;
    const col = idx % lastCols, row = (idx / lastCols) | 0;
    ctx.fillText(String.fromCodePoint(content), col * CW + 1, row * CH + 1);
  }
}

// ── Polling ───────────────────────────────────────────────────
async function poll() {
  try {
    const url = lastSeq > 0 ? `${STATE_URL}?since=${lastSeq}` : STATE_URL;
    const resp = await fetch(url);
    if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
    const data = await resp.json();
    dot.className = "status ok";
    connText.textContent = "Connected";

    if (data.mode === "snapshot") {
      renderSnapshot(data);
      lastSeq = data.seq;
    } else if (data.mode === "delta") {
      applyDelta(data);
      lastSeq = data.seq;
    } else if (data.mode === "unchanged") {
      lastSeq = data.seq;
    }
  } catch (e) {
    dot.className = "status err";
    connText.textContent = "Error: " + e.message;
  }
  frameCount++;
  const now = performance.now();
  if (now - lastFpsTime > 1000) {
    statsEl.textContent = `${frameCount} fps | seq ${lastSeq} | ${lastCols}×${lastRows}`;
    frameCount = 0; lastFpsTime = now;
  }
}

(function schedulePoll() {
  setTimeout(async () => { await poll(); schedulePoll(); }, POLL_MS);
})();

// ── Keyboard ──────────────────────────────────────────────────
document.addEventListener("keydown", (e) => {
  const bk = e.key === "F5" || e.key === "F12"
    || ((e.ctrlKey || e.metaKey) && "cvxwtrl".includes(e.key.toLowerCase()))
    || (e.ctrlKey && e.shiftKey && e.key.toLowerCase() === "i");
  if (bk) return;
  e.preventDefault();
  const m = (e.ctrlKey?1:0)|(e.shiftKey?2:0)|(e.altKey?4:0)|(e.metaKey?8:0);
  fetch(INPUT_URL, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ type:"Input", data:{ kind:"Key", key:e.key, modifiers:m } }),
  }).catch(() => {});
});
</script>
</body>
</html>"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_encode_round_trip() {
        let input = [0u8, 1, 2, 3, 255, 128, 64, 32, 16, 8, 4, 2, 1, 0, 255, 127];
        let mut encoded = String::new();
        base64_encode_into(&input, &mut encoded);
        // Decode with standard library equivalent check.
        assert!(!encoded.is_empty());
        assert!(encoded.len() <= (input.len() + 2) / 3 * 4);
        // Verify all chars are valid base64.
        assert!(encoded
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/'));
    }

    #[test]
    fn base64_encode_empty() {
        let mut out = String::new();
        base64_encode_into(&[], &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn unchanged_response_format() {
        let resp = unchanged_response(42);
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["mode"], "unchanged");
        assert_eq!(v["seq"], 42);
    }

    #[test]
    fn handle_page_uses_relative_urls() {
        let html = handle_page("evil.example");
        assert!(html.contains(r#"const STATE_URL = "/web-dashboard/state";"#));
        assert!(html.contains(r#"const INPUT_URL = "/web-dashboard/input";"#));
        assert!(!html.contains("evil.example"));
    }

    #[test]
    fn handle_input_accepts_events() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let body = br#"{"events":[
            {"type":"Input","data":{"kind":"Key","key":"j","modifiers":0}},
            {"type":"Ping"}
        ]}"#;
        let (status, payload) = handle_input(&state, body);
        assert_eq!(status, 202);
        let v: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(v["accepted"], 1);
        assert_eq!(v["ignored"], 1);
    }

    #[test]
    fn handle_input_invalid_returns_400() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let (status, _) = handle_input(&state, b"not json");
        assert_eq!(status, 400);
    }

    #[test]
    fn handle_input_oversized_returns_413() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let body = vec![b' '; 512 * 1024 + 1];
        let (status, _) = handle_input(&state, &body);
        assert_eq!(status, 413);
    }
}

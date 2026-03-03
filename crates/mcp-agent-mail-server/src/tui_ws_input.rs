#![allow(clippy::module_name_repetitions)]

use crate::tui_bridge::RemoteTerminalEvent;
use serde::Deserialize;

const MAX_INGRESS_EVENTS: usize = 2048;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRemoteEvents {
    pub events: Vec<RemoteTerminalEvent>,
    pub ignored: usize,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum IngressEnvelope {
    Single(IngressMessage),
    Batch { events: Vec<IngressMessage> },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", content = "data")]
enum IngressMessage {
    #[serde(rename = "Input", alias = "input")]
    Input(IngressInputEvent),
    #[serde(rename = "Resize", alias = "resize")]
    Resize { cols: u16, rows: u16 },
    #[serde(rename = "Ping", alias = "ping")]
    Ping,
    #[serde(rename = "Pong", alias = "pong")]
    Pong,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind")]
enum IngressInputEvent {
    #[serde(rename = "Key", alias = "key")]
    Key {
        key: String,
        #[serde(default)]
        modifiers: u8,
    },
    #[serde(other)]
    Unsupported,
}

pub fn parse_remote_terminal_events(body: &[u8]) -> Result<ParsedRemoteEvents, String> {
    if body.is_empty() {
        return Err("Request body must not be empty".to_string());
    }
    if body.len() > 512 * 1024 {
        return Err("Request body too large (max 512KB)".to_string());
    }

    let envelope: IngressEnvelope = serde_json::from_slice(body)
        .map_err(|err| format!("Invalid /mail/ws-input payload: {err}"))?;
    let messages = match envelope {
        IngressEnvelope::Single(message) => vec![message],
        IngressEnvelope::Batch { events } => events,
    };

    if messages.len() > MAX_INGRESS_EVENTS {
        return Err(format!(
            "Too many ingress events: {} (max {MAX_INGRESS_EVENTS})",
            messages.len()
        ));
    }

    let mut events = Vec::with_capacity(messages.len());
    let mut ignored = 0_usize;
    for message in messages {
        match message {
            IngressMessage::Input(IngressInputEvent::Key { mut key, modifiers }) => {
                if key.len() > 4096 {
                    let mut idx = 4096;
                    while idx > 0 && !key.is_char_boundary(idx) {
                        idx -= 1;
                    }
                    key.truncate(idx);
                }
                events.push(RemoteTerminalEvent::Key { key, modifiers });
            }
            IngressMessage::Resize { cols, rows } => {
                events.push(RemoteTerminalEvent::Resize { cols, rows });
            }
            IngressMessage::Input(IngressInputEvent::Unsupported)
            | IngressMessage::Ping
            | IngressMessage::Pong => {
                ignored += 1;
            }
        }
    }

    Ok(ParsedRemoteEvents { events, ignored })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_single_key_event() {
        let payload = br#"{"type":"Input","data":{"kind":"Key","key":"j","modifiers":1}}"#;
        let parsed = parse_remote_terminal_events(payload).expect("parse key event");
        assert_eq!(parsed.ignored, 0);
        assert_eq!(parsed.events.len(), 1);
        assert!(matches!(
            parsed.events[0],
            RemoteTerminalEvent::Key {
                ref key,
                modifiers: 1
            } if key == "j"
        ));
    }

    #[test]
    fn parse_single_resize_event() {
        let payload = br#"{"type":"Resize","data":{"cols":120,"rows":40}}"#;
        let parsed = parse_remote_terminal_events(payload).expect("parse resize event");
        assert_eq!(parsed.ignored, 0);
        assert_eq!(
            parsed.events,
            vec![RemoteTerminalEvent::Resize {
                cols: 120,
                rows: 40
            }]
        );
    }

    #[test]
    fn parse_batch_skips_unsupported_and_ping() {
        let payload = br#"{
            "events": [
                {"type":"Input","data":{"kind":"Key","key":"k","modifiers":0}},
                {"type":"Ping"},
                {"type":"Input","data":{"kind":"Mouse","x":1,"y":2,"button":1}},
                {"type":"Resize","data":{"cols":80,"rows":24}}
            ]
        }"#;
        let parsed = parse_remote_terminal_events(payload).expect("parse batch");
        assert_eq!(parsed.ignored, 2);
        assert_eq!(parsed.events.len(), 2);
        assert!(matches!(
            parsed.events[0],
            RemoteTerminalEvent::Key {
                ref key,
                modifiers: 0
            } if key == "k"
        ));
        assert!(matches!(
            parsed.events[1],
            RemoteTerminalEvent::Resize { cols: 80, rows: 24 }
        ));
    }

    #[test]
    fn parse_rejects_too_many_events() {
        let events: Vec<serde_json::Value> = (0..=MAX_INGRESS_EVENTS)
            .map(|_| json!({"type":"Ping"}))
            .collect();
        let body = serde_json::to_vec(&json!({ "events": events })).expect("serialize payload");
        let err = parse_remote_terminal_events(&body).expect_err("expected too-many-events error");
        assert!(err.contains("Too many ingress events"));
    }

    #[test]
    fn parse_rejects_invalid_payload() {
        let err = parse_remote_terminal_events(br#"{"type":"Input""#)
            .expect_err("expected invalid payload error");
        assert!(err.contains("Invalid /mail/ws-input payload"));
    }

    #[test]
    fn parse_rejects_empty_body() {
        let err = parse_remote_terminal_events(&[]).expect_err("expected empty-body error");
        assert!(err.contains("must not be empty"));
    }

    #[test]
    fn parse_accepts_max_batch_size_boundary() {
        let events: Vec<serde_json::Value> = (0..MAX_INGRESS_EVENTS)
            .map(|_| json!({"type":"Ping"}))
            .collect();
        let body = serde_json::to_vec(&json!({ "events": events })).expect("serialize payload");
        let parsed = parse_remote_terminal_events(&body).expect("parse max-sized batch");
        assert!(parsed.events.is_empty());
        assert_eq!(parsed.ignored, MAX_INGRESS_EVENTS);
    }

    #[test]
    fn parse_alias_forms_are_supported() {
        let payload = br#"{
            "events": [
                {"type":"input","data":{"kind":"key","key":"x","modifiers":2}},
                {"type":"resize","data":{"cols":99,"rows":41}}
            ]
        }"#;
        let parsed = parse_remote_terminal_events(payload).expect("parse alias forms");
        assert_eq!(
            parsed.events,
            vec![
                RemoteTerminalEvent::Key {
                    key: "x".to_string(),
                    modifiers: 2,
                },
                RemoteTerminalEvent::Resize { cols: 99, rows: 41 }
            ]
        );
        assert_eq!(parsed.ignored, 0);
    }

    #[test]
    fn parse_ping_only_payload_is_ignored_not_error() {
        let payload = br#"{"type":"Ping"}"#;
        let parsed = parse_remote_terminal_events(payload).expect("parse ping payload");
        assert!(parsed.events.is_empty());
        assert_eq!(parsed.ignored, 1);
    }

    #[test]
    fn parse_rejects_oversized_body() {
        let body = vec![b' '; 512 * 1024 + 1];
        let err = parse_remote_terminal_events(&body).expect_err("expected oversized error");
        assert!(err.contains("too large"));
    }

    #[test]
    fn parse_accepts_body_at_size_limit() {
        let prefix = br#"{"type":"Ping"}"#;
        let mut body = Vec::with_capacity(prefix.len());
        body.extend_from_slice(prefix);
        let parsed = parse_remote_terminal_events(&body).expect("should accept at limit");
        assert_eq!(parsed.ignored, 1);
    }

    #[test]
    fn parse_long_key_is_truncated() {
        let long_key = "x".repeat(5000);
        let payload = serde_json::to_vec(&json!({
            "type": "Input",
            "data": {"kind": "Key", "key": long_key, "modifiers": 0}
        }))
        .expect("serialize");
        let parsed = parse_remote_terminal_events(&payload).expect("parse long key");
        assert_eq!(parsed.events.len(), 1);
        if let RemoteTerminalEvent::Key { ref key, .. } = parsed.events[0] {
            assert!(key.len() <= 4096, "key should be truncated to 4096 bytes");
        } else {
            panic!("expected Key event");
        }
    }

    #[test]
    fn parse_long_multibyte_key_truncates_at_char_boundary() {
        let emoji = "\u{1F600}"; // 4 bytes per char
        let key = emoji.repeat(1025); // 4100 bytes
        assert!(key.len() > 4096);
        let payload = serde_json::to_vec(&json!({
            "type": "Input",
            "data": {"kind": "Key", "key": key, "modifiers": 0}
        }))
        .expect("serialize");
        let parsed = parse_remote_terminal_events(&payload).expect("parse multibyte key");
        assert_eq!(parsed.events.len(), 1);
        if let RemoteTerminalEvent::Key { ref key, .. } = parsed.events[0] {
            assert!(key.len() <= 4096);
            assert!(std::str::from_utf8(key.as_bytes()).is_ok());
        } else {
            panic!("expected Key event");
        }
    }

    #[test]
    fn parse_pong_is_ignored() {
        let payload = br#"{"type":"Pong"}"#;
        let parsed = parse_remote_terminal_events(payload).expect("parse pong");
        assert!(parsed.events.is_empty());
        assert_eq!(parsed.ignored, 1);
    }

    #[test]
    fn parse_batch_mixed_pong_and_keys() {
        let payload = br#"{
            "events": [
                {"type":"Pong"},
                {"type":"Input","data":{"kind":"Key","key":"a","modifiers":0}},
                {"type":"Pong"},
                {"type":"Ping"}
            ]
        }"#;
        let parsed = parse_remote_terminal_events(payload).expect("parse mixed");
        assert_eq!(parsed.events.len(), 1);
        assert_eq!(parsed.ignored, 3);
    }

    #[test]
    fn parse_key_with_zero_modifiers_default() {
        let payload = br#"{"type":"Input","data":{"kind":"Key","key":"Enter"}}"#;
        let parsed = parse_remote_terminal_events(payload).expect("parse default modifiers");
        assert_eq!(parsed.events.len(), 1);
        if let RemoteTerminalEvent::Key { modifiers, .. } = parsed.events[0] {
            assert_eq!(modifiers, 0, "default modifiers should be 0");
        }
    }

    #[test]
    fn parse_empty_batch() {
        let payload = br#"{"events":[]}"#;
        let parsed = parse_remote_terminal_events(payload).expect("parse empty batch");
        assert!(parsed.events.is_empty());
        assert_eq!(parsed.ignored, 0);
    }
}

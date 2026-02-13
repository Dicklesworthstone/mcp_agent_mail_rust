#![allow(clippy::module_name_repetitions)]

use crate::tui_bridge::RemoteTerminalEvent;
use serde::Deserialize;

const MAX_INGRESS_EVENTS: usize = 256;

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
            IngressMessage::Input(IngressInputEvent::Key { key, modifiers }) => {
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
}

#![allow(clippy::module_name_repetitions)]

use crate::tui_bridge::TuiSharedState;
use crate::tui_events::MailEvent;
use serde_json::{Value, json};
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_EVENT_LIMIT: usize = 200;
const MAX_EVENT_LIMIT: usize = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PollParams {
    since: Option<u64>,
    limit: usize,
}

fn parse_poll_params(query: Option<&str>) -> PollParams {
    let mut since = None;
    let mut limit = DEFAULT_EVENT_LIMIT;

    let Some(query) = query else {
        return PollParams { since, limit };
    };

    for pair in query.split('&').filter(|segment| !segment.is_empty()) {
        let (key, value) = pair
            .split_once('=')
            .map_or((pair, ""), |(lhs, rhs)| (lhs, rhs));
        match key {
            "since" => {
                if let Ok(parsed) = value.parse::<u64>() {
                    since = Some(parsed);
                }
            }
            "limit" => {
                if let Ok(parsed) = value.parse::<usize>() {
                    limit = parsed.clamp(1, MAX_EVENT_LIMIT);
                }
            }
            _ => {}
        }
    }

    PollParams { since, limit }
}

fn now_micros() -> i64 {
    let Ok(delta) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return 0;
    };
    i64::try_from(delta.as_micros()).unwrap_or(i64::MAX)
}

fn config_json(state: &TuiSharedState) -> Value {
    let cfg = state.config_snapshot();
    json!({
        "endpoint": cfg.endpoint,
        "http_path": cfg.http_path,
        "web_ui_url": cfg.web_ui_url,
        "app_environment": cfg.app_environment,
        "auth_enabled": cfg.auth_enabled,
        "database_url": cfg.database_url,
        "storage_root": cfg.storage_root,
        "console_theme": cfg.console_theme,
        "tool_filter_profile": cfg.tool_filter_profile,
    })
}

fn snapshot_payload(state: &TuiSharedState, limit: usize) -> Value {
    let counters = state.request_counters();
    let ring = state.event_ring_stats();
    let next_seq = ring.next_seq;
    let events = state.recent_events(limit);

    json!({
        "schema_version": "am_ws_state_poll.v1",
        "transport": "http-poll",
        "mode": "snapshot",
        "generated_at_us": now_micros(),
        "next_seq": next_seq,
        "event_count": events.len(),
        "request_counters": {
            "total": counters.total,
            "status_2xx": counters.status_2xx,
            "status_4xx": counters.status_4xx,
            "status_5xx": counters.status_5xx,
            "latency_total_ms": counters.latency_total_ms,
            "avg_latency_ms": state.avg_latency_ms(),
        },
        "event_ring_stats": ring,
        "config": config_json(state),
        "db_stats": state.db_stats_snapshot(),
        "sparkline_ms": state.sparkline_snapshot(),
        "events": events,
    })
}

fn delta_payload(state: &TuiSharedState, since: u64, limit: usize) -> Value {
    let counters = state.request_counters();
    let ring = state.event_ring_stats();
    let mut events = state.events_since(since);
    if events.len() > limit {
        let keep_from = events.len().saturating_sub(limit);
        events.drain(..keep_from);
    }
    let to_seq = events.last().map_or(since, MailEvent::seq);

    json!({
        "schema_version": "am_ws_state_poll.v1",
        "transport": "http-poll",
        "mode": "delta",
        "generated_at_us": now_micros(),
        "since_seq": since,
        "to_seq": to_seq,
        "event_count": events.len(),
        "request_counters": {
            "total": counters.total,
            "status_2xx": counters.status_2xx,
            "status_4xx": counters.status_4xx,
            "status_5xx": counters.status_5xx,
            "latency_total_ms": counters.latency_total_ms,
            "avg_latency_ms": state.avg_latency_ms(),
        },
        "event_ring_stats": ring,
        "db_stats": state.db_stats_snapshot(),
        "sparkline_ms": state.sparkline_snapshot(),
        "events": events,
    })
}

#[must_use]
pub fn poll_payload(state: &TuiSharedState, query: Option<&str>) -> Value {
    let params = parse_poll_params(query);
    params.since.map_or_else(
        || snapshot_payload(state, params.limit),
        |since| delta_payload(state, since, params.limit),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui_events::MailEvent;

    #[test]
    fn parse_poll_params_defaults() {
        let parsed = parse_poll_params(None);
        assert_eq!(parsed.since, None);
        assert_eq!(parsed.limit, DEFAULT_EVENT_LIMIT);
    }

    #[test]
    fn parse_poll_params_clamps_limit_and_parses_since() {
        let parsed = parse_poll_params(Some("since=42&limit=100000"));
        assert_eq!(parsed.since, Some(42));
        assert_eq!(parsed.limit, MAX_EVENT_LIMIT);
    }

    #[test]
    fn poll_payload_snapshot_mode() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let payload = poll_payload(&state, None);
        assert_eq!(payload["mode"], "snapshot");
        assert_eq!(payload["transport"], "http-poll");
        assert!(payload["next_seq"].as_u64().is_some());
    }

    #[test]
    fn poll_payload_delta_mode() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let _ = state.push_event(MailEvent::server_started(
            "http://127.0.0.1:8765/mcp/",
            "cfg",
        ));
        let _ = state.push_event(MailEvent::server_shutdown());
        let payload = poll_payload(&state, Some("since=0&limit=10"));

        assert_eq!(payload["mode"], "delta");
        assert_eq!(payload["since_seq"], 0);
        assert!(payload["to_seq"].as_u64().is_some());
        assert!(
            payload["events"]
                .as_array()
                .is_some_and(|events| !events.is_empty())
        );
    }
}

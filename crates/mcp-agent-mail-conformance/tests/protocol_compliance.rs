#![allow(clippy::too_many_lines)]

use fastmcp::{Cx, JsonRpcMessage, JsonRpcRequest, StdioTransport, Transport};
use fastmcp_transport::http::{
    HttpHandlerConfig, HttpMethod, HttpRequest, HttpRequestHandler, HttpTransport,
};
use mcp_agent_mail_core::Config;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{Cursor, Write};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug)]
enum TransportKind {
    Stdio,
    Http,
}

impl TransportKind {
    const ALL: [Self; 2] = [Self::Stdio, Self::Http];

    const fn label(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Http => "http",
        }
    }
}

#[derive(Clone)]
enum Payload {
    Json(JsonRpcRequest),
    Raw(Vec<u8>),
}

#[derive(Clone, Default)]
struct SharedBufferWriter {
    buf: Arc<Mutex<Vec<u8>>>,
}

impl SharedBufferWriter {
    fn snapshot(&self) -> Vec<u8> {
        self.buf
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl Write for SharedBufferWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buf
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct TransportExecution {
    raw_output: Vec<u8>,
    responses: Vec<Value>,
    http_headers: Vec<HashMap<String, String>>,
}

/// Agent Mail's locked HTTP MCP path (`HTTP_PATH` default `/mcp/`).
/// `FastMCP` 0.3 defaulted `HttpHandlerConfig::base_path` to `/mcp/v1`; tests
/// that use the raw `FastMCP` HTTP transport must pin Agent Mail's trailing-slash
/// contract instead of inheriting that default (br-w9v59.1).
fn agent_mail_http_config() -> HttpHandlerConfig {
    HttpHandlerConfig {
        base_path: "/mcp/".to_string(),
        ..HttpHandlerConfig::default()
    }
}

fn initialize_request<T: Into<Value>>(id: T) -> JsonRpcRequest {
    let params = Some(json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {"name": "protocol-test-client", "version": "1.0"}
    }));
    match id.into() {
        Value::Number(number) => JsonRpcRequest::new(
            "initialize",
            params,
            number
                .as_i64()
                .expect("initialize numeric id must fit in i64"),
        ),
        Value::String(text) => JsonRpcRequest::new("initialize", params, text),
        other => panic!("unsupported initialize id: {other:?}"),
    }
}

fn execute_transport(transport: TransportKind, payloads: Vec<Payload>) -> TransportExecution {
    // Inlined former `protocol_server()` helper: the raw `fastmcp_server::Server`
    // type returned by build_server is no longer nameable via the `fastmcp` facade.
    let server = mcp_agent_mail_server::build_server(&Config::default());
    let writer = SharedBufferWriter::default();
    let output = writer.clone();

    match transport {
        TransportKind::Stdio => {
            let mut input = Vec::new();
            for payload in payloads {
                let mut bytes = match payload {
                    Payload::Json(request) => {
                        serde_json::to_vec(&request).expect("serialize stdio request")
                    }
                    Payload::Raw(bytes) => bytes,
                };
                if !bytes.ends_with(b"\n") {
                    bytes.push(b'\n');
                }
                input.extend_from_slice(&bytes);
            }

            let transport = StdioTransport::new(Cursor::new(input), writer);
            let handle = std::thread::spawn(move || {
                let cx = Cx::for_testing();
                if let Err(error) = server.run_transport_returning_with_cx(&cx, transport) {
                    eprintln!("stdio transport stopped with an error: {error}");
                }
            });
            handle.join().expect("stdio server thread");

            let raw_output = output.snapshot();
            TransportExecution {
                responses: parse_ndjson_responses(&raw_output),
                raw_output,
                http_headers: Vec::new(),
            }
        }
        TransportKind::Http => {
            let mut input = Vec::new();
            for payload in payloads {
                let body = match payload {
                    Payload::Json(request) => {
                        serde_json::to_vec(&request).expect("serialize http request")
                    }
                    Payload::Raw(bytes) => bytes,
                };
                input.extend_from_slice(&build_raw_http_request(&body));
            }

            let transport =
                HttpTransport::with_config(Cursor::new(input), writer, agent_mail_http_config());
            let handle = std::thread::spawn(move || {
                let cx = Cx::for_testing();
                if let Err(error) = server.run_transport_returning_with_cx(&cx, transport) {
                    eprintln!("HTTP transport stopped with an error: {error}");
                }
            });
            handle.join().expect("http server thread");

            let raw_output = output.snapshot();
            let (responses, http_headers) = parse_http_responses(&raw_output);
            TransportExecution {
                raw_output,
                responses,
                http_headers,
            }
        }
    }
}

fn build_raw_http_request(body: &[u8]) -> Vec<u8> {
    let mut request = format!(
        "POST /mcp/ HTTP/1.1\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(body);
    request
}

fn parse_ndjson_responses(raw: &[u8]) -> Vec<Value> {
    String::from_utf8(raw.to_vec())
        .expect("stdio output must be utf8")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("stdio response must be valid json"))
        .collect()
}

fn parse_http_responses(raw: &[u8]) -> (Vec<Value>, Vec<HashMap<String, String>>) {
    let mut cursor = 0_usize;
    let mut responses = Vec::new();
    let mut headers_out = Vec::new();

    while cursor < raw.len() {
        let remaining = &raw[cursor..];
        let Some(header_end) = remaining
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
        else {
            panic!(
                "missing HTTP header terminator in {:?}",
                String::from_utf8_lossy(remaining)
            );
        };
        let header_bytes = &remaining[..header_end];
        let header_text = String::from_utf8(header_bytes.to_vec()).expect("http headers utf8");
        let mut lines = header_text.split("\r\n");
        let status_line = lines.next().expect("http status line");
        let mut headers = HashMap::new();
        // Synthetic ":status" entry so tests can assert HTTP status codes
        // (202 notification ACKs, 400 malformed-body rejections).
        headers.insert(
            ":status".to_string(),
            status_line
                .split_whitespace()
                .nth(1)
                .expect("http status code")
                .to_string(),
        );
        for line in lines {
            if let Some((name, value)) = line.split_once(':') {
                headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
            }
        }
        let content_length = headers
            .get("content-length")
            .and_then(|value| value.parse::<usize>().ok())
            .expect("http response must include content-length");
        let body_start = cursor + header_end + 4;
        let body_end = body_start + content_length;
        let body = &raw[body_start..body_end];
        // Streamable HTTP acknowledges notification POSTs with 202 Accepted
        // and rejects malformed bodies with 400 Bad Request, both without a
        // JSON body. Only frames that carry a body are JSON-RPC responses.
        // Always keep headers so tests can assert :status on empty-body ACKs.
        if !body.is_empty() {
            responses.push(serde_json::from_slice(body).expect("http response body must be json"));
        }
        headers_out.push(headers);
        cursor = body_end;
    }

    (responses, headers_out)
}

#[test]
fn json_rpc_invariants_hold_across_stdio_and_http_round_trips() {
    let cases = vec![
        initialize_request(7_i64),
        initialize_request("req-7"),
        JsonRpcRequest::new("totally/unknown/method", None, 99_i64),
    ];

    for transport in TransportKind::ALL {
        for request in &cases {
            let request_json = serde_json::to_value(request).expect("serialize request");
            let expected_id = request_json["id"].clone();
            let execution = execute_transport(transport, vec![Payload::Json(request.clone())]);
            assert_eq!(
                execution.responses.len(),
                1,
                "request must yield one response for {}",
                transport.label()
            );
            let response = &execution.responses[0];
            assert_eq!(
                response["id"],
                expected_id,
                "response id must echo request id for {}",
                transport.label()
            );
            let has_result = response.get("result").is_some();
            let has_error = response.get("error").is_some();
            assert_ne!(
                has_result,
                has_error,
                "response must contain exactly one of result or error for {}",
                transport.label()
            );
        }
    }
}

#[test]
fn notifications_do_not_emit_responses_across_transports() {
    let notifications = vec![
        Payload::Json(JsonRpcRequest::notification(
            "notifications/initialized",
            None,
        )),
        Payload::Json(JsonRpcRequest::notification(
            "notifications/cancelled",
            Some(json!({"requestId": 7, "reason": "operator cancel"})),
        )),
    ];

    for transport in TransportKind::ALL {
        let execution = execute_transport(transport, notifications.clone());
        assert!(
            execution.responses.is_empty(),
            "notifications must not emit JSON-RPC responses for {}",
            transport.label()
        );
        match transport {
            // A stdio session whose opening frame is not a valid era
            // selection is refused by fastmcp 0.3.x era admission, but that
            // refusal is connection-terminal only: notifications carry no id,
            // so no JSON-RPC bytes may reach the wire.
            TransportKind::Stdio => assert!(
                execution.raw_output.is_empty(),
                "notifications must not write response bytes for stdio"
            ),
            // Streamable HTTP acknowledges each notification POST at the
            // HTTP layer with 202 Accepted and an empty body; JSON-RPC
            // itself still produces no response.
            TransportKind::Http => {
                for headers in &execution.http_headers {
                    assert_eq!(
                        headers.get(":status").map(String::as_str),
                        Some("202"),
                        "http notification frames must be 202 Accepted ACKs"
                    );
                    assert_eq!(
                        headers.get("content-length").map(String::as_str),
                        Some("0"),
                        "http notification ACKs must carry no body"
                    );
                }
            }
        }
    }
}

#[test]
fn error_shape_and_standard_codes_are_stable() {
    // JSON-RPC 2.0 taxonomy (GH#250 spec ruling): a structurally valid
    // request naming an unavailable method is METHOD_NOT_FOUND (-32601);
    // INVALID_REQUEST (-32600) is reserved for envelope-structure failures.
    // fastmcp < 2a5ee3b reported the alien-method case as -32600 from its
    // era-aware router; that was fixed upstream (fastmcp_rust 2a5ee3b, the
    // rev the ../fastmcp-rel-0328 pin now points at), so this suite asserts
    // the exact spec code.
    let cases = vec![
        (
            JsonRpcRequest::new("totally/unknown/method", None, 41_i64),
            -32601,
            "method",
        ),
        // The era-admission adapter reports method-owned params failures
        // with its frozen taxonomy message ("invalid exact MCP 2024-11-05
        // parameters"), so assert the params vocabulary rather than the
        // pre-0.3.x "required" phrasing.
        (
            JsonRpcRequest::new("tools/call", None, 42_i64),
            -32602,
            "parameters",
        ),
    ];

    for transport in TransportKind::ALL {
        for (request, expected_code, expected_message_fragment) in &cases {
            let request_json = serde_json::to_value(request).expect("serialize error request");
            let expected_id = request_json["id"].clone();
            let execution = execute_transport(
                transport,
                vec![
                    Payload::Json(initialize_request("init-error-shape")),
                    Payload::Json(JsonRpcRequest::notification(
                        "notifications/initialized",
                        None,
                    )),
                    Payload::Json(request.clone()),
                ],
            );
            let response = execution
                .responses
                .iter()
                .find(|response| response["id"] == expected_id)
                .expect("error case must return a response for the requested id");
            let error = response.get("error").cloned().unwrap_or_else(|| {
                panic!(
                    "error response must include error object for {} id {}: response={response:?} all_responses={:?}",
                    transport.label(),
                    expected_id,
                    execution.responses
                )
            });

            assert!(
                error.get("code").is_some_and(Value::is_i64),
                "error.code must be numeric for {}",
                transport.label()
            );
            assert!(
                error.get("message").is_some_and(Value::is_string),
                "error.message must be string for {}",
                transport.label()
            );
            assert_eq!(
                error["code"],
                json!(expected_code),
                "unexpected error code for {}",
                transport.label()
            );
            let lower = error["message"]
                .as_str()
                .unwrap_or_default()
                .to_ascii_lowercase();
            assert!(
                lower.contains(expected_message_fragment),
                "error message should mention {:?} for {}: {error:?}",
                expected_message_fragment,
                transport.label()
            );
        }
    }
}

#[test]
fn transport_framing_and_utf8_round_trip_are_stable() {
    let unicode_request = JsonRpcRequest::new(
        "initialize",
        Some(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "é中👋 protocol", "version": "1.0"}
        })),
        1_i64,
    );
    let encoded = format!(
        "{}\n",
        serde_json::to_string(&unicode_request).expect("serialize unicode request")
    );

    let cx = Cx::for_testing();
    let mut stdio = StdioTransport::new(Cursor::new(encoded.clone().into_bytes()), Vec::new());
    let message = stdio.recv(&cx).expect("stdio must decode unicode request");
    let JsonRpcMessage::Request(request) = message else {
        panic!("stdio unicode payload must decode as request");
    };
    assert_eq!(
        request
            .params
            .as_ref()
            .and_then(|params| params.pointer("/clientInfo/name"))
            .and_then(Value::as_str),
        Some("é中👋 protocol"),
        "stdio must preserve UTF-8 payloads"
    );

    let handler = HttpRequestHandler::with_config(agent_mail_http_config());
    let http_request = HttpRequest::new(HttpMethod::Post, "/mcp/")
        .with_header("Content-Type", "application/json")
        .with_body(encoded.into_bytes());
    let request = handler.parse_request(&http_request).expect("http parse");
    assert_eq!(
        request
            .params
            .as_ref()
            .and_then(|params| params.pointer("/clientInfo/name"))
            .and_then(Value::as_str),
        Some("é中👋 protocol"),
        "http parsing must preserve UTF-8 payloads"
    );

    let stdio_exec = execute_transport(
        TransportKind::Stdio,
        vec![Payload::Json(initialize_request(11_i64))],
    );
    let stdio_output = String::from_utf8(stdio_exec.raw_output).expect("stdio output utf8");
    assert!(
        stdio_output.ends_with('\n'),
        "stdio responses must end with LF"
    );
    assert!(
        !stdio_output.contains("\r\n"),
        "stdio responses must not emit CRLF framing"
    );

    let http_exec = execute_transport(
        TransportKind::Http,
        vec![Payload::Json(initialize_request(12_i64))],
    );
    assert_eq!(
        http_exec
            .http_headers
            .first()
            .and_then(|headers| headers.get("content-type"))
            .map(String::as_str),
        Some("application/json"),
        "http responses must advertise application/json"
    );
    assert!(
        http_exec.responses.first().is_some_and(Value::is_object),
        "http transport must emit a single JSON response object"
    );
}

#[test]
fn protocol_lifecycle_initialize_then_initialized_then_tools_list() {
    let payloads = vec![
        Payload::Json(initialize_request(1_i64)),
        Payload::Json(JsonRpcRequest::notification(
            "notifications/initialized",
            None,
        )),
        Payload::Json(JsonRpcRequest::new("tools/list", Some(json!({})), 2_i64)),
    ];

    for transport in TransportKind::ALL {
        let execution = execute_transport(transport, payloads.clone());
        assert_eq!(
            execution.responses.len(),
            2,
            "initialize + tools/list should produce exactly two responses for {}",
            transport.label()
        );
        assert_eq!(execution.responses[0]["id"], json!(1));
        let tools = execution.responses[1]
            .pointer("/result/tools")
            .and_then(Value::as_array)
            .expect("tools/list must include tools array");
        assert!(
            !tools.is_empty(),
            "tools/list must return tools after initialize for {}",
            transport.label()
        );
    }
}

#[test]
fn malformed_inputs_do_not_crash_the_server_loop() {
    for transport in TransportKind::ALL {
        match transport {
            // Stdio recovers in-stream: the malformed frame is dropped and
            // the next valid request on the same session is answered.
            TransportKind::Stdio => {
                let execution = execute_transport(
                    transport,
                    vec![
                        Payload::Raw(b"{not-json}".to_vec()),
                        Payload::Json(initialize_request(5_i64)),
                    ],
                );
                assert_eq!(
                    execution.responses.len(),
                    1,
                    "only the valid request should produce a response after malformed input for {}",
                    transport.label()
                );
                assert_eq!(
                    execution.responses[0]["id"],
                    json!(5),
                    "server should recover and answer the next valid request for {}",
                    transport.label()
                );
            }
            // A malformed HTTP body is declined without ever echoing
            // attacker-controlled bytes as a JSON-RPC response (since the
            // GH#250 fastmcp rev the listener answers 400 and keeps the
            // connection open rather than dropping it — either behavior
            // satisfies these assertions). A fresh connection must serve
            // valid traffic normally, i.e. the process survived.
            TransportKind::Http => {
                let poisoned =
                    execute_transport(transport, vec![Payload::Raw(b"{not-json}".to_vec())]);
                assert!(
                    poisoned.responses.is_empty(),
                    "malformed http input must not produce a JSON-RPC response"
                );
                if let Some(status) = poisoned
                    .http_headers
                    .first()
                    .and_then(|headers| headers.get(":status"))
                    .map(String::as_str)
                {
                    assert_eq!(
                        status, "400",
                        "malformed http body must be rejected with 400 Bad Request"
                    );
                }
                let recovered =
                    execute_transport(transport, vec![Payload::Json(initialize_request(5_i64))]);
                assert_eq!(
                    recovered.responses.len(),
                    1,
                    "a fresh http connection must serve valid traffic after a malformed one"
                );
                assert_eq!(
                    recovered.responses[0]["id"],
                    json!(5),
                    "http recovery response must echo the request id"
                );
            }
        }
    }
}

#[test]
fn large_payloads_are_explicitly_accepted_or_rejected() {
    let large_string = "x".repeat(1_048_576);
    let request = JsonRpcRequest::new(
        "tools/list",
        Some(json!({ "marker": large_string })),
        500_i64,
    );
    let encoded = format!(
        "{}\n",
        serde_json::to_string(&request).expect("serialize large request")
    );

    let cx = Cx::for_testing();
    let mut stdio = StdioTransport::new(Cursor::new(encoded.into_bytes()), Vec::new());
    let message = stdio.recv(&cx).expect("1 MiB stdio request must parse");
    let JsonRpcMessage::Request(request) = message else {
        panic!("large stdio payload must decode as request");
    };
    let marker = request
        .params
        .as_ref()
        .and_then(|params| params.get("marker"))
        .and_then(Value::as_str)
        .expect("large marker must survive parsing");
    assert_eq!(marker.len(), 1_048_576);

    let handler = HttpRequestHandler::with_config(HttpHandlerConfig {
        max_body_size: 1024,
        ..agent_mail_http_config()
    });
    let oversized_body = vec![b'x'; 2048];
    let request = HttpRequest::new(HttpMethod::Post, "/mcp/")
        .with_header("Content-Type", "application/json")
        .with_body(oversized_body);
    let error = handler
        .parse_request(&request)
        .expect_err("oversized HTTP payload must be rejected");
    let text = error.to_string();
    assert!(
        text.contains("body too large") || text.contains("max"),
        "oversized HTTP rejection should explain the limit, got: {text}"
    );
}

#[test]
fn burst_requests_preserve_request_ids() {
    let payloads = vec![
        Payload::Json(JsonRpcRequest::new("totally/unknown/method", None, 1_i64)),
        Payload::Json(JsonRpcRequest::new(
            "totally/unknown/method",
            None,
            "req-two",
        )),
        Payload::Json(JsonRpcRequest::new("totally/unknown/method", None, 3_i64)),
    ];

    for transport in TransportKind::ALL {
        let execution = execute_transport(transport, payloads.clone());
        let actual_ids: Vec<Value> = execution
            .responses
            .iter()
            .map(|response| response["id"].clone())
            .collect();
        assert_eq!(
            actual_ids,
            vec![json!(1), json!("req-two"), json!(3)],
            "burst request ids must remain aligned to responses for {}",
            transport.label()
        );
    }
}

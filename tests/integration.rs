//! Loads the compiled guest into the http-wasm host and drives it end to end.
//!
//! The live redirection.io agent is replaced by a `Fetcher` we control: it
//! records the request the guest built, and answers with an `Action` JSON as
//! the agent's `/action` endpoint would. So the test exercises the real path —
//! request JSON building, agent round-trip via `http_fetch`, action parsing,
//! status + Location application — with no network.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

use http_wasm_host::{
    FetchRequest, FetchResponse, Fetcher, HeaderKind, Host, Limits, Next, Plugin,
};

fn guest_wasm() -> &'static [u8] {
    static WASM: OnceLock<Vec<u8>> = OnceLock::new();
    WASM.get_or_init(|| {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let status = Command::new("cargo")
            .args(["build", "--release", "--target", "wasm32-unknown-unknown"])
            .current_dir(&dir)
            .status()
            .expect("cargo build for guest");
        assert!(status.success());
        std::fs::read(dir.join("target/wasm32-unknown-unknown/release/sozune_redirectionio.wasm"))
            .expect("read guest wasm")
    })
}

/// In-memory HTTP exchange. The guest reads the request line/headers and, on a
/// short-circuit, writes the response status/headers back here for assertions.
struct TestHost {
    method: String,
    uri: String,
    req_headers: HashMap<String, Vec<String>>,
    resp_headers: HashMap<String, Vec<String>>,
    status: u32,
    config: Vec<u8>,
}

impl TestHost {
    fn new(host: &str, uri: &str, config: &[u8]) -> Self {
        let mut h = HashMap::new();
        h.insert("host".to_string(), vec![host.to_string()]);
        TestHost {
            method: "GET".to_string(),
            uri: uri.to_string(),
            req_headers: h,
            resp_headers: HashMap::new(),
            status: 0,
            config: config.to_vec(),
        }
    }

    fn response_header(&self, name: &str) -> Option<String> {
        self.resp_headers
            .get(&name.to_ascii_lowercase())
            .and_then(|v| v.first())
            .cloned()
    }
}

impl Host for TestHost {
    fn method(&self) -> String {
        self.method.clone()
    }
    fn set_method(&mut self, m: &str) {
        self.method = m.to_string();
    }
    fn uri(&self) -> String {
        self.uri.clone()
    }
    fn set_uri(&mut self, u: &str) {
        self.uri = u.to_string();
    }
    fn protocol_version(&self) -> String {
        "HTTP/1.1".into()
    }
    fn source_addr(&self) -> String {
        "203.0.113.1:4444".into()
    }
    fn status_code(&self) -> u32 {
        self.status
    }
    fn set_status_code(&mut self, status: u32) {
        self.status = status;
    }
    fn header_names(&self, kind: HeaderKind) -> Vec<String> {
        match kind {
            HeaderKind::Request => self.req_headers.keys().cloned().collect(),
            HeaderKind::Response => self.resp_headers.keys().cloned().collect(),
            _ => vec![],
        }
    }
    fn header_values(&self, kind: HeaderKind, name: &str) -> Vec<String> {
        let map = match kind {
            HeaderKind::Request => &self.req_headers,
            HeaderKind::Response => &self.resp_headers,
            _ => return vec![],
        };
        map.get(&name.to_ascii_lowercase())
            .cloned()
            .unwrap_or_default()
    }
    fn set_header_value(&mut self, kind: HeaderKind, name: &str, value: &str) {
        if matches!(kind, HeaderKind::Response) {
            self.resp_headers
                .insert(name.to_ascii_lowercase(), vec![value.to_string()]);
        }
    }
    fn add_header_value(&mut self, kind: HeaderKind, name: &str, value: &str) {
        if matches!(kind, HeaderKind::Response) {
            self.resp_headers
                .entry(name.to_ascii_lowercase())
                .or_default()
                .push(value.to_string());
        }
    }
    fn remove_header(&mut self, kind: HeaderKind, name: &str) {
        if matches!(kind, HeaderKind::Response) {
            self.resp_headers.remove(&name.to_ascii_lowercase());
        }
    }
    fn read_body(&mut self, _: HeaderKind, _: usize) -> Vec<u8> {
        vec![]
    }
    fn write_body(&mut self, _: HeaderKind, _: &[u8]) {}
    fn config(&self) -> Vec<u8> {
        self.config.clone()
    }
}

/// A stand-in for the redirection.io agent. `reply` is returned for every call;
/// `seen` records the requests the guest made so the test can assert on them.
struct FakeAgent {
    reply: FetchResponse,
    seen: Mutex<Vec<FetchRequest>>,
}

impl FakeAgent {
    fn new(status: u16, body: &str) -> Arc<Self> {
        Arc::new(Self {
            reply: FetchResponse {
                status,
                headers: vec![],
                body: body.as_bytes().to_vec(),
            },
            seen: Mutex::new(Vec::new()),
        })
    }
}

impl Fetcher for FakeAgent {
    fn fetch(&self, request: FetchRequest) -> Result<FetchResponse, String> {
        self.seen.lock().unwrap().push(request);
        Ok(self.reply.clone())
    }
}

/// A 301→/new action, verbatim from the live agent (agent.redirection.io) for a
/// `/old` -> `/new` rule. The Location is nested under
/// header_filters[].filter.header / .value.
const REDIRECT_ACTION: &str = r#"{"status_code_update":{"status_code":301,"on_response_status_codes":[],"exclude_response_status_codes":false,"fallback_status_code":0,"rule_id":"7b8508e2-b24d-4fd8-aaa9-295a9f066b28","fallback_rule_id":null,"unit_id":"c57ac3c8-48f1-40dc-8aa6-3e46646bf70e","target_hash":"status_code"},"header_filters":[{"filter":{"action":"override","header":"Location","value":"/new","id":"c57ac3c8-48f1-40dc-8aa6-3e46646bf70e","target_hash":"header::location"},"on_response_status_codes":[],"exclude_response_status_codes":false,"rule_id":"7b8508e2-b24d-4fd8-aaa9-295a9f066b28"}],"body_filters":[],"rule_ids":["7b8508e2-b24d-4fd8-aaa9-295a9f066b28"],"rule_traces":[],"rules_applied":[],"log_override":null,"peer_override":null,"variables":[]}"#;

const CONFIG: &[u8] = br#"{"token":"tok-123","agent_host":"https://agent.redirection.io"}"#;

fn run(host: &str, uri: &str, fake: Arc<FakeAgent>) -> (Next, TestHost) {
    let mut h = TestHost::new(host, uri, CONFIG);
    let plugin = Plugin::from_bytes(guest_wasm(), Limits::default())
        .unwrap()
        .with_fetcher(fake);
    let next = plugin.handle_request(&mut h).unwrap();
    (next, h)
}

#[test]
fn agent_redirect_action_short_circuits() {
    let fake = FakeAgent::new(200, REDIRECT_ACTION);
    let (next, host) = run("app.example.com", "/old", fake.clone());

    assert_eq!(next, Next::Stop);
    assert_eq!(host.status_code(), 301);
    assert_eq!(host.response_header("location").as_deref(), Some("/new"));

    // The guest POSTed the request to the agent /action endpoint with the token
    // in the path, and the request body carried the incoming host and path.
    let seen = fake.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let req = &seen[0];
    assert_eq!(req.method, "POST");
    assert_eq!(req.url, "https://agent.redirection.io/tok-123/action");
    let body = String::from_utf8_lossy(&req.body);
    assert!(
        body.contains("app.example.com"),
        "body carries host: {body}"
    );
    assert!(body.contains("/old"), "body carries path: {body}");
}

#[test]
fn agent_404_forwards() {
    let fake = FakeAgent::new(404, "");
    let (next, host) = run("app.example.com", "/keep", fake);
    assert!(matches!(next, Next::Continue(_)));
    assert!(host.response_header("location").is_none());
}

#[test]
fn agent_action_without_status_forwards() {
    // A 200 whose action carries no status change means "proxy normally".
    let fake = FakeAgent::new(200, r#"{"status_code_update":null,"header_filters":[]}"#);
    let (next, host) = run("app.example.com", "/x", fake);
    assert!(matches!(next, Next::Continue(_)));
    assert!(host.response_header("location").is_none());
}

#[test]
fn agent_malformed_body_forwards() {
    let fake = FakeAgent::new(200, "not json");
    let (next, host) = run("app.example.com", "/x", fake);
    assert!(matches!(next, Next::Continue(_)));
    assert!(host.response_header("location").is_none());
}

#[test]
fn no_token_forwards_without_calling_agent() {
    let fake = FakeAgent::new(200, REDIRECT_ACTION);
    let mut h = TestHost::new("app.example.com", "/old", b"{}");
    let plugin = Plugin::from_bytes(guest_wasm(), Limits::default())
        .unwrap()
        .with_fetcher(fake.clone());
    let next = plugin.handle_request(&mut h).unwrap();
    assert!(matches!(next, Next::Continue(_)));
    assert!(fake.seen.lock().unwrap().is_empty());
}

//! Loads the compiled guest into the http-wasm host and drives it end to end.
//!
//! v2 (local matching): the ruleset is carried in the plugin config; the guest
//! matches locally via the embedded redirectionio engine. No agent, no Fetcher.
//! A `/old` -> `/new` (301) rule must short-circuit; a non-matching path passes
//! through.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use http_wasm_host::{HeaderKind, Host, Limits, Next, Plugin};

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

/// One engine-format redirect rule in the plugin config.
const CONFIG: &[u8] = br#"{"rules":[
    {"id":"r1","source":{"path":"/old"},"rank":0,"target":"/new","status_code":301}
]}"#;

fn run(host: &str, uri: &str, config: &[u8]) -> (Next, TestHost) {
    let mut h = TestHost::new(host, uri, config);
    let plugin = Plugin::from_bytes(guest_wasm(), Limits::default()).unwrap();
    let next = plugin.handle_request(&mut h).unwrap();
    (next, h)
}

#[test]
fn local_rule_short_circuits_with_redirect() {
    let (next, host) = run("app.example.com", "/old", CONFIG);
    assert_eq!(next, Next::Stop);
    assert_eq!(host.status_code(), 301);
    assert_eq!(host.response_header("location").as_deref(), Some("/new"));
}

#[test]
fn non_matching_path_forwards() {
    let (next, host) = run("app.example.com", "/keep", CONFIG);
    assert!(matches!(next, Next::Continue(_)));
    assert!(host.response_header("location").is_none());
}

#[test]
fn empty_ruleset_forwards() {
    let (next, host) = run("app.example.com", "/old", b"{\"rules\":[]}");
    assert!(matches!(next, Next::Continue(_)));
    assert!(host.response_header("location").is_none());
}

#[test]
fn no_config_forwards() {
    let (next, host) = run("app.example.com", "/old", b"{}");
    assert!(matches!(next, Next::Continue(_)));
    assert!(host.response_header("location").is_none());
}

#[test]
fn dated_rule_uses_host_clock() {
    // A window around "now" only matches if the guest reads the host clock:
    // without a request timestamp the engine treats date conditions as unmet.
    let config = br#"{"rules":[
        {"id":"d1","source":{"path":"/promo","datetime":[["2000-01-01T00:00:00Z","2999-12-31T23:59:59Z"]]},"rank":0,"target":"/sale","status_code":302}
    ]}"#;
    let (next, host) = run("app.example.com", "/promo", config);
    assert_eq!(next, Next::Stop);
    assert_eq!(host.status_code(), 302);
    assert_eq!(host.response_header("location").as_deref(), Some("/sale"));
}

#[test]
fn expired_dated_rule_forwards() {
    let config = br#"{"rules":[
        {"id":"d1","source":{"path":"/promo","datetime":[["2000-01-01T00:00:00Z","2001-01-01T00:00:00Z"]]},"rank":0,"target":"/sale","status_code":302}
    ]}"#;
    let (next, _) = run("app.example.com", "/promo", config);
    assert!(matches!(next, Next::Continue(_)));
}

#[test]
fn sampled_rule_varies_across_requests() {
    // Every request runs in a fresh instance, so a guest-local PRNG would draw
    // the same value each time and a 50% rule would be all-or-nothing. With
    // host entropy, 64 requests yield both outcomes (miss chance ~ 2^-63).
    let config = br#"{"rules":[
        {"id":"s1","source":{"path":"/ab","sampling":50},"rank":0,"target":"/b","status_code":302}
    ]}"#;
    let plugin = Plugin::from_bytes(guest_wasm(), Limits::default()).unwrap();
    let (mut redirected, mut forwarded) = (0, 0);
    for _ in 0..64 {
        let mut h = TestHost::new("app.example.com", "/ab", config);
        match plugin.handle_request(&mut h).unwrap() {
            Next::Stop => redirected += 1,
            Next::Continue(_) => forwarded += 1,
        }
    }
    assert!(
        redirected > 0 && forwarded > 0,
        "expected a mix, got {redirected} redirected / {forwarded} forwarded"
    );
}

#[test]
fn large_ruleset_is_read_in_full() {
    // ~170 KiB of config: well past the guest's first 64 KiB read, so the
    // guest must re-read with the full length. Truncated JSON would parse as
    // no rules and silently disable every redirect.
    let rules: Vec<String> = (0..2000)
        .map(|i| {
            format!(r#"{{"id":"r{i}","source":{{"path":"/old-{i}"}},"rank":0,"target":"/new-{i}","status_code":301}}"#)
        })
        .collect();
    let config = format!(r#"{{"rules":[{}]}}"#, rules.join(","));
    assert!(config.len() > 128 * 1024);

    let mut h = TestHost::new("app.example.com", "/old-1999", config.as_bytes());
    let limits = Limits {
        timeout: std::time::Duration::from_secs(5),
        ..Limits::default()
    };
    let plugin = Plugin::from_bytes(guest_wasm(), limits).unwrap();
    assert_eq!(plugin.handle_request(&mut h).unwrap(), Next::Stop);
    assert_eq!(h.response_header("location").as_deref(), Some("/new-1999"));
}

#[test]
fn reused_instance_follows_config_changes() {
    // With instance reuse the guest caches its Router. One plugin serving two
    // routes with different configs must still match each request against its
    // own route's rules, on the same instance.
    let plugin = Plugin::from_bytes(guest_wasm(), Limits::default())
        .unwrap()
        .with_instance_reuse(1);
    let other: &[u8] = br#"{"rules":[
        {"id":"r2","source":{"path":"/old"},"rank":0,"target":"/elsewhere","status_code":302}
    ]}"#;
    for (config, status, location) in [
        (CONFIG, 301, "/new"),
        (CONFIG, 301, "/new"),
        (other, 302, "/elsewhere"),
        (CONFIG, 301, "/new"),
    ] {
        let mut h = TestHost::new("app.example.com", "/old", config);
        assert_eq!(plugin.handle_request(&mut h).unwrap(), Next::Stop);
        assert_eq!(h.status_code(), status);
        assert_eq!(h.response_header("location").as_deref(), Some(location));
    }
    let mut h = TestHost::new("app.example.com", "/old", b"{\"rules\":[]}");
    assert!(matches!(
        plugin.handle_request(&mut h).unwrap(),
        Next::Continue(_)
    ));
}

#[test]
fn reused_instance_still_samples_per_request() {
    // A cached Router must not freeze sampling: draws stay per request.
    let config = br#"{"rules":[
        {"id":"s1","source":{"path":"/ab","sampling":50},"rank":0,"target":"/b","status_code":302}
    ]}"#;
    let plugin = Plugin::from_bytes(guest_wasm(), Limits::default())
        .unwrap()
        .with_instance_reuse(1);
    let (mut redirected, mut forwarded) = (0, 0);
    for _ in 0..64 {
        let mut h = TestHost::new("app.example.com", "/ab", config);
        match plugin.handle_request(&mut h).unwrap() {
            Next::Stop => redirected += 1,
            Next::Continue(_) => forwarded += 1,
        }
    }
    assert!(redirected > 0 && forwarded > 0, "{redirected}/{forwarded}");
}

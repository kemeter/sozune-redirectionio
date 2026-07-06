//! redirection.io redirect engine as an http-wasm plugin for Sōzune — **local
//! matching**, no agent.
//!
//! The plugin embeds redirection.io's own [`redirectionio`] crate (the engine
//! its agent and nginx/Cloudflare integrations use) and matches rules **locally**
//! inside the wasm guest. Rules are carried in the plugin config as an array of
//! engine-format `api::Rule` objects; a separate out-of-band process refreshes
//! them periodically from redirection.io. There is no per-request network call
//! and no agent to keep running.
//!
//! Per request it builds the redirection.io request model, matches it against
//! the compiled `Router`, and applies the resulting action: a redirect writes
//! the status + `Location` and short-circuits (the backend is never called);
//! no match lets the request continue.
//!
//! Configuration (JSON via `get_config`):
//! ```json
//! { "rules": [ { "id": "...", "source": {"path":"/old"}, "rank": 0,
//!               "target": "/new", "status_code": 301 } ] }
//! ```
//! Each entry is an engine-format `redirectionio::api::Rule`. Producing these
//! from the Public API `GET /rules` (editor format) is the sync process's job,
//! out of scope for the guest (see PLAN-v2.md).
//!
//! Targets wasm32-unknown-unknown, run under wasmtime. Because that host is not
//! wasm-bindgen, the redirectionio/chrono/getrandom JS deps are patched out via
//! `[patch.crates-io]` (see Cargo.toml) and getrandom uses a custom backend.

use redirectionio::RouterConfig;
use redirectionio::action::Action;
use redirectionio::api::Rule;
use redirectionio::http::{Header, PathAndQueryWithSkipped, Request};
use redirectionio::router::Router;

// The editor->engine rule translator runs host-side (the sync process), never
// in the wasm guest, so it is compiled out of the wasm build.
#[cfg(not(target_arch = "wasm32"))]
pub mod translate;

/// getrandom custom backend (enabled via `--cfg getrandom_backend="custom"`).
/// redirectionio pulls `rand -> getrandom`, whose default wasm32 backend imports
/// JS; wasmtime has none. Randomness here is only rule sampling (an A/B
/// percentage), not security-sensitive, so a small non-crypto PRNG suffices.
#[cfg(target_arch = "wasm32")]
#[unsafe(no_mangle)]
unsafe extern "Rust" fn __getrandom_v03_custom(
    dest: *mut u8,
    len: usize,
) -> Result<(), getrandom::Error> {
    use core::sync::atomic::{AtomicU64, Ordering};
    static STATE: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);
    let mut x = STATE.fetch_add(0x2545_F491_4F6C_DD1D, Ordering::Relaxed) | 1;
    let buf = unsafe { core::slice::from_raw_parts_mut(dest, len) };
    for byte in buf.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *byte = (x & 0xff) as u8;
    }
    Ok(())
}

const REQUEST: i32 = 0;
const RESPONSE: i32 = 1;

#[link(wasm_import_module = "http_handler")]
unsafe extern "C" {
    fn get_config(buf: i32, buf_limit: i32) -> i32;
    fn get_method(buf: i32, buf_limit: i32) -> i32;
    fn get_uri(buf: i32, buf_limit: i32) -> i32;
    fn get_header_names(kind: i32, buf: i32, buf_limit: i32) -> i64;
    fn get_header_values(kind: i32, name: i32, name_len: i32, buf: i32, buf_limit: i32) -> i64;
    fn set_status_code(status: i32);
    fn set_header_value(kind: i32, name: i32, name_len: i32, value: i32, value_len: i32);
}

fn read_via<F: Fn(i32, i32) -> i32>(cap: usize, f: F) -> String {
    let mut buf = vec![0u8; cap];
    let len = f(buf.as_mut_ptr() as i32, cap as i32);
    let len = if len < 0 { 0 } else { (len as usize).min(cap) };
    buf.truncate(len);
    String::from_utf8(buf).unwrap_or_default()
}

fn request_header_names() -> Vec<String> {
    let mut buf = vec![0u8; 4096];
    let res = unsafe { get_header_names(REQUEST, buf.as_mut_ptr() as i32, 4096) };
    let len = (res & 0xffff_ffff) as usize;
    buf.truncate(len.min(4096));
    split_nul(&buf)
}

fn request_header_values(name: &str) -> Vec<String> {
    let mut buf = vec![0u8; 2048];
    let res = unsafe {
        get_header_values(
            REQUEST,
            name.as_ptr() as i32,
            name.len() as i32,
            buf.as_mut_ptr() as i32,
            2048,
        )
    };
    let len = (res & 0xffff_ffff) as usize;
    buf.truncate(len.min(2048));
    split_nul(&buf)
}

fn split_nul(buf: &[u8]) -> Vec<String> {
    buf.split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

fn read_request_headers() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for name in request_header_names() {
        for value in request_header_values(&name) {
            out.push((name.clone(), value));
        }
    }
    out
}

fn write_response_header(name: &str, value: &str) {
    unsafe {
        set_header_value(
            RESPONSE,
            name.as_ptr() as i32,
            name.len() as i32,
            value.as_ptr() as i32,
            value.len() as i32,
        );
    }
}

/// Parse the config's `rules` array into engine-format `Rule`s. Unparseable
/// entries are skipped (log-and-skip would need host logging; here we just drop
/// them) so one malformed rule doesn't sink the whole ruleset.
fn parse_ruleset(config_json: &str) -> Vec<Rule> {
    let value: serde_json::Value = match serde_json::from_str(config_json) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let arr = match value.get("rules").and_then(|r| r.as_array()) {
        Some(a) => a,
        None => return Vec::new(),
    };
    arr.iter()
        .filter_map(|r| serde_json::from_value::<Rule>(r.clone()).ok())
        .collect()
}

/// Build a Router from a ruleset. Returns None if empty (nothing to match).
fn build_router(rules: Vec<Rule>) -> Option<Router<Rule>> {
    if rules.is_empty() {
        return None;
    }
    let mut router = Router::<Rule>::from_config(RouterConfig::default());
    for rule in rules {
        router.insert(rule);
    }
    router.cache(Some(100));
    Some(router)
}

/// Build the redirection.io request model from request parts. Pure (no ABI),
/// so unit-testable off-wasm. `created_at: None` avoids `Utc::now()`, which
/// traps under wasmtime.
fn build_rio_request(
    host: &str,
    scheme: &str,
    method: &str,
    path_and_query: &str,
    headers: Vec<(String, String)>,
) -> Request {
    let mut request = Request {
        path_and_query_skipped: PathAndQueryWithSkipped::from_static(path_and_query),
        path_and_query: Some(path_and_query.to_string()),
        host: (!host.is_empty()).then(|| host.to_string()),
        scheme: (!scheme.is_empty()).then(|| scheme.to_string()),
        method: (!method.is_empty()).then(|| method.to_string()),
        headers: Vec::new(),
        remote_addr: None,
        created_at: None,
        sampling_override: None,
    };
    for (name, value) in headers {
        request.add_header(name, value, false);
    }
    request
}

/// Sōzune terminates TLS; report https for rule evaluation (http-wasm does not
/// expose the scheme).
fn scheme() -> &'static str {
    "https"
}

#[unsafe(no_mangle)]
pub extern "C" fn handle_request() -> i64 {
    let cfg = read_via(65536, |p, l| unsafe { get_config(p, l) });
    let Some(router) = build_router(parse_ruleset(&cfg)) else {
        return 1; // no rules → let the request through
    };

    let host = request_header_values("host")
        .into_iter()
        .next()
        .unwrap_or_default();
    let method = read_via(16, |p, l| unsafe { get_method(p, l) });
    let uri = read_via(2048, |p, l| unsafe { get_uri(p, l) });

    let request = build_rio_request(&host, scheme(), &method, &uri, read_request_headers());
    let request = Request::rebuild_with_config(&router.config, &request);

    let matched = router.match_request(&request);
    if matched.is_empty() {
        return 1; // no rule matched → forward untouched
    }
    let mut action = Action::from_routes_rule(matched, &request, None);

    let status = action.get_status_code(0, None);
    if status == 0 {
        return 1; // action carries no status change → forward
    }
    unsafe {
        set_status_code(status as i32);
    }
    for header in action.filter_headers(Vec::<Header>::new(), status, false, None) {
        write_response_header(&header.name, &header.value);
    }
    // next=0 → short-circuit; Sōzune returns the response we just wrote.
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn handle_response(_req_ctx: i32, _is_error: i32) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal engine-format redirect rule: /old -> /new, 301.
    const RULESET: &str = r#"{"rules":[
        {"id":"r1","source":{"path":"/old"},"rank":0,"target":"/new","status_code":301}
    ]}"#;

    #[test]
    fn parses_ruleset_into_rules() {
        let rules = parse_ruleset(RULESET);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id, "r1");
        assert_eq!(rules[0].source.path, "/old");
        assert_eq!(rules[0].target.as_deref(), Some("/new"));
        assert_eq!(rules[0].status_code, Some(301));
    }

    #[test]
    fn empty_or_missing_rules_yields_no_router() {
        assert!(parse_ruleset("{}").is_empty());
        assert!(parse_ruleset(r#"{"rules":[]}"#).is_empty());
        assert!(build_router(parse_ruleset(r#"{"rules":[]}"#)).is_none());
    }

    #[test]
    fn skips_malformed_rule_keeps_valid_one() {
        let cfg = r#"{"rules":[{"bad":true},{"id":"ok","source":{"path":"/x"},"rank":0,"target":"/y","status_code":302}]}"#;
        let rules = parse_ruleset(cfg);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id, "ok");
    }

    #[test]
    fn matches_locally_and_yields_redirect() {
        // The core Phase-1 proof, off-wasm: ruleset -> Router -> match -> Action.
        let router = build_router(parse_ruleset(RULESET)).expect("a router");
        let req = build_rio_request("app.example.com", "https", "GET", "/old", vec![]);
        let req = Request::rebuild_with_config(&router.config, &req);
        let matched = router.match_request(&req);
        assert!(!matched.is_empty(), "/old should match");
        let mut action = Action::from_routes_rule(matched, &req, None);
        assert_eq!(action.get_status_code(0, None), 301);
        let headers = action.filter_headers(Vec::<Header>::new(), 0, false, None);
        assert!(
            headers
                .iter()
                .any(|h| h.name.eq_ignore_ascii_case("location") && h.value == "/new"),
            "Location should be /new, got {headers:?}"
        );
    }

    #[test]
    fn non_matching_path_does_not_match() {
        let router = build_router(parse_ruleset(RULESET)).expect("a router");
        let req = build_rio_request("app.example.com", "https", "GET", "/keep", vec![]);
        let req = Request::rebuild_with_config(&router.config, &req);
        assert!(router.match_request(&req).is_empty());
    }

    #[test]
    fn request_built_without_now() {
        let req = build_rio_request("example.com", "https", "GET", "/p?x=1", vec![]);
        assert!(req.created_at.is_none());
        assert_eq!(req.host.as_deref(), Some("example.com"));
    }
}

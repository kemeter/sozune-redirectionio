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
//! `[patch.crates-io]` (see Cargo.toml). The guest has no clock or entropy of
//! its own, so it takes both from the host's `get_time_unix_nanos` and
//! `get_random` extensions (getrandom uses a custom backend wired to the
//! latter).

use chrono::{DateTime, Utc};
use redirectionio::action::Action;
use redirectionio::api::Rule;
use redirectionio::http::{Header, PathAndQueryWithSkipped, Request};
use redirectionio::router::Router;
use redirectionio::RouterConfig;

// The editor->engine rule translator runs host-side (the sync process), never
// in the wasm guest, so it is compiled out of the wasm build.
#[cfg(not(target_arch = "wasm32"))]
pub mod translate;

/// getrandom custom backend (enabled via `--cfg getrandom_backend="custom"`).
/// redirectionio pulls `rand -> getrandom`, whose default wasm32 backend imports
/// JS; wasmtime has none. The bytes come from the host's `get_random`
/// extension instead. A local PRNG would not do: the host runs every request in
/// a fresh instance, so its state would restart identically each time and rule
/// sampling would always draw the same value.
#[cfg(target_arch = "wasm32")]
#[unsafe(no_mangle)]
unsafe extern "Rust" fn __getrandom_v03_custom(
    dest: *mut u8,
    len: usize,
) -> Result<(), getrandom::Error> {
    if len == 0 {
        return Ok(());
    }
    let Ok(len) = i32::try_from(len) else {
        return Err(getrandom::Error::new_custom(0));
    };
    match unsafe { get_random(dest as i32, len) } {
        1 => Ok(()),
        _ => Err(getrandom::Error::new_custom(1)),
    }
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
    // Non-standard host extensions (clock and entropy).
    fn get_time_unix_nanos() -> i64;
    // Only the wasm32 getrandom backend calls it.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    fn get_random(buf: i32, len: i32) -> i32;
}

/// Current time from the host clock. `chrono::Utc::now()` is unavailable: it
/// traps under wasmtime without the JS bindings we patched out.
fn now() -> Option<DateTime<Utc>> {
    let nanos = unsafe { get_time_unix_nanos() };
    (nanos > 0).then(|| DateTime::from_timestamp_nanos(nanos))
}

fn read_via<F: Fn(i32, i32) -> i32>(cap: usize, f: F) -> String {
    let mut buf = vec![0u8; cap];
    let len = f(buf.as_mut_ptr() as i32, cap as i32);
    let len = if len < 0 { 0 } else { (len as usize).min(cap) };
    buf.truncate(len);
    String::from_utf8(buf).unwrap_or_default()
}

/// Read the whole plugin config. `get_config` returns the full length even
/// when the buffer is smaller, so re-call with an exact-size buffer instead of
/// truncating: a cut JSON ruleset would parse as no rules at all.
fn read_config() -> String {
    const FIRST: usize = 64 * 1024;
    let mut buf = vec![0u8; FIRST];
    let len = unsafe { get_config(buf.as_mut_ptr() as i32, FIRST as i32) }.max(0) as usize;
    if len > FIRST {
        buf = vec![0u8; len];
        unsafe { get_config(buf.as_mut_ptr() as i32, len as i32) };
    }
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
/// so unit-testable off-wasm. `created_at` drives date/time/weekday rule
/// conditions; without it such rules never match.
fn build_rio_request(
    host: &str,
    scheme: &str,
    method: &str,
    path_and_query: &str,
    headers: Vec<(String, String)>,
    created_at: Option<DateTime<Utc>>,
) -> Request {
    let mut request = Request {
        path_and_query_skipped: PathAndQueryWithSkipped::from_static(path_and_query),
        path_and_query: Some(path_and_query.to_string()),
        host: (!host.is_empty()).then(|| host.to_string()),
        scheme: (!scheme.is_empty()).then(|| scheme.to_string()),
        method: (!method.is_empty()).then(|| method.to_string()),
        headers: Vec::new(),
        remote_addr: None,
        created_at,
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
    let cfg = read_config();
    with_cached_router(&cfg, |router| match router {
        Some(router) => route_request(router),
        None => 1, // no rules → let the request through
    })
}

thread_local! {
    /// The Router built from the last config seen, keyed by a hash of that
    /// config. Only useful when the host reuses guest instances; with a fresh
    /// instance per request it is simply rebuilt each time. Keyed so that one
    /// instance serving several routes (each with its own config) never
    /// matches against another route's rules.
    static ROUTER: std::cell::RefCell<Option<(u64, Option<Router<Rule>>)>> =
        const { std::cell::RefCell::new(None) };
}

fn config_key(cfg: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    cfg.hash(&mut hasher);
    hasher.finish()
}

/// Run `f` with the Router for `cfg`, building it only if the config changed.
fn with_cached_router<R>(cfg: &str, f: impl FnOnce(Option<&Router<Rule>>) -> R) -> R {
    let key = config_key(cfg);
    ROUTER.with(|cell| {
        let mut cached = cell.borrow_mut();
        if cached.as_ref().is_none_or(|(k, _)| *k != key) {
            *cached = Some((key, build_router(parse_ruleset(cfg))));
        }
        let (_, router) = cached.as_ref().expect("just filled");
        f(router.as_ref())
    })
}

/// Match the current request against `router` and apply the action.
fn route_request(router: &Router<Rule>) -> i64 {
    let host = request_header_values("host")
        .into_iter()
        .next()
        .unwrap_or_default();
    let method = read_via(16, |p, l| unsafe { get_method(p, l) });
    let uri = read_via(2048, |p, l| unsafe { get_uri(p, l) });

    let request = build_rio_request(
        &host,
        scheme(),
        &method,
        &uri,
        read_request_headers(),
        now(),
    );
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
        let req = build_rio_request("app.example.com", "https", "GET", "/old", vec![], None);
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
        let req = build_rio_request("app.example.com", "https", "GET", "/keep", vec![], None);
        let req = Request::rebuild_with_config(&router.config, &req);
        assert!(router.match_request(&req).is_empty());
    }

    /// A redirect limited to a datetime window: /promo -> /sale.
    fn dated_router(from: &str, to: &str) -> Router<Rule> {
        let cfg = format!(
            r#"{{"rules":[{{"id":"d1","source":{{"path":"/promo","datetime":[["{from}","{to}"]]}},"rank":0,"target":"/sale","status_code":302}}]}}"#
        );
        build_router(parse_ruleset(&cfg)).expect("a router")
    }

    fn matches_at(router: &Router<Rule>, at: Option<DateTime<Utc>>) -> bool {
        let req = build_rio_request("example.com", "https", "GET", "/promo", vec![], at);
        let req = Request::rebuild_with_config(&router.config, &req);
        !router.match_request(&req).is_empty()
    }

    #[test]
    fn dated_rule_matches_inside_its_window_only() {
        let router = dated_router("2026-01-01T00:00:00Z", "2026-12-31T23:59:59Z");
        let inside = "2026-11-06T12:00:00Z".parse().unwrap();
        let after = "2027-02-01T00:00:00Z".parse().unwrap();
        assert!(matches_at(&router, Some(inside)));
        assert!(!matches_at(&router, Some(after)));
    }

    #[test]
    fn dated_rule_never_matches_without_a_clock() {
        // Why the guest must pass the host time: with no created_at the engine
        // treats every date condition as unmet.
        let router = dated_router("2000-01-01T00:00:00Z", "2999-12-31T23:59:59Z");
        assert!(!matches_at(&router, None));
    }
}

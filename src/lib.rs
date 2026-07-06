//! redirection.io redirect engine as an http-wasm plugin for Sōzune.
//!
//! Per request it asks the redirection.io **agent** what to do and applies the
//! answer:
//!
//! 1. build a redirection.io request JSON from the incoming request;
//! 2. POST it to the agent at `{agent_host}/{token}/action` via the host's
//!    `http_fetch` extension;
//! 3. read the returned `Action`: if it carries a status code (a redirect or
//!    hard response) write that status plus the `Location` header and
//!    short-circuit, so the backend is never called; otherwise let the request
//!    continue untouched.
//!
//! The agent (not the rate-limited public API) is queried: it keeps the
//! compiled rules locally and answers per request. `agent_host` must be listed
//! in the plugin's `allowed_hosts` so Sōzune permits the outbound call.
//!
//! The request and action JSON are built and read by hand rather than by
//! linking redirection.io's `redirectionio` crate: that crate is wired for a
//! wasm-bindgen (JS) host and does not load under Sōzune's wasmtime host. The
//! wire shapes handled here mirror `redirectionio::http::Request` (note the
//! `path_and_query` / `path_and_query_v2` serde renames) and
//! `redirectionio::action::Action` (`status_code_update` + header filters).
//!
//! Configuration (JSON via `get_config`):
//! ```json
//! { "token": "<project-token>", "agent_host": "https://agent.redirection.io" }
//! ```
//! `agent_host` defaults to `https://agent.redirection.io` when absent.
//!
//! Targets wasm32-unknown-unknown; std is used only for its allocator (no I/O).

const REQUEST: i32 = 0;
const RESPONSE: i32 = 1;

/// Default redirection.io agent endpoint, matching the Cloudflare/Fastly worker.
const DEFAULT_AGENT_HOST: &str = "https://agent.redirection.io";

#[link(wasm_import_module = "http_handler")]
unsafe extern "C" {
    fn get_config(buf: i32, buf_limit: i32) -> i32;
    fn get_method(buf: i32, buf_limit: i32) -> i32;
    fn get_uri(buf: i32, buf_limit: i32) -> i32;
    fn get_header_names(kind: i32, buf: i32, buf_limit: i32) -> i64;
    fn get_header_values(kind: i32, name: i32, name_len: i32, buf: i32, buf_limit: i32) -> i64;
    fn set_status_code(status: i32);
    fn set_header_value(kind: i32, name: i32, name_len: i32, value: i32, value_len: i32);
    fn http_fetch(req_ptr: i32, req_len: i32, resp_ptr: i32, resp_limit: i32) -> i64;
}

fn read_via<F: Fn(i32, i32) -> i32>(cap: usize, f: F) -> String {
    let mut buf = vec![0u8; cap];
    let len = f(buf.as_mut_ptr() as i32, cap as i32);
    let len = if len < 0 { 0 } else { (len as usize).min(cap) };
    buf.truncate(len);
    String::from_utf8(buf).unwrap_or_default()
}

/// NUL-separated list of request header names (lowercased by the host).
fn request_header_names() -> Vec<String> {
    let mut buf = vec![0u8; 4096];
    let res = unsafe { get_header_names(REQUEST, buf.as_mut_ptr() as i32, 4096) };
    let len = (res & 0xffff_ffff) as usize;
    buf.truncate(len.min(4096));
    split_nul(&buf)
}

/// All values of a request header (NUL-separated list).
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

/// Read every request header as `(name, value)` pairs via the ABI. Separate
/// from the JSON builder so the latter stays testable off-wasm.
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

// --- JSON helpers (no serde: hand-built request, hand-read action) ---

/// JSON-escape a string value (the characters JSON requires escaped).
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Extract a flat string field `"key": "value"` from a JSON fragment.
fn json_str_field(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let start = json.find(&needle)? + needle.len();
    let after = &json[start..];
    let colon = after.find(':')?;
    let rest = &after[colon + 1..];
    let q1 = rest.find('"')? + 1;
    // Find the closing quote, honoring backslash escapes.
    let bytes = rest.as_bytes();
    let mut i = q1;
    let mut value = String::new();
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if i + 1 < bytes.len() => {
                match bytes[i + 1] {
                    b'"' => value.push('"'),
                    b'\\' => value.push('\\'),
                    b'n' => value.push('\n'),
                    b'r' => value.push('\r'),
                    b't' => value.push('\t'),
                    b'/' => value.push('/'),
                    other => {
                        value.push('\\');
                        value.push(other as char);
                    }
                }
                i += 2;
            }
            b'"' => return Some(value),
            b => {
                value.push(b as char);
                i += 1;
            }
        }
    }
    None
}

/// Extract a flat numeric field `"key": 123` from a JSON fragment.
fn json_num_field(json: &str, key: &str) -> Option<u32> {
    let needle = format!("\"{key}\"");
    let start = json.find(&needle)? + needle.len();
    let after = &json[start..];
    let colon = after.find(':')?;
    let rest = after[colon + 1..].trim_start();
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// The scheme reported to redirection.io. http-wasm does not expose it; Sōzune
/// terminates TLS, so https is the correct default for evaluated traffic.
fn scheme() -> &'static str {
    "https"
}

/// Build the redirection.io request JSON the agent's `/action` endpoint expects.
///
/// Mirrors `redirectionio::http::Request`'s serde model: the composite
/// `path_and_query` object (renamed from `path_and_query_skipped`), the plain
/// `path_and_query_v2` string (renamed from `path_and_query`), host/scheme/
/// method, and the header list. `created_at` is null (the agent stamps its own).
fn build_request_json(
    host: &str,
    scheme: &str,
    method: &str,
    path_and_query: &str,
    headers: &[(String, String)],
) -> String {
    let mut headers_json = String::from("[");
    for (i, (name, value)) in headers.iter().enumerate() {
        if i > 0 {
            headers_json.push(',');
        }
        headers_json.push_str(&format!(
            "{{\"name\":\"{}\",\"value\":\"{}\"}}",
            esc(name),
            esc(value)
        ));
    }
    headers_json.push(']');

    let opt = |v: &str| {
        if v.is_empty() {
            "null".to_string()
        } else {
            format!("\"{}\"", esc(v))
        }
    };

    format!(
        concat!(
            "{{",
            "\"path_and_query\":{{",
            "\"path_and_query\":\"{pq}\",",
            "\"path_and_query_matching\":\"{pq}\",",
            "\"skipped_query_params\":null,",
            "\"original\":\"{pq}\"",
            "}},",
            "\"path_and_query_v2\":\"{pq}\",",
            "\"host\":{host},\"scheme\":{scheme},\"method\":{method},",
            "\"headers\":{headers},",
            "\"remote_addr\":null,\"created_at\":null,\"sampling_override\":null",
            "}}"
        ),
        pq = esc(path_and_query),
        host = opt(host),
        scheme = opt(scheme),
        method = opt(method),
        headers = headers_json,
    )
}

/// A redirect decision read from the agent's `Action` JSON.
struct Decision {
    status: u16,
    location: Option<String>,
}

/// Read the redirect decision from an `Action` JSON.
///
/// The status lives in `status_code_update.status_code` (0 or absent means "no
/// status change" → proxy normally). The `Location` lives in a header filter
/// whose `name` is `Location`; we surface its `value`. Returns `None` when the
/// action carries no redirecting status.
fn parse_action(json: &str) -> Option<Decision> {
    let scu = extract_object(json, "status_code_update")?;
    let status = json_num_field(&scu, "status_code")? as u16;
    if status == 0 {
        return None;
    }
    let location = find_header_filter_value(json, "location");
    Some(Decision { status, location })
}

/// Return the substring of the JSON object value for `key`, from its opening
/// brace to the matching closing brace (brace-balanced, string-aware).
fn extract_object(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let start = json.find(&needle)? + needle.len();
    let after = &json[start..];
    let brace = after.find('{')?;
    let bytes = after.as_bytes();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(brace) {
        if in_str {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(after[brace..=i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// Find the value of the header filter whose header name matches `target`
/// (case-insensitive).
///
/// The agent nests each header filter as
/// `{"filter":{"action":"override","header":"Location","value":"/new",...},...}`
/// — the header name is under the `header` key (not `name`), inside the inner
/// `filter` object. We scan every `"header"` occurrence and, when it matches,
/// read the sibling `value` from the same object window.
fn find_header_filter_value(json: &str, target: &str) -> Option<String> {
    let mut rest = json;
    while let Some(pos) = rest.find("\"header\"") {
        // Bound the search to the object window around this header/value pair.
        let window = &rest[pos..(pos + 512).min(rest.len())];
        if let Some(header) = json_str_field(window, "header") {
            if header.eq_ignore_ascii_case(target) {
                if let Some(value) = json_str_field(window, "value") {
                    return Some(value);
                }
            }
        }
        rest = &rest[pos + 8..];
    }
    None
}

// --- http_fetch plumbing ---

/// A decoded `http_fetch` response.
struct FetchResponse {
    status: u16,
    body: Vec<u8>,
}

/// POST the request JSON to the agent and return its `Action` body. `None` on
/// denial, non-2xx, or transport error — the caller then forwards untouched.
fn fetch_action(agent_host: &str, token: &str, request_json: &str) -> Option<FetchResponse> {
    let url = format!("{agent_host}/{token}/action");
    let headers = [
        ("Content-Type", "application/json"),
        ("User-Agent", "sozune-redirectionio"),
    ];
    http_fetch_call("POST", &url, &headers, request_json.as_bytes())
}

fn http_fetch_call(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Option<FetchResponse> {
    let mut req = Vec::new();
    put(&mut req, method.as_bytes());
    put(&mut req, url.as_bytes());
    req.extend_from_slice(&(headers.len() as u32).to_le_bytes());
    for (name, value) in headers {
        put(&mut req, name.as_bytes());
        put(&mut req, value.as_bytes());
    }
    put(&mut req, body);

    let mut cap = 8192usize;
    loop {
        let mut resp_buf = vec![0u8; cap];
        let packed = unsafe {
            http_fetch(
                req.as_ptr() as i32,
                req.len() as i32,
                resp_buf.as_mut_ptr() as i32,
                cap as i32,
            )
        };
        let ok = (packed >> 32) as u32 == 1;
        let full = (packed & 0xffff_ffff) as usize;
        if !ok {
            return None;
        }
        if full > cap {
            cap = full; // response didn't fit; grow and re-call
            continue;
        }
        resp_buf.truncate(full);
        return decode_fetch_response(&resp_buf);
    }
}

/// Decode the host's response wire format: `status(u32), header-count,
/// [name,value]*, body`. Only status and body are needed.
fn decode_fetch_response(buf: &[u8]) -> Option<FetchResponse> {
    let mut pos = 0usize;
    let status = take_u32(buf, &mut pos)? as u16;
    let count = take_u32(buf, &mut pos)?;
    for _ in 0..count {
        skip_bytes(buf, &mut pos)?; // name
        skip_bytes(buf, &mut pos)?; // value
    }
    let body = take_bytes(buf, &mut pos)?;
    Some(FetchResponse { status, body })
}

fn take_u32(buf: &[u8], pos: &mut usize) -> Option<u32> {
    let end = pos.checked_add(4)?;
    if end > buf.len() {
        return None;
    }
    let v = u32::from_le_bytes(buf[*pos..end].try_into().ok()?);
    *pos = end;
    Some(v)
}

fn take_bytes(buf: &[u8], pos: &mut usize) -> Option<Vec<u8>> {
    let len = take_u32(buf, pos)? as usize;
    let end = pos.checked_add(len)?;
    if end > buf.len() {
        return None;
    }
    let out = buf[*pos..end].to_vec();
    *pos = end;
    Some(out)
}

fn skip_bytes(buf: &[u8], pos: &mut usize) -> Option<()> {
    let len = take_u32(buf, pos)? as usize;
    *pos = pos.checked_add(len).filter(|&e| e <= buf.len())?;
    Some(())
}

/// Length-prefix `data` into `out` (u32 LE length, then bytes) — the host's wire
/// format for `http_fetch` request fields.
fn put(out: &mut Vec<u8>, data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
}

#[unsafe(no_mangle)]
pub extern "C" fn handle_request() -> i64 {
    let cfg = read_via(2048, |p, l| unsafe { get_config(p, l) });
    let Some(token) = json_str_field(&cfg, "token").filter(|t| !t.is_empty()) else {
        return 1; // no token → nothing we can do, let the request through
    };
    let agent_host =
        json_str_field(&cfg, "agent_host").unwrap_or_else(|| DEFAULT_AGENT_HOST.to_string());

    let host = request_header_values("host")
        .into_iter()
        .next()
        .unwrap_or_default();
    let method = read_via(16, |p, l| unsafe { get_method(p, l) });
    let uri = read_via(2048, |p, l| unsafe { get_uri(p, l) });
    let request_json = build_request_json(&host, scheme(), &method, &uri, &read_request_headers());

    let Some(resp) = fetch_action(&agent_host, &token, &request_json) else {
        return 1; // agent unreachable / denied → forward
    };
    if !(200..300).contains(&resp.status) {
        return 1; // 404 = no rule; other non-2xx = no decision → forward
    }
    let body = String::from_utf8_lossy(&resp.body);
    let Some(decision) = parse_action(&body) else {
        return 1; // action carries no redirecting status → forward
    };

    unsafe {
        set_status_code(decision.status as i32);
    }
    if let Some(location) = decision.location {
        write_response_header("location", &location);
    }
    // next=0 → short-circuit; Sōzune returns the response we just wrote.
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn handle_response(_req_ctx: i32, _is_error: i32) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_token_and_agent_host() {
        let cfg = r#"{"token":"abc","agent_host":"https://agent.example"}"#;
        assert_eq!(json_str_field(cfg, "token").as_deref(), Some("abc"));
        assert_eq!(
            json_str_field(cfg, "agent_host").as_deref(),
            Some("https://agent.example")
        );
    }

    #[test]
    fn missing_agent_host_field_is_none() {
        assert!(json_str_field(r#"{"token":"abc"}"#, "agent_host").is_none());
    }

    #[test]
    fn json_str_field_handles_escapes() {
        let json = r#"{"value":"a\"b\\c"}"#;
        assert_eq!(json_str_field(json, "value").as_deref(), Some("a\"b\\c"));
    }

    #[test]
    fn esc_escapes_quotes_and_backslashes() {
        assert_eq!(esc("a\"b\\c"), "a\\\"b\\\\c");
    }

    #[test]
    fn request_json_has_renamed_fields_and_headers() {
        let json = build_request_json(
            "example.com",
            "https",
            "GET",
            "/p?x=1",
            &[("accept".to_string(), "text/html".to_string())],
        );
        // Composite object under the "path_and_query" key + the v2 string.
        assert!(json.contains("\"path_and_query\":{"));
        assert!(json.contains("\"path_and_query_v2\":\"/p?x=1\""));
        assert!(json.contains("\"original\":\"/p?x=1\""));
        assert!(json.contains("\"host\":\"example.com\""));
        assert!(json.contains("\"method\":\"GET\""));
        assert!(json.contains("{\"name\":\"accept\",\"value\":\"text/html\"}"));
    }

    #[test]
    fn request_json_empty_host_is_null() {
        let json = build_request_json("", "https", "GET", "/", &[]);
        assert!(json.contains("\"host\":null"));
        assert!(json.contains("\"headers\":[]"));
    }

    #[test]
    fn parse_action_reads_status_and_location() {
        // Verbatim Action returned by the live agent for a 301 /old -> /new rule
        // (captured from agent.redirection.io). Note the Location lives under
        // header_filters[].filter.header / .value, not a flat name/value.
        let action = r#"{"status_code_update":{"status_code":301,"on_response_status_codes":[],"exclude_response_status_codes":false,"fallback_status_code":0,"rule_id":"7b8508e2-b24d-4fd8-aaa9-295a9f066b28","fallback_rule_id":null,"unit_id":"c57ac3c8-48f1-40dc-8aa6-3e46646bf70e","target_hash":"status_code"},"header_filters":[{"filter":{"action":"override","header":"Location","value":"/new","id":"c57ac3c8-48f1-40dc-8aa6-3e46646bf70e","target_hash":"header::location"},"on_response_status_codes":[],"exclude_response_status_codes":false,"rule_id":"7b8508e2-b24d-4fd8-aaa9-295a9f066b28"}],"body_filters":[],"rule_ids":["7b8508e2-b24d-4fd8-aaa9-295a9f066b28"],"rule_traces":[],"rules_applied":[],"log_override":null,"peer_override":null,"variables":[]}"#;
        let d = parse_action(action).expect("a redirect decision");
        assert_eq!(d.status, 301);
        assert_eq!(d.location.as_deref(), Some("/new"));
    }

    #[test]
    fn parse_action_zero_status_is_no_decision() {
        let action = r#"{"status_code_update":{"status_code":0,"fallback_status_code":0}}"#;
        assert!(parse_action(action).is_none());
    }

    #[test]
    fn parse_action_missing_update_is_no_decision() {
        // An Action with status_code_update: null means "proxy normally".
        let action = r#"{"status_code_update":null,"header_filters":[]}"#;
        assert!(parse_action(action).is_none());
    }

    #[test]
    fn parse_action_redirect_without_location() {
        // A hard status (e.g. 410 Gone) with no Location is still a decision.
        let action = r#"{"status_code_update":{"status_code":410,"fallback_status_code":0},"header_filters":[]}"#;
        let d = parse_action(action).expect("a decision");
        assert_eq!(d.status, 410);
        assert!(d.location.is_none());
    }

    #[test]
    fn extract_object_is_brace_balanced() {
        let json = r#"{"a":{"b":{"c":1},"d":2},"e":3}"#;
        assert_eq!(
            extract_object(json, "a").as_deref(),
            Some("{\"b\":{\"c\":1},\"d\":2}")
        );
    }

    #[test]
    fn find_header_filter_value_is_case_insensitive() {
        // Header name is under filter.header, matched case-insensitively.
        let json = r#"{"header_filters":[{"filter":{"header":"X-Other","value":"z"}},{"filter":{"header":"location","value":"/dst"}}]}"#;
        assert_eq!(
            find_header_filter_value(json, "location").as_deref(),
            Some("/dst")
        );
    }

    #[test]
    fn decode_fetch_response_reads_status_and_body() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&200u32.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        put(&mut buf, b"{}");
        let resp = decode_fetch_response(&buf).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"{}");
    }

    #[test]
    fn decode_fetch_response_rejects_truncated() {
        assert!(decode_fetch_response(&[0, 0]).is_none());
    }
}

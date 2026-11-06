//! Rule sync: periodically pull the project's rules from the redirection.io
//! Public API, translate them into the engine format the plugin matches, and
//! write them to the file the plugin reads. Runs beside Sōzune (a small daemon
//! or a cron one-shot) — NOT in the wasm guest.
//!
//! On a failed fetch or translate, the previous ruleset file is left untouched:
//! the plugin keeps matching with the last known rules, so nothing has to be
//! "up" at request time.
//!
//! Config via env:
//!   RIO_API_TOKEN   API token (Settings > API tokens — NOT the agent token)
//!   RIO_PROJECT_ID  project UUID (from the /organization endpoint)
//!   RIO_OUT         path to write the plugin ruleset config (default: rules.json)
//!   RIO_INTERVAL    seconds between syncs (default: 300; 0 = run once and exit)
//!   RIO_API_BASE    API base URL (default: https://api.redirection.io)
//!
//! Usage: RIO_API_TOKEN=... RIO_PROJECT_ID=... cargo run --bin sync
//!
//! Host-side only: on wasm32 (no reqwest, no translator) it compiles to an empty
//! `main` so `cargo build --target wasm32` still builds the guest.

#[cfg(target_arch = "wasm32")]
fn main() {}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    native::run();
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use std::time::Duration;

    use sozune_redirectionio::translate::{to_plugin_config, translate_ruleset};

    pub struct Config {
        pub token: String,
        pub project_id: String,
        pub out_path: String,
        pub interval_secs: u64,
        pub api_base: String,
    }

    impl Config {
        fn from_env() -> Result<Self, String> {
            let token = std::env::var("RIO_API_TOKEN")
                .map_err(|_| "RIO_API_TOKEN is required".to_string())?;
            let project_id = std::env::var("RIO_PROJECT_ID")
                .map_err(|_| "RIO_PROJECT_ID is required".to_string())?;
            Ok(Config {
                token,
                project_id,
                out_path: std::env::var("RIO_OUT").unwrap_or_else(|_| "rules.json".to_string()),
                interval_secs: std::env::var("RIO_INTERVAL")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(300),
                api_base: std::env::var("RIO_API_BASE")
                    .unwrap_or_else(|_| "https://api.redirection.io".to_string()),
            })
        }
    }

    /// Build the `GET /rules` URL for a project, optionally resuming after the
    /// last rule id of the previous page.
    fn rules_url(api_base: &str, project_id: &str, after: Option<&str>) -> String {
        let mut url = format!(
            "{}/rules?projectId={}",
            api_base.trim_end_matches('/'),
            project_id
        );
        if let Some(id) = after {
            url.push_str("&searchAfterId=");
            url.push_str(id);
        }
        url
    }

    /// How many times a rate-limited (429) page is retried before giving up.
    const MAX_RATE_LIMIT_RETRIES: u32 = 5;

    /// Fetch one page of `GET /rules`, waiting out rate limits (the API allows
    /// about 10 requests a minute and answers 429 with `Retry-After`).
    fn fetch_page(
        client: &reqwest::blocking::Client,
        cfg: &Config,
        after: Option<&str>,
    ) -> Result<Vec<serde_json::Value>, String> {
        let url = rules_url(&cfg.api_base, &cfg.project_id, after);
        for attempt in 0..=MAX_RATE_LIMIT_RETRIES {
            let resp = client
                .get(&url)
                .bearer_auth(&cfg.token)
                .header("Accept", "application/json")
                .send()
                .map_err(|e| format!("request failed: {e}"))?;
            let status = resp.status();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS && attempt < MAX_RATE_LIMIT_RETRIES
            {
                let wait = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .unwrap_or(10);
                eprintln!("rate limited, retrying in {wait}s");
                std::thread::sleep(Duration::from_secs(wait));
                continue;
            }
            let body = resp.text().unwrap_or_default();
            if !status.is_success() {
                return Err(format!("API returned {status}: {body}"));
            }
            return serde_json::from_str(&body).map_err(|e| format!("unexpected body: {e}"));
        }
        Err("still rate limited after retries".to_string())
    }

    /// Walk every page (`searchAfterId` = last id of the previous page) until
    /// an empty page. Any page failing fails the whole fetch, so a partial
    /// ruleset never replaces the previous one.
    fn fetch_all(
        mut fetch_page: impl FnMut(Option<&str>) -> Result<Vec<serde_json::Value>, String>,
    ) -> Result<Vec<serde_json::Value>, String> {
        let mut all = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let page = fetch_page(after.as_deref())?;
            let Some(last) = page.last() else {
                return Ok(all);
            };
            let last_id = last
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or("rule without an id in page")?
                .to_string();
            if after.as_deref() == Some(last_id.as_str()) {
                return Err(format!("pagination did not advance past {last_id}"));
            }
            all.extend(page);
            after = Some(last_id);
        }
    }

    /// Fetch every rule of the project as one JSON array.
    fn fetch_rules(client: &reqwest::blocking::Client, cfg: &Config) -> Result<String, String> {
        let rules = fetch_all(|after| fetch_page(client, cfg, after))?;
        serde_json::to_string(&rules).map_err(|e| e.to_string())
    }

    /// Turn a raw `GET /rules` body into the plugin config string, logging skips.
    fn build_plugin_config(rules_json: &str) -> Result<String, String> {
        let (rules, skipped) = translate_ruleset(rules_json)?;
        for (id, reason) in &skipped {
            eprintln!("skip rule {id}: {reason:?}");
        }
        eprintln!(
            "translated {} rule(s), skipped {}",
            rules.len(),
            skipped.len()
        );
        to_plugin_config(&rules)
    }

    /// Write `content` to `path` atomically (write temp + rename) so the plugin
    /// never reads a half-written file.
    fn write_atomic(path: &str, content: &str) -> Result<(), String> {
        let tmp = format!("{path}.tmp");
        std::fs::write(&tmp, content).map_err(|e| format!("write {tmp}: {e}"))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("rename {tmp} -> {path}: {e}"))
    }

    /// One sync cycle: fetch, translate, write. On any error, leave the existing
    /// file untouched and return the error for logging.
    fn sync_once(client: &reqwest::blocking::Client, cfg: &Config) -> Result<(), String> {
        let raw = fetch_rules(client, cfg)?;
        let config = build_plugin_config(&raw)?;
        write_atomic(&cfg.out_path, &config)?;
        eprintln!("wrote ruleset to {}", cfg.out_path);
        Ok(())
    }

    pub fn run() {
        let cfg = match Config::from_env() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("config error: {e}");
                std::process::exit(2);
            }
        };
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("http client");

        loop {
            if let Err(e) = sync_once(&client, &cfg) {
                // Keep the previous ruleset; just report.
                eprintln!("sync failed (keeping previous ruleset): {e}");
            }
            if cfg.interval_secs == 0 {
                break; // one-shot mode
            }
            std::thread::sleep(Duration::from_secs(cfg.interval_secs));
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn rules_url_is_well_formed() {
            assert_eq!(
                rules_url("https://api.redirection.io", "abc-123", None),
                "https://api.redirection.io/rules?projectId=abc-123"
            );
            assert_eq!(
                rules_url("https://api.redirection.io/", "abc-123", Some("r9")),
                "https://api.redirection.io/rules?projectId=abc-123&searchAfterId=r9"
            );
        }

        fn page(ids: &[&str]) -> Vec<serde_json::Value> {
            ids.iter()
                .map(|id| serde_json::json!({ "id": id }))
                .collect()
        }

        #[test]
        fn fetch_all_follows_pages_until_empty() {
            let mut calls = Vec::new();
            let all = fetch_all(|after| {
                calls.push(after.map(str::to_string));
                Ok(match after {
                    None => page(&["a", "b"]),
                    Some("b") => page(&["c"]),
                    Some("c") => Vec::new(),
                    other => panic!("unexpected cursor {other:?}"),
                })
            })
            .unwrap();
            let ids: Vec<_> = all.iter().map(|r| r["id"].as_str().unwrap()).collect();
            assert_eq!(ids, ["a", "b", "c"]);
            assert_eq!(calls, [None, Some("b".into()), Some("c".into())]);
        }

        #[test]
        fn fetch_all_fails_whole_sync_on_a_failed_page() {
            let result = fetch_all(|after| match after {
                None => Ok(page(&["a"])),
                _ => Err("boom".to_string()),
            });
            assert_eq!(result, Err("boom".to_string()));
        }

        #[test]
        fn fetch_all_stops_if_cursor_does_not_advance() {
            let result = fetch_all(|_| Ok(page(&["a"])));
            assert!(result.unwrap_err().contains("did not advance"));
        }

        #[test]
        fn build_plugin_config_translates_and_wraps() {
            let editor = r#"[{
                "id":"r1","trigger":{"source":"/old"},
                "actions":[{"location":"/new","statusCode":301,"type":"redirection"}],
                "enabled":true
            }]"#;
            let cfg = build_plugin_config(editor).unwrap();
            assert!(cfg.starts_with("{\"rules\":["));
            assert!(cfg.contains("\"/old\""));
            assert!(cfg.contains("\"/new\""));
        }

        #[test]
        fn write_atomic_roundtrips() {
            let dir = std::env::temp_dir();
            let path = dir
                .join("sozune_sync_test_ruleset.json")
                .to_string_lossy()
                .into_owned();
            write_atomic(&path, "{\"rules\":[]}").unwrap();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"rules\":[]}");
            let _ = std::fs::remove_file(&path);
        }
    }
}

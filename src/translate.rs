//! Translate redirection.io **editor** rules (the `GET /rules` Public API shape)
//! into **engine** rules (`redirectionio::api::Rule`, what the Router consumes).
//!
//! The Public API returns `{ id, trigger, actions, priority, enabled, ... }`;
//! the engine wants `{ id, source, target, status_code, rank, ... }`. The crate
//! does NOT parse the editor shape, so this bridges the two.
//!
//! Scope: **redirects** (`actions[].type == "redirection"`). Anything else is
//! reported so the caller can log-and-skip rather than silently dropping it.
//! Extend as real rules require it (see PLAN-v2.md Phase 2).
//!
//! This runs host-side (the sync process), not in the wasm guest — it may use
//! serde and std freely.

use serde::Deserialize;

use redirectionio::api::{Rule, Source};

/// One rule as returned by `GET /rules`. Only the fields we translate are
/// modelled; the rest are ignored.
#[derive(Debug, Deserialize)]
pub struct EditorRule {
    pub id: String,
    pub trigger: EditorTrigger,
    #[serde(default)]
    pub actions: Vec<EditorAction>,
    #[serde(default)]
    pub priority: u16,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Deserialize)]
pub struct EditorTrigger {
    /// Source URL: a path (`/old`) or an absolute URL (`https://h/old`).
    pub source: String,
    #[serde(default)]
    pub methods: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct EditorAction {
    #[serde(rename = "type")]
    pub action_type: String,
    pub location: Option<String>,
    #[serde(alias = "statusCode")]
    pub status_code: Option<u16>,
}

/// Why a rule was skipped (for log-and-skip by the caller).
#[derive(Debug, PartialEq)]
pub enum Skip {
    Disabled,
    NoRedirectAction,
    MissingTarget,
}

/// Translate one editor rule into an engine `Rule`, or a reason it was skipped.
///
/// Only enabled rules with a `redirection` action carrying a `location` are
/// translated. `trigger.source` is split into host + path when it is an
/// absolute URL, otherwise treated as a path.
pub fn translate_rule(er: &EditorRule) -> Result<Rule, Skip> {
    if !er.enabled {
        return Err(Skip::Disabled);
    }
    let redirect = er
        .actions
        .iter()
        .find(|a| a.action_type == "redirection")
        .ok_or(Skip::NoRedirectAction)?;
    let target = redirect.location.clone().ok_or(Skip::MissingTarget)?;
    if target.is_empty() {
        return Err(Skip::MissingTarget);
    }

    let (host, path) = split_source(&er.trigger.source);
    let methods = if er.trigger.methods.is_empty() {
        None
    } else {
        Some(er.trigger.methods.clone())
    };

    Ok(Rule {
        id: er.id.clone(),
        source: Source {
            scheme: None,
            host,
            ips: None,
            datetime: None,
            time: None,
            path,
            query: None,
            headers: None,
            methods,
            exclude_methods: None,
            response_status_codes: None,
            exclude_response_status_codes: None,
            sampling: None,
            weekdays: None,
        },
        target: Some(target),
        status_code: redirect.status_code.or(Some(301)),
        rank: er.priority,
        markers: Vec::new(),
        variables: Vec::new(),
        body_filters: None,
        header_filters: None,
        log_override: None,
        peer_override: None,
        reset: None,
        stop: None,
        examples: None,
        redirect_unit_id: None,
        configuration_log_unit_id: None,
        configuration_reset_unit_id: None,
        peer_unit_id: None,
        target_hash: None,
    })
}

/// Split a trigger source into (host, path). An absolute URL yields both; a
/// bare path yields (None, path).
fn split_source(source: &str) -> (Option<String>, String) {
    if let Some(rest) = source
        .strip_prefix("https://")
        .or_else(|| source.strip_prefix("http://"))
    {
        match rest.find('/') {
            Some(i) => (Some(rest[..i].to_string()), rest[i..].to_string()),
            None => (Some(rest.to_string()), "/".to_string()),
        }
    } else {
        (None, source.to_string())
    }
}

/// Result of translating a ruleset: the engine rules, and the `(id, reason)`
/// pairs for rules that were skipped (for log-and-skip by the caller).
pub type TranslatedRuleset = (Vec<Rule>, Vec<(String, Skip)>);

/// Translate a `GET /rules` JSON array into engine rules plus a list of skipped
/// rules and why.
pub fn translate_ruleset(rules_json: &str) -> Result<TranslatedRuleset, String> {
    let editor_rules: Vec<EditorRule> =
        serde_json::from_str(rules_json).map_err(|e| e.to_string())?;
    let mut engine = Vec::new();
    let mut skipped = Vec::new();
    for er in editor_rules {
        match translate_rule(&er) {
            Ok(rule) => engine.push(rule),
            Err(reason) => skipped.push((er.id, reason)),
        }
    }
    Ok((engine, skipped))
}

/// Serialize engine rules into the plugin's config ruleset shape:
/// `{"rules":[ <api::Rule>, ... ]}`.
pub fn to_plugin_config(rules: &[Rule]) -> Result<String, String> {
    let value = serde_json::json!({ "rules": rules });
    serde_json::to_string(&value).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A captured-shape editor rule: /old -> /new, 302.
    const EDITOR: &str = r#"[{
        "id":"20fdf49e-85db-461f-bde9-033974870329",
        "trigger":{"source":"/old","methods":[],"requestHeaders":[],"ipAddress":null,"sampling":null},
        "actions":[{"location":"/new","statusCode":302,"type":"redirection"}],
        "markers":[],"variables":[],"priority":10,"enabled":true,"tags":[]
    }]"#;

    #[test]
    fn translates_a_simple_redirect() {
        let (rules, skipped) = translate_ruleset(EDITOR).unwrap();
        assert!(skipped.is_empty());
        assert_eq!(rules.len(), 1);
        let r = &rules[0];
        assert_eq!(r.source.path, "/old");
        assert_eq!(r.target.as_deref(), Some("/new"));
        assert_eq!(r.status_code, Some(302));
        assert_eq!(r.rank, 10);
    }

    #[test]
    fn translated_rule_deserializes_via_engine_from_json() {
        // The whole point: the output must be a valid engine Rule. Round-trip it
        // through the crate's own Rule::from_json.
        let (rules, _) = translate_ruleset(EDITOR).unwrap();
        let json = serde_json::to_string(&rules[0]).unwrap();
        let back = Rule::from_json(&json);
        assert!(back.is_some(), "engine failed to parse translated rule");
    }

    #[test]
    fn plugin_config_wraps_rules() {
        let (rules, _) = translate_ruleset(EDITOR).unwrap();
        let cfg = to_plugin_config(&rules).unwrap();
        assert!(cfg.starts_with("{\"rules\":["));
        // And it parses back as a value with a rules array.
        let v: serde_json::Value = serde_json::from_str(&cfg).unwrap();
        assert!(v["rules"].as_array().unwrap().len() == 1);
    }

    #[test]
    fn absolute_source_splits_host_and_path() {
        let (h, p) = split_source("https://app.example.com/old");
        assert_eq!(h.as_deref(), Some("app.example.com"));
        assert_eq!(p, "/old");
        let (h2, p2) = split_source("/bare/path");
        assert!(h2.is_none());
        assert_eq!(p2, "/bare/path");
    }

    #[test]
    fn skips_disabled_rule() {
        let json = r#"[{"id":"d","trigger":{"source":"/x"},"actions":[{"location":"/y","statusCode":301,"type":"redirection"}],"enabled":false}]"#;
        let (rules, skipped) = translate_ruleset(json).unwrap();
        assert!(rules.is_empty());
        assert_eq!(skipped, vec![("d".to_string(), Skip::Disabled)]);
    }

    #[test]
    fn skips_non_redirect_action() {
        let json = r#"[{"id":"h","trigger":{"source":"/x"},"actions":[{"type":"header"}],"enabled":true}]"#;
        let (rules, skipped) = translate_ruleset(json).unwrap();
        assert!(rules.is_empty());
        assert_eq!(skipped, vec![("h".to_string(), Skip::NoRedirectAction)]);
    }

    #[test]
    fn status_code_defaults_to_301() {
        let json = r#"[{"id":"n","trigger":{"source":"/x"},"actions":[{"location":"/y","type":"redirection"}],"enabled":true}]"#;
        let (rules, _) = translate_ruleset(json).unwrap();
        assert_eq!(rules[0].status_code, Some(301));
    }
}

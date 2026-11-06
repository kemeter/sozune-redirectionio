//! Translate redirection.io **editor** rules (the `GET /rules` Public API shape)
//! into **engine** rules (`redirectionio::api::Rule`, what the Router consumes).
//!
//! The Public API returns `{ id, trigger, actions, priority, enabled, ... }`;
//! the engine wants `{ id, source, target, status_code, rank, ... }`. The crate
//! does NOT parse the editor shape, so this bridges the two.
//!
//! Scope: **redirects** (`actions[].type == "redirection"`), optionally gated by
//! methods, sampling, and date/datetime/weekday conditions. Any condition or
//! action the plugin cannot honour faithfully makes the whole rule skipped and
//! reported, never translated without it: dropping a condition would widen the
//! rule (a bot-only redirect would hit every visitor). Extend as real rules
//! require it (see PLAN-v2.md Phase 2).
//!
//! Dates in the editor format carry no timezone (`2021-01-01 08:27:09`); they
//! are read as **UTC**, which is how the engine compares the request time.
//!
//! This runs host-side (the sync process), not in the wasm guest — it may use
//! serde and std freely.

use chrono::{NaiveDate, NaiveDateTime, SecondsFormat, TimeDelta};
use serde::Deserialize;
use serde_json::Value;

use redirectionio::api::{DateTimeConstraint, Rule, Source};

/// One rule as returned by `GET /rules`. Only the fields we translate or must
/// refuse are modelled; the rest are ignored.
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
    #[serde(default)]
    pub markers: Option<Vec<Value>>,
    #[serde(default)]
    pub variables: Option<Vec<Value>>,
}

fn default_enabled() -> bool {
    true
}

/// The rule trigger. Fields are optional/nullable because the API omits or
/// nulls the ones that are not set.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorTrigger {
    /// Source URL: a path (`/old`) or an absolute URL (`https://h/old`).
    pub source: String,
    #[serde(default)]
    pub methods: Option<Vec<String>>,
    #[serde(default)]
    pub exclude_methods: Option<bool>,
    #[serde(default)]
    pub request_headers: Option<Vec<Value>>,
    #[serde(default)]
    pub response_status_codes: Option<Vec<Value>>,
    #[serde(default)]
    pub ip_address: Option<Value>,
    #[serde(default)]
    pub sampling: Option<u32>,
    #[serde(default)]
    pub date: Option<EditorCondition>,
    #[serde(default)]
    pub datetime: Option<EditorCondition>,
    #[serde(default)]
    pub time: Option<Value>,
}

/// A `date` / `datetime` trigger: `{ "operator": ..., "value": ... }`.
#[derive(Debug, Deserialize)]
pub struct EditorCondition {
    pub operator: String,
    pub value: Value,
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
    /// The rule uses something the plugin does not support yet.
    Unsupported(String),
    /// A condition value could not be parsed.
    Invalid(String),
}

fn non_empty<T>(v: &Option<Vec<T>>) -> bool {
    v.as_ref().is_some_and(|v| !v.is_empty())
}

/// Refuse rules relying on features the plugin cannot honour, so they are
/// skipped loudly instead of being translated into a wider rule.
fn check_supported(er: &EditorRule) -> Result<(), Skip> {
    let unsupported = |what: &str| Err(Skip::Unsupported(what.to_string()));
    let t = &er.trigger;
    if non_empty(&t.request_headers) {
        return unsupported("requestHeaders trigger");
    }
    if non_empty(&t.response_status_codes) {
        return unsupported("responseStatusCodes trigger");
    }
    if t.ip_address.as_ref().is_some_and(|v| !v.is_null()) {
        return unsupported("ipAddress trigger");
    }
    if t.time.as_ref().is_some_and(|v| !v.is_null()) {
        return unsupported("time trigger");
    }
    if non_empty(&er.markers) {
        return unsupported("markers");
    }
    if non_empty(&er.variables) {
        return unsupported("variables");
    }
    if let Some(other) = er.actions.iter().find(|a| a.action_type != "redirection") {
        return Err(Skip::Unsupported(format!("{} action", other.action_type)));
    }
    Ok(())
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
    check_supported(er)?;
    let target = redirect.location.clone().ok_or(Skip::MissingTarget)?;
    if target.is_empty() {
        return Err(Skip::MissingTarget);
    }

    let (host, path) = split_source(&er.trigger.source);
    let methods = er.trigger.methods.clone().filter(|m| !m.is_empty());
    let exclude_methods = methods
        .as_ref()
        .and(er.trigger.exclude_methods)
        .filter(|&x| x);
    let sampling = match er.trigger.sampling {
        Some(s) if s > 100 => return Err(Skip::Invalid(format!("sampling {s}"))),
        // 100% is the same as no sampling.
        Some(100) | None => None,
        Some(s) => Some(s),
    };
    let (datetime, weekdays) = translate_dates(&er.trigger)?;

    Ok(Rule {
        id: er.id.clone(),
        source: Source {
            scheme: None,
            host,
            ips: None,
            datetime,
            time: None,
            path,
            query: None,
            headers: None,
            methods,
            exclude_methods,
            response_status_codes: None,
            exclude_response_status_codes: None,
            sampling,
            weekdays,
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

/// A half-open UTC range `[start, end)`; `None` is unbounded.
type Range = (Option<NaiveDateTime>, Option<NaiveDateTime>);

/// Map the `date` and `datetime` triggers onto the engine's `datetime` ranges
/// (any range matches) and `weekdays`.
#[allow(clippy::type_complexity)]
fn translate_dates(
    t: &EditorTrigger,
) -> Result<(Option<Vec<DateTimeConstraint>>, Option<Vec<String>>), Skip> {
    let mut weekdays = None;
    let mut ranges: Option<Vec<Range>> = None;

    if let Some(date) = &t.date {
        if date.operator == "weekdays" {
            weekdays = Some(translate_weekdays(&date.value)?);
        } else {
            ranges = Some(date_ranges(date)?);
        }
    }
    if let Some(datetime) = &t.datetime {
        // The engine ORs the ranges of a single `datetime` list, so a date and
        // a datetime condition (an AND) cannot both be expressed there.
        if ranges.is_some() {
            return Err(Skip::Unsupported(
                "date and datetime triggers together".to_string(),
            ));
        }
        ranges = Some(datetime_ranges(datetime)?);
    }

    let datetime = ranges.map(|ranges| {
        ranges
            .into_iter()
            .map(|(start, end)| DateTimeConstraint(start.map(rfc3339), end.map(rfc3339)))
            .collect()
    });
    Ok((datetime, weekdays))
}

fn rfc3339(dt: NaiveDateTime) -> String {
    dt.and_utc().to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn invalid(c: &EditorCondition) -> Skip {
    Skip::Invalid(format!("{} {}", c.operator, c.value))
}

/// The operand(s) of a condition: one value, or exactly two for ranges.
fn operands(c: &EditorCondition, two: bool) -> Result<Vec<&str>, Skip> {
    let values: Vec<&str> = match (&c.value, two) {
        (Value::String(s), false) => vec![s.as_str()],
        (Value::Array(a), true) if a.len() == 2 => {
            a.iter().filter_map(Value::as_str).collect::<Vec<_>>()
        }
        _ => Vec::new(),
    };
    if values.len() == if two { 2 } else { 1 } {
        Ok(values)
    } else {
        Err(invalid(c))
    }
}

fn parse_datetime(s: &str) -> Option<NaiveDateTime> {
    ["%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M"]
        .iter()
        .find_map(|f| NaiveDateTime::parse_from_str(s, f).ok())
        .or_else(|| {
            chrono::DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|d| d.naive_utc())
        })
}

fn parse_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .ok()
        .or_else(|| parse_datetime(s).map(|d| d.date()))
}

fn datetime_ranges(c: &EditorCondition) -> Result<Vec<Range>, Skip> {
    let two = matches!(c.operator.as_str(), "between" | "notBetween");
    let v = operands(c, two)?
        .into_iter()
        .map(|s| parse_datetime(s).ok_or_else(|| invalid(c)))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(match c.operator.as_str() {
        "before" => vec![(None, Some(v[0]))],
        "after" => vec![(Some(v[0]), None)],
        "between" => vec![(Some(v[0]), Some(v[1]))],
        "notBetween" => vec![(None, Some(v[0])), (Some(v[1]), None)],
        other => return Err(Skip::Unsupported(format!("datetime operator {other}"))),
    })
}

/// Whole-day conditions: a day `d` covers `[d 00:00, d+1 00:00)`.
fn date_ranges(c: &EditorCondition) -> Result<Vec<Range>, Skip> {
    let two = matches!(c.operator.as_str(), "between" | "notBetween");
    let days = operands(c, two)?
        .into_iter()
        .map(|s| parse_date(s).ok_or_else(|| invalid(c)))
        .collect::<Result<Vec<_>, _>>()?;
    let start = |d: NaiveDate| Some(d.and_time(chrono::NaiveTime::MIN));
    let end = |d: NaiveDate| start(d + TimeDelta::days(1));
    Ok(match c.operator.as_str() {
        "equals" => vec![(start(days[0]), end(days[0]))],
        "notEquals" => vec![(None, start(days[0])), (end(days[0]), None)],
        "before" => vec![(None, start(days[0]))],
        "after" => vec![(end(days[0]), None)],
        "between" => vec![(start(days[0]), end(days[1]))],
        "notBetween" => vec![(None, start(days[0])), (end(days[1]), None)],
        other => return Err(Skip::Unsupported(format!("date operator {other}"))),
    })
}

/// `weekdays` values are integers, 0 = Sunday … 6 = Saturday.
fn translate_weekdays(value: &Value) -> Result<Vec<String>, Skip> {
    const NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let bad = || Skip::Invalid(format!("weekdays {value}"));
    let days = value.as_array().ok_or_else(bad)?;
    let names = days
        .iter()
        .map(|d| {
            d.as_u64()
                .and_then(|i| NAMES.get(i as usize))
                .map(|n| n.to_string())
                .ok_or_else(bad)
        })
        .collect::<Result<Vec<_>, _>>()?;
    if names.is_empty() {
        return Err(bad());
    }
    Ok(names)
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

    /// Build an editor rule `/promo -> /sale` with the given trigger extras.
    fn editor_with(trigger_extra: &str) -> String {
        format!(
            r#"[{{"id":"p","trigger":{{"source":"/promo"{trigger_extra}}},"actions":[{{"location":"/sale","statusCode":302,"type":"redirection"}}],"enabled":true}}]"#
        )
    }

    fn translate_one(trigger_extra: &str) -> Result<Rule, Skip> {
        let (mut rules, mut skipped) = translate_ruleset(&editor_with(trigger_extra)).unwrap();
        match (rules.pop(), skipped.pop()) {
            (Some(rule), None) => Ok(rule),
            (None, Some((_, reason))) => Err(reason),
            other => panic!("unexpected translation result {other:?}"),
        }
    }

    /// Does the translated rule match `/promo` at `at` (UTC), through the real
    /// engine (JSON round-trip, Router, match)?
    fn matches_at(rule: &Rule, at: &str) -> bool {
        let rule = Rule::from_json(&serde_json::to_string(rule).unwrap()).expect("engine rule");
        let router = crate::build_router(vec![rule]).expect("router");
        let at = at.parse().unwrap();
        let req =
            crate::build_rio_request("example.com", "https", "GET", "/promo", vec![], Some(at));
        let req = redirectionio::http::Request::rebuild_with_config(&router.config, &req);
        !router.match_request(&req).is_empty()
    }

    #[test]
    fn datetime_between_matches_only_inside() {
        let rule = translate_one(
            r#","datetime":{"operator":"between","value":["2026-01-01 08:00:00","2026-01-31 19:00:00"]}"#,
        )
        .unwrap();
        assert!(matches_at(&rule, "2026-01-15T12:00:00Z"));
        assert!(!matches_at(&rule, "2025-12-31T23:59:59Z"));
        assert!(!matches_at(&rule, "2026-01-31T19:00:01Z"));
    }

    #[test]
    fn datetime_before_after_and_not_between() {
        let before =
            translate_one(r#","datetime":{"operator":"before","value":"2026-06-01 00:00:00"}"#)
                .unwrap();
        assert!(matches_at(&before, "2026-05-31T23:00:00Z"));
        assert!(!matches_at(&before, "2026-06-01T01:00:00Z"));

        let after =
            translate_one(r#","datetime":{"operator":"after","value":"2026-06-01 00:00:00"}"#)
                .unwrap();
        assert!(!matches_at(&after, "2026-05-31T23:00:00Z"));
        assert!(matches_at(&after, "2026-06-01T01:00:00Z"));

        let outside = translate_one(
            r#","datetime":{"operator":"notBetween","value":["2026-06-01 00:00:00","2026-06-30 00:00:00"]}"#,
        )
        .unwrap();
        assert!(matches_at(&outside, "2026-05-15T00:00:00Z"));
        assert!(!matches_at(&outside, "2026-06-15T00:00:00Z"));
        assert!(matches_at(&outside, "2026-07-15T00:00:00Z"));
    }

    #[test]
    fn date_conditions_cover_whole_days() {
        let on = translate_one(r#","date":{"operator":"equals","value":"2026-03-07"}"#).unwrap();
        assert!(matches_at(&on, "2026-03-07T00:00:00Z"));
        assert!(matches_at(&on, "2026-03-07T23:59:59Z"));
        assert!(!matches_at(&on, "2026-03-08T00:00:00Z"));
        assert!(!matches_at(&on, "2026-03-06T23:59:59Z"));

        let not_on =
            translate_one(r#","date":{"operator":"notEquals","value":"2026-03-07"}"#).unwrap();
        assert!(!matches_at(&not_on, "2026-03-07T12:00:00Z"));
        assert!(matches_at(&not_on, "2026-03-08T12:00:00Z"));

        let after = translate_one(r#","date":{"operator":"after","value":"2026-03-07"}"#).unwrap();
        assert!(!matches_at(&after, "2026-03-07T23:00:00Z"));
        assert!(matches_at(&after, "2026-03-08T00:00:00Z"));

        let span =
            translate_one(r#","date":{"operator":"between","value":["2026-03-01","2026-03-07"]}"#)
                .unwrap();
        assert!(matches_at(&span, "2026-03-07T23:00:00Z"));
        assert!(!matches_at(&span, "2026-03-08T00:00:00Z"));
    }

    #[test]
    fn weekdays_use_sunday_zero() {
        // 2026-11-07 is a Saturday, 2026-11-08 a Sunday, 2026-11-09 a Monday.
        let weekend = translate_one(r#","date":{"operator":"weekdays","value":[0,6]}"#).unwrap();
        assert!(matches_at(&weekend, "2026-11-07T10:00:00Z"));
        assert!(matches_at(&weekend, "2026-11-08T10:00:00Z"));
        assert!(!matches_at(&weekend, "2026-11-09T10:00:00Z"));
    }

    #[test]
    fn sampling_is_carried_and_full_sampling_dropped() {
        assert_eq!(
            translate_one(r#","sampling":23"#).unwrap().source.sampling,
            Some(23)
        );
        assert_eq!(
            translate_one(r#","sampling":100"#).unwrap().source.sampling,
            None
        );
        assert_eq!(
            translate_one(r#","sampling":null"#)
                .unwrap()
                .source
                .sampling,
            None
        );
        assert!(matches!(
            translate_one(r#","sampling":150"#),
            Err(Skip::Invalid(_))
        ));
    }

    #[test]
    fn excluded_methods_are_carried() {
        let rule = translate_one(r#","methods":["POST"],"excludeMethods":true"#).unwrap();
        assert_eq!(rule.source.methods, Some(vec!["POST".to_string()]));
        assert_eq!(rule.source.exclude_methods, Some(true));
    }

    #[test]
    fn unsupported_conditions_skip_the_rule_instead_of_widening_it() {
        for extra in [
            r#","requestHeaders":[{"name":"User-Agent","operator":"contains","value":"bot"}]"#,
            r#","ipAddress":{"type":"equals","ip":"1.2.3.4"}"#,
            r#","responseStatusCodes":[404]"#,
            r#","time":{"operator":"between","value":["08:00","18:00"]}"#,
            r#","date":{"operator":"equals","value":"2026-03-07"},"datetime":{"operator":"after","value":"2026-01-01 00:00:00"}"#,
        ] {
            assert!(
                matches!(translate_one(extra), Err(Skip::Unsupported(_))),
                "should be unsupported: {extra}"
            );
        }
    }

    #[test]
    fn unparseable_dates_skip_the_rule() {
        // The engine treats an unparseable bound as unbounded (a wider rule),
        // so the translator must refuse rather than pass it through.
        for extra in [
            r#","datetime":{"operator":"after","value":"tomorrow"}"#,
            r#","datetime":{"operator":"between","value":["2026-01-01 00:00:00"]}"#,
            r#","date":{"operator":"weekdays","value":[7]}"#,
        ] {
            assert!(
                matches!(translate_one(extra), Err(Skip::Invalid(_))),
                "should be invalid: {extra}"
            );
        }
    }

    #[test]
    fn extra_actions_or_markers_skip_the_rule() {
        let json = r#"[{"id":"x","trigger":{"source":"/x"},"actions":[{"location":"/y","statusCode":301,"type":"redirection"},{"type":"header","name":"X","value":"1"}],"enabled":true}]"#;
        let (_, skipped) = translate_ruleset(json).unwrap();
        assert_eq!(
            skipped,
            vec![("x".into(), Skip::Unsupported("header action".into()))]
        );

        let json = r#"[{"id":"m","trigger":{"source":"/a/@name"},"actions":[{"location":"/b/@name","statusCode":301,"type":"redirection"}],"markers":[{"name":"name","type":"anything"}],"enabled":true}]"#;
        let (_, skipped) = translate_ruleset(json).unwrap();
        assert_eq!(
            skipped,
            vec![("m".into(), Skip::Unsupported("markers".into()))]
        );
    }
}

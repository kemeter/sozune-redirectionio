# Plan — sozune-redirectionio v2 (local matching, no agent)

## Goal

A plugin that reprend **all** redirection.io rules but needs **no agent
container** and no per-request network call. It matches rules **locally** inside
the wasm guest using redirection.io's own engine (`libredirectionio`), fed by a
ruleset that a small out-of-band process refreshes periodically. If the refresh
source is down, the plugin keeps matching with its last known ruleset — nothing
has to be "up" at request time.

This is a **separate deliverable** from the shipped v1 (the agent-backed plugin
on `main`). v1 stays as-is; v2 lives on a branch until proven end to end.

## Why v2 (vs the shipped v1 agent plugin)

- v1 asks a redirection.io **agent** per request. Correct and simple, but needs
  the agent reachable — locally for acceptable latency. The user does not want a
  mandatory agent container (uncertain uptime, extra moving part).
- v2 removes the agent entirely: rules are pulled periodically and matched
  locally. No agent, latency-free, resilient to the rule source being briefly
  unavailable.

## What is already proven (do not re-litigate)

1. **`libredirectionio` loads under wasmtime** (Sōzune's host). A probe crate
   using `Router` / `Rule` / `Action::from_routes_rule` builds to
   `wasm32-unknown-unknown` with **zero external imports** (no wasm-bindgen / JS),
   at the cost of **3 crate patches** (details below).
2. **Local matching works**: compile a `Rule` into a `Router`, `match_request`,
   `Action::from_routes_rule`, then `get_status_code` + `filter_headers` — this
   is the exact path v1's earlier probe already exercised.
3. **Rule source**: the Public API `GET /rules` returns the project's rules.
   Rate-limited (~10 req/min) so it is a **periodic pull**, never per request.

## The one real unknown, and the plan's core work

`GET /rules` returns the **editor** format (`trigger` / `actions` / `priority` /
`enabled`), NOT `redirectionio::api::Rule` (the **engine** format:
`source{path,host,...}` / `target` / `status_code` / `rank`). The crate has
`Rule::from_json` but it only accepts the engine format. **`libredirectionio`
does not parse the `trigger`/`actions` format** — confirmed by source grep.

So v2's central task is a **translator**: `GET /rules` JSON → `api::Rule` JSON.
Start minimal (redirects), extend on demand — the user's rule scope is not yet
known, so do NOT try to cover everything up front.

Minimal required fields for a redirect (from source grep):
`Rule { id, source: Source { path }, rank, target, status_code }`
(`markers`/`variables` are `Vec` with `#[serde(default)]`, so they can be empty.)

## Architecture

```
[sync process, out-of-band]
   GET https://api.redirection.io/rules?projectId=<uuid>   (API token, periodic)
      → translate each editor rule → api::Rule JSON
      → write ruleset into the plugin config (or a shared file)
                              │
[plugin wasm: libredirectionio (patched) ]
   on load / on config change: build Router<Rule> from the ruleset
   per request: match_request → Action::from_routes_rule
                → get_status_code + Location  → short-circuit  (else continue)
```

Key property: the per-request path touches **only** the in-memory Router. No
network, no agent. The sync process is the only thing that talks to r.io, and
only every N minutes.

## The 3 crate patches (via `[patch.crates-io]`)

redirectionio hard-wires JS deps on wasm32 assuming a browser host. Neutralize
them (does not touch the plugin's own logic):

1. **getrandom**: drop the `wasm_js` feature (vendored copy with `wasm_js = []`)
   and gate the `WEB_CRYPTO` arm in `error.rs` on `getrandom_backend = "wasm_js"`
   (upstream bug: the arm is compiled by feature but the const only exists per
   backend). Plus `--cfg getrandom_backend="custom"` in `.cargo/config.toml` and
   a `__getrandom_v03_custom` fn in the guest (non-crypto PRNG; only used for
   rule sampling).
2. **chrono** (the resolved version, currently 0.4.45): empty the `wasmbind`
   feature and drop it from `default`.
3. **redirectionio**: in its `[target.wasm32]` block, drop the JS deps —
   `chrono/wasmbind`, `getrandom/wasm_js`, and the direct `wasm-bindgen` +
   `wasm-tracing` deps. Its `wasm_api.rs` (the only source using wasm-bindgen) is
   gated `feature = "wasmbind"`, which we never enable, so it is not compiled;
   `web-sys`/`wasm-bindgen` remain in the dep graph but are tree-shaken (0 imports
   in the final wasm — verified).

All patches are small (a few lines each). They are vendored copies pinned via
`[patch.crates-io] = { path = ... }`. Maintenance cost (re-patch on version
bumps) is accepted per the user. A cleaner long-term fix is a `no-js` feature
upstream — file that later (out of scope for v2).

## Work breakdown

### Phase 0 — Branch + scaffolding
- Branch `v2-local-matching` off `main`.
- Vendor the 3 patched crates under `patches/` in the repo; wire
  `[patch.crates-io]` + `.cargo/config.toml`.
- Add `redirectionio` (patched), `serde_json`, `getrandom` deps.
- **Gate:** the guest builds to wasm32 with **0 external imports** (assert with
  `wasm-tools print | grep import`).

### Phase 1 — Local matching in the guest
- Guest loads a ruleset (array of `api::Rule` JSON) from `get_config`.
- Build `Router<Rule>` once (cache on first request; rebuild if config changes).
- Per request: build `redirectionio::http::Request` (created_at: None — no
  Utc::now trap), `match_request`, `Action::from_routes_rule`, then apply
  `get_status_code` + `Location` and short-circuit; else continue.
- Reuse v1's http-wasm ABI plumbing (get_uri, headers, set_status_code,
  set_header_value, Next::Stop) — that part is unchanged and already tested.
- **Gate:** integration test — feed a hand-written `api::Rule` ruleset, drive a
  request through the compiled wasm, assert 301 + Location. (No agent, no
  Fetcher this time — the ruleset is in config.)

### Phase 2 — The translator (start minimal)
- A pure function `translate(editor_json) -> Vec<api::Rule JSON>` covering the
  **redirect** case: `trigger.source` → `Source{path[,host]}`,
  `actions[0].location` → `target`, `actions[0].statusCode` → `status_code`,
  `priority` → `rank`, skip `enabled:false`, drop rules whose action `type` is
  not `redirection` (log-and-skip).
- Lives in the **sync process** (host-side, normal Rust), NOT the guest — keeps
  the guest small and lets the translator use serde freely.
- **Gate:** unit tests translating captured `GET /rules` payloads; feed the
  output through `Rule::from_json` to prove it deserializes into the engine.
- Extend coverage (methods, headers, markers, filters) **only when the user
  actually needs them** — log-and-skip anything unsupported so it fails loud, not
  silent.

### Phase 3 — The sync process
- Small standalone binary / script: `GET /rules` (API token + projectId UUID),
  translate, write the ruleset where the plugin reads it, on an interval.
- Robustness: on fetch failure, keep the previous ruleset (never blank it).
- **Gate:** run against the real project (needs an **API token** — distinct from
  the agent token; the agent token we used returns 401 on the Public API) and a
  published redirect rule; confirm the plugin then redirects with no agent
  running.

### Phase 4 — CI + docs
- CI: same shape as v1 (fmt, clippy, build wasm asserting 0 imports, tests).
  The patched crates must be vendored so CI can resolve them.
- README: document the no-agent model, the API token requirement, the sync
  cadence / staleness tradeoff, and the supported-rule scope (redirects first).

## Open risks (flagged honestly)

- **API token**: the user has an agent/project token that 401s on the Public
  API. v2 needs a real API token (Settings > API tokens). Blocker for Phase 3
  live test until created.
- **Translator scope creep**: covering advanced rules faithfully reproduces
  r.io's server-side compilation and can diverge from their semantics. Mitigation:
  start with redirects, log-and-skip the rest, extend on real need only.
- **Patch drift**: 3 vendored patches to re-apply on `redirectionio` version
  bumps. Accepted; the upstream `no-js` feature request would remove this.
- **Staleness**: rules are only as fresh as the last successful sync (vs v1's
  real-time agent). Tune the interval to taste.

## Decision checkpoint before coding

Phase 0 + 1 prove the load-and-match end to end with a hand-fed ruleset (no
translator, no token needed). That is the cheapest way to de-risk. Recommend
building Phase 0+1 first, reviewing, then committing to the translator + sync.
```

# sozune-redirectionio

A [redirection.io](https://redirection.io/) redirect engine, run as a
[Sōzune](https://github.com/kemeter/sozune) plugin compiled to WebAssembly —
**matching rules locally, with no agent**.

The plugin embeds redirection.io's own [`redirectionio`](https://crates.io/crates/redirectionio)
engine (the same crate their agent and integrations use) and matches each request
**inside the wasm guest**. Rules are carried in the plugin config; a small
companion process refreshes them periodically from redirection.io. There is **no
per-request network call and no agent container** — nothing has to be running at
request time, and if the refresh source is briefly down the plugin keeps matching
with its last known rules.

## Why no agent

redirection.io's usual model runs an **agent** (a sidecar) that every request
queries. This plugin removes it: rules are pulled out-of-band and matched
locally. Trade-offs: latency-free and resilient (no agent to keep up), but rules
are only as fresh as the last sync, and advanced rule types beyond redirects need
the translator extended (see Limitations).

## How it works

```
[sync binary, beside Sōzune]                       [plugin, inside Sōzune]
  GET /rules (API token, periodic)                   reads ruleset from config
    → translate editor → engine rules      ─────▶    builds a Router once
    → write rules.json (atomic)                       per request: match locally
  keeps previous file on failure                      → redirect (301 + Location)
                                                       → or continue to backend
```

The per-request path touches only the in-memory Router. No agent, no network.

## Build

```sh
# the wasm guest
cargo build --release --target wasm32-unknown-unknown
# -> target/wasm32-unknown-unknown/release/sozune_redirectionio.wasm

# the sync binary (host-side)
cargo build --release --bin sync
```

The guest embeds `libredirectionio`, which is wired for a wasm-bindgen (JS) host.
Sōzune's host is wasmtime (no JS), so three crates are vendored under `patches/`
with their JS deps removed (see the top of `Cargo.toml`). The resulting `.wasm`
imports nothing but the http-wasm host functions — CI asserts this.

## Refreshing rules (the sync binary)

Runs beside Sōzune (a small daemon, or a cron one-shot). It pulls the project's
rules, translates them to the engine format, and writes the ruleset file the
plugin reads.

```sh
RIO_API_TOKEN=<api-token> \
RIO_PROJECT_ID=<project-uuid> \
RIO_OUT=/plugins/redirectionio-rules.json \
RIO_INTERVAL=300 \
  sync
```

| Env | Description |
|---|---|
| `RIO_API_TOKEN` | redirection.io **API token** (Settings > API tokens). Not the agent token. Required. |
| `RIO_PROJECT_ID` | Project UUID (from the `/organization` endpoint). Required. |
| `RIO_OUT` | Path to write the ruleset config. Default `rules.json`. |
| `RIO_INTERVAL` | Seconds between syncs. Default `300`. `0` = run once and exit (for cron). |
| `RIO_API_BASE` | API base URL. Default `https://api.redirection.io`. |

On a failed fetch or translate, the previous ruleset file is left untouched.

## Use with Sōzune

The plugin reads its ruleset from `config.rules` (the shape the sync binary
writes). Point both at the same place, or have the sync binary write into the
plugin config.

```yaml
# config.yaml
plugins:
  redirectionio:
    path: /plugins/sozune_redirectionio.wasm
    config:
      rules:
        # engine-format rules, as produced by the sync binary:
        - { id: "r1", source: { path: "/old" }, rank: 0, target: "/new", status_code: 301 }
```

```yaml
labels:
  - "sozune.enable=true"
  - "sozune.http.app.host=app.example.com"
  - "sozune.http.app.plugins=redirectionio"
```

Each `rules` entry is an engine-format `redirectionio::api::Rule`. You normally
never hand-write these — the sync binary produces them from your project.

## Behaviour

- **Redirect** — a matching rule with a status code sets that status + `Location`
  and short-circuits; the backend is never reached.
- **No match / empty ruleset / no config** — the request continues untouched.

## Limitations

- **Redirect scope.** The translator (`GET /rules` editor format → engine format)
  currently covers **redirects**. Rules whose action is not `redirection` are
  logged and skipped by the sync binary. Extend `src/translate.rs` as you need
  header filters, markers, conditions, etc. — the engine supports them; the
  translator just has to map them.
- **Staleness.** Rules are as fresh as the last successful sync (tune
  `RIO_INTERVAL`). This is the cost of not running an agent.
- **API token.** The sync binary needs an API token, distinct from the agent
  token. Create one under Settings > API tokens.
- **Scheme is assumed https.** http-wasm does not expose the scheme; traffic
  reaching Sōzune over TLS is reported as https for rule evaluation.

## Patched dependencies

`patches/{redirectionio,chrono,getrandom}` are vendored copies with their wasm32
JS deps removed so the engine loads under wasmtime. They are pinned via
`[patch.crates-io]`. On a `redirectionio` version bump these must be
re-applied; the clean long-term fix is an upstream `no-js` feature.

## License

MIT.

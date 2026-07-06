# sozune-redirectionio

A [redirection.io](https://redirection.io/) redirect engine, run as a
[Sōzune](https://github.com/kemeter/sozune) plugin compiled to WebAssembly.

For each request it asks the redirection.io **agent** what to do (over Sōzune's
`http_fetch` extension) and applies the answer: a redirect or hard response
short-circuits at the edge (the backend is never called), otherwise the request
continues untouched. The rule matching stays in the agent, where redirection.io
maintains it; the plugin only builds the request, calls `/action`, and applies
the returned status and `Location`.

The plugin is deliberately thin: it speaks the agent's JSON directly rather than
linking redirection.io's `redirectionio` crate, which is wired for a
wasm-bindgen (JS) host and does not load under Sōzune's wasmtime host. So the
`.wasm` imports nothing but the http-wasm host functions.

## Build

```sh
cargo build --release --target wasm32-unknown-unknown
# -> target/wasm32-unknown-unknown/release/sozune_redirectionio.wasm
```

## How it works

```
request ──▶ plugin ──http_fetch──▶ redirection.io agent  (POST {agent}/{token}/action)
                │                        │
                │◀────── Action (JSON) ──┘
                ▼
   status_code > 0 ?  ── yes ──▶ write status + filtered headers, short-circuit
                     ── no  ──▶ continue to the backend
```

The plugin queries the **agent**, not the public API. The agent keeps the
compiled rules locally and answers per request, so there is no per-request rate
limit (the public API is limited to ~10 req/min and is not used here). Run the
[redirection.io agent](https://redirection.io/documentation/developer-documentation/the-agent-as-a-reverse-proxy)
reachable from the proxy, or point `agent_host` at the hosted agent.

## Use with Sōzune

`agent_host` must be in the plugin's `allowed_hosts` so the proxy permits the
outbound call.

```yaml
# config.yaml
plugins:
  redirectionio:
    path: /plugins/sozune_redirectionio.wasm
    allowed_hosts: ["agent.redirection.io"]
    config:
      token: "<project-token>"
      agent_host: "https://agent.redirection.io"
```

```yaml
labels:
  - "sozune.enable=true"
  - "sozune.http.app.host=app.example.com"
  - "sozune.http.app.plugins=redirectionio"
```

The token can also be injected per route rather than globally, using
`sozune.http.<route>.plugins.redirectionio.token=<token>` labels, so a single
deployment carries its own redirection.io project.

## Configuration

| Key | Description |
|---|---|
| `token` | redirection.io project token. Required; without it the plugin lets every request through. |
| `agent_host` | Base URL of the redirection.io agent. Defaults to `https://agent.redirection.io`. Must be in the plugin's `allowed_hosts`. |

## Behaviour

- **Redirect / hard response** — when the agent's action carries a status code,
  the plugin sets it and the `Location` header and short-circuits; the backend
  is never reached.
- **No match** — a `404` from the agent, or an action with no status code, lets
  the request continue to the backend untouched.
- **Agent unreachable / malformed reply** — treated as "no decision": the
  request is forwarded rather than failing. Pair with the plugin's `fail_open`
  policy in Sōzune for the desired posture.

## Limitations

- **Status + `Location` only.** The plugin applies the redirect status and the
  `Location` header. redirection.io's other header filters and its response
  **body** filters (content rewriting) are not applied yet — the agent still
  evaluates them, but this thin plugin does not replay them onto the response.
- **Scheme is assumed https.** http-wasm does not expose the scheme; traffic
  reaching Sōzune over TLS is reported as https to the agent. Rules that key on
  scheme should account for this.
- **No local logging.** The Cloudflare/Fastly workers POST a log event back to
  the agent; this plugin does not (yet).

## License

MIT.

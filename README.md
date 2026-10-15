# opencode-wrap

OpenAI-compatible HTTP API in front of `opencode serve`, using the free model
`opencode/muse-spark-1.3-contributor-free`. No `OPENCODE_API_KEY` needed —
reuses whatever auth the `opencode` CLI already has.

Targets the **OpenCode 2** server API (`/api/*`, basic auth, session-scoped
model/agent, `session.text.delta` events). OpenCode 1.x servers are detected
and rejected at startup; upgrade with `opencode upgrade --major`.

Rust implementation (formerly a Node `server.js` wrapper). Same env vars and HTTP
surface. **Not published to npm** — use Cargo or a release binary instead of `npx`.

## Prerequisites

- `opencode` CLI **2.x** installed and authenticated:

  ```bash
  opencode --version   # must report v2.x
  opencode auth login
  ```

- Rust 1.75+ (`rustup` / `cargo`) when installing from source. Prebuilt release
  binaries do not require Rust.

Server checks for `opencode` at startup and exits if CLI is missing.

## Install

### Cargo from GitHub

Closest current equivalent to a global npm install:

```bash
cargo install --git https://github.com/Fast-Editor/OpenCode-Wrap.git
opencode-wrap
```

### Cargo from crates.io

Once `opencode-wrap` is published to crates.io:

```bash
cargo install opencode-wrap
opencode-wrap
```

### Prebuilt release binary

Once GitHub Releases are configured with `cargo-dist`, users can install without
Rust using generated installer:

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/Fast-Editor/OpenCode-Wrap/releases/latest/download/opencode-wrap-installer.sh | sh
```

This installer command requires release workflow and installer asset to exist.
Until then, use Cargo-from-GitHub command above.

### Local source checkout

```bash
git clone https://github.com/Fast-Editor/OpenCode-Wrap.git
cd OpenCode-Wrap
cargo install --path . --force
opencode-wrap
```

## Build and run

From this repository:

```bash
cargo build --release
./target/release/opencode-wrap
# OpenAI-compatible API at http://127.0.0.1:8000/v1
```

Or during development:

```bash
cargo run --release
```

If `cargo` is not found after installing Rust:

```bash
export PATH="$HOME/.cargo/bin:$(dirname "$(rustup which rustc)"):$PATH"
cargo run --release
```

Set custom wrapper port and working directory:

```bash
WRAP_PORT=8080 WRAP_CWD="$PWD" cargo run --release
```

Spawns its own `opencode serve` on `:4100` if none is reachable. OpenCode 2
protects its API with HTTP basic auth: the wrapper generates a random password
for the server it spawns, or uses `OPENCODE_SERVER_PASSWORD` when set. To point
it at an `opencode serve` you started yourself, export the same
`OPENCODE_SERVER_PASSWORD` (and `OPENCODE_SERVER_USERNAME` if you changed it)
before starting the wrapper.

## Configuration

| Env | Default | Meaning |
| --- | ------- | ------- |
| `WRAP_PORT` | `8000` | This server's port |
| `OPENCODE_BASE` | `http://127.0.0.1:4100` | Existing `opencode serve` URL (trailing `/` ok) |
| `OPENCODE_PORT` | `4100` | Port for auto-spawned serve |
| `OPENCODE_SERVER_PASSWORD` | random per start | Basic-auth password for `opencode serve`. Required when `OPENCODE_BASE` points at a server you started yourself |
| `OPENCODE_SERVER_USERNAME` | `opencode` | Basic-auth username for `opencode serve` |
| `WRAP_MODEL` | `muse-spark-1.3-contributor-free` | Backend model |
| `WRAP_PROVIDER` | `opencode` | Backend provider |
| `WRAP_AGENT` | `build` | OpenCode agent each per-request session runs under |
| `WRAP_CWD` | `/tmp` (Unix) / `%TEMP%` (Windows) | Existing directory for spawned serve. Native bash tools run here — use an empty sandbox, not your home directory |
| `WRAP_OCO_TIMEOUT_MS` | `180000` | Per-backend-request timeout |
| `WRAP_MAX_BODY_BYTES` | `8388608` | Max chat body. Oversize requests return OpenAI-shaped `413` |
| `WRAP_ZEN_MODELS_URL` | `https://opencode.ai/zen/v1/models` | Zen model allowlist |
| `WRAP_API_KEY` | unset | When set, `/v1/*` routes require `Authorization: Bearer <key>` (health stays open) |
| `WRAP_CORS_ORIGINS` | unset (any) | Comma-separated allowed CORS origins |
| `WRAP_MAX_IN_FLIGHT` | `8` | Concurrent chat requests before replying `429` |

## Windows

Works with a native `opencode.exe` on `PATH` (official installer, scoop,
etc.). npm installs expose OpenCode as an `opencode.cmd` shim; the wrapper
finds that too, but then only the shim is killed on shutdown, so the spawned
`opencode serve` can outlive the wrapper — prefer the native install, or run
`opencode serve` yourself and point `OPENCODE_BASE` at it.

## Tests

```bash
cargo test
```

Unit tests cover translation, validation, retries, `prepare_turn`, the V2
message projection and event filtering. CI also runs `cargo fmt --check` and
`cargo clippy --all-targets -- -D warnings`.

## Endpoints

- `GET /health`, `/v1/health` — liveness probe
- `GET /v1/models` — default backend first, then full Zen list (`opencode/<id>`)
- `POST /v1/chat/completions` — `model`, `messages`, `tools`, `tool_choice`
  (`none` / `auto` / `required` / `{"type":"function","function":{"name"}}`),
  `stream`, vision `image_url` parts, `developer` role, `response_format`,
  `stop` (at most 4 sequences), `max_tokens` / `max_completion_tokens`, `n=1`
  only. `temperature` / `top_p` validated but the backend has no per-request
  sampling knobs. `stream: true` forwards backend `session.text.delta` events
  as OpenAI SSE chunks and ends with `data: [DONE]`. Append `#variant` to an
  `opencode/<id>` model (e.g. `opencode/<id>#high`) to select an OpenCode 2
  model variant.

## Examples

### Union Alpha

```bash
curl http://127.0.0.1:8000/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"opencode/union-alpha",
       "messages":[{"role":"user","content":"say hi"}]}'
```

List models:

```bash
curl http://127.0.0.1:8000/v1/models
```

Health check:

```bash
curl http://127.0.0.1:8000/health
```

### Default model

```bash
curl http://127.0.0.1:8000/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "opencode/muse-spark-1.3-contributor-free",
    "messages": [
      {
        "role": "user",
        "content": "Explain Rust ownership in one sentence."
      }
    ]
  }'
```

Streaming:

```bash
curl -N http://127.0.0.1:8000/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "opencode/muse-spark-1.3-contributor-free",
    "stream": true,
    "messages": [
      {
        "role": "user",
        "content": "Write a short Rust hello-world program."
      }
    ]
  }'
```

### Default model and tools

```bash
curl http://127.0.0.1:8000/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"opencode/muse-spark-1.3-contributor-free",
       "messages":[{"role":"user","content":"Explain closures in one paragraph."}]}'

curl http://127.0.0.1:8000/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"opencode/muse-spark-1.3-contributor-free",
       "messages":[{"role":"user","content":"Weather in Paris?"}],
       "tools":[{"type":"function","function":{"name":"get_weather",
         "description":"Get weather for a city",
         "parameters":{"type":"object","properties":{"city":{"type":"string"}},
         "required":["city"]}}}]}'
```


## Notes

- Stateless: fresh opencode session per request (`POST /api/session` with the
  model and agent, prompt, wait for idle, read the projected messages,
  `DELETE`); history replayed each turn. Very long histories keep the newest
  turns (about 200k characters) and omit the oldest.
- `system` / `developer` messages (plus the tool and JSON-mode instructions)
  are attached to the session as an instruction entry, since OpenCode 2 has no
  per-prompt system field. If that experimental endpoint is unavailable the
  text is folded into the prompt instead.
- `WRAP_OCO_TIMEOUT_MS` bounds the whole run of one request (prompt until the
  session goes idle), not just a single HTTP call.
- Spawned `opencode serve` logs go to `$WRAP_CWD/opencode-serve.log` (mode
  `0600` on Unix), not the wrapper process.
- File / image parts must be `data:` URIs or `http(s)` URLs. OpenCode 2 only
  accepts inline data, so remote URLs are downloaded by the wrapper (20 s
  timeout) and inlined. Anything over 4 MB is rejected with `400`. The MIME
  type comes from the data URI or the `Content-Type` header, falling back to
  the file extension.
- Opencode-native tools (bash/read/edit) execute server-side automatically.
- Bare `muse-spark-*-contributor` aliased to `...-contributor-free`.
- Explicit `opencode/<id>` typos → HTTP 400 with suggestions when Zen list loads.
- Empty contentless turns retried (fresh session), not served as `stop`.
- Caller tools use instruction + `tool_call` fence parsing; returned as OpenAI
  `tool_calls`. Under `tool_choice:auto` the model may answer from knowledge —
  use `required` (or a named tool) to force.
- Transient backend failures retried 3×; rate limits → OpenAI-shaped `429`.


## Fair use

Personal / light use only. Shared free-tier backend — no bulk jobs or parallel
hammering. Respect upstream Zen / Meta terms. Default 8 MB body cap
(OpenAI-shaped `413` on oversize). Listens on `127.0.0.1` only; it reuses your
`opencode` CLI credentials, so do not expose it on a network interface.
`WRAP_CWD` is where spawned tools run — point it at an empty sandbox. Sessions
are per-request and deleted afterward.

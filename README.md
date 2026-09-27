# opencode-wrap

OpenAI-compatible HTTP API in front of `opencode serve`, using the free model
`opencode/muse-spark-1.3-contributor-free`. No `OPENCODE_API_KEY` needed —
reuses whatever auth the `opencode` CLI already has.

Rust implementation (formerly a Node `server.js` wrapper). Same env vars and HTTP
surface. **Not published to npm** — use Cargo or a release binary instead of `npx`.

## Prerequisites

- `opencode` CLI installed and authenticated:

  ```bash
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

Spawns its own `opencode serve` on `:4100` if none is reachable.

## Configuration

| Env | Default | Meaning |
| --- | ------- | ------- |
| `WRAP_PORT` | `8000` | This server's port |
| `OPENCODE_BASE` | `http://127.0.0.1:4100` | Existing `opencode serve` URL (trailing `/` ok) |
| `OPENCODE_PORT` | `4100` | Port for auto-spawned serve |
| `WRAP_MODEL` | `muse-spark-1.3-contributor-free` | Backend model |
| `WRAP_PROVIDER` | `opencode` | Backend provider |
| `WRAP_CWD` | `/tmp` | cwd for spawned serve — native bash tools run here |
| `WRAP_OCO_TIMEOUT_MS` | `180000` | Per-backend-request timeout |
| `WRAP_MAX_BODY_BYTES` | `8388608` | Max chat body (`413` if exceeded) |
| `WRAP_ZEN_MODELS_URL` | `https://opencode.ai/zen/v1/models` | Zen model allowlist |

## Tests

```bash
cargo test
```

Unit tests cover translation, validation, retries, and `prepare_turn` (parity with the old Node `test/compat.test.js`).

## Endpoints

- `GET /health`, `/v1/health` — liveness probe
- `GET /v1/models` — default backend first, then full Zen list (`opencode/<id>`)
- `POST /v1/chat/completions` — `model`, `messages`, `tools`, `tool_choice`
  (`none`/`auto`/`required`/`{function:{name}}`), `stream`, vision `image_url`
  parts, `developer` role, `response_format`, `stop`, `max_tokens` /
  `max_completion_tokens`, `n=1` only. `temperature` / `top_p` validated but
  backend has no per-request sampling knobs. `stream: true` forwards backend
  `message.part.delta` as OpenAI SSE chunks.

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

- Stateless: fresh opencode session per request; history replayed each turn.
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
hammering. Respect upstream Zen / Meta terms. Default 8 MB body cap (`413` on
oversize). Localhost only; reuses your `opencode` CLI credentials. Sessions are
per-request and deleted afterward.

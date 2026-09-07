# opencode-wrap

OpenAI-compatible API in front of `opencode serve`, using the free model
`opencode/muse-spark-1.3-contributor-free`. No `OPENCODE_API_KEY` needed —
reuses whatever auth the `opencode` CLI already has.

## Prerequisites

- Node.js >= 20, zero npm dependencies.
- The `opencode` CLI installed and authenticated (`opencode auth login`).
  The server checks this on startup and exits with a clear error if the
  CLI is missing. No `OPENCODE_API_KEY` needed.

## Run

```bash
cd ~/opencode-wrap
npm start
# or: WRAP_PORT=8000 node server.js
# -> OpenAI-compatible API at http://127.0.0.1:8000/v1
```

Spawns its own `opencode serve` on `:4100` if none is reachable

## Configuration

| Env | Default | Meaning |
| --- | ------- | ------- |
| `WRAP_PORT` | `8000` | This server's port |
| `OPENCODE_BASE` | `http://127.0.0.1:4100` | Existing `opencode serve` URL (trailing `/` ok) |
| `OPENCODE_PORT` | `4100` | Port for the auto-spawned serve (if needed) |
| `WRAP_MODEL` | `muse-spark-1.3-contributor-free` | Backend model |
| `WRAP_PROVIDER` | `opencode` | Backend provider |
| `WRAP_CWD` | `/tmp` | cwd for the spawned serve — native bash tools run here, so point it at your project for codebase questions |
| `WRAP_OCO_TIMEOUT_MS` | `180000` | Per-backend-request timeout |
| `WRAP_MAX_BODY_BYTES` | `8388608` | Max chat request body, larger → `413` |

Run `npm test` for the smoke tests (`node --test`).

## Endpoints

- `GET /health` (also `/v1/health`) — liveness probe.
- `GET /v1/models`
- `POST /v1/chat/completions` — `model`, `messages`, `tools`, `tool_choice`
  (`none`/`auto`/`required`/`{function:{name}}`), `stream` all supported.

## Examples

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
# -> finish_reason tool_calls, then send back role:tool result for final answer
```

Point Lynkr at it as a generic OpenAI endpoint (`http://127.0.0.1:8000/v1`).

## Notes

- Stateless: fresh opencode session per request, full history replayed each turn.
- Opencode-native tools (bash/read/edit) execute server-side automatically.
- Model IDs are trimmed; bare `muse-spark-X-contributor` is aliased to
  `...-contributor-free` (that bare ID only exists on direct Meta endpoints,
  not Zen). Explicit `opencode/<id>` typos fail fast with HTTP 400 +
  suggestion; any other name (inline-provider / virtual names like
  `wrap/muse-spark-free`) falls back to the backend model with a log line.
- Empty contentless turns are retried (fresh session), not served as `stop`.

## Skipping Lynkr for chores (fix #3)

`~/.config/opencode/opencode.jsonc` defines a direct `wrap` provider
(`http://127.0.0.1:8000/v1`) and sets it as `small_model`, so session titles
and compaction go straight to the free backend instead of through Lynkr
routing. Main model stays as selected in TUI (`lynkr/lynkr-auto`).
- Caller tools are translated via instruction + `tool_call` fence parsing
  (`opencode serve` has no custom-tool passthrough), then returned as
  OpenAI `tool_calls`. Under `tool_choice:auto` the model sometimes answers
  from knowledge instead of calling — use `required` (or a named tool) to force.
- Transient backend 500s (free-tier flakes) are retried 3x with fresh
  sessions + backoff; persistent failures surface as OpenAI-shaped
  `429` (rate-limited) / `502` (bad gateway) so callers can retry/cascade.

## Fair use

This wrapper sits in front of a shared free-tier backend — treat it gently:

- Personal / light use only. Don't run bulk jobs, benchmarks, or
  parallel hammering through it; the free tier is rate-limited and the
  server backs off and returns `429` when the backend says slow down.
- Respect the upstream terms of the model provider (Zen / Meta). Don't use
  this to circumvent rate limits, quotas, or access controls.
- Keep request bodies reasonable (default cap 8 MB). Malformed JSON gets a
  `400`, oversize bodies a `413` — fix the caller instead of retrying.
- It reuses your `opencode` CLI credentials, so run it on localhost only
  and never expose it to a network you don't trust.
- Sessions are created per request and deleted afterwards; disconnecting
  mid-request aborts the backend work instead of leaving it running.

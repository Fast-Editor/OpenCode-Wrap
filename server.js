/**
 * opencode-wrap — OpenAI-compatible API in front of `opencode serve`.
 *
 * Exposes:
 *   GET  /v1/models
 *   POST /v1/chat/completions   (tools + tool_choice + stream supported)
 *
 * Backend: opencode session API with model opencode/muse-spark-1.3-contributor-free.
 * No OPENCODE_API_KEY needed — reuses whatever auth `opencode` CLI already has.
 * If no `opencode serve` is reachable, one is spawned automatically.
 *
 * Zero dependencies. Node >= 20.
 *
 * Env:
 *   WRAP_PORT=8000            this server's port
 *   OPENCODE_BASE=             existing serve URL (default http://127.0.0.1:4100)
 *   OPENCODE_PORT=4100        port for the auto-spawned serve (if needed)
 *   WRAP_MODEL=muse-spark-1.3-contributor-free
 *   WRAP_PROVIDER=opencode
 *   WRAP_CWD=/tmp             working dir for spawned serve (native bash tools run here)
 */

const http = require("node:http");
const { spawn, spawnSync } = require("node:child_process");

function numEnv(name, def, min, max) {
  const raw = process.env[name];
  if (raw === undefined || raw === "") return def;
  const n = Number(raw);
  if (!Number.isFinite(n) || n < min || n > max) {
    throw new Error(`invalid ${name}=${JSON.stringify(raw)} (expected number ${min}..${max})`);
  }
  return n;
}

function normalizeBase(u) {
  return String(u || "").replace(/\/+$/, "");
}

const PORT = numEnv("WRAP_PORT", 8000, 1, 65535);
const OPENCODE_PORT = numEnv("OPENCODE_PORT", 4100, 1, 65535);
const OPENCODE_BASE = normalizeBase(process.env.OPENCODE_BASE || `http://127.0.0.1:${OPENCODE_PORT}`);
if (!/^https?:\/\//.test(OPENCODE_BASE)) throw new Error(`invalid OPENCODE_BASE=${JSON.stringify(OPENCODE_BASE)} (must start with http:// or https://)`);
const DEFAULT_MODEL = process.env.WRAP_MODEL || "muse-spark-1.3-contributor-free";
const DEFAULT_PROVIDER = process.env.WRAP_PROVIDER || "opencode";
const WRAP_CWD = process.env.WRAP_CWD || "/tmp";
const OCO_TIMEOUT_MS = numEnv("WRAP_OCO_TIMEOUT_MS", 180000, 100, 1800000);
const WRAP_MAX_BODY_BYTES = numEnv("WRAP_MAX_BODY_BYTES", 8 * 1024 * 1024, 1024, 64 * 1024 * 1024);

let spawned = null;
let inFlight = 0;
let shuttingDown = false;

class UpstreamError extends Error {
  constructor(status, bodyText) {
    super(`opencode backend -> ${status}`);
    this.upstreamStatus = status;
    this.upstreamBody = bodyText;
  }
}

class OcoTimeoutError extends Error {
  constructor(method, path, timeoutMs, cause) {
    super(`opencode request timeout after ${timeoutMs}ms: ${method} ${path}`);
    this.name = "OcoTimeoutError";
    this.code = "OCO_TIMEOUT";
    this.method = method;
    this.path = path;
    if (cause) this.cause = cause;
  }
}

function isTimeoutAbort(err) {
  if (!err) return false;
  if (err instanceof OcoTimeoutError) return true;
  if (err.name === "AbortError" || err.name === "TimeoutError" || err.code === "ABORT_ERR" || err.code === "OCO_TIMEOUT") return true;
  return /This operation was aborted|was aborted|aborted/i.test(err.message || "");
}

function isTransientConnectionError(err) {
  if (!err) return false;
  if (err.code && ["ECONNREFUSED", "ECONNRESET", "EPIPE", "ETIMEDOUT", "ENOTFOUND"].includes(err.code)) return true;
  if (err.cause && typeof err.cause === "object" && err.cause.code && ["ECONNREFUSED", "ECONNRESET", "EPIPE", "ETIMEDOUT"].includes(err.cause.code)) return true;
  return /fetch failed|ECONNREFUSED|ECONNRESET|EPIPE|socket hang up|network socket disconnected/i.test(err.message || "");
}

class ValidationError extends Error {
  constructor(message, httpStatus = 400, code = "model_not_found") {
    super(message);
    this.httpStatus = httpStatus;
    this.code = code;
  }
}

class ClientAbortError extends Error {
  constructor(message = "client disconnected") {
    super(message);
    this.name = "ClientAbortError";
    this.code = "CLIENT_ABORT";
  }
}

// Known free-tier aliases: bare "<family>-contributor" IDs don't exist on Zen
// (only "<family>-contributor-free" does) — users coming from direct Meta
// endpoints (OPENAI_MODEL=muse-spark-1.2-contributor) hit this. Map + warn.
function resolveModelId(raw) {
  let id = String(raw || "").trim();
  const m = id.match(/^(muse-spark-.+-contributor)$/);
  if (m && !id.endsWith("-free")) {
    console.log(`[wrap] aliasing unknown model "${id}" -> "${id}-free"`);
    id = `${id}-free`;
  }
  return id;
}

// Cached allowlist of Zen model IDs (public endpoint) for fail-fast 400s
// with suggestions, instead of 3x retry + opaque 502 on typos.
let zenModelsCache = { at: 0, ids: new Set() };
const ZEN_MODELS_URL = process.env.WRAP_ZEN_MODELS_URL || "https://opencode.ai/zen/v1/models";
async function zenModelIds() {
  if (Date.now() - zenModelsCache.at < 3600_000 && zenModelsCache.ids.size) return zenModelsCache.ids;
  try {
    const res = await fetch(ZEN_MODELS_URL, { signal: AbortSignal.timeout(15000) });
    const json = await res.json();
    const ids = new Set((json.data || []).map((m) => m.id));
    if (ids.size) zenModelsCache = { at: Date.now(), ids };
    return zenModelsCache.ids;
  } catch (e) {
    console.error(`[wrap] zen models fetch failed (${e.message}), skipping validation`);
    return new Set();
  }
}

function combineSignals(signals) {
  const list = (signals || []).filter(Boolean);
  if (list.length === 0) return undefined;
  if (list.length === 1) return list[0];
  if (typeof AbortSignal.any === "function") return AbortSignal.any(list);
  const ctrl = new AbortController();
  for (const s of list) {
    if (s.aborted) { ctrl.abort(s.reason); break; }
    s.addEventListener("abort", () => ctrl.abort(s.reason), { once: true });
  }
  return ctrl.signal;
}

async function oco(path, method = "GET", body, opts = {}) {
  const timeoutMs = opts.timeoutMs || OCO_TIMEOUT_MS;
  const ctrl = new AbortController();
  const t = setTimeout(() => ctrl.abort(new OcoTimeoutError(method, path, timeoutMs)), timeoutMs);
  const signal = combineSignals([ctrl.signal, opts.signal]);
  try {
    if (opts.signal?.aborted) throw opts.signal.reason instanceof Error ? opts.signal.reason : new ClientAbortError();
    const res = await fetch(`${OPENCODE_BASE}${path}`, {
      method,
      headers: { "Content-Type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
      signal,
    });
    const text = await res.text();
    let json = null;
    try { json = text ? JSON.parse(text) : null; } catch { /* raw */ }
    if (!res.ok) throw new UpstreamError(res.status, text.slice(0, 2000));
    return json;
  } catch (err) {
    // Client abort wins over timeout mapping: no point retrying when
    // the caller is gone. `fetch` rejects with the abort reason when
    // available, otherwise a generic AbortError.
    if (err instanceof ClientAbortError) throw err;
    if (opts.signal?.aborted && (err === opts.signal.reason || err.name === "AbortError")) {
      throw opts.signal.reason instanceof Error ? opts.signal.reason : new ClientAbortError();
    }
    // `fetch` rejects with the abort reason when available, otherwise a
    // generic AbortError — normalize both to OcoTimeoutError so retry +
    // status mapping treat backend stalls as transient 502s, not 500s.
    if (err instanceof OcoTimeoutError) throw err;
    if (isTimeoutAbort(err)) throw new OcoTimeoutError(method, path, timeoutMs, err);
    throw err;
  } finally {
    clearTimeout(t);
  }
}

// Free-tier Zen flakes as transient 500 UnknownError (and rate limits).
// Retry those with a fresh session + backoff; fail fast on 4xx (our bug).
// Backend stalls (OCO timeout/abort) and transient connection errors are
// also retryable — previously they fell through as non-UpstreamError and
// failed fast as opaque 500 on attempt 1/3.
function isRetryableUpstream(err) {
  if (err instanceof ClientAbortError) return false;
  if (isTimeoutAbort(err) || isTransientConnectionError(err)) return true;
  if (!(err instanceof UpstreamError)) return false;
  if (err.upstreamStatus >= 400 && err.upstreamStatus < 500) return false;
  return true;
}
function isRateLimitedUpstream(err) {
  return err instanceof UpstreamError && /rate.?limit|429|freeusagelimit|overloaded|capacity|too many requests/i.test(err.upstreamBody || "");
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function promptWithRetry(fn, maxAttempts = 3, signal) {
  let lastErr;
  for (let attempt = 1; attempt <= maxAttempts; attempt++) {
    if (signal?.aborted) throw signal.reason instanceof Error ? signal.reason : new ClientAbortError();
    try {
      return await fn();
    } catch (err) {
      lastErr = err;
      if (err instanceof ClientAbortError || signal?.aborted) throw err instanceof ClientAbortError ? err : (signal.reason instanceof Error ? signal.reason : new ClientAbortError());
      const detail = err instanceof UpstreamError
        ? `upstream ${err.upstreamStatus}: ${(err.upstreamBody || "").slice(0, 300)}`
        : `${err.name || "Error"}: ${err.message}`;
      console.error(`[wrap] attempt ${attempt}/${maxAttempts} failed: ${detail}`);
      if (!isRetryableUpstream(err) || attempt === maxAttempts) throw err;
      const delay = isRateLimitedUpstream(err) ? 4000 : 1500 * attempt;
      if (signal) {
        await Promise.race([
          sleep(delay),
          new Promise((_, rej) => signal.addEventListener("abort", () => rej(signal.reason instanceof Error ? signal.reason : new ClientAbortError()), { once: true })),
        ]);
      } else {
        await sleep(delay);
      }
    }
  }
  throw lastErr;
}

// Map backend failures to OpenAI-shaped errors so callers (Lynkr) can
// retry/cascade instead of dying on an opaque 500:
//   rate-limited -> 429, transient 5xx -> 502, client bug -> 400.
function upstreamHttpStatus(err) {
  if (err instanceof ClientAbortError) return 499;
  if (isRateLimitedUpstream(err)) return 429;
  if (isTimeoutAbort(err) || isTransientConnectionError(err)) return 502;
  if (err instanceof UpstreamError) return err.upstreamStatus >= 500 ? 502 : err.upstreamStatus;
  return 500;
}

function isOpencodeInstalled() {
  try {
    const r = spawnSync("opencode", ["--version"], { stdio: "ignore", timeout: 15000 });
    return !r.error && r.status === 0;
  } catch {
    return false;
  }
}

function requireOpencodeInstalled() {
  if (isOpencodeInstalled()) return;
  console.error("[wrap] opencode CLI not found or not runnable.");
  console.error("[wrap] Install it first (see https://opencode.ai/docs), then run `opencode auth login` and restart this server.");
  process.exit(1);
}

async function ensureOpencode() {
  try {
    const h = await oco("/global/health");
    if (h && h.healthy) { console.log(`[wrap] using existing opencode serve at ${OPENCODE_BASE}`); return; }
  } catch { /* spawn below */ }
  console.log(`[wrap] spawning \`opencode serve\` on :${OPENCODE_PORT} (cwd=${WRAP_CWD}) ...`);
  spawned = spawn("opencode", ["serve", "--port", String(OPENCODE_PORT), "--hostname", "127.0.0.1"], {
    cwd: WRAP_CWD, stdio: ["ignore", "inherit", "inherit"],
  });
  spawned.on("exit", (c) => console.log(`[wrap] opencode serve exited (${c})`));
  spawned.on("error", (e) => {
    console.error(`[wrap] failed to spawn \`opencode serve\`: ${e.message}`);
    console.error("[wrap] Is the opencode CLI installed? See https://opencode.ai/docs");
    process.exit(1);
  });
  const deadline = Date.now() + 20000;
  for (;;) {
    try {
      const h = await oco("/global/health");
      if (h && h.healthy) { console.log(`[wrap] opencode serve ready`); return; }
    } catch { /* retry */ }
    if (Date.now() > deadline) throw new Error("opencode serve did not become healthy in 20s");
    await new Promise((r) => setTimeout(r, 500));
  }
}

// ---------- OpenAI <-> opencode translation ----------

function readBody(req, opts = {}) {
  const maxBytes = opts.maxBytes || WRAP_MAX_BODY_BYTES;
  const signal = opts.signal;
  return new Promise((resolve, reject) => {
    let settled = false;
    const done = (fn, val) => { if (!settled) { settled = true; fn(val); } };
    const chunks = [];
    let len = 0;
    let tooBig = false;
    const onAbort = () => done(reject, signal.reason instanceof Error ? signal.reason : new ClientAbortError("request body read aborted"));
    if (signal?.aborted) return onAbort();
    signal?.addEventListener("abort", onAbort, { once: true });
    req.on("data", (c) => {
      if (tooBig) return; // drain rest without buffering; 'end' below is a no-op once settled
      len += c.length;
      if (len > maxBytes) {
        tooBig = true;
        chunks.length = 0;
        // NOTE: do NOT req.destroy() here — that kills the socket before
        // the 413 response can be written. Just reject and drain.
        done(reject, new ValidationError(`request body exceeds ${maxBytes} bytes`, 413, "payload_too_large"));
        return;
      }
      chunks.push(c);
    });
    req.on("end", () => {
      signal?.removeEventListener("abort", onAbort);
      if (settled) return;
      const buf = Buffer.concat(chunks).toString("utf8");
      if (!buf) return done(resolve, {});
      try { done(resolve, JSON.parse(buf)); }
      catch { done(reject, new ValidationError("malformed JSON body", 400, "parse_error")); }
    });
    req.on("error", (e) => done(reject, e));
    req.on("close", () => {
      if (!settled && !req.complete) done(reject, new ClientAbortError("client disconnected during body read"));
    });
  });
}

function msgText(m) {
  const c = m.content;
  if (typeof c === "string") return c;
  if (Array.isArray(c)) return c.map((p) => (typeof p === "string" ? p : p.text || "")).join("");
  return "";
}

// Render full OpenAI history into one user text (stateless: fresh session per request).
function renderHistory(messages) {
  const out = [];
  for (const m of messages || []) {
    if (m.role === "system") continue;
    if (m.role === "tool") {
      out.push(`TOOL RESULT (name=${m.name || "?"} id=${m.tool_call_id || "?"}):\n${msgText(m)}`);
    } else if (m.role === "assistant" && m.tool_calls) {
      if (msgText(m)) out.push(`ASSISTANT: ${msgText(m)}`);
      out.push(`ASSISTANT TOOL CALLS: ${JSON.stringify(m.tool_calls.map((t) => ({ id: t.id, name: t.function?.name, arguments: t.function?.arguments })))}`);
    } else {
      out.push(`${String(m.role || "user").toUpperCase()}: ${msgText(m)}`);
    }
  }
  return out.join("\n\n");
}

function toolInstruction(tools, toolChoice) {
  if (!Array.isArray(tools) || tools.length === 0) return "";
  if (toolChoice === "none") return "";
  const defs = tools.map((t) => (t.type === "function" ? { name: t.function.name, description: t.function.description || "", parameters: t.function.parameters || {} } : t));
  let rule = "If the request needs a tool, reply with one or more tool call blocks. If no tool is needed, answer normally with no blocks.";
  if (toolChoice === "required" || toolChoice === "auto") {
    if (toolChoice === "required") rule = "You MUST call at least one tool — reply with tool call block(s). Do not answer in plain text.";
  } else if (toolChoice && typeof toolChoice === "object" && toolChoice.function?.name) {
    rule = `You MUST call the tool named "${toolChoice.function.name}" — reply with its tool call block. Do not answer in plain text.`;
  }
  return [
    "You have access to these tools (OpenAI function format):",
    JSON.stringify(defs),
    "RULES: " + rule,
    'Each call is exactly one fenced block, nothing else inside the block:',
    "```tool_call",
    '{"name":"<tool name>","arguments":{...}}',
    "```",
    "Use only the listed tool names. arguments must be a JSON object matching the tool's parameters schema. Put any explanation OUTSIDE the blocks.",
  ].join("\n");
}

function parseToolCalls(text, knownNames) {
  const calls = [];
  const re = /```tool_call\s*([\s\S]*?)```/g;
  let m, rest = text;
  while ((m = re.exec(text))) {
    try {
      const obj = JSON.parse(m[1].trim());
      if (obj && typeof obj.name === "string" && (!knownNames || knownNames.has(obj.name))) {
        calls.push({ name: obj.name, args: obj.arguments && typeof obj.arguments === "object" ? obj.arguments : {} });
        rest = rest.replace(m[0], "");
      }
    } catch { /* not valid JSON — leave as text */ }
  }
  return { content: rest.trim(), calls };
}

const rid = (p) => `${p}_${Date.now().toString(36)}${Math.floor(Math.random() * 1e6).toString(36)}`;

// Shared first half of a turn: model resolution + prompt rendering.
// Used by both buffered and streaming paths (streaming must run this
// BEFORE sending SSE headers so ValidationErrors still map to clean 400s).
async function prepareTurn(body) {
  const rawReq = String(body.model || `${DEFAULT_PROVIDER}/${DEFAULT_MODEL}`).trim();
  const explicit = rawReq.includes("/");
  const [prov, ...rest] = explicit ? rawReq.split("/") : [DEFAULT_PROVIDER, rawReq];
  const reqProvider = (explicit ? prov.trim() : DEFAULT_PROVIDER) || DEFAULT_PROVIDER;
  let reqId = resolveModelId(explicit ? rest.join("/").trim() : rawReq);

  // Gateway behavior: this server fronts ONE backend model. An explicit
  // `opencode/<id>` names a real Zen model (validate, fail fast on typos);
  // anything else (inline-provider names like `wrap/muse-spark-free`,
  // Lynkr virtual names) falls back to the default backend with a warning.
  const ids = await zenModelIds();
  let model;
  if (ids.has(reqId)) {
    model = { providerID: "opencode", modelID: reqId }; // valid Zen id (bare or opencode/-prefixed)
  } else if (explicit && reqProvider === "opencode" && ids.size) {
    const sug = [...ids].filter((id) => id.includes(reqId.replace(/-free$/, ""))).slice(0, 3);
    throw new ValidationError(`unknown Zen model "${reqId}"${sug.length ? `, did you mean: ${sug.join(", ")}?` : ""}`);
  } else if (explicit && reqProvider === "opencode" && !ids.size) {
    model = { providerID: "opencode", modelID: reqId }; // allowlist unfetchable: trust it
  } else {
    console.log(`[wrap] virtual model "${rawReq}" -> backend ${DEFAULT_PROVIDER}/${DEFAULT_MODEL}`);
    model = { providerID: DEFAULT_PROVIDER, modelID: resolveModelId(DEFAULT_MODEL) };
  }

  const messages = body.messages || [];
  const system = messages.filter((m) => m.role === "system").map(msgText).join("\n\n");
  const tools = Array.isArray(body.tools) ? body.tools : [];
  const toolChoice = body.tool_choice;
  const knownNames = new Set(tools.filter((t) => t.type === "function").map((t) => t.function.name));

  const history = renderHistory(messages.slice(0, -1));
  const lastUser = messages.length ? msgText(messages[messages.length - 1]) : "";
  const lastRole = messages.length ? messages[messages.length - 1].role : "user";
  const turn = lastRole === "tool" || lastRole === "assistant" ? lastUser : `USER: ${lastUser}`;

  const instruction = toolInstruction(tools, toolChoice);
  const promptText = [history, turn].filter(Boolean).join("\n\n");
  const systemText = [system, instruction].filter(Boolean).join("\n\n");

  const reqBody = { model, agent: "build", parts: [{ type: "text", text: promptText }] };
  if (systemText) reqBody.system = systemText;
  const modelName = body.model || `${model.providerID}/${model.modelID}`;
  return { model, modelName, reqBody, tools, toolChoice, knownNames };
}

async function handleChatCompletions(body, opts = {}) {
  const signal = opts.signal;
  const { model, modelName, reqBody, knownNames } = await prepareTurn(body);
  // promptWithRetry owns session lifecycle (fresh session per attempt).
  // Also retry empty completions (upstream sometimes returns contentless
  // stop turns; callers read those as "done" and stall mid-task).
  // Sessions are always DELETE'd: per-attempt on failure before retry,
  // and the winning session after the follow-up GET in the finally below.
  const { resp, text: firstText, sessionId: createdId } = await promptWithRetry(async () => {
    const session = await oco("/session", "POST", {}, { signal });
    const sid = session.id;
    try {
      const r = await oco(`/session/${sid}/message`, "POST", reqBody, { signal });
      const t = (r.parts || []).filter((p) => p.type === "text").map((p) => p.text || "").join("");
      if (!t) throw new UpstreamError(502, "empty completion");
      return { resp: r, text: t, sessionId: sid };
    } catch (e) {
      try { await oco(`/session/${sid}`, "DELETE", undefined, { timeoutMs: 15000 }); } catch { /* best-effort */ }
      throw e;
    }
  }, 3, signal);
  try {
  const sessionId = createdId || resp.info?.sessionID || null;
  let text = firstText;

  // resp = { info, parts } for the final assistant message; collect text across
  // the session in case the answer spans tool steps (native tools run server-side).
  let finish = resp.info?.finish || "stop";
  if (!text || finish === "tool-calls") {
    const all = sessionId ? await oco(`/session/${sessionId}/message?limit=20`, "GET", undefined, { signal }) : [];
    const texts = [];
    for (const m of all) {
      if (m.info?.role === "assistant") for (const p of m.parts || []) if (p.type === "text" && p.text) texts.push(p.text);
    }
    if (texts.length) text = texts[texts.length - 1];
  }

  const { content, calls } = parseToolCalls(text || "", knownNames.size ? knownNames : null);
  const id = rid("chatcmpl");
  const created = Math.floor(Date.now() / 1000);
  const usage = {
    prompt_tokens: resp.info?.tokens?.input ?? 0,
    completion_tokens: resp.info?.tokens?.output ?? 0,
    total_tokens: (resp.info?.tokens?.input ?? 0) + (resp.info?.tokens?.output ?? 0),
  };

  if (calls.length) {
    return {
      id, object: "chat.completion", created, model: modelName,
      choices: [{
        index: 0,
        message: {
          role: "assistant",
          content: content || null,
          tool_calls: calls.map((c) => ({ id: rid("call"), type: "function", function: { name: c.name, arguments: JSON.stringify(c.args) } })),
        },
        finish_reason: "tool_calls",
      }],
      usage,
    };
  }
  return {
    id, object: "chat.completion", created, model: modelName,
    choices: [{ index: 0, message: { role: "assistant", content: content || text || "" }, finish_reason: "stop" }],
    usage,
  };
  } finally {
    if (createdId) { try { await oco(`/session/${createdId}`, "DELETE", undefined, { timeoutMs: 15000 }); } catch { /* best-effort */ } }
  }
}

// ---------- True streaming via the backend event bus ----------
// `POST /session/:id/message` is buffered server-side, but every backend
// also broadcasts live `message.part.delta` events on `GET /event`
// ({ properties: { sessionID, field: "text", delta } }).
// For stream=true we subscribe first, then POST, and forward text deltas
// as OpenAI chunks the moment they arrive.

function parseEventBlock(block) {
  const out = [];
  for (const line of String(block).split("\n")) {
    const t = line.trim();
    if (!t.startsWith("data:")) continue;
    const payload = t.slice(5).trim();
    if (!payload || payload === "[DONE]") continue;
    try { out.push(JSON.parse(payload)); } catch { /* heartbeat / partial */ }
  }
  return out;
}

// Opens GET /event and forwards parsed events to onEvent.
// Resolves with { close } once subscribed (headers received); the reader
// loop runs in the background until close() or signal abort.
async function subscribeOcoEvents(onEvent, opts = {}) {
  const signal = opts.signal;
  if (signal?.aborted) throw signal.reason instanceof Error ? signal.reason : new ClientAbortError();
  const res = await fetch(`${OPENCODE_BASE}/event`, {
    headers: { Accept: "text/event-stream" },
    signal: signal || undefined,
  });
  if (!res.ok || !res.body) throw new UpstreamError(res.status || 502, "event stream unavailable");
  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let buf = "";
  let closed = false;
  const close = () => { closed = true; try { reader.cancel(); } catch { /* ignore */ } };
  (async () => {
    try {
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        buf += decoder.decode(value, { stream: true });
        let idx;
        while ((idx = buf.indexOf("\n\n")) !== -1) {
          const block = buf.slice(0, idx);
          buf = buf.slice(idx + 2);
          for (const ev of parseEventBlock(block)) {
            if (closed) break;
            try { onEvent(ev); } catch { /* ignore listener errors */ }
          }
        }
        if (closed) break;
      }
    } catch { /* aborted or upstream hung up */ }
  })();
  return { close };
}

function sseSend(res, base, delta, finish = null) {
  res.write(`data: ${JSON.stringify({ ...base, choices: [{ index: 0, delta, finish_reason: finish }] })}\n\n`);
}

function sseErrorAndEnd(res, base, message) {
  try { res.write(`data: ${JSON.stringify({ ...base, error: { message } })}\n\n`); } catch { /* ignore */ }
  try { res.end(); } catch { /* ignore */ }
}

async function streamChatCompletions(body, res, opts = {}) {
  const signal = opts.signal;
  // Before headers: ValidationErrors still map to clean HTTP 400s upstream.
  const { modelName, reqBody, knownNames } = await prepareTurn(body);
  const id = rid("chatcmpl");
  const created = Math.floor(Date.now() / 1000);
  const base = { id, object: "chat.completion.chunk", created, model: modelName };
  res.writeHead(200, { "Content-Type": "text/event-stream", "Cache-Control": "no-cache", Connection: "keep-alive" });
  sseSend(res, base, { role: "assistant", content: "" });

  // Retry only while nothing has been streamed yet — after the first delta
  // the client owns partial output and a fresh session can't take it back.
  let lastErr = null;
  for (let attempt = 1; attempt <= 3; attempt++) {
    if (signal?.aborted) throw signal.reason instanceof Error ? signal.reason : new ClientAbortError();
    const session = await oco("/session", "POST", {}, { signal });
    const sid = session.id;
    let streamedText = "";
    let subDone = false;
    let sub = null;
    try {
      try {
        sub = await subscribeOcoEvents((ev) => {
          if (subDone || ev?.type !== "message.part.delta") return;
          const p = ev.properties || {};
          if (p.sessionID !== sid || p.field !== "text" || typeof p.delta !== "string" || !p.delta) return;
          streamedText += p.delta;
          sseSend(res, base, { content: p.delta });
        }, { signal });
      } catch (e) {
        console.error(`[wrap] event subscription failed, falling back to buffered replay (${e.message})`);
      }
      const resp = await oco(`/session/${sid}/message`, "POST", reqBody, { signal });
      subDone = true;
      try { sub?.close(); } catch { /* ignore */ }
      let text = (resp.parts || []).filter((p) => p.type === "text").map((p) => p.text || "").join("");
      let finish = resp.info?.finish || "stop";
      if (!text || finish === "tool-calls") {
        const all = await oco(`/session/${sid}/message?limit=20`, "GET", undefined, { signal });
        const texts = [];
        for (const m of all) {
          if (m.info?.role === "assistant") for (const p of m.parts || []) if (p.type === "text" && p.text) texts.push(p.text);
        }
        if (texts.length) text = texts[texts.length - 1];
      }
      if (!text) throw new UpstreamError(502, "empty completion");
      // Tail: deltas normally concatenate to the final text; forward anything
      // the subscription missed (or everything if it never connected).
      if (text.length > streamedText.length && text.startsWith(streamedText)) {
        const tail = text.slice(streamedText.length);
        for (let i = 0; i < tail.length; i += 500) sseSend(res, base, { content: tail.slice(i, i + 500) });
      }
      const { content, calls } = parseToolCalls(text, knownNames.size ? knownNames : null);
      if (calls.length) {
        // Caller tools were fenced inside the text; surface them as deltas.
        // (content outside the fences was already streamed verbatim.)
        calls.forEach((c, i) => sseSend(res, base, { tool_calls: [{ index: i, id: rid("call"), type: "function", function: { name: c.name, arguments: JSON.stringify(c.args) } }] }));
        sseSend(res, base, {}, "tool_calls");
      } else {
        void content;
        sseSend(res, base, {}, "stop");
      }
      res.write("data: [DONE]\n\n");
      res.end();
      return;
    } catch (e) {
      lastErr = e;
      subDone = true;
      try { sub?.close(); } catch { /* ignore */ }
      try { await oco(`/session/${sid}`, "DELETE", undefined, { timeoutMs: 15000 }); } catch { /* best-effort */ }
      if (e instanceof ClientAbortError || signal?.aborted) throw e;
      const detail = e instanceof UpstreamError ? `upstream ${e.upstreamStatus}: ${(e.upstreamBody || "").slice(0, 200)}` : `${e.name || "Error"}: ${e.message}`;
      console.error(`[wrap] stream attempt ${attempt}/3 failed: ${detail}`);
      if (streamedText.length > 0 || !isRetryableUpstream(e) || attempt === 3) {
        // Headers are already sent: surface the failure in-band so the
        // client doesn't read a truncated turn as complete.
        sseErrorAndEnd(res, base, e instanceof UpstreamError ? `Upstream model backend failed: ${(e.upstreamBody || "").slice(0, 200)}` : e.message);
        try { await oco(`/session/${sid}`, "DELETE", undefined, { timeoutMs: 15000 }); } catch { /* ignore */ }
        return;
      }
      await new Promise((resolve, reject) => {
        const t = setTimeout(resolve, isRateLimitedUpstream(e) ? 4000 : 1500 * attempt);
        if (signal) signal.addEventListener("abort", () => { clearTimeout(t); reject(signal.reason instanceof Error ? signal.reason : new ClientAbortError()); }, { once: true });
      });
    }
  }
  throw lastErr;
}

// ---------- HTTP server ----------

const server = http.createServer(async (req, res) => {
  if (shuttingDown) {
    res.writeHead(503, { "Content-Type": "application/json" });
    res.end(JSON.stringify({ error: { message: "server shutting down", type: "server_error", code: "shutting_down" } }));
    return;
  }
  inFlight++;
  const clientCtrl = new AbortController();
  let completed = false;
  // res 'close' without writableEnded means the client went away mid-flight.
  // (req 'close' fires on every fully-received request, so it must NOT be used here.)
  const onResClose = () => { if (!completed && !res.writableEnded) clientCtrl.abort(new ClientAbortError()); };
  res.on("close", onResClose);
  const url = new URL(req.url, "http://x");
  res.setHeader("Access-Control-Allow-Origin", "*");
  if (req.method === "OPTIONS") { completed = true; res.removeListener("close", onResClose); inFlight--; res.writeHead(204); res.end(); return; }
  try {
    if (req.method === "GET" && (url.pathname === "/health" || url.pathname === "/v1/health")) {
      res.writeHead(200, { "Content-Type": "application/json" });
      res.end(JSON.stringify({ status: "ok", upstream: OPENCODE_BASE, spawned: Boolean(spawned) }));
      return;
    }
    if (req.method === "GET" && url.pathname === "/v1/models") {
      res.writeHead(200, { "Content-Type": "application/json" });
      res.end(JSON.stringify({ object: "list", data: [{ id: `${DEFAULT_PROVIDER}/${DEFAULT_MODEL}`, object: "model", created: Math.floor(Date.now() / 1000), owned_by: "opencode-wrap" }] }));
      return;
    }
    if (req.method === "POST" && url.pathname === "/v1/chat/completions") {
      const body = await readBody(req, { signal: clientCtrl.signal });
      console.log(`[wrap] chat: model=${JSON.stringify(body.model)} msgs=${(body.messages || []).length} tools=${(body.tools || []).length} stream=${!!body.stream}`);
      if (body.stream) {
        // True streaming: backend text deltas are forwarded live. Errors
        // after the first byte are delivered in-band (headers already sent).
        await streamChatCompletions(body, res, { signal: clientCtrl.signal });
      } else {
        const completion = await handleChatCompletions(body, { signal: clientCtrl.signal });
        if (clientCtrl.signal.aborted) return; // caller gone — session already cleaned up
        res.writeHead(200, { "Content-Type": "application/json" });
        res.end(JSON.stringify(completion));
      }
      return;
    }
    res.writeHead(404, { "Content-Type": "application/json" });
    res.end(JSON.stringify({ error: { message: "not found", type: "invalid_request_error" } }));
  } catch (e) {
    if (e instanceof ClientAbortError || clientCtrl.signal.aborted) { try { res.destroy(); } catch { /* ignore */ } return; }
    if (e instanceof ValidationError) {
      if (!res.headersSent) res.writeHead(e.httpStatus || 400, { "Content-Type": "application/json" });
      res.end(JSON.stringify({ error: { message: e.message, type: "invalid_request_error", code: e.code || "invalid_request_error" } }));
      return;
    }
    const status = upstreamHttpStatus(e);
    const type = status === 429 ? "rate_limit_error" : status === 502 ? "server_error" : "server_error";
    const code = status === 429 ? "rate_limit_exceeded" : status === 502 ? "bad_gateway" : (e instanceof UpstreamError ? "upstream_error" : "internal_error");
    const message = e instanceof UpstreamError
      ? (status === 429
          ? "Upstream model backend is rate-limited, retry with backoff."
          : `Upstream model backend failed after retries${e.upstreamBody ? `: ${(e.upstreamBody || "").slice(0, 200)}` : ""}`)
      : (status === 429
          ? "Upstream model backend is rate-limited, retry with backoff."
          : status === 502
            ? `Upstream model backend timed out / unavailable after retries (${e.message}).`
            : e.message);
    console.error(`[wrap] -> ${status}:`, message);
    // For SSE callers, an HTTP error status (not a 200 with error chunk) lets
    // agent clients retry/cascade instead of reading it as end-of-turn.
    if (!res.headersSent) res.writeHead(status, { "Content-Type": "application/json" });
    res.end(JSON.stringify({ error: { message, type, code } }));
  } finally {
    completed = true;
    res.removeListener("close", onResClose);
    inFlight--;
  }
});

function shutdown(sig) {
  if (shuttingDown) return;
  shuttingDown = true;
  console.log(`[wrap] ${sig} received, draining ${inFlight} in-flight request(s)...`);
  server.close(() => { if (spawned) spawned.kill(); process.exit(0); });
  setTimeout(() => {
    console.error(`[wrap] graceful shutdown timed out with ${inFlight} in-flight, forcing exit`);
    if (spawned) spawned.kill("SIGKILL");
    process.exit(1);
  }, 30000).unref();
}
process.on("SIGINT", () => shutdown("SIGINT"));
process.on("SIGTERM", () => shutdown("SIGTERM"));

function start() {
  requireOpencodeInstalled();
  return ensureOpencode().then(() => server.listen(PORT, () => console.log(`[wrap] OpenAI-compatible API at http://127.0.0.1:${PORT}/v1`)))
    .catch((e) => { console.error("[wrap] failed to start:", e.message); process.exit(1); });
}

if (require.main === module) {
  start();
}

module.exports = {
  server, start, ensureOpencode, handleChatCompletions, streamChatCompletions, prepareTurn,
  subscribeOcoEvents, parseEventBlock,
  resolveModelId, zenModelIds, oco, promptWithRetry,
  readBody, msgText, renderHistory, toolInstruction, parseToolCalls,
  isTimeoutAbort, isTransientConnectionError, isRetryableUpstream, isRateLimitedUpstream,
  upstreamHttpStatus, normalizeBase, numEnv, isOpencodeInstalled,
  UpstreamError, OcoTimeoutError, ValidationError, ClientAbortError,
};

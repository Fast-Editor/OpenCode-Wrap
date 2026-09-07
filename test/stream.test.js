const { test } = require("node:test");
const assert = require("node:assert/strict");
const http = require("node:http");
const { spawn } = require("node:child_process");

const wrap = require("../server.js");

test("parseEventBlock parses SSE blocks, skips noise", () => {
  const evs = wrap.parseEventBlock(': heartbeat\ndata: {"type":"message.part.delta"}\n\ndata: [DONE]\n\ndata: not-json\n\ndata: {"type":"text"}');
  assert.deepEqual(evs, [{ type: "message.part.delta" }, { type: "text" }]);
});

function startFakeUpstream() {
  let latestSid = null;
  let n = 0;
  const srv = http.createServer((req, res) => {
    const url = new URL(req.url, "http://x");
    if (url.pathname === "/global/health") {
      res.writeHead(200, { "Content-Type": "application/json" });
      res.end(JSON.stringify({ healthy: true }));
      return;
    }
    if (req.method === "POST" && url.pathname === "/session") {
      n++;
      latestSid = `ses_stream${n}`;
      res.writeHead(200, { "Content-Type": "application/json" });
      res.end(JSON.stringify({ id: latestSid }));
      return;
    }
    if (req.method === "GET" && url.pathname === "/event") {
      res.writeHead(200, { "Content-Type": "text/event-stream", "Cache-Control": "no-cache", Connection: "keep-alive" });
      res.write('data: {"id":"e0","type":"server.connected","properties":{}}\n\n');
      const timers = [
        setTimeout(() => {
          if (latestSid) res.write(`data: ${JSON.stringify({ id: "e1", type: "message.part.delta", properties: { sessionID: latestSid, messageID: "m1", partID: "p1", field: "text", delta: "hello " } })}\n\n`);
        }, 300),
        setTimeout(() => {
          if (latestSid) res.write(`data: ${JSON.stringify({ id: "e2", type: "message.part.delta", properties: { sessionID: latestSid, messageID: "m1", partID: "p1", field: "text", delta: "world" } })}\n\n`);
        }, 600),
      ];
      req.on("close", () => timers.forEach(clearTimeout));
      return;
    }
    const m = url.pathname.match(/^\/session\/([^/]+)\/message$/);
    if (m && req.method === "POST") {
      let b = "";
      req.on("data", (c) => { b += c; });
      req.on("end", () => {
        // Hold the POST open long after the deltas were emitted: if the
        // wrapper were fake-streaming, nothing would arrive before this.
        setTimeout(() => {
          res.writeHead(200, { "Content-Type": "application/json" });
          res.end(JSON.stringify({
            info: { sessionID: m[1], finish: "stop", tokens: { input: 1, output: 2 } },
            parts: [{ type: "text", text: "hello world" }],
          }));
        }, 1500);
      });
      return;
    }
    const d = url.pathname.match(/^\/session\/([^/]+)$/);
    if (d && req.method === "DELETE") {
      res.writeHead(200, { "Content-Type": "application/json" });
      res.end(JSON.stringify({}));
      return;
    }
    if (m && req.method === "GET") {
      res.writeHead(200, { "Content-Type": "application/json" });
      res.end(JSON.stringify([]));
      return;
    }
    res.writeHead(404);
    res.end();
  });
  return new Promise((resolve) => srv.listen(0, "127.0.0.1", () => resolve(srv)));
}

function postStream(port, payload) {
  return new Promise((resolve, reject) => {
    const t0 = Date.now();
    const firstChunkAt = { t: null };
    let content = "";
    let done = false;
    let buf = "";
    const req = http.request(
      { host: "127.0.0.1", port, path: "/v1/chat/completions", method: "POST", headers: { "Content-Type": "application/json" } },
      (res) => {
        res.on("data", (c) => {
          buf += c.toString();
          let idx;
          while ((idx = buf.indexOf("\n\n")) !== -1) {
            const block = buf.slice(0, idx);
            buf = buf.slice(idx + 2);
            for (const line of block.split("\n")) {
              const t = line.trim();
              if (!t.startsWith("data:")) continue;
              const p = t.slice(5).trim();
              if (p === "[DONE]") { done = true; continue; }
              try {
                const ev = JSON.parse(p);
                const delta = ev.choices?.[0]?.delta;
                if (delta?.content) {
                  if (firstChunkAt.t === null) firstChunkAt.t = Date.now() - t0;
                  content += delta.content;
                }
              } catch { /* ignore */ }
            }
          }
        });
        res.on("end", () => resolve({ status: res.statusCode, content, firstChunkAt: firstChunkAt.t, elapsed: Date.now() - t0, done }));
        res.on("error", reject);
      }
    );
    req.on("error", reject);
    req.end(JSON.stringify(payload));
  });
}

test("stream=true forwards backend deltas before the POST completes", async () => {
  const fake = await startFakeUpstream();
  const fakePort = fake.address().port;
  const child = spawn("node", ["server.js"], {
    cwd: __dirname + "/..",
    env: {
      ...process.env,
      OPENCODE_BASE: `http://127.0.0.1:${fakePort}`,
      WRAP_PORT: "8897",
      WRAP_OCO_TIMEOUT_MS: "20000",
      WRAP_ZEN_MODELS_URL: "http://127.0.0.1:1/nope",
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  try {
    for (let i = 0; i < 40; i++) {
      try {
        await new Promise((resolve, reject) => {
          http.get(`http://127.0.0.1:8897/v1/models`, (r) => { r.resume(); r.on("end", resolve); }).on("error", reject);
        });
        break;
      } catch { await new Promise((r) => setTimeout(r, 250)); }
    }
    const r = await postStream(8897, {
      model: "wrap/test-model",
      messages: [{ role: "user", content: "hi" }],
      stream: true,
    });
    assert.equal(r.status, 200);
    assert.equal(r.done, true);
    assert.equal(r.content, "hello world");
    assert.ok(r.firstChunkAt !== null, "no content chunk arrived");
    assert.ok(r.firstChunkAt < r.elapsed - 800, `first chunk at ${r.firstChunkAt}ms, stream ended at ${r.elapsed}ms — not true streaming`);
  } finally {
    child.kill("SIGTERM");
    await new Promise((r) => setTimeout(r, 300));
    child.kill("SIGKILL");
    fake.close();
  }
});

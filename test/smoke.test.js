const { test } = require("node:test");
const assert = require("node:assert/strict");
const { EventEmitter } = require("node:events");

const wrap = require("../server.js");

function fakeReq(chunks = [], { complete = true } = {}) {
  const req = new EventEmitter();
  req.complete = complete;
  process.nextTick(() => {
    for (const c of chunks) req.emit("data", Buffer.isBuffer(c) ? c : Buffer.from(c));
    req.emit("end");
  });
  req.destroy = () => {};
  return req;
}

test("resolveModelId aliases bare -contributor to -free", () => {
  assert.equal(wrap.resolveModelId("muse-spark-1.3-contributor"), "muse-spark-1.3-contributor-free");
  assert.equal(wrap.resolveModelId("muse-spark-1.3-contributor-free"), "muse-spark-1.3-contributor-free");
});

test("normalizeBase strips trailing slashes", () => {
  assert.equal(wrap.normalizeBase("http://127.0.0.1:4100/"), "http://127.0.0.1:4100");
  assert.equal(wrap.normalizeBase("http://127.0.0.1:4100///"), "http://127.0.0.1:4100");
});

test("parseToolCalls extracts valid fence, ignores single-quotes/prose", () => {
  const text = [
    "hello",
    "```tool_call",
    '{"name":"get_weather","arguments":{"city":"Paris"}}',
    "```",
  ].join("\n");
  const { content, calls } = wrap.parseToolCalls(text, new Set(["get_weather"]));
  assert.equal(calls.length, 1);
  assert.equal(calls[0].name, "get_weather");
  assert.equal(content, "hello");

  const bad = "```tool_call\n{'name':'get_weather'}\n```";
  assert.equal(wrap.parseToolCalls(bad, new Set(["get_weather"])).calls.length, 0);
});

test("timeouts map to 502 and are retryable; client abort is not", () => {
  const t = new wrap.OcoTimeoutError("POST", "/session", 10);
  assert.equal(wrap.upstreamHttpStatus(t), 502);
  assert.equal(wrap.isRetryableUpstream(t), true);
  const abort = new wrap.ClientAbortError();
  assert.equal(wrap.isRetryableUpstream(abort), false);
  assert.equal(wrap.upstreamHttpStatus(abort), 499);
});

test("isOpencodeInstalled detects the CLI", () => {
  assert.equal(wrap.isOpencodeInstalled(), true);
});

test("readBody parses JSON", async () => {
  const body = await wrap.readBody(fakeReq(['{"a":1}']));
  assert.deepEqual(body, { a: 1 });
});

test("readBody rejects malformed JSON as 400 parse_error", async () => {
  await assert.rejects(wrap.readBody(fakeReq(["{nope"])), (e) => e instanceof wrap.ValidationError && e.httpStatus === 400 && e.code === "parse_error");
});

test("readBody rejects oversize as 413 payload_too_large", async () => {
  await assert.rejects(
    wrap.readBody(fakeReq([Buffer.alloc(100)]), { maxBytes: 10 }),
    (e) => e instanceof wrap.ValidationError && e.httpStatus === 413 && e.code === "payload_too_large"
  );
});

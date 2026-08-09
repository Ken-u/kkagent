import test from "node:test";
import assert from "node:assert/strict";
import { KkagentHttpClient, JsonRpcClient } from "../src/index.js";

test("client constructs", () => {
  const c = new KkagentHttpClient({ baseUrl: "http://127.0.0.1:8787" });
  assert.equal(c.baseUrl, "http://127.0.0.1:8787");
});

test("HTTP requests use bearer authorization instead of query tokens", async () => {
  const originalFetch = globalThis.fetch;
  let observed;
  globalThis.fetch = async (url, options) => {
    observed = { url: String(url), options };
    return { ok: true, json: async () => ({ ok: true }) };
  };
  try {
    const client = new KkagentHttpClient({ token: "secret" });
    await client.meta();
    assert.equal(new URL(observed.url).searchParams.has("token"), false);
    assert.equal(observed.options.headers.authorization, "Bearer secret");
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test("jsonrpc shapes request", async () => {
  const rpc = new JsonRpcClient(async (line) => {
    const req = JSON.parse(line);
    assert.equal(req.method, "ping");
    return JSON.stringify({ jsonrpc: "2.0", id: req.id, result: { ok: true } });
  });
  const r = await rpc.call("ping");
  assert.deepEqual(r, { ok: true });
});

test("postMessage sends an idempotency key", async () => {
  const originalFetch = globalThis.fetch;
  let observed;
  globalThis.fetch = async (_url, options) => {
    observed = options;
    return { ok: true, json: async () => ({ task_id: "task-1" }) };
  };
  try {
    const client = new KkagentHttpClient();
    const result = await client.postMessage("session", "hello", { idempotencyKey: "request-1" });
    assert.equal(observed.headers["idempotency-key"], "request-1");
    assert.equal(result.task_id, "task-1");
  } finally {
    globalThis.fetch = originalFetch;
  }
});

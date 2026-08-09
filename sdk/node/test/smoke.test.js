import test from "node:test";
import assert from "node:assert/strict";
import { KkagentHttpClient, JsonRpcClient } from "../src/index.js";

test("client constructs", () => {
  const c = new KkagentHttpClient({ baseUrl: "http://127.0.0.1:8787" });
  assert.equal(c.baseUrl, "http://127.0.0.1:8787");
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

/**
 * Minimal Node SDK for kkagent HTTP + JSON-RPC (JS entry for zero-build use).
 */

export class KkagentHttpClient {
  constructor(opts = {}) {
    this.baseUrl = (opts.baseUrl ?? "http://127.0.0.1:8787").replace(/\/$/, "");
    this.token = opts.token;
  }

  url(path) {
    const u = new URL(path, this.baseUrl + "/");
    if (this.token) u.searchParams.set("token", this.token);
    return u.toString();
  }

  async meta() {
    const res = await fetch(this.url("/api/v1/meta"));
    if (!res.ok) throw new Error(`meta ${res.status}`);
    return res.json();
  }

  async listSessions() {
    const res = await fetch(this.url("/api/v1/sessions"));
    if (!res.ok) throw new Error(`sessions ${res.status}`);
    const body = await res.json();
    return body.sessions ?? [];
  }

  async createSession(workspace, title) {
    const res = await fetch(this.url("/api/v1/sessions"), {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ workspace, title }),
    });
    if (!res.ok) throw new Error(`createSession ${res.status}`);
    return res.json();
  }

  async postMessage(sessionId, text) {
    const res = await fetch(this.url(`/api/v1/sessions/${sessionId}/messages`), {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ text }),
    });
    if (!res.ok) throw new Error(`postMessage ${res.status}`);
    return res.json();
  }

  async listTools() {
    const res = await fetch(this.url("/api/v1/tools"));
    if (!res.ok) throw new Error(`tools ${res.status}`);
    return res.json();
  }

  async modelCatalog() {
    const res = await fetch(this.url("/api/v1/modelCatalog"));
    if (!res.ok) throw new Error(`modelCatalog ${res.status}`);
    return res.json();
  }

  async eventsSince(since = 0, sessionId, limit = 500) {
    const query = new URLSearchParams({ since: String(since), limit: String(limit) });
    if (sessionId) query.set("session_id", sessionId);
    const res = await fetch(this.url(`/api/v1/events?${query}`));
    if (!res.ok) throw new Error(`events ${res.status}`);
    return res.json();
  }

  async turnStatus(sessionId) {
    const res = await fetch(this.url(`/api/v1/turns/${sessionId}`));
    if (!res.ok) throw new Error(`turnStatus ${res.status}`);
    return res.json();
  }

  connectEvents(onEvent, options = {}) {
    const u = new URL("/api/v1/ws", this.baseUrl.replace(/^http/, "ws") + "/");
    if (this.token) u.searchParams.set("token", this.token);
    if (options.since !== undefined) u.searchParams.set("since", String(options.since));
    if (options.sessionId) u.searchParams.set("session_id", options.sessionId);
    const ws = new WebSocket(u);
    ws.addEventListener("message", (msg) => {
      try {
        onEvent(JSON.parse(String(msg.data)));
      } catch {
        onEvent(msg.data);
      }
    });
    return ws;
  }
}

export class JsonRpcClient {
  constructor(send) {
    this.send = send;
  }

  async call(method, params = {}, id = 1) {
    const req = JSON.stringify({ jsonrpc: "2.0", id, method, params });
    const raw = await this.send(req);
    const resp = JSON.parse(raw);
    if (resp.error) throw Object.assign(new Error("RPC error"), { error: resp.error });
    return resp.result;
  }
}

export { KkagentHttpClient as KkagentClient };

/**
 * Minimal Node SDK for kkagent HTTP + JSON-RPC (JS entry for zero-build use).
 */

export class KkagentHttpClient {
  constructor(opts = {}) {
    this.baseUrl = (opts.baseUrl ?? "http://127.0.0.1:8787").replace(/\/$/, "");
    this.token = opts.token;
  }

  url(path) {
    return new URL(path, this.baseUrl + "/").toString();
  }

  headers(json = false) {
    return {
      ...(json ? { "content-type": "application/json" } : {}),
      ...(this.token ? { authorization: `Bearer ${this.token}` } : {}),
    };
  }

  async meta() {
    const res = await fetch(this.url("/api/v1/meta"), { headers: this.headers() });
    if (!res.ok) throw new Error(`meta ${res.status}`);
    return res.json();
  }

  async listSessions() {
    const res = await fetch(this.url("/api/v1/sessions"), { headers: this.headers() });
    if (!res.ok) throw new Error(`sessions ${res.status}`);
    const body = await res.json();
    return body.sessions ?? [];
  }

  async createSession(workspace, title) {
    const res = await fetch(this.url("/api/v1/sessions"), {
      method: "POST",
      headers: this.headers(true),
      body: JSON.stringify({ workspace, title }),
    });
    if (!res.ok) throw new Error(`createSession ${res.status}`);
    return res.json();
  }

  async postMessage(sessionId, text, options = {}) {
    const res = await fetch(this.url(`/api/v1/sessions/${sessionId}/messages`), {
      method: "POST",
      headers: {
        ...this.headers(true),
        ...(options.idempotencyKey ? { "idempotency-key": options.idempotencyKey } : {}),
      },
            body: JSON.stringify({
                text,
                images: options.images?.map((image) => ({
                    media_type: image.mediaType,
                    data: image.data,
                })),
            }),
    });
    if (!res.ok) throw new Error(`postMessage ${res.status}`);
    return res.json();
  }

  async listTools() {
    const res = await fetch(this.url("/api/v1/tools"), { headers: this.headers() });
    if (!res.ok) throw new Error(`tools ${res.status}`);
    return res.json();
  }

  async search(query) {
    const q = new URLSearchParams({ q: query });
    const res = await fetch(this.url(`/api/v1/search?${q}`), { headers: this.headers() });
    if (!res.ok) throw new Error(`search ${res.status}`);
    return res.json();
  }

  async sessionTimeline(sessionId) {
    const res = await fetch(this.url(`/api/v1/sessions/${sessionId}/timeline`), {
      headers: this.headers(),
    });
    if (!res.ok) throw new Error(`timeline ${res.status}`);
    return res.json();
  }

  async modelCatalog() {
    const res = await fetch(this.url("/api/v1/modelCatalog"), { headers: this.headers() });
    if (!res.ok) throw new Error(`modelCatalog ${res.status}`);
    return res.json();
  }

  async eventsSince(since = 0, sessionId, limit = 500) {
    const query = new URLSearchParams({ since: String(since), limit: String(limit) });
    if (sessionId) query.set("session_id", sessionId);
    const res = await fetch(this.url(`/api/v1/events?${query}`), { headers: this.headers() });
    if (!res.ok) throw new Error(`events ${res.status}`);
    return res.json();
  }

  async turnStatus(taskOrSessionId) {
    const res = await fetch(this.url(`/api/v1/turns/${taskOrSessionId}`), { headers: this.headers() });
    if (!res.ok) throw new Error(`turnStatus ${res.status}`);
    return res.json();
  }

  async cancelTurn(taskId) {
    const res = await fetch(this.url(`/api/v1/turns/${taskId}`), {
      method: "DELETE",
      headers: this.headers(),
    });
    if (!res.ok) throw new Error(`cancelTurn ${res.status}`);
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

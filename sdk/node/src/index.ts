/**
 * Minimal Node SDK for kkagent HTTP + JSON-RPC over WebSocket/stdio.
 * Mirrors ref/node-sdk surface at a smaller scope (non-Kimi).
 */

export interface KkagentClientOptions {
  baseUrl?: string;
  token?: string;
  /** Path to `kkagent` binary for local RPC spawn helpers */
  binary?: string;
}

export interface Session {
  session_id: string;
  workspace?: string;
  title?: string | null;
  messages?: unknown[];
}

export interface EventSubscriptionOptions {
  since?: number;
  sessionId?: string;
}

export interface EventHistory {
  events: unknown[];
  latest_event_seq: number;
  history_capacity: number;
}

export interface PostMessageOptions {
  /** Stable key used to safely retry the same request after a network failure. */
  idempotencyKey?: string;
}

export class KkagentHttpClient {
  readonly baseUrl: string;
  readonly token?: string;

  constructor(opts: KkagentClientOptions = {}) {
    this.baseUrl = (opts.baseUrl ?? "http://127.0.0.1:8787").replace(/\/$/, "");
    this.token = opts.token;
  }

  private url(path: string): string {
    return new URL(path, this.baseUrl + "/").toString();
  }

  private headers(json = false): Record<string, string> {
    return {
      ...(json ? { "content-type": "application/json" } : {}),
      ...(this.token ? { authorization: `Bearer ${this.token}` } : {}),
    };
  }

  async meta(): Promise<unknown> {
    const res = await fetch(this.url("/api/v1/meta"), { headers: this.headers() });
    if (!res.ok) throw new Error(`meta ${res.status}`);
    return res.json();
  }

  async listSessions(): Promise<Session[]> {
    const res = await fetch(this.url("/api/v1/sessions"), { headers: this.headers() });
    if (!res.ok) throw new Error(`sessions ${res.status}`);
    const body = (await res.json()) as { sessions?: Session[] };
    return body.sessions ?? [];
  }

  async createSession(workspace?: string, title?: string): Promise<Session> {
    const res = await fetch(this.url("/api/v1/sessions"), {
      method: "POST",
      headers: this.headers(true),
      body: JSON.stringify({ workspace, title }),
    });
    if (!res.ok) throw new Error(`createSession ${res.status}`);
    return (await res.json()) as Session;
  }

  async postMessage(
    sessionId: string,
    text: string,
    options: PostMessageOptions = {},
  ): Promise<unknown> {
    const res = await fetch(this.url(`/api/v1/sessions/${sessionId}/messages`), {
      method: "POST",
      headers: {
        ...this.headers(true),
        ...(options.idempotencyKey ? { "idempotency-key": options.idempotencyKey } : {}),
      },
      body: JSON.stringify({ text }),
    });
    if (!res.ok) throw new Error(`postMessage ${res.status}`);
    return res.json();
  }

  async listTools(): Promise<unknown> {
    const res = await fetch(this.url("/api/v1/tools"), { headers: this.headers() });
    if (!res.ok) throw new Error(`tools ${res.status}`);
    return res.json();
  }

  async modelCatalog(): Promise<unknown> {
    const res = await fetch(this.url("/api/v1/modelCatalog"), { headers: this.headers() });
    if (!res.ok) throw new Error(`modelCatalog ${res.status}`);
    return res.json();
  }

  async eventsSince(since = 0, sessionId?: string, limit = 500): Promise<EventHistory> {
    const query = new URLSearchParams({ since: String(since), limit: String(limit) });
    if (sessionId) query.set("session_id", sessionId);
    const res = await fetch(this.url(`/api/v1/events?${query}`), { headers: this.headers() });
    if (!res.ok) throw new Error(`events ${res.status}`);
    return (await res.json()) as EventHistory;
  }

  async turnStatus(taskOrSessionId: string): Promise<unknown> {
    const res = await fetch(this.url(`/api/v1/turns/${taskOrSessionId}`), { headers: this.headers() });
    if (!res.ok) throw new Error(`turnStatus ${res.status}`);
    return res.json();
  }

  async cancelTurn(taskId: string): Promise<unknown> {
    const res = await fetch(this.url(`/api/v1/turns/${taskId}`), {
      method: "DELETE",
      headers: this.headers(),
    });
    if (!res.ok) throw new Error(`cancelTurn ${res.status}`);
    return res.json();
  }

  connectEvents(
    onEvent: (ev: unknown) => void,
    options: EventSubscriptionOptions = {},
  ): WebSocket {
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
  constructor(private readonly send: (line: string) => Promise<string>) {}

  async call(method: string, params: unknown = {}, id = 1): Promise<unknown> {
    const req = JSON.stringify({ jsonrpc: "2.0", id, method, params });
    const raw = await this.send(req);
    const resp = JSON.parse(raw) as { result?: unknown; error?: unknown };
    if (resp.error) throw Object.assign(new Error("RPC error"), { error: resp.error });
    return resp.result;
  }
}

export { KkagentHttpClient as KkagentClient };

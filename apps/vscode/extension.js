const vscode = require("vscode");
const { spawn } = require("child_process");
const http = require("http");
const https = require("https");

/** @type {import('child_process').ChildProcess | undefined} */
let acpProc;

/** @type {AcpClient | undefined} */
let acpClient;

/** @type {string | undefined} */
let activeSessionId;

function httpBase() {
  return vscode.workspace.getConfiguration("kkagent").get("httpUrl", "http://127.0.0.1:8787");
}

/**
 * Minimal newline-delimited JSON-RPC client for `kkagent acp`.
 * Handles request/response correlation, `session/update` notifications and
 * the official agent→client requests (permission / input).
 */
class AcpClient {
  constructor(proc) {
    this.proc = proc;
    this.nextId = 1;
    /** @type {Map<string, {resolve: Function, reject: Function}>} */
    this.pending = new Map();
    /** @type {Map<string, Function[]>} server-initiated request handlers */
    this.serverHandlers = new Map();
    this.buffer = "";
    this.onUpdate = undefined;
    this.onExit = undefined;
    proc.stdout.setEncoding("utf8");
    proc.stdout.on("data", (chunk) => this.onStdout(chunk));
    proc.on("exit", (code) => this.onExit && this.onExit(code));
  }

  onStdout(chunk) {
    this.buffer += chunk;
    let idx;
    while ((idx = this.buffer.indexOf("\n")) >= 0) {
      const line = this.buffer.slice(0, idx);
      this.buffer = this.buffer.slice(idx + 1);
      if (!line.trim()) continue;
      let msg;
      try {
        msg = JSON.parse(line);
      } catch {
        continue;
      }
      this.dispatch(msg);
    }
  }

  dispatch(msg) {
    if (msg.method === undefined && msg.id !== undefined) {
      // Response to one of our requests.
      const entry = this.pending.get(String(msg.id));
      if (!entry) return;
      this.pending.delete(String(msg.id));
      if (msg.error) entry.reject(new Error(msg.error.message || "ACP error"));
      else entry.resolve(msg.result);
      return;
    }
    if (msg.method === "session/update" && this.onUpdate) {
      this.onUpdate(msg.params || {});
      return;
    }
    if (msg.method === "session/request_permission") {
      this.handlePermissionRequest(msg).catch(() => {});
      return;
    }
    if (msg.method === "session/request_input") {
      this.handleInputRequest(msg).catch(() => {});
      return;
    }
  }

  async handlePermissionRequest(msg) {
    const params = msg.params || {};
    const kind = params.kind || {};
    const detail =
      kind.kind === "command"
        ? `Command: ${kind.command || "(empty)"}`
        : kind.kind === "fetch"
          ? `Fetch: ${kind.url || "(empty)"}`
          : `Edit: ${kind.file && (kind.file.relative || kind.file.absolute) || "(file)"}`;
    const options = (params.options || []).map((o) => o.optionKind);
    const pick = await vscode.window.showInformationMessage(
      `kkagent permission — ${detail}`,
      { modal: true },
      ...options
    );
    const result = pick
      ? { outcomeKind: "selected", optionId: pick }
      : { outcomeKind: "cancelled" };
    this.respond(msg.id, result);
  }

  async handleInputRequest(msg) {
    const params = msg.params || {};
    const promptText = (params.prompt || [])
      .map((b) => (b.type === "text" ? b.text : ""))
      .join("");
    const kind = params.kind || {};
    if (kind.kind === "select") {
      const picks = await vscode.window.showQuickPick(
        (kind.options || []).map((o) => ({
          label: o.label,
          description: o.description,
        })),
        {
          placeHolder: promptText || "kkagent input",
          canPickMany: !!kind.multiSelect,
        }
      );
      const cancelled = !picks || (Array.isArray(picks) && picks.length === 0);
      const text = Array.isArray(picks)
        ? picks.map((p) => p.label).join(", ")
        : picks
          ? picks.label
          : "";
      this.respond(msg.id, {
        content: cancelled ? undefined : [{ type: "text", text }],
        canceled: cancelled || undefined,
      });
      return;
    }
    const answer = await vscode.window.showInputBox({
      prompt: promptText || "kkagent input",
      password: !!kind.password,
    });
    this.respond(msg.id, {
      content: answer === undefined ? undefined : [{ type: "text", text: answer }],
      canceled: answer === undefined || undefined,
    });
  }

  respond(id, result) {
    this.proc.stdin.write(
      JSON.stringify({ jsonrpc: "2.0", id, result }) + "\n"
    );
  }

  call(method, params) {
    return new Promise((resolve, reject) => {
      if (!this.proc.stdin.writable) {
        reject(new Error("kkagent ACP process not running"));
        return;
      }
      const id = this.nextId++;
      this.pending.set(String(id), { resolve, reject });
      this.proc.stdin.write(
        JSON.stringify({ jsonrpc: "2.0", id, method, params: params || {} }) + "\n"
      );
      setTimeout(() => {
        if (this.pending.has(String(id))) {
          this.pending.delete(String(id));
          reject(new Error(`ACP ${method} timed out`));
        }
      }, 120000);
    });
  }

  kill() {
    if (this.proc && !this.proc.killed) this.proc.kill();
  }
}

/**
 * Streaming chat channel: renders agent_message_chunk / tool updates into a
 * VS Code output channel so a turn is visible while it runs.
 */
class AcpChatChannel {
  constructor() {
    this.channel = vscode.window.createOutputChannel("kkagent (ACP)");
    /** @type {Map<string, string>} */
    this.turnText = new Map();
    /** @type {Map<string, string>} */
    this.toolLines = new Map();
  }

  show() {
    this.channel.show(true);
  }

  append(text) {
    this.channel.append(text);
  }

  line(text) {
    this.channel.appendLine(text);
  }

  /** @param {{sessionId: string, update: any}} params */
  handleUpdate(params) {
    const sid = params.sessionId || "";
    const update = params.update || {};
    switch (update.sessionUpdate) {
      case "agent_message_chunk": {
        const text = (update.content && update.content.text) || "";
        const last = this.toolLines.get(sid) || "";
        if (last) {
          this.channel.appendLine("");
          this.toolLines.delete(sid);
        }
        this.channel.append(text);
        this.turnText.set(sid, (this.turnText.get(sid) || "") + text);
        break;
      }
      case "agent_thought_chunk": {
        const text = (update.content && update.content.text) || "";
        this.channel.append(`\x1b[90m${text}\x1b[0m`);
        break;
      }
      case "user_message_chunk": {
        const text = (update.content && update.content.text) || "";
        this.channel.appendLine(`\n> ${text}`);
        break;
      }
      case "tool_call": {
        const name = update.toolName || "tool";
        const line = `\n[${name}]`;
        this.channel.appendLine(line);
        this.toolLines.set(sid, line);
        this.turnText.set(sid, (this.turnText.get(sid) || "") + line);
        break;
      }
      case "tool_call_update": {
        const content = (update.content || [])
          .map((b) => (b.type === "text" ? b.text : ""))
          .join("");
        if (content) {
          this.channel.appendLine(String(content).slice(0, 400));
        }
        break;
      }
      default:
        break;
    }
  }
}

function activate(context) {
  const sessionsProvider = new SessionsProvider();
  const chat = new AcpChatChannel();
  context.subscriptions.push(
    vscode.window.registerTreeDataProvider("kkagent.sessions", sessionsProvider),
    chat.channel
  );

  context.subscriptions.push(
    vscode.commands.registerCommand("kkagent.start", async () => {
      if (acpClient) {
        vscode.window.showInformationMessage("kkagent ACP already running");
        return;
      }
      const bin = vscode.workspace.getConfiguration("kkagent").get("binary", "kkagent");
      const proc = spawn(bin, ["acp"], {
        stdio: ["pipe", "pipe", "pipe"],
        cwd: vscode.workspace.workspaceFolders?.[0]?.uri.fsPath,
      });
      proc.stderr?.on("data", (d) => console.log(String(d)));
      acpClient = new AcpClient(proc);
      acpClient.onUpdate = (params) => chat.handleUpdate(params);
      acpClient.onExit = (code) => {
        vscode.window.showWarningMessage(`kkagent ACP exited (${code})`);
        acpClient = undefined;
        activeSessionId = undefined;
      };
      try {
        const init = await acpClient.call("initialize", {});
        const created = await acpClient.call("session/new", {
          cwd: vscode.workspace.workspaceFolders?.[0]?.uri.fsPath,
        });
        activeSessionId = created.sessionId;
        chat.show();
        chat.line(`kkagent ACP ready (loadSession=${init.agentCapabilities?.loadSession})`);
        chat.line(`session: ${activeSessionId}`);
        vscode.window.showInformationMessage("kkagent ACP bridge started");
      } catch (e) {
        vscode.window.showErrorMessage(`kkagent ACP init failed: ${e}`);
        acpClient.kill();
        acpClient = undefined;
      }
    }),

    vscode.commands.registerCommand("kkagent.prompt", async () => {
      const text = await vscode.window.showInputBox({ prompt: "Prompt for kkagent" });
      if (!text) return;
      await sendPrompt(text);
    }),

    vscode.commands.registerCommand("kkagent.commit", async () => {
      await sendPrompt(
        "/commit Create a concise commit message for the current workspace changes and run git commit when appropriate."
      );
    }),
    vscode.commands.registerCommand("kkagent.explain", async () => {
      const editor = vscode.window.activeTextEditor;
      const selected = editor?.document.getText(editor.selection) || "";
      await sendPrompt(
        selected
          ? `/explain Explain this code:\n\n${selected}`
          : "/explain Explain the current file and its purpose."
      );
    }),
    vscode.commands.registerCommand("kkagent.fix", async () => {
      await sendPrompt(
        "/fix Review recent changes for bugs, regressions, and missing tests. Be concrete."
      );
    }),

    vscode.commands.registerCommand("kkagent.refreshSessions", async () => {
      await sessionsProvider.refresh();
    }),

    vscode.commands.registerCommand("kkagent.openSession", async (sessionId) => {
      activeSessionId = sessionId;
      vscode.window.showInformationMessage(`kkagent: active session ${sessionId}`);
    }),

    vscode.commands.registerCommand("kkagent.showDiff", async () => {
      const left = await vscode.window.showInputBox({
        prompt: "Original text (left)",
        value: "",
      });
      if (left === undefined) return;
      const right = await vscode.window.showInputBox({
        prompt: "Modified text (right)",
        value: left,
      });
      if (right === undefined) return;
      const leftUri = vscode.Uri.parse(`kkagent-diff:left?${encodeURIComponent(left)}`);
      const rightUri = vscode.Uri.parse(`kkagent-diff:right?${encodeURIComponent(right)}`);
      await vscode.commands.executeCommand("vscode.diff", leftUri, rightUri, "kkagent diff");
    })
  );

  context.subscriptions.push(
    vscode.workspace.registerTextDocumentContentProvider("kkagent-diff", {
      provideTextDocumentContent(uri) {
        return decodeURIComponent(uri.query || "");
      },
    })
  );
}

async function sendPrompt(text) {
  // Preferred: ACP stdio bridge with official streaming updates.
  if (acpClient) {
    if (!activeSessionId) {
      const created = await acpClient.call("session/new", {
        cwd: vscode.workspace.workspaceFolders?.[0]?.uri.fsPath,
      });
      activeSessionId = created.sessionId;
    }
    try {
      const res = await acpClient.call("session/prompt", {
        sessionId: activeSessionId,
        prompt: [{ type: "text", text }],
      });
      vscode.window.setStatusBarMessage(
        `kkagent: turn finished (${res.stopReason || "end_turn"})`,
        4000
      );
    } catch (e) {
      vscode.window.showErrorMessage(`kkagent prompt failed: ${e}`);
    }
    return;
  }
  // Fallback: HTTP API when no ACP bridge is running.
  const url = httpBase();
  try {
    if (!activeSessionId) {
      const session = await httpJson(`${url}/api/v1/sessions`, {
        method: "POST",
        body: JSON.stringify({
          workspace: vscode.workspace.workspaceFolders?.[0]?.uri.fsPath,
        }),
      });
      activeSessionId = session.session_id;
    }
    await httpJson(`${url}/api/v1/sessions/${activeSessionId}/messages`, {
      method: "POST",
      body: JSON.stringify({ text }),
    });
    vscode.window.showInformationMessage(`kkagent: message sent to ${activeSessionId}`);
  } catch (e) {
    vscode.window.showErrorMessage(
      `kkagent prompt failed: ${e}. Start the ACP bridge with "kkagent: Start ACP bridge".`
    );
  }
}

class SessionsProvider {
  constructor() {
    this._onDidChangeTreeData = new vscode.EventEmitter();
    this.onDidChangeTreeData = this._onDidChangeTreeData.event;
    this.items = [];
  }
  refresh() {
    this._onDidChangeTreeData.fire();
  }
  getTreeItem(element) {
    return element;
  }
  async getChildren() {
    try {
      const body = await httpJson(`${httpBase()}/api/v1/sessions`);
      const sessions = body.sessions || [];
      return sessions.map((s) => {
        const id = s.session_id || s.id;
        const item = new vscode.TreeItem(
          s.title || id.slice(0, 12),
          vscode.TreeItemCollapsibleState.None
        );
        item.command = {
          command: "kkagent.openSession",
          title: "Open",
          arguments: [id],
        };
        item.description = id.slice(0, 8);
        return item;
      });
    } catch (e) {
      const item = new vscode.TreeItem(`offline: ${e}`, vscode.TreeItemCollapsibleState.None);
      return [item];
    }
  }
}

function httpJson(url, opts = {}) {
  return new Promise((resolve, reject) => {
    const u = new URL(url);
    const lib = u.protocol === "https:" ? https : http;
    const req = lib.request(
      {
        hostname: u.hostname,
        port: u.port,
        path: u.pathname + u.search,
        method: opts.method || "GET",
        headers: {
          "content-type": "application/json",
          ...(opts.headers || {}),
          ...(opts.body ? { "content-length": Buffer.byteLength(opts.body) } : {}),
        },
      },
      (res) => {
        let data = "";
        res.on("data", (c) => (data += c));
        res.on("end", () => {
          if (res.statusCode && res.statusCode >= 400) {
            reject(new Error(`${res.statusCode} ${data}`));
            return;
          }
          try {
            resolve(data ? JSON.parse(data) : {});
          } catch (e) {
            reject(e);
          }
        });
      }
    );
    req.on("error", reject);
    if (opts.body) req.write(opts.body);
    req.end();
  });
}

function deactivate() {
  if (acpClient) acpClient.kill();
}

module.exports = { activate, deactivate };

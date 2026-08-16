const vscode = require("vscode");
const { spawn } = require("child_process");
const http = require("http");
const https = require("https");

/** @type {import('child_process').ChildProcess | undefined} */
let acpProc;

/** @type {string | undefined} */
let activeSessionId;

function httpBase() {
  return vscode.workspace.getConfiguration("kkagent").get("httpUrl", "http://127.0.0.1:8787");
}

function activate(context) {
  const sessionsProvider = new SessionsProvider();
  context.subscriptions.push(
    vscode.window.registerTreeDataProvider("kkagent.sessions", sessionsProvider)
  );

  context.subscriptions.push(
    vscode.commands.registerCommand("kkagent.start", async () => {
      const bin = vscode.workspace.getConfiguration("kkagent").get("binary", "kkagent");
      if (acpProc && !acpProc.killed) {
        vscode.window.showInformationMessage("kkagent ACP already running");
        return;
      }
      acpProc = spawn(bin, ["acp"], { stdio: ["pipe", "pipe", "pipe"] });
      acpProc.stderr?.on("data", (d) => console.log(String(d)));
      acpProc.on("exit", (code) => {
        vscode.window.showWarningMessage(`kkagent ACP exited (${code})`);
        acpProc = undefined;
      });
      acpProc.stdin?.write(
        JSON.stringify({ jsonrpc: "2.0", id: 1, method: "initialize", params: {} }) + "\n"
      );
      vscode.window.showInformationMessage("kkagent ACP bridge started");
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
    vscode.window.showErrorMessage(`kkagent prompt failed: ${e}`);
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
  if (acpProc && !acpProc.killed) acpProc.kill();
}

module.exports = { activate, deactivate };

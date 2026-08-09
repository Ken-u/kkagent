const vscode = require("vscode");
const { spawn } = require("child_process");
const http = require("http");
const https = require("https");

/** @type {import('child_process').ChildProcess | undefined} */
let acpProc;

function activate(context) {
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
      // initialize
      const init = JSON.stringify({
        jsonrpc: "2.0",
        id: 1,
        method: "initialize",
        params: {},
      });
      acpProc.stdin?.write(init + "\n");
      vscode.window.showInformationMessage("kkagent ACP bridge started");
    }),
    vscode.commands.registerCommand("kkagent.prompt", async () => {
      const text = await vscode.window.showInputBox({ prompt: "Prompt for kkagent" });
      if (!text) return;
      const url = vscode.workspace.getConfiguration("kkagent").get("httpUrl", "http://127.0.0.1:8787");
      try {
        const session = await httpJson(`${url}/api/v1/sessions`, {
          method: "POST",
          body: JSON.stringify({
            workspace: vscode.workspace.workspaceFolders?.[0]?.uri.fsPath,
          }),
        });
        const sid = session.session_id;
        await httpJson(`${url}/api/v1/sessions/${sid}/messages`, {
          method: "POST",
          body: JSON.stringify({ text }),
        });
        vscode.window.showInformationMessage(`kkagent: message sent to ${sid}`);
      } catch (e) {
        // fallback: ACP stdio if running
        if (acpProc?.stdin) {
          const req = JSON.stringify({
            jsonrpc: "2.0",
            id: Date.now(),
            method: "session/prompt",
            params: { prompt: text },
          });
          acpProc.stdin.write(req + "\n");
          vscode.window.showInformationMessage("kkagent: prompt sent via ACP");
        } else {
          vscode.window.showErrorMessage(`kkagent prompt failed: ${e}`);
        }
      }
    }),
  );
}

function deactivate() {
  if (acpProc && !acpProc.killed) acpProc.kill();
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
        },
      },
      (res) => {
        let data = "";
        res.on("data", (c) => (data += c));
        res.on("end", () => {
          try {
            resolve(JSON.parse(data || "{}"));
          } catch (e) {
            reject(e);
          }
        });
      },
    );
    req.on("error", reject);
    if (opts.body) req.write(opts.body);
    req.end();
  });
}

module.exports = { activate, deactivate };

const params = new URLSearchParams(location.search);
const base = params.get("base") || "";

let token = params.get("token") || localStorage.getItem("kkagent_token") || "";
if (params.get("token")) {
  localStorage.setItem("kkagent_token", token);
  params.delete("token");
  const rest = params.toString();
  history.replaceState(null, "", location.pathname + (rest ? `?${rest}` : ""));
}

const state = {
  sessionId: null,
  sessions: [],
  collapsed: new Set(),
  attachments: [],
  scope: "workspace",
  cwd: "",
  live: null,
  btwLive: null,
};

const logEl = document.getElementById("log");
const sessionsEl = document.getElementById("sessions");
const statusEl = document.getElementById("status");
const promptEl = document.getElementById("prompt");
const sendBtn = document.getElementById("sendBtn");
const fileInput = document.getElementById("fileInput");
const attachBtn = document.getElementById("attachBtn");
const attachmentRow = document.getElementById("attachmentRow");
const sidebar = document.getElementById("sidebar");
const menuToggle = document.getElementById("menuToggle");
const cmdPalette = document.getElementById("cmdPalette");
const workspaceTabs = document.getElementById("workspaceTabs");
const btwPanel = document.getElementById("btwPanel");
const btwContext = document.getElementById("btwContext");
const btwLog = document.getElementById("btwLog");
const btwPrompt = document.getElementById("btwPrompt");
const btwSend = document.getElementById("btwSend");
const btwForm = document.getElementById("btwForm");
const btwClose = document.getElementById("btwClose");
const timelinePanel = document.getElementById("timelinePanel");
const timelineList = document.getElementById("timelineList");
const timelineClose = document.getElementById("timelineClose");
const noticeBar = document.getElementById("noticeBar");
const noticeText = document.getElementById("noticeText");
const noticeActions = document.getElementById("noticeActions");

const ICONS = {
  copy: '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="9" y="9" width="13" height="13" rx="2" ry="2"></rect><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"></path></svg>',
  edit: '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M11 4H4a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7"></path><path d="M18.5 2.5a2.121 2.121 0 0 1 3 3L12 15l-4 1 1-4 9.5-9.5z"></path></svg>',
  fork: '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="6" y1="3" x2="6" y2="15"></line><circle cx="18" cy="6" r="3"></circle><circle cx="6" cy="18" r="3"></circle><path d="M18 9a9 9 0 0 1-9 9"></path></svg>',
  follow: '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 11.5a8.38 8.38 0 0 1-.9 3.8 8.5 8.5 0 0 1-7.6 4.7 8.38 8.38 0 0 1-3.8-.9L3 21l1.9-5.7a8.38 8.38 0 0 1-.9-3.8 8.5 8.5 0 0 1 4.7-7.6 8.38 8.38 0 0 1 3.8-.9h.5a8.48 8.48 0 0 1 8 8v.5z"></path></svg>',
  check: '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="20 6 9 17 4 12"></polyline></svg>',
  chevron: '<svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><polyline points="9 18 15 12 9 6"></polyline></svg>',
  spinner: '<svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round"><path d="M21 12a9 9 0 1 1-6.219-8.56"></path></svg>',
  x: '<svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round"><line x1="18" y1="6" x2="6" y2="18"></line><line x1="6" y1="6" x2="18" y2="18"></line></svg>',
};

const TOOL_KINDS = {
  read_file: { verb: "读取", arg: "path" },
  read: { verb: "读取", arg: "path" },
  write_file: { verb: "写入", arg: "path" },
  write: { verb: "写入", arg: "path" },
  edit: { verb: "编辑", arg: "path" },
  replace: { verb: "编辑", arg: "path" },
  bash: { verb: "执行", arg: "command" },
  shell: { verb: "执行", arg: "command" },
  exec: { verb: "执行", arg: "command" },
  run_command: { verb: "执行", arg: "command" },
  grep: { verb: "搜索", arg: "pattern" },
  search: { verb: "搜索", arg: "query" },
  glob: { verb: "查找", arg: "pattern" },
  find: { verb: "查找", arg: "pattern" },
  fetch: { verb: "请求", arg: "url" },
  curl: { verb: "请求", arg: "url" },
  list_dir: { verb: "列出", arg: "path" },
  ls: { verb: "列出", arg: "path" },
  todo: { verb: "计划", arg: null },
};

const COMMANDS = [
  { name: "/explain", desc: "解释选中代码或上下文", example: "/explain 这段代码的作用" },
  { name: "/fix", desc: "修复当前问题", example: "/fix 修复当前问题" },
  { name: "/commit", desc: "提交当前变更", example: "/commit 提交修复" },
  { name: "/fork", desc: "Fork 当前会话", example: "/fork" },
  { name: "/btw", desc: "在侧边窗口追问", example: "/btw 还有别的方案吗" },
  { name: "/timeline", desc: "打开 Timeline", example: "/timeline" },
  { name: "/new", desc: "新建会话", example: "/new" },
  { name: "/test", desc: "运行相关测试", example: "/test" },
];

async function api(path, opts = {}) {
  const headers = { ...(opts.headers || {}) };
  if (token) headers.Authorization = `Bearer ${token}`;
  if (opts.body && !headers["content-type"]) headers["content-type"] = "application/json";
  const res = await fetch(base + path, { ...opts, headers });
  if (res.status === 401) {
    const error = new Error(`${path} 401`);
    error.unauthorized = true;
    throw error;
  }
  if (!res.ok) {
    let detail = `${path} ${res.status}`;
    try {
      const body = await res.json();
      if (body && body.error) detail = body.error;
    } catch {
      /* ignore */
    }
    const error = new Error(detail);
    error.status = res.status;
    throw error;
  }
  if (res.status === 204) return {};
  return res.json();
}

function escapeHtml(str) {
  return String(str)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#039;");
}

function formatTime(iso) {
  if (!iso) return "";
  const d = typeof iso === "number" ? new Date(iso) : new Date(iso);
  return isNaN(d) ? "" : d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

function normalizePath(path) {
  return String(path || "").replace(/\\/g, "/").replace(/\/+$/, "");
}

function sameWorkspace(session) {
  const cwd = normalizePath(state.cwd);
  if (!cwd) return true;
  const path = normalizePath(session.workspace || session.working_dir || session.path || "");
  return path === cwd;
}

function stripHarness(text) {
  let out = String(text || "");
  for (const tag of ["system-reminder", "cron-fire", "kimi-skill-loaded"]) {
    const close = `</${tag}>`;
    let cursor = 0;
    while (true) {
      const start = out.indexOf(`<${tag}`, cursor);
      if (start < 0) break;
      const after = out.charAt(start + tag.length + 1);
      if (after && after !== ">" && after !== " " && after !== "\t" && after !== "\n") {
        cursor = start + tag.length + 1;
        continue;
      }
      const end = out.indexOf(close, start);
      if (end < 0) {
        out = out.slice(0, start);
        break;
      }
      out = out.slice(0, start) + out.slice(end + close.length);
      cursor = start;
    }
  }
  return out.trim();
}

function visibleUserText(text) {
  return stripHarness(text);
}

function toolKind(name) {
  const lower = String(name || "").toLowerCase();
  return TOOL_KINDS[lower] || { verb: lower.replace(/_/g, " ") || "工具", arg: null };
}

function toolArgSummary(tool) {
  const kind = toolKind(tool.name);
  const input = tool.input || {};
  const keys = [];
  if (kind.arg) keys.push(kind.arg);
  for (const k of ["command", "path", "pattern", "query", "url", "file", "name", "title"]) {
    if (!keys.includes(k)) keys.push(k);
  }
  for (const k of keys) {
    const v = input[k];
    if (v !== undefined && v !== null && String(v).trim() !== "") {
      return String(v).replace(/\s+/g, " ").trim();
    }
  }
  return "";
}

function formatDuration(ms) {
  if (ms === undefined || ms === null || ms === "" || isNaN(ms)) return "";
  if (ms < 1000) return Math.round(ms) + "ms";
  return (ms / 1000).toFixed(1) + "s";
}

function toolStatus(tool) {
  if (tool.status === "error" || tool.error) return "error";
  if (tool.status === "success") return "success";
  if (tool.status === "running") return "running";
  return "success";
}

function highlightJson(value) {
  let json;
  try {
    json = JSON.stringify(value, null, 2);
  } catch {
    json = String(value);
  }
  if (json === undefined || json === "undefined") return "";
  return json.replace(
    /("(?:\\.|[^"\\])*")(\s*:)?|\b(true|false|null)\b|(-?\d+(?:\.\d+)?)/g,
    (m, str, colon, kw, num) => {
      if (str) return colon ? `<span class="j-key">${escapeHtml(str)}</span>${colon}` : `<span class="j-str">${escapeHtml(str)}</span>`;
      if (kw) return `<span class="j-bool">${kw}</span>`;
      if (num !== undefined) return `<span class="j-num">${escapeHtml(num)}</span>`;
      return escapeHtml(m);
    }
  );
}

function highlightCode(code) {
  const rules = [
    { regex: /(\/\/.*)/g, color: "#8b949e" },
    { regex: /(\/\*[\s\S]*?\*\/)/g, color: "#8b949e" },
    { regex: /\b(function|return|const|let|var|if|else|for|while|async|await|class|import|from|export|new|try|catch|throw|true|false|null|undefined)\b/g, color: "#ff7b72" },
    { regex: /\b(\d+)\b/g, color: "#79c0ff" },
    { regex: /("[^"]*"|'[^']*'|`[^`]*`)/g, color: "#a5d6ff" },
  ];
  let html = code;
  for (const { regex, color } of rules) {
    html = html.replace(regex, (match) => `<span style="color:${color}">${match}</span>`);
  }
  return html;
}

function markdownToHtml(text) {
  let html = escapeHtml(text || "").replace(/\n/g, "<br>");
  html = html.replace(/```(\w*)\n?([\s\S]*?)```/g, (_, lang, code) => {
    const clean = code.replace(/<br>/g, "\n").replace(/\n$/, "");
    const escaped = escapeHtml(clean);
    return `<div class="code-block"><button class="copy-btn code-copy" data-code="${escaped.replace(/"/g, "&quot;")}" title="复制代码">${ICONS.copy}</button><pre><code class="lang-${lang || "text"}">${highlightCode(escaped)}</code></pre></div>`;
  });
  html = html.replace(/`([^`]+)`/g, "<code>$1</code>");
  html = html.replace(/^######\s+(.+)$/gm, "<h6>$1</h6>");
  html = html.replace(/^#####\s+(.+)$/gm, "<h5>$1</h5>");
  html = html.replace(/^####\s+(.+)$/gm, "<h4>$1</h4>");
  html = html.replace(/^###\s+(.+)$/gm, "<h3>$1</h3>");
  html = html.replace(/^##\s+(.+)$/gm, "<h2>$1</h2>");
  html = html.replace(/^#\s+(.+)$/gm, "<h1>$1</h1>");
  html = html.replace(/\*\*\*(.+?)\*\*\*/g, "<strong><em>$1</em></strong>");
  html = html.replace(/\*\*(.+?)\*\*/g, "<strong>$1</strong>");
  html = html.replace(/\*(.+?)\*/g, "<em>$1</em>");
  html = html.replace(/^>\s+(.+)$/gm, "<blockquote>$1</blockquote>");
  html = html.replace(/^((?:\s*[-*]\s+.+?<br>)+)/gm, (_, list) => {
    const items = list.replace(/\s*[-*]\s+(.+?)(?:<br>|$)/g, "<li>$1</li>");
    return `<ul>${items}</ul>`;
  });
  html = html.replace(/^((?:\s*\d+\.\s+.+?<br>)+)/gm, (_, list) => {
    const items = list.replace(/\s*\d+\.\s+(.+?)(?:<br>|$)/g, "<li>$1</li>");
    return `<ol>${items}</ol>`;
  });
  html = html.replace(/\[([^\]]+)\]\(([^)]+)\)/g, '<a href="$2" target="_blank" rel="noopener">$1</a>');
  return html;
}

function copyText(text) {
  if (navigator.clipboard && window.isSecureContext) return navigator.clipboard.writeText(text);
  return new Promise((resolve, reject) => {
    const ta = document.createElement("textarea");
    ta.value = text;
    ta.style.position = "fixed";
    ta.style.left = "-9999px";
    document.body.appendChild(ta);
    ta.focus();
    ta.select();
    try {
      const ok = document.execCommand("copy");
      document.body.removeChild(ta);
      ok ? resolve() : reject(new Error("copy failed"));
    } catch (err) {
      document.body.removeChild(ta);
      reject(err);
    }
  });
}

function flashBtn(btn, iconKey) {
  btn.innerHTML = ICONS.check;
  setTimeout(() => (btn.innerHTML = ICONS[iconKey]), 1500);
}

function renderToolCall(tool) {
  const status = toolStatus(tool);
  const kind = toolKind(tool.name);
  const summary = toolArgSummary(tool);
  const duration = formatDuration(tool.durationMs);
  const statusIcon =
    status === "running" ? `<span class="spin">${ICONS.spinner}</span>` :
    status === "error" ? ICONS.x :
    ICONS.check;
  const input = tool.input || {};
  const kvRows = Object.entries(input)
    .map(([k, v]) => `
      <div class="kv-row">
        <div class="kv-key" title="${escapeHtml(k)}">${escapeHtml(k)}</div>
        <div class="kv-val">${typeof v === "string" ? escapeHtml(v) : highlightJson(v)}</div>
      </div>`)
    .join("");
  const error = tool.error ? String(tool.error) : "";
  const hasOutput = tool.output !== undefined && tool.output !== null && tool.output !== "";
  const outputText = hasOutput
    ? (typeof tool.output === "string" ? escapeHtml(tool.output) : highlightJson(tool.output))
    : "";
  const div = document.createElement("div");
  div.className = `tool-call ${status}`;
  if (tool._id) div.dataset.id = tool._id;
  div.innerHTML = `
    <div class="tool-row" role="button" tabindex="0" aria-expanded="false">
      <span class="tool-status-icon">${statusIcon}</span>
      <span class="tool-verb">${escapeHtml(kind.verb)}</span>
      <span class="tool-arg" title="${escapeHtml(summary)}">${escapeHtml(summary)}</span>
      ${duration ? `<span class="tool-meta">${duration}</span>` : ""}
      <span class="tool-chevron">${ICONS.chevron}</span>
    </div>
    <div class="tool-detail">
      <div class="tool-detail-inner">
        <div class="tool-detail-content">
          ${Object.keys(input).length ? `
          <div class="tool-sec">
            <div class="tool-sec-title"><span>工具</span><span class="tool-name-pill">${escapeHtml(tool.name)}</span><button class="copy-btn tool-copy" data-kind="input" title="复制参数">${ICONS.copy}</button></div>
            <div class="kv-list">${kvRows}</div>
          </div>` : ""}
          ${error ? `
          <div class="tool-sec">
            <div class="tool-sec-title"><span>错误</span></div>
            <pre class="tool-error-text">${escapeHtml(error)}</pre>
          </div>` : ""}
          ${hasOutput ? `
          <div class="tool-sec">
            <div class="tool-sec-title"><span>输出</span><button class="copy-btn tool-copy" data-kind="output" title="复制输出">${ICONS.copy}</button></div>
            <pre class="tool-output">${outputText}</pre>
          </div>` : ""}
        </div>
      </div>
    </div>
  `;
  return div;
}

function renderToolGroup(toolCalls) {
  if (!toolCalls.length) return "";
  const hasError = toolCalls.some((t) => toolStatus(t) === "error");
  const isRunning = toolCalls.some((t) => toolStatus(t) === "running");
  const cls = `tool-group${hasError ? " has-error" : ""}${isRunning ? " is-running" : ""}`;
  return `
    <div class="${cls}">
      <div class="tool-group-label"><span class="dot"></span><span>工具调用 · ${toolCalls.length} 步</span></div>
      ${toolCalls.map((t) => renderToolCall(t).outerHTML).join("")}
    </div>
  `;
}

function bindMessageActions(div, role, content, toolCalls, messageIndex) {
  const msgCopy = div.querySelector(".msg-copy");
  if (msgCopy) msgCopy.onclick = () => copyText(content).then(() => flashBtn(msgCopy, "copy"));
  const msgEdit = div.querySelector(".msg-edit");
  if (msgEdit) {
    msgEdit.onclick = async () => {
      promptEl.value = content;
      promptEl.focus();
      adjustPromptHeight();
      if (typeof messageIndex === "number") {
        try {
          const forked = await api(`/api/v1/sessions/${state.sessionId}/fork`, {
            method: "POST",
            body: JSON.stringify({ message_limit: messageIndex, title: "Edit" }),
          });
          await refreshSessions();
          await selectSession(forked.session_id || forked.id);
        } catch (err) {
          appendMessage("system", `fork 失败: ${err.message || err}`, new Date().toISOString());
        }
      }
    };
  }
  const msgFork = div.querySelector(".msg-fork");
  if (msgFork) msgFork.onclick = () => forkCurrentSession();
  const msgFollow = div.querySelector(".msg-follow");
  if (msgFollow) msgFollow.onclick = () => openBtwPanel(content);
  for (const btn of div.querySelectorAll(".code-copy")) {
    btn.onclick = () => copyText(btn.dataset.code || "").then(() => flashBtn(btn, "copy"));
  }
  for (const tc of div.querySelectorAll(".tool-call")) {
    const row = tc.querySelector(".tool-row");
    const toggle = () => {
      tc.classList.toggle("open");
      row.setAttribute("aria-expanded", tc.classList.contains("open") ? "true" : "false");
    };
    row.onclick = toggle;
    row.onkeydown = (e) => {
      if (e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        toggle();
      }
    };
    const tool = toolCalls.find((t) => t._id === tc.dataset.id);
    for (const btn of tc.querySelectorAll(".tool-copy")) {
      btn.onclick = (e) => {
        e.stopPropagation();
        const kind = btn.dataset.kind;
        const text = kind === "input"
          ? JSON.stringify((tool && tool.input) || {}, null, 2)
          : String((tool && tool.output) || "");
        copyText(text).then(() => flashBtn(btn, "copy"));
      };
    }
  }
}

function renderMessage(role, content, timestamp, toolCalls = [], extras = {}) {
  const div = document.createElement("div");
  div.className = `message ${role}`;
  const roleLabel = role === "assistant" ? "kkagent" : role === "user" ? "你" : "system";
  const roleIcon = role === "assistant" ? "K" : role === "user" ? "U" : "!";
  const toolsHtml = renderToolGroup(toolCalls);
  div.innerHTML = `
    <div class="avatar ${role}">${roleIcon}</div>
    <div class="message-body">
      <div class="message-header">
        <span class="role-name">${roleLabel}</span>
        <span class="timestamp">${formatTime(timestamp)}</span>
      </div>
      <div class="bubble">${content ? markdownToHtml(content) : ""}${toolsHtml}</div>
      <span class="msg-actions">
        <button class="copy-btn msg-copy" title="复制消息">${ICONS.copy}</button>
        ${role === "user" ? '<button class="copy-btn msg-edit" title="编辑消息">' + ICONS.edit + "</button>" : ""}
        ${role === "assistant" ? '<button class="copy-btn msg-fork" title="Fork 新会话">' + ICONS.fork + "</button>" : ""}
        ${role === "assistant" ? '<button class="copy-btn msg-follow" title="BTW 追问">' + ICONS.follow + "</button>" : ""}
      </span>
    </div>
  `;
  const bubble = div.querySelector(".bubble");
  if (bubble && !content && !toolCalls.length) bubble.style.display = "none";
  bindMessageActions(div, role, content, toolCalls, extras.messageIndex);
  return div;
}

function showTyping() {
  const div = document.createElement("div");
  div.className = "message assistant typing-msg";
  div.innerHTML = `
    <div class="avatar assistant">K</div>
    <div class="message-body">
      <div class="message-header"><span class="role-name">kkagent</span></div>
      <div class="bubble"><div class="typing"><span></span><span></span><span></span></div></div>
    </div>
  `;
  logEl.appendChild(div);
  logEl.scrollTop = logEl.scrollHeight;
  return div;
}

function removeTyping() {
  const el = logEl.querySelector(".typing-msg");
  if (el) el.remove();
}

function appendMessage(role, content, timestamp, toolCalls = [], extras = {}) {
  removeTyping();
  const el = renderMessage(role, content, timestamp, toolCalls, extras);
  if (extras.images && extras.images.length) {
    const body = el.querySelector(".message-body");
    const row = document.createElement("div");
    row.className = "msg-attachments";
    for (const image of extras.images) row.appendChild(createImagePreview(image));
    body.appendChild(row);
  }
  logEl.appendChild(el);
  logEl.scrollTop = logEl.scrollHeight;
  return el;
}

function createImagePreview(image) {
  const src = image.src || image.dataUrl || (image.data ? `data:${image.media_type || image.mediaType || "image/png"};base64,${image.data}` : "");
  const pill = document.createElement("div");
  pill.className = "attachment-pill image-pill";
  pill.innerHTML = `<img src="${escapeHtml(src)}" alt="">`;
  const img = pill.querySelector("img");
  if (img) {
    img.onclick = (e) => {
      e.stopPropagation();
      const box = document.createElement("div");
      box.className = "image-lightbox";
      box.innerHTML = `<img src="${escapeHtml(src)}" alt="">`;
      box.onclick = () => box.remove();
      document.body.appendChild(box);
    };
  }
  return pill;
}

function showWelcome() {
  logEl.innerHTML = `
    <div class="welcome">
      <h2>kkagent</h2>
      <p>在下方输入框发送消息开始对话。可以要求它解释代码、修复 bug、提交变更或执行其他任务。</p>
      <div class="samples">
        <div class="sample" data-text="/explain 这段代码的作用">解释代码</div>
        <div class="sample" data-text="/fix 修复当前问题">修复问题</div>
        <div class="sample" data-text="帮我写个 Rust 的 HTTP 客户端示例">写示例</div>
        <div class="sample" data-text="总结一下今天的改动">总结改动</div>
      </div>
    </div>
  `;
  for (const sample of logEl.querySelectorAll(".sample")) {
    sample.onclick = () => {
      promptEl.value = sample.dataset.text;
      promptEl.focus();
      adjustPromptHeight();
    };
  }
}

function parseTranscript(messages) {
  const out = [];
  const toolsById = new Map();
  for (const raw of messages || []) {
    const role = raw.role || "assistant";
    const parts = Array.isArray(raw.content) ? raw.content : null;
    if (role === "user") {
      let text = "";
      const images = [];
      let onlyTools = true;
      if (parts) {
        for (const part of parts) {
          if (part.type === "text") {
            onlyTools = false;
            text += (text ? "\n" : "") + (part.text || "");
          } else if (part.type === "image") {
            onlyTools = false;
            images.push(part);
          } else if (part.type === "tool_result") {
            const tool = toolsById.get(part.tool_use_id);
            if (tool) {
              tool.output = part.content || "";
              tool.status = part.is_error ? "error" : "success";
              if (part.is_error) tool.error = part.content;
            }
          }
        }
      } else {
        onlyTools = false;
        text = raw.text || (typeof raw.content === "string" ? raw.content : "");
      }
      if (onlyTools && !text.trim() && images.length === 0) continue;
      const visible = visibleUserText(text);
      if (!visible && images.length === 0) continue;
      out.push({
        role: "user",
        content: visible,
        created_at: raw.created_at || raw.at,
        images,
        tool_calls: [],
        sourceIndex: messages.indexOf(raw),
      });
    } else if (role === "assistant") {
      let text = "";
      const tools = [];
      if (parts) {
        for (const part of parts) {
          if (part.type === "text") text += part.text || "";
          else if (part.type === "thinking") continue;
          else if (part.type === "tool_use") {
            const tool = {
              _id: part.id,
              name: part.name,
              input: part.input || {},
              status: "success",
            };
            if (part.id) toolsById.set(part.id, tool);
            tools.push(tool);
          }
        }
      } else {
        text = raw.text || (typeof raw.content === "string" ? raw.content : "");
        for (const tool of raw.tool_calls || []) tools.push(tool);
      }
      if (!text.trim() && tools.length === 0) continue;
      out.push({
        role: "assistant",
        content: text,
        created_at: raw.created_at || raw.at,
        tool_calls: tools,
      });
    } else if (role === "system") {
      const text = visibleUserText(raw.text || (typeof raw.content === "string" ? raw.content : ""));
      if (text) out.push({ role: "system", content: text, created_at: raw.created_at, tool_calls: [] });
    }
  }
  return out;
}

function renderLog(messages) {
  const parsed = parseTranscript(messages);
  logEl.innerHTML = "";
  if (!parsed.length) {
    showWelcome();
    return;
  }
  parsed.forEach((message, index) => {
    appendMessage(message.role, message.content, message.created_at, message.tool_calls || [], {
      images: message.images,
      messageIndex: message.sourceIndex ?? index,
    });
  });
}

function sessionIdOf(session) {
  return session.session_id || session.id;
}

function buildSessionTree() {
  const byId = new Map(state.sessions.map((s) => [sessionIdOf(s), { ...s, children: [] }]));
  const roots = [];
  for (const s of byId.values()) {
    const parent = s.forked_from || s.parent_id;
    if (parent && byId.has(parent)) byId.get(parent).children.push(s);
    else roots.push(s);
  }
  const out = [];
  function walk(list, depth) {
    for (const s of list) {
      out.push({ ...s, depth, hasChildren: s.children.length > 0 });
      if (!state.collapsed.has(sessionIdOf(s))) walk(s.children, depth + 1);
    }
  }
  walk(roots, 0);
  return out;
}

function filterSessions() {
  let sessions = buildSessionTree();
  if (state.scope === "workspace") sessions = sessions.filter((s) => sameWorkspace(s));
  return sessions;
}

function renderSessions() {
  sessionsEl.innerHTML = "";
  for (const s of filterSessions()) {
    const id = sessionIdOf(s);
    const div = document.createElement("div");
    div.className = "session" + (id === state.sessionId ? " active" : "");
    div.dataset.depth = String(s.depth || 0);
    div.style.paddingLeft = (12 + (s.depth || 0) * 16) + "px";
    const expander = s.hasChildren
      ? `<span class="session-expander">${state.collapsed.has(id) ? "▸" : "▾"}</span>`
      : '<span class="session-expander-spacer"></span>';
    const title = s.title || id.slice(0, 8);
    const path = escapeHtml(s.workspace || s.working_dir || s.path || "");
    const preview = s.preview || "";
    div.innerHTML = `
      <div class="session-title">
        ${expander}${(s.depth || 0) > 0 ? '<span class="session-fork-icon">↳</span>' : ""}${escapeHtml(title)}
      </div>
      ${path ? `<div class="session-path">${path}</div>` : ""}
      <div class="session-meta">${escapeHtml(preview)}</div>
    `;
    div.onclick = () => {
      const wasActive = div.classList.contains("active");
      closeSidebar();
      if (wasActive && s.hasChildren) {
        if (state.collapsed.has(id)) state.collapsed.delete(id);
        else state.collapsed.add(id);
        renderSessions();
      } else {
        selectSession(id);
      }
    };
    sessionsEl.appendChild(div);
  }
}

async function refreshSessions() {
  const body = await api("/api/v1/sessions");
  const live = body.sessions || [];
  const archived = body.transcript || [];
  const seen = new Set(live.map(sessionIdOf));
  state.sessions = live.concat(archived.filter((item) => !seen.has(sessionIdOf(item))));
  renderSessions();
}

async function selectSession(id) {
  state.sessionId = id;
  state.live = null;
  renderSessions();
  logEl.innerHTML = "";
  const sess = await api(`/api/v1/sessions/${id}`);
  const current = state.sessions.find((s) => sessionIdOf(s) === id);
  if (current && sess.forked_from) {
    current.forked_from = sess.forked_from;
    current.parent_id = sess.forked_from;
  }
  const messages = sess.messages || [];
  renderLog(messages);
}

async function ensureSession() {
  if (state.sessionId) return state.sessionId;
  const created = await api("/api/v1/sessions", {
    method: "POST",
    body: JSON.stringify({ workspace: state.cwd || "." }),
  });
  state.sessionId = created.session_id;
  await refreshSessions();
  return state.sessionId;
}

async function forkCurrentSession(title) {
  if (!state.sessionId) return;
  try {
    const forked = await api(`/api/v1/sessions/${state.sessionId}/fork`, {
      method: "POST",
      body: JSON.stringify({ title: title || undefined }),
    });
    await refreshSessions();
    await selectSession(forked.session_id || forked.id);
    appendMessage("system", "已 fork 新会话。", new Date().toISOString());
  } catch (err) {
    appendMessage("system", `fork 失败: ${err.message || err}`, new Date().toISOString());
  }
}

function showTokenPrompt(message) {
  document.getElementById("tokenGate").style.display = "flex";
  const form = document.getElementById("tokenForm");
  const input = document.getElementById("tokenInput");
  const hint = document.getElementById("tokenHint");
  hint.textContent = message || "";
  input.focus();
  form.onsubmit = (e) => {
    e.preventDefault();
    const value = input.value.trim();
    if (!value) return;
    token = value;
    localStorage.setItem("kkagent_token", token);
    document.getElementById("tokenGate").style.display = "none";
    boot();
  };
}

function hideNotice() {
  noticeBar.classList.remove("active");
  noticeActions.innerHTML = "";
}

function showNotice(text, actions = []) {
  noticeText.textContent = text;
  noticeActions.innerHTML = "";
  for (const action of actions) {
    const btn = document.createElement("button");
    btn.className = action.primary ? "btn-primary" : "btn-secondary";
    btn.textContent = action.label;
    btn.onclick = action.onclick;
    noticeActions.appendChild(btn);
  }
  noticeBar.classList.add("active");
}

function adjustPromptHeight() {
  promptEl.style.height = "auto";
  promptEl.style.height = Math.min(promptEl.scrollHeight, 180) + "px";
}

function adjustBtwPromptHeight() {
  btwPrompt.style.height = "auto";
  btwPrompt.style.height = Math.min(btwPrompt.scrollHeight, 120) + "px";
}

function renderCmdPalette(query) {
  const items = COMMANDS.filter((c) => c.name.startsWith(query) || c.desc.includes(query));
  if (!items.length) {
    cmdPalette.innerHTML = "";
    cmdPalette.classList.remove("active");
    return;
  }
  cmdPalette.innerHTML = items
    .map((c, i) => `
      <div class="cmd-item ${i === 0 ? "selected" : ""}" data-cmd="${c.name}" data-example="${escapeHtml(c.example)}">
        <span class="cmd-name">${c.name}</span>
        <span class="cmd-desc">${escapeHtml(c.desc)}</span>
      </div>`)
    .join("");
  cmdPalette.classList.add("active");
}

function closeCmdPalette() {
  cmdPalette.classList.remove("active");
  cmdPalette.innerHTML = "";
}

function showCmdPalette(text) {
  const match = text.match(/(^|\s)\/([a-zA-Z_]*)$/);
  if (match) renderCmdPalette("/" + match[2]);
  else closeCmdPalette();
}

function insertCommand(example) {
  const text = promptEl.value;
  const before = text.slice(0, Math.max(0, text.lastIndexOf("/")));
  promptEl.value = before + example;
  closeCmdPalette();
  promptEl.focus();
  adjustPromptHeight();
}

promptEl.addEventListener("input", () => {
  adjustPromptHeight();
  showCmdPalette(promptEl.value);
});
promptEl.addEventListener("click", () => showCmdPalette(promptEl.value));
promptEl.addEventListener("keydown", (e) => {
  if (cmdPalette.classList.contains("active")) {
    const selected = cmdPalette.querySelector(".selected");
    if (e.key === "ArrowDown") {
      e.preventDefault();
      const next = selected?.nextElementSibling || cmdPalette.firstElementChild;
      selected?.classList.remove("selected");
      next?.classList.add("selected");
      return;
    }
    if (e.key === "ArrowUp") {
      e.preventDefault();
      const prev = selected?.previousElementSibling || cmdPalette.lastElementChild;
      selected?.classList.remove("selected");
      prev?.classList.add("selected");
      return;
    }
    if (e.key === "Enter" || e.key === "Tab") {
      e.preventDefault();
      if (selected?.dataset.example) insertCommand(selected.dataset.example);
      return;
    }
    if (e.key === "Escape") {
      closeCmdPalette();
      return;
    }
  }
  if (e.key === "Enter" && !e.shiftKey) {
    e.preventDefault();
    document.getElementById("composer").dispatchEvent(new Event("submit"));
  }
});
cmdPalette.addEventListener("click", (e) => {
  const item = e.target.closest(".cmd-item");
  if (item) insertCommand(item.dataset.example);
});

let btwContextText = "";
const btwFab = document.getElementById("btwFab");

function openBtwPanel(context) {
  btwContextText = context || "当前上下文";
  btwContext.textContent = btwContextText.length > 200 ? btwContextText.slice(0, 200) + "…" : btwContextText;
  document.body.classList.add("btw-open");
  btwPrompt.focus();
}

function closeBtwPanel() {
  document.body.classList.remove("btw-open");
}

btwFab.onclick = () => openBtwPanel("当前上下文");
btwClose.onclick = closeBtwPanel;

function appendBtwMessage(role, content, timestamp) {
  btwLog.appendChild(renderMessage(role, content, timestamp));
  btwLog.scrollTop = btwLog.scrollHeight;
}

async function sendBtwMessage(text) {
  btwPrompt.value = "";
  btwPrompt.style.height = "auto";
  appendBtwMessage("user", text, new Date().toISOString());
  const sid = await ensureSession();
  btwSend.disabled = true;
  state.btwLive = { agentId: null, text: "", el: null };
  const typing = document.createElement("div");
  typing.className = "message assistant typing-msg";
  typing.innerHTML = `
    <div class="avatar assistant">K</div>
    <div class="message-body">
      <div class="message-header"><span class="role-name">kkagent</span></div>
      <div class="bubble"><div class="typing"><span></span><span></span><span></span></div></div>
    </div>
  `;
  btwLog.appendChild(typing);
  btwLog.scrollTop = btwLog.scrollHeight;
  try {
    const result = await api(`/api/v1/sessions/${sid}/btw`, {
      method: "POST",
      body: JSON.stringify({ text: `关于以下内容：\n${btwContextText}\n\n${text}` }),
    });
    if (result.agent_id) state.btwLive.agentId = result.agent_id;
    if (result.answer) {
      typing.remove();
      appendBtwMessage("assistant", result.answer, new Date().toISOString());
      state.btwLive = null;
    }
  } catch (err) {
    typing.remove();
    state.btwLive = null;
    appendBtwMessage("system", String(err.message || err), new Date().toISOString());
  } finally {
    btwSend.disabled = false;
  }
}

btwForm.onsubmit = async (e) => {
  e.preventDefault();
  const text = btwPrompt.value.trim();
  if (!text) return;
  await sendBtwMessage(text);
};
btwPrompt.addEventListener("input", adjustBtwPromptHeight);
btwPrompt.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !e.shiftKey) {
    e.preventDefault();
    btwForm.dispatchEvent(new Event("submit"));
  }
});

(function makeDraggable() {
  const header = btwPanel.querySelector(".btw-header");
  let dragging = false;
  let offsetX = 0;
  let offsetY = 0;
  header.addEventListener("mousedown", (e) => {
    dragging = true;
    const rect = btwPanel.getBoundingClientRect();
    offsetX = e.clientX - rect.left;
    offsetY = e.clientY - rect.top;
    btwPanel.style.transition = "none";
  });
  window.addEventListener("mousemove", (e) => {
    if (!dragging) return;
    btwPanel.style.left = e.clientX - offsetX + "px";
    btwPanel.style.top = e.clientY - offsetY + "px";
    btwPanel.style.right = "auto";
    btwPanel.style.bottom = "auto";
  });
  window.addEventListener("mouseup", () => {
    dragging = false;
    btwPanel.style.transition = "";
  });
})();

async function openTimelinePanel() {
  document.body.classList.add("timeline-open");
  timelineList.innerHTML = "";
  if (!state.sessionId) {
    timelineList.innerHTML = '<div class="timeline-empty">当前没有会话。</div>';
    return;
  }
  try {
    const tl = await api(`/api/v1/sessions/${state.sessionId}/timeline`);
    const events = tl.events || [];
    if (!events.length) {
      timelineList.innerHTML = '<div class="timeline-empty">当前会话没有事件记录。</div>';
      return;
    }
    for (const item of events) {
      const el = document.createElement("div");
      el.className = "timeline-item " + (item.kind || item.role || "");
      const changes = (item.changes || []).map((c) => {
        const diff = c.diff ? renderDiffHtml(String(c.diff)) : "";
        return `<div class="timeline-change-row"><div class="timeline-file">${escapeHtml(String(c.path || c.type || ""))}</div>${diff}</div>`;
      }).join("");
      el.innerHTML = `
        <div class="timeline-dot"></div>
        <div class="timeline-body">
          <div class="timeline-time">${escapeHtml(formatTime(item.time))}</div>
          <div class="timeline-title">${escapeHtml(item.title || item.kind || "")}</div>
          ${item.desc ? `<div class="timeline-desc">${escapeHtml(item.desc)}</div>` : ""}
          ${changes ? `<div class="timeline-changes">${changes}</div>` : ""}
        </div>
      `;
      timelineList.appendChild(el);
    }
  } catch (err) {
    timelineList.innerHTML = `<div class="timeline-empty">${escapeHtml(String(err.message || err))}</div>`;
  }
}

function renderDiffHtml(diffText) {
  const lines = diffText.split("\n").slice(0, 60);
  const html = lines.map((line) => {
    let cls = "";
    if (line.startsWith("+")) cls = "diff-add";
    else if (line.startsWith("-")) cls = "diff-del";
    else if (line.startsWith("@@")) cls = "diff-hunk";
    else if (line.startsWith("diff ") || line.startsWith("index ") || line.startsWith("---") || line.startsWith("+++")) cls = "diff-meta";
    return `<div class="diff-line ${cls}">${escapeHtml(line)}</div>`;
  }).join("");
  return `<div class="diff-block">${html}</div>`;
}

document.getElementById("timelineBtn").onclick = () => openTimelinePanel();
if (timelineClose) timelineClose.onclick = () => document.body.classList.remove("timeline-open");
document.addEventListener("click", (e) => {
  if (!document.body.classList.contains("timeline-open")) return;
  if (e.target.closest("#timelinePanel") || e.target.id === "timelineBtn") return;
  document.body.classList.remove("timeline-open");
});

document.getElementById("newSession").onclick = async () => {
  try {
    state.sessionId = null;
    await ensureSession();
    showWelcome();
    appendMessage("system", "新会话已创建。", new Date().toISOString());
  } catch (err) {
    appendMessage("system", `创建会话失败: ${err.message || err}`, new Date().toISOString());
  }
};

let searchTimer = null;
document.getElementById("searchBox").oninput = (e) => {
  const q = e.target.value.trim();
  clearTimeout(searchTimer);
  searchTimer = setTimeout(async () => {
    if (!q) {
      await refreshSessions();
      return;
    }
    try {
      const body = await api(`/api/v1/search?q=${encodeURIComponent(q)}`);
      sessionsEl.innerHTML = "";
      for (const hit of body.hits || []) {
        const div = document.createElement("div");
        div.className = "session";
        div.innerHTML = `
          <div class="session-title">${escapeHtml(hit.title || hit.session_id?.slice(0, 8) || "?")}</div>
          <div class="session-meta">${escapeHtml(hit.preview || "")}</div>
        `;
        div.onclick = () => { closeSidebar(); selectSession(hit.session_id); };
        sessionsEl.appendChild(div);
      }
    } catch (err) {
      statusEl.textContent = `search: ${err}`;
    }
  }, 200);
};

if (workspaceTabs) {
  workspaceTabs.onclick = (e) => {
    const tab = e.target.closest(".workspace-tab");
    if (!tab) return;
    state.scope = tab.dataset.scope;
    workspaceTabs.querySelectorAll(".workspace-tab").forEach((t) => t.classList.toggle("active", t.dataset.scope === state.scope));
    renderSessions();
  };
}

function fileToAttachment(file) {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve({ name: file.name, type: file.type, size: file.size, dataUrl: reader.result, file });
    reader.onerror = reject;
    reader.readAsDataURL(file);
  });
}

async function addFiles(files) {
  for (const f of files) state.attachments.push(await fileToAttachment(f));
  renderAttachments();
}

function createAttachmentPreview(a, removable = true) {
  const pill = document.createElement("div");
  const isImage = String(a.type || a.media_type || "").startsWith("image/");
  pill.className = "attachment-pill" + (isImage ? " image-pill" : "");
  if (isImage) {
    pill.innerHTML = `<img src="${escapeHtml(a.dataUrl)}" alt="">${removable ? '<span class="rm">×</span>' : ""}`;
    pill.querySelector("img").onclick = (e) => {
      e.stopPropagation();
      const box = document.createElement("div");
      box.className = "image-lightbox";
      box.innerHTML = `<img src="${escapeHtml(a.dataUrl)}" alt="">`;
      box.onclick = () => box.remove();
      document.body.appendChild(box);
    };
  } else {
    pill.innerHTML = `
      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"></path><polyline points="14 2 14 8 20 8"></polyline></svg>
      <span style="white-space:nowrap;overflow:hidden;text-overflow:ellipsis;max-width:140px">${escapeHtml(a.name)}</span>
      ${removable ? '<span class="rm">×</span>' : ""}
    `;
  }
  const rm = pill.querySelector(".rm");
  if (rm) {
    rm.onclick = (e) => {
      e.stopPropagation();
      state.attachments = state.attachments.filter((item) => item !== a);
      renderAttachments();
    };
  }
  return pill;
}

function renderAttachments() {
  attachmentRow.innerHTML = "";
  for (const a of state.attachments) attachmentRow.appendChild(createAttachmentPreview(a));
  attachmentRow.style.display = state.attachments.length ? "flex" : "none";
}

attachBtn.onclick = () => fileInput.click();
fileInput.onchange = (e) => { addFiles(e.target.files); fileInput.value = ""; };
document.addEventListener("paste", async (e) => {
  if (e.target !== promptEl) return;
  const files = [];
  for (const item of e.clipboardData.items) {
    if (item.kind === "file") {
      const f = item.getAsFile();
      if (f) files.push(f);
    }
  }
  if (files.length) {
    e.preventDefault();
    await addFiles(files);
  }
});

function dataUrlParts(dataUrl) {
  const match = String(dataUrl).match(/^data:([^;]+);base64,(.+)$/);
  if (!match) return null;
  return { media_type: match[1], data: match[2] };
}

async function handleSlash(text) {
  const match = text.match(/^\/([a-zA-Z_-]+)(?:\s+([\s\S]*))?$/);
  if (!match) return false;
  const name = match[1].toLowerCase();
  const args = (match[2] || "").trim();
  if (name === "new") {
    document.getElementById("newSession").click();
    return true;
  }
  if (name === "fork") {
    await forkCurrentSession(args || undefined);
    return true;
  }
  if (name === "btw") {
    openBtwPanel(args || "当前上下文");
    if (args) await sendBtwMessage(args);
    return true;
  }
  if (name === "timeline") {
    await openTimelinePanel();
    return true;
  }
  return false;
}

document.getElementById("composer").onsubmit = async (e) => {
  e.preventDefault();
  const text = promptEl.value.trim();
  if (!text && state.attachments.length === 0) return;
  closeCmdPalette();
  if (text && await handleSlash(text)) {
    promptEl.value = "";
    promptEl.style.height = "auto";
    return;
  }
  promptEl.value = "";
  promptEl.style.height = "auto";
  const welcome = logEl.querySelector(".welcome");
  if (welcome) welcome.remove();

  const attachments = state.attachments.splice(0);
  renderAttachments();
  const images = [];
  const extraTexts = [];
  for (const a of attachments) {
    if (String(a.type || "").startsWith("image/")) {
      const parts = dataUrlParts(a.dataUrl);
      if (parts) images.push(parts);
    } else if (a.file) {
      extraTexts.push(`附件 ${a.name}:\n\`\`\`\n${await a.file.text()}\n\`\`\``);
    }
  }
  const payloadText = [text, ...extraTexts].filter(Boolean).join("\n\n");
  const sid = await ensureSession();
  appendMessage("user", payloadText || " ", new Date().toISOString(), [], {
    images: images.map((img) => ({ media_type: img.media_type, data: img.data })),
  });
  showTyping();
  sendBtn.disabled = true;
  try {
    await api(`/api/v1/sessions/${sid}/messages`, {
      method: "POST",
      body: JSON.stringify({ text: payloadText, images }),
    });
  } catch (err) {
    removeTyping();
    sendBtn.disabled = false;
    appendMessage("system", String(err.message || err), new Date().toISOString());
  }
};

function replaceLiveMessage() {
  if (!state.live) return;
  const fresh = renderMessage("assistant", state.live.text, state.live.time, state.live.tools);
  if (state.live.el && state.live.el.parentNode) state.live.el.replaceWith(fresh);
  else logEl.appendChild(fresh);
  state.live.el = fresh;
  logEl.scrollTop = logEl.scrollHeight;
}

function ensureLive() {
  if (state.live) return state.live;
  removeTyping();
  state.live = { text: "", tools: [], time: new Date().toISOString(), el: null };
  replaceLiveMessage();
  return state.live;
}

function handleAgentEvent(event) {
  const type = event.type;
  const sessionId = event.session_id;
  if (type === "session.created" || type === "session.forked" || type === "session.deleted") {
    refreshSessions().catch(() => {});
    return;
  }
  if (sessionId && state.sessionId && sessionId !== state.sessionId && type !== "btw_delta" && type !== "btw_end") {
    return;
  }
  if (type === "turn_start") {
    if (sessionId === state.sessionId) {
      const welcome = logEl.querySelector(".welcome");
      if (welcome) welcome.remove();
      showTyping();
      sendBtn.disabled = true;
      state.live = null;
    }
    return;
  }
  if (type === "message_delta" && event.text) {
    if (sessionId === state.sessionId) {
      const live = ensureLive();
      live.text += event.text;
      replaceLiveMessage();
    }
    return;
  }
  if (type === "tool_call") {
    if (sessionId === state.sessionId) {
      const live = ensureLive();
      live.tools.push({
        _id: event.tool_call_id,
        name: event.tool_name,
        input: event.input || {},
        status: "running",
      });
      replaceLiveMessage();
    }
    return;
  }
  if (type === "tool_result") {
    if (sessionId === state.sessionId && state.live) {
      const tool = state.live.tools.find((t) => t._id === event.tool_call_id);
      if (tool) {
        tool.output = event.output;
        tool.status = event.is_error ? "error" : "success";
        if (event.is_error) tool.error = event.output;
        replaceLiveMessage();
      }
    }
    return;
  }
  if (type === "turn_end") {
    sendBtn.disabled = false;
    if (sessionId === state.sessionId) {
      const sid = state.sessionId;
      state.live = null;
      selectSession(sid).catch(() => {});
      refreshSessions().catch(() => {});
    }
    return;
  }
  if (type === "error") {
    sendBtn.disabled = false;
    removeTyping();
    appendMessage("system", event.message || "turn error", new Date().toISOString());
    return;
  }
  if (type === "approval_requested") {
    const request = event.request || {};
    const id = request.approval_id;
    showNotice(`需要批准工具：${request.tool_name || "tool"}`, [
      {
        label: "批准",
        primary: true,
        onclick: async () => {
          await api(`/api/v1/approvals/${id}`, { method: "POST", body: JSON.stringify({ decision: "approved" }) });
          hideNotice();
        },
      },
      {
        label: "拒绝",
        onclick: async () => {
          await api(`/api/v1/approvals/${id}`, { method: "POST", body: JSON.stringify({ decision: "rejected" }) });
          hideNotice();
        },
      },
    ]);
    return;
  }
  if (type === "question_asked") {
    const question = event.question || {};
    showNotice(question.prompt || question.header || "需要你的回答", [
      {
        label: "关闭",
        onclick: () => hideNotice(),
      },
    ]);
    return;
  }
  if (type === "btw_delta" && event.text) {
    const typing = btwLog.querySelector(".typing-msg");
    if (typing) typing.remove();
    if (!state.btwLive) state.btwLive = { agentId: event.agent_id, text: "", el: null };
    state.btwLive.agentId = event.agent_id || state.btwLive.agentId;
    state.btwLive.text += event.text;
    const fresh = renderMessage("assistant", state.btwLive.text, new Date().toISOString());
    if (state.btwLive.el && state.btwLive.el.parentNode) state.btwLive.el.replaceWith(fresh);
    else btwLog.appendChild(fresh);
    state.btwLive.el = fresh;
    btwLog.scrollTop = btwLog.scrollHeight;
    return;
  }
  if (type === "btw_end") {
    const typing = btwLog.querySelector(".typing-msg");
    if (typing) typing.remove();
    btwSend.disabled = false;
    state.btwLive = null;
    if (event.error) appendBtwMessage("system", event.error, new Date().toISOString());
  }
}

function connectWs() {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  const qs = new URLSearchParams();
  if (token) qs.set("token", token);
  const ws = new WebSocket(`${proto}//${location.host}${base}/api/v1/ws?${qs.toString()}`);
  ws.onmessage = (ev) => {
    try {
      handleAgentEvent(JSON.parse(ev.data));
    } catch {
      /* ignore malformed frames */
    }
  };
  ws.onclose = () => setTimeout(connectWs, 1500);
}

function openSidebar() { sidebar.classList.add("open"); }
function closeSidebar() { sidebar.classList.remove("open"); }
menuToggle.onclick = () => sidebar.classList.toggle("open");
document.addEventListener("click", (e) => {
  if (window.innerWidth > 760) return;
  if (!sidebar.contains(e.target) && e.target !== menuToggle && sidebar.classList.contains("open")) closeSidebar();
});

async function boot() {
  try {
    const meta = await api("/api/v1/meta");
    const workspace = await api("/api/v1/workspaces").catch(() => ({}));
    state.cwd = workspace.cwd || "";
    statusEl.textContent = `已连接 · ${meta.name || "kkagent"}`;
    statusEl.className = "online";
    connectWs();
    await refreshSessions();
    const first = filterSessions()[0] || state.sessions[0];
    if (first) await selectSession(sessionIdOf(first));
    else showWelcome();
  } catch (err) {
    if (err.unauthorized) {
      localStorage.removeItem("kkagent_token");
      showTokenPrompt("token 无效或已过期，请重新输入");
      return;
    }
    statusEl.textContent = `离线: ${err}`;
    statusEl.className = "offline";
    showWelcome();
  }
}

if (!token) showTokenPrompt();
else boot();

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
  views: {},
  btwLive: null,
  followBottom: true,
  suppressScroll: false,
  running: false,
  title: "",
  permissionMode: "manual",
  defaultPermissionMode: "manual",
  planMode: false,
  model: "",
  defaultModel: "",
  models: [],
  usage: null,
  mdRaf: 0,
  pendingMd: null,
  selectionVersion: 0,
  sessionsRefreshVersion: 0,
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
const sessionBar = document.getElementById("sessionBar");
const sessionTitle = document.getElementById("sessionTitle");
const sessionMeta = document.getElementById("sessionMeta");
const permissionModeEl = document.getElementById("permissionMode");
const planBtn = document.getElementById("planBtn");
const moreBtn = document.getElementById("moreBtn");
const sessionMenu = document.getElementById("sessionMenu");
const jumpBottom = document.getElementById("jumpBottom");
const confirmModal = document.getElementById("confirmModal");
const confirmTitle = document.getElementById("confirmTitle");
const confirmBody = document.getElementById("confirmBody");
const confirmOk = document.getElementById("confirmOk");
const confirmCancel = document.getElementById("confirmCancel");
const modelSelect = document.getElementById("modelSelect");
const promptModal = document.getElementById("promptModal");
const promptTitle = document.getElementById("promptTitle");
const promptBody = document.getElementById("promptBody");
const promptExtra = document.getElementById("promptExtra");
const promptActions = document.getElementById("promptActions");

const ICONS = {
  copy: '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="9" y="9" width="13" height="13" rx="2" ry="2"></rect><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"></path></svg>',
  edit: '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M11 4H4a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7"></path><path d="M18.5 2.5a2.121 2.121 0 0 1 3 3L12 15l-4 1 1-4 9.5-9.5z"></path></svg>',
  fork: '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="6" y1="3" x2="6" y2="15"></line><circle cx="18" cy="6" r="3"></circle><circle cx="6" cy="18" r="3"></circle><path d="M18 9a9 9 0 0 1-9 9"></path></svg>',
  follow: '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 11.5a8.38 8.38 0 0 1-.9 3.8 8.5 8.5 0 0 1-7.6 4.7 8.38 8.38 0 0 1-3.8-.9L3 21l1.9-5.7a8.38 8.38 0 0 1-.9-3.8 8.5 8.5 0 0 1 4.7-7.6 8.38 8.38 0 0 1 3.8-.9h.5a8.48 8.48 0 0 1 8 8v.5z"></path></svg>',
  check: '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="20 6 9 17 4 12"></polyline></svg>',
  chevron: '<svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><polyline points="9 18 15 12 9 6"></polyline></svg>',
  spinner: '<svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round"><path d="M21 12a9 9 0 1 1-6.219-8.56"></path></svg>',
  x: '<svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round"><line x1="18" y1="6" x2="6" y2="18"></line><line x1="6" y1="6" x2="18" y2="18"></line></svg>',
  trash: '<svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="3 6 5 6 21 6"></polyline><path d="M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6"></path><path d="M10 11v6"></path><path d="M14 11v6"></path><path d="M9 6V4a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v2"></path></svg>',
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
  { name: "/new", desc: "新建会话", example: "/new" },
  { name: "/fork", desc: "Fork 当前会话", example: "/fork" },
  { name: "/title", desc: "设置会话标题", example: "/title " },
  { name: "/rename", desc: "重命名会话", example: "/rename " },
  { name: "/delete", desc: "删除当前会话", example: "/delete" },
  { name: "/yolo", desc: "切换 YOLO 自动批准", example: "/yolo" },
  { name: "/auto", desc: "切换 Auto 全自动", example: "/auto" },
  { name: "/permission", desc: "设置权限模式", example: "/permission manual" },
  { name: "/plan", desc: "切换 Plan 模式", example: "/plan" },
  { name: "/compact", desc: "压缩上下文", example: "/compact" },
  { name: "/undo", desc: "撤销上一轮", example: "/undo" },
  { name: "/interrupt", desc: "中断当前 turn", example: "/interrupt" },
  { name: "/model", desc: "切换模型", example: "/model " },
  { name: "/status", desc: "会话状态", example: "/status" },
  { name: "/usage", desc: "上下文用量", example: "/usage" },
  { name: "/export", desc: "导出会话 JSON", example: "/export" },
  { name: "/btw", desc: "在侧边窗口追问", example: "/btw " },
  { name: "/timeline", desc: "打开 Timeline", example: "/timeline" },
  { name: "/explain", desc: "解释选中代码或上下文", example: "/explain 这段代码的作用" },
  { name: "/fix", desc: "修复当前问题", example: "/fix 修复当前问题" },
  { name: "/commit", desc: "提交当前变更", example: "/commit 提交修复" },
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
  if (!text) return "";
  const lines = String(text).replace(/\r\n/g, "\n").split("\n");
  const out = [];
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    if (!line.trim()) {
      i += 1;
      continue;
    }
    if (/^```/.test(line)) {
      const lang = line.replace(/^```/, "").trim();
      const body = [];
      i += 1;
      while (i < lines.length && !/^```/.test(lines[i])) {
        body.push(lines[i]);
        i += 1;
      }
      if (i < lines.length) i += 1;
      const code = body.join("\n");
      const escaped = escapeHtml(code);
      out.push(`<div class="code-block"><button class="copy-btn code-copy" data-code="${escaped.replace(/"/g, "&quot;")}" title="复制代码">${ICONS.copy}</button><pre><code class="lang-${escapeHtml(lang || "text")}">${highlightCode(escaped)}</code></pre></div>`);
      continue;
    }
    if (isTableStart(lines, i)) {
      const header = splitTableRow(lines[i]);
      i += 2;
      const rows = [];
      while (i < lines.length && lines[i].includes("|") && !isTableSep(lines[i]) && lines[i].trim()) {
        rows.push(splitTableRow(lines[i]));
        i += 1;
      }
      const head = header.map((cell) => `<th>${inlineMd(cell)}</th>`).join("");
      const body = rows.map((row) => `<tr>${row.map((cell) => `<td>${inlineMd(cell)}</td>`).join("")}</tr>`).join("");
      out.push(`<div class="md-table-wrap"><table><thead><tr>${head}</tr></thead><tbody>${body}</tbody></table></div>`);
      continue;
    }
    const heading = line.match(/^(#{1,6})\s+(.+)$/);
    if (heading) {
      const level = heading[1].length;
      out.push(`<h${level}>${inlineMd(heading[2])}</h${level}>`);
      i += 1;
      continue;
    }
    if (/^(\*\s*){3,}$|^(-{3,})$|^(_\s*){3,}$/.test(line.trim())) {
      out.push("<hr>");
      i += 1;
      continue;
    }
    if (/^>\s?/.test(line)) {
      const quote = [];
      while (i < lines.length && /^>\s?/.test(lines[i])) {
        quote.push(lines[i].replace(/^>\s?/, ""));
        i += 1;
      }
      out.push(`<blockquote>${inlineMd(quote.join("\n")).replace(/\n/g, "<br>")}</blockquote>`);
      continue;
    }
    if (/^\s*[-*+]\s+/.test(line)) {
      const items = [];
      while (i < lines.length && /^\s*[-*+]\s+/.test(lines[i])) {
        items.push(`<li>${inlineMd(lines[i].replace(/^\s*[-*+]\s+/, ""))}</li>`);
        i += 1;
      }
      out.push(`<ul>${items.join("")}</ul>`);
      continue;
    }
    if (/^\s*\d+\.\s+/.test(line)) {
      const items = [];
      while (i < lines.length && /^\s*\d+\.\s+/.test(lines[i])) {
        items.push(`<li>${inlineMd(lines[i].replace(/^\s*\d+\.\s+/, ""))}</li>`);
        i += 1;
      }
      out.push(`<ol>${items.join("")}</ol>`);
      continue;
    }
    const para = [];
    while (i < lines.length && lines[i].trim() && !isBlockStart(lines, i)) {
      para.push(lines[i]);
      i += 1;
    }
    out.push(`<p>${inlineMd(para.join("\n")).replace(/\n/g, "<br>")}</p>`);
  }
  return out.join("");
}

function isTableSep(line) {
  const cells = splitTableRow(line);
  return cells.length > 0 && cells.every((cell) => /^:?-{3,}:?$/.test(cell.replace(/\s/g, "")));
}

function isTableStart(lines, i) {
  return lines[i].includes("|") && i + 1 < lines.length && isTableSep(lines[i + 1]);
}

function splitTableRow(line) {
  let value = line.trim();
  if (value.startsWith("|")) value = value.slice(1);
  if (value.endsWith("|")) value = value.slice(0, -1);
  return value.split("|").map((cell) => cell.trim());
}

function isBlockStart(lines, i) {
  const line = lines[i] || "";
  return /^```/.test(line)
    || isTableStart(lines, i)
    || /^#{1,6}\s+/.test(line)
    || /^(\*\s*){3,}$|^(-{3,})$|^(_\s*){3,}$/.test(line.trim())
    || /^>\s?/.test(line)
    || /^\s*[-*+]\s+/.test(line)
    || /^\s*\d+\.\s+/.test(line);
}

function inlineMd(text) {
  const codes = [];
  let html = String(text || "").replace(/`([^`]+)`/g, (_, code) => {
    codes.push(`<code>${escapeHtml(code)}</code>`);
    return `\0C${codes.length - 1}\0`;
  });
  html = escapeHtml(html);
  html = html.replace(/\[([^\]]+)\]\((https?:[^)\s]+)\)/g, '<a href="$2" target="_blank" rel="noopener">$1</a>');
  html = html.replace(/\*\*\*(.+?)\*\*\*/g, "<strong><em>$1</em></strong>");
  html = html.replace(/\*\*(.+?)\*\*/g, "<strong>$1</strong>");
  html = html.replace(/\*(.+?)\*/g, "<em>$1</em>");
  html = html.replace(/~~(.+?)~~/g, "<del>$1</del>");
  html = html.replace(/\0C(\d+)\0/g, (_, index) => codes[Number(index)] || "");
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

function bindCodeCopies(root) {
  for (const btn of root.querySelectorAll(".code-copy")) {
    btn.onclick = () => copyText(btn.dataset.code || "").then(() => flashBtn(btn, "copy"));
  }
}

function bindToolToggles(div, toolCalls) {
  for (const tc of div.querySelectorAll(".tool-call")) {
    const row = tc.querySelector(".tool-row");
    if (!row) continue;
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
    const tool = toolCalls.find((item) => item._id === tc.dataset.id);
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

function bindMessageActions(div, role, content, toolCalls, messageIndex) {
  const msgCopy = div.querySelector(".msg-copy");
  if (msgCopy) msgCopy.onclick = () => copyText(content).then(() => flashBtn(msgCopy, "copy"));
  const msgEdit = div.querySelector(".msg-edit");
  if (msgEdit) {
    msgEdit.onclick = async () => {
      const sessionId = state.sessionId;
      if (!sessionId) return;
      const selectionVersion = state.selectionVersion;
      promptEl.value = content;
      promptEl.focus();
      adjustPromptHeight();
      if (typeof messageIndex === "number") {
        try {
          const forked = await api(`/api/v1/sessions/${sessionId}/fork`, {
            method: "POST",
            body: JSON.stringify({ message_limit: messageIndex, title: "Edit" }),
          });
          await refreshSessions();
          if (state.sessionId === sessionId && state.selectionVersion === selectionVersion) {
            await selectSession(forked.session_id || forked.id);
          }
        } catch (err) {
          appendSessionMessage(sessionId, "system", `fork 失败: ${err.message || err}`, new Date().toISOString());
        }
      }
    };
  }
  const msgFork = div.querySelector(".msg-fork");
  if (msgFork) msgFork.onclick = () => forkCurrentSession();
  const msgFollow = div.querySelector(".msg-follow");
  if (msgFollow) msgFollow.onclick = () => openBtwPanel(content);
  bindCodeCopies(div);
  bindToolToggles(div, toolCalls);
}

function renderThinking(thinking, { done = true } = {}) {
  if (!thinking) return "";
  const open = done ? "" : " open";
  const label = done ? "思考过程" : "思考中…";
  return `<details class="thinking${done ? "" : " is-live"}"${open}>
      <summary><span class="thinking-label">${label}</span></summary>
      <div class="thinking-body">${escapeHtml(thinking)}</div>
    </details>`;
}

function renderMessage(role, content, timestamp, toolCalls = [], extras = {}) {
  const div = document.createElement("div");
  div.className = `message ${role}${extras.live ? " is-live" : ""}`;
  const roleLabel = role === "assistant" ? "kkagent" : role === "user" ? "你" : "system";
  const roleIcon = role === "assistant" ? "K" : role === "user" ? "U" : "!";
  const toolsHtml = renderToolGroup(toolCalls);
  const thinkingHtml = role === "assistant" ? renderThinking(extras.thinking, { done: extras.thinkingDone !== false }) : "";
  div.innerHTML = `
    <div class="avatar ${role}">${roleIcon}</div>
    <div class="message-body">
      <div class="message-header">
        <span class="role-name">${roleLabel}</span>
        <span class="timestamp">${formatTime(timestamp)}</span>
      </div>
      ${thinkingHtml}
      <div class="bubble">
        <div class="bubble-md">${content ? markdownToHtml(content) : ""}</div>
        ${toolsHtml}
      </div>
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

function isNearBottom() {
  return logEl.scrollHeight - logEl.scrollTop - logEl.clientHeight < 80;
}

function updateJumpButton() {
  if (!jumpBottom) return;
  jumpBottom.classList.toggle("visible", Boolean(state.sessionId) && !state.followBottom);
}

function maybeScrollLog() {
  if (state.suppressScroll) return;
  if (!state.followBottom) {
    updateJumpButton();
    return;
  }
  logEl.scrollTop = logEl.scrollHeight;
  updateJumpButton();
}

function jumpToLatest() {
  state.followBottom = true;
  logEl.scrollTop = logEl.scrollHeight;
  updateJumpButton();
}

function setRunning(running) {
  state.running = running;
  // Keep the composer enabled while running so extra input is steered into
  // the active turn instead of being blocked. The send button morphs into a
  // stop button while the agent loop is active.
  sendBtn.disabled = false;
  sendBtn.classList.toggle("running", running);
  sendBtn.title = running ? "停止 (Esc)" : "发送 (Enter)";
  sendBtn.setAttribute("aria-label", running ? "Stop" : "Send");
}

function applySessionMeta(sess = {}) {
  state.title = sess.title || "";
  const mode = String(sess.permission_mode || state.defaultPermissionMode || "manual").toLowerCase();
  state.permissionMode = ["manual", "yolo", "auto"].includes(mode) ? mode : state.defaultPermissionMode || "manual";
  state.planMode = Boolean(sess.plan_mode);
  state.model = sess.model || sess.model_alias || state.defaultModel || "";
  state.usage = sess.usage || null;
  if (sessionBar) sessionBar.classList.toggle("active", Boolean(state.sessionId));
  if (sessionTitle && document.activeElement !== sessionTitle) {
    sessionTitle.value = state.title;
    sessionTitle.placeholder = state.sessionId ? state.sessionId.slice(0, 8) : "会话标题";
  }
  if (permissionModeEl) permissionModeEl.value = ["manual", "yolo", "auto"].includes(state.permissionMode)
    ? state.permissionMode
    : "manual";
  if (planBtn) planBtn.classList.toggle("active", state.planMode);
  if (sessionMeta) sessionMeta.textContent = "";
  renderModelSelect();
}

function updateSessionMeta(sessionId, sess = {}) {
  if (!sessionId) return {};
  const view = viewOf(sessionId);
  const meta = { ...(view.meta || {}) };
  if (Object.hasOwn(sess, "title")) meta.title = sess.title || "";
  if (Object.hasOwn(sess, "permission_mode") && sess.permission_mode != null) {
    meta.permission_mode = sess.permission_mode;
  }
  if (Object.hasOwn(sess, "plan_mode") && sess.plan_mode != null) meta.plan_mode = Boolean(sess.plan_mode);
  if (Object.hasOwn(sess, "model") && sess.model != null) meta.model = sess.model;
  else if (Object.hasOwn(sess, "model_alias") && sess.model_alias != null) meta.model = sess.model_alias;
  if (Object.hasOwn(sess, "usage")) meta.usage = sess.usage || null;
  view.meta = meta;
  return meta;
}

function confirmAction({ title, body, ok = "确定", danger = false }) {
  return new Promise((resolve) => {
    confirmTitle.textContent = title;
    confirmBody.textContent = body;
    confirmOk.textContent = ok;
    confirmOk.className = danger ? "btn-danger" : "btn-primary";
    confirmModal.classList.add("active");
    const finish = (value) => {
      confirmModal.classList.remove("active");
      confirmOk.onclick = null;
      confirmCancel.onclick = null;
      resolve(value);
    };
    confirmOk.onclick = () => finish(true);
    confirmCancel.onclick = () => finish(false);
  });
}

function showTyping() {
  // Idempotent: the composer and the turn_start WS event can both call this
  // for the same turn — reuse the existing bubble instead of stacking a
  // second "typing" message.
  const existing = logEl.querySelector(".typing-msg");
  if (existing) return existing;
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
  maybeScrollLog();
  return div;
}

function removeTyping() {
  // Remove every stale typing bubble (historically more than one could be
  // stacked before showTyping became idempotent / after session repaints).
  logEl.querySelectorAll(".typing-msg").forEach((el) => el.remove());
}

function viewMessageKey(message) {
  const tools = (message.tool_calls || []).map((tool) => tool._id || tool.id || "").join(",");
  const images = (message.images || []).map((image) => image.path || image.src || image.media_type || image.mediaType || "image").join(",");
  return `${message.role || "assistant"}\u0000${message.content || ""}\u0000${tools}\u0000${images}`;
}

function mergeTransientMessages(serverMessages, previousMessages) {
  const transientMessages = (previousMessages || []).filter((message) => message.localOnly || message.optimistic);
  if (!transientMessages.length) return serverMessages;

  // A system message may eventually become part of the canonical transcript.
  // Consume matching server entries first so repeated fetches do not duplicate
  // it while still preserving multiple equal local notices in their order.
  const serverKeyCounts = new Map();
  for (const message of serverMessages) {
    const key = viewMessageKey(message);
    serverKeyCounts.set(key, (serverKeyCounts.get(key) || 0) + 1);
  }
  const pending = transientMessages.filter((message) => {
    const key = viewMessageKey(message);
    const count = serverKeyCounts.get(key) || 0;
    if (!count) return true;
    serverKeyCounts.set(key, count - 1);
    return false;
  });

  const unanchored = [];
  const anchored = new Map();
  for (const message of pending) {
    if (!message.afterMessageKey) {
      unanchored.push(message);
      continue;
    }
    const occurrence = message.afterMessageOccurrence || 1;
    const anchor = `${message.afterMessageKey}\u0000${occurrence}`;
    if (!anchored.has(anchor)) anchored.set(anchor, []);
    anchored.get(anchor).push(message);
  }

  const merged = [...unanchored];
  const occurrences = new Map();
  for (const message of serverMessages) {
    merged.push(message);
    const key = viewMessageKey(message);
    const occurrence = (occurrences.get(key) || 0) + 1;
    occurrences.set(key, occurrence);
    const anchor = `${key}\u0000${occurrence}`;
    const following = anchored.get(anchor);
    if (following) {
      merged.push(...following);
      anchored.delete(anchor);
    }
  }
  // If compaction removed a local notice's anchor, keep the notice at the end
  // instead of silently dropping it.
  for (const messages of anchored.values()) merged.push(...messages);
  return merged;
}

function cacheSessionMessage(sessionId, role, content, timestamp, toolCalls = [], extras = {}) {
  if (!sessionId) return null;
  const view = viewOf(sessionId);
  const previousServerMessage = [...view.messages].reverse().find((message) => !message.localOnly);
  const afterMessageKey = previousServerMessage ? viewMessageKey(previousServerMessage) : null;
  const afterMessageOccurrence = afterMessageKey
    ? view.messages.filter((message) => !message.localOnly && viewMessageKey(message) === afterMessageKey).length
    : null;
  const message = {
    role,
    content,
    created_at: timestamp,
    tool_calls: toolCalls,
    images: extras.images || [],
    thinking: extras.thinking || "",
    thinkingDone: extras.thinkingDone !== false,
    localOnly: extras.localOnly ?? role === "system",
    optimistic: extras.optimistic ?? role === "user",
    afterMessageKey,
    afterMessageOccurrence,
  };
  view.messages.push(message);
  touchView(view);
  return message;
}

function appendMessage(role, content, timestamp, toolCalls = [], extras = {}) {
  removeTyping();
  logEl.querySelector(".welcome")?.remove();
  if (!state.suppressScroll && extras.cache !== false) {
    cacheSessionMessage(state.sessionId, role, content, timestamp, toolCalls, extras);
  }
  const el = renderMessage(role, content, timestamp, toolCalls, extras);
  if (extras.images && extras.images.length) {
    const body = el.querySelector(".message-body");
    const row = document.createElement("div");
    row.className = "msg-attachments";
    for (const image of extras.images) row.appendChild(createImagePreview(image));
    body.appendChild(row);
  }
  logEl.appendChild(el);
  maybeScrollLog();
  return el;
}

function appendSessionMessage(sessionId, role, content, timestamp, toolCalls = [], extras = {}) {
  if (!sessionId) return null;
  if (sessionId === state.sessionId) return appendMessage(role, content, timestamp, toolCalls, extras);
  return cacheSessionMessage(sessionId, role, content, timestamp, toolCalls, extras);
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
      let thinking = "";
      const tools = [];
      if (parts) {
        for (const part of parts) {
          if (part.type === "text") text += part.text || "";
          else if (part.type === "thinking") thinking += part.thinking || part.text || "";
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
        thinking = raw.thinking || "";
        for (const tool of raw.tool_calls || []) tools.push(tool);
      }
      if (!text.trim() && tools.length === 0 && !thinking.trim()) continue;
      out.push({
        role: "assistant",
        content: text,
        thinking,
        thinkingDone: true,
        created_at: raw.created_at || raw.at,
        tool_calls: tools,
      });
    } else if (role === "system") {
      const rawText = parts
        ? parts.filter((part) => part.type === "text").map((part) => part.text || "").join("\n")
        : raw.text || (typeof raw.content === "string" ? raw.content : "");
      const text = visibleUserText(rawText);
      if (text) out.push({ role: "system", content: text, created_at: raw.created_at, tool_calls: [] });
    }
  }
  // Merge thinking content from consecutive assistant messages that belong to
  // the same multi-step agent turn.  The server stores each step as a separate
  // assistant message (each with its own thinking block), but tool-result-only
  // user messages are skipped above, so consecutive assistant entries in `out`
  // are part of the same turn.  Without this merge the UI would show one
  // collapsed thinking box per step, cluttering the transcript.
  //
  // Walk backwards so tool/text steps append their thinking to the *first*
  // assistant in the run (the one that originally opened the turn). Preserve
  // a thinking-only terminal message: clearing it would leave an empty bubble
  // and hide the only payload some providers emit for the final step.
  for (let i = out.length - 1; i > 0; i--) {
    const hasVisiblePayload = Boolean(out[i].content.trim() || out[i].tool_calls.length);
    if (out[i].role === "assistant" && out[i].thinking && hasVisiblePayload && out[i - 1].role === "assistant") {
      const prev = out[i - 1].thinking;
      out[i - 1].thinking = prev ? prev + "\n" + out[i].thinking : out[i].thinking;
      out[i].thinking = "";
    }
  }
  return out;
}

function renderLog(messages, { jump = true, parsed = false } = {}) {
  let items = parsed ? messages || [] : parseTranscript(messages);
  if (!parsed && state.sessionId) {
    const view = viewOf(state.sessionId);
    if (!view.live && !view.running) {
      view.messages = mergeTransientMessages(items, view.messages);
      items = view.messages;
      touchView(view);
    }
  }
  state.suppressScroll = true;
  logEl.classList.add("is-history");
  logEl.innerHTML = "";
  if (!items.length) {
    logEl.classList.remove("is-history");
    state.suppressScroll = false;
    const view = state.sessionId ? viewOf(state.sessionId) : null;
    if (!view || (!view.running && !view.live)) showWelcome();
    if (jump) jumpToLatest();
    else updateJumpButton();
    return;
  }
  items.forEach((message, index) => {
    appendMessage(message.role, message.content, message.created_at, message.tool_calls || [], {
      images: message.images,
      messageIndex: message.sourceIndex ?? index,
      thinking: message.thinking,
      thinkingDone: message.thinkingDone !== false,
    });
  });
  logEl.classList.remove("is-history");
  state.suppressScroll = false;
  if (jump) {
    state.followBottom = true;
    logEl.scrollTop = logEl.scrollHeight;
    requestAnimationFrame(() => {
      logEl.scrollTop = logEl.scrollHeight;
      updateJumpButton();
    });
  } else {
    updateJumpButton();
  }
}

function sessionIdOf(session) {
  return session.session_id || session.id;
}

function emptySessionView() {
  return {
    messages: [],
    meta: null,
    draft: "",
    attachments: [],
    live: null,
    running: false,
    pendingApproval: null,
    pendingQuestion: null,
    revision: 0,
  };
}

function viewOf(id) {
  if (!id) return emptySessionView();
  if (!state.views[id]) state.views[id] = emptySessionView();
  return state.views[id];
}

function saveComposerState(sessionId) {
  if (!sessionId) return;
  const view = viewOf(sessionId);
  view.draft = promptEl.value;
  view.attachments = [...state.attachments];
}

function restoreComposerState(sessionId) {
  const view = viewOf(sessionId);
  promptEl.value = view.draft || "";
  state.attachments = [...(view.attachments || [])];
  adjustPromptHeight();
  renderAttachments();
  closeCmdPalette();
}

function updatePlanBtn() {
  if (planBtn) planBtn.classList.toggle("active", Boolean(state.planMode));
}

// --- Todo / plan / subagent side panels -------------------------------------

function ensurePanel(id, title) {
  let el = document.getElementById(id);
  if (!el) {
    el = document.createElement("section");
    el.id = id;
    el.className = "side-panel";
    el.innerHTML = `<header class="side-panel-header">${escapeHtml(title)}<button type="button" class="side-panel-close" aria-label="关闭">×</button></header><div class="side-panel-body"></div>`;
    el.querySelector(".side-panel-close").onclick = () => { el.classList.remove("open"); };
    document.getElementById("log")?.parentElement?.appendChild(el);
  }
  return el;
}

function ensureTodoPopover() {
  let el = document.getElementById("todoPanel");
  if (!el) {
    el = document.createElement("section");
    el.id = "todoPanel";
    el.className = "todo-popover";
    el.innerHTML = `
      <header class="todo-pop-header">
        <span class="todo-pop-title">任务清单</span>
        <span class="todo-pop-count"></span>
        <button type="button" class="todo-pop-toggle" aria-label="展开/折叠" title="展开/折叠">⤢</button>
        <button type="button" class="todo-pop-close" aria-label="关闭">×</button>
      </header>
      <div class="todo-pop-body"><ul class="todo-list"></ul></div>`;
    el.querySelector(".todo-pop-close").onclick = () => { el.classList.remove("open"); };
    el.querySelector(".todo-pop-toggle").onclick = () => {
      el.classList.toggle("expanded");
      el.querySelector(".todo-pop-toggle").textContent = el.classList.contains("expanded") ? "⤡" : "⤢";
    };
    // Anchor inside the composer wrap so the popover floats right above the
    // input box, aligned to the right edge (next to the send button).
    document.querySelector(".composer-wrap")?.appendChild(el);
  }
  return el;
}

function renderTodos(todos) {
  const el = ensureTodoPopover();
  const ul = el.querySelector(".todo-list");
  if (!Array.isArray(todos) || !todos.length) {
    ul.innerHTML = '<li class="todo-empty">暂无任务</li>';
    const count = el.querySelector(".todo-pop-count");
    if (count) count.textContent = "0/0";
    el.classList.remove("open");
    return;
  }
  const rows = todos
    .map((t) => {
      const done = t.status === "done" || t.status === "completed";
      const cancelled = t.status === "cancelled" || t.status === "canceled";
      const mark = done ? "☑" : cancelled ? "☒" : t.status === "in_progress" ? "◐" : "☐";
      const cls = done || cancelled ? "todo-done" : t.status === "in_progress" ? "todo-active" : "";
      return `<li class="${cls}"><span class="todo-mark">${mark}</span><span>${escapeHtml(t.title || t.content || "")}</span></li>`;
    })
    .join("");
  ul.innerHTML = rows;
  const count = el.querySelector(".todo-pop-count");
  if (count) {
    const done = todos.filter((t) => t.status === "done" || t.status === "completed").length;
    count.textContent = `${done}/${todos.length}`;
  }
  el.classList.add("open");
}

function renderPlanPanel(plan) {
  if (!plan || (!plan.content && !plan.path)) return;
  const el = ensurePanel("planPanel", "当前计划");
  const body = el.querySelector(".side-panel-body");
  body.innerHTML = `${plan.path ? `<div class="plan-path">${escapeHtml(plan.path)}</div>` : ""}<div class="plan-content">${markdownToHtml(plan.content || "")}</div>`;
  el.classList.add("open");
}

function renderSubagents(view) {
  const agents = view.subagents;
  if (!agents || !agents.size) return;
  const el = ensurePanel("subagentPanel", "子代理");
  const body = el.querySelector(".side-panel-body");
  const rows = [...agents.values()]
    .map((a) => {
      const dot = a.status === "done" ? "●" : a.status === "failed" ? "✕" : "◐";
      const cls = a.status === "failed" ? "sub-failed" : a.status === "done" ? "sub-done" : "sub-running";
      return `<li class="${cls}"><span>${dot}</span><div><div class="sub-desc">${escapeHtml(a.description || a.id)}</div>${a.detail ? `<div class="sub-detail">${escapeHtml(a.detail.slice(-200))}</div>` : ""}</div></li>`;
    })
    .join("");
  body.innerHTML = `<ul class="subagent-list">${rows}</ul>`;
  el.classList.add("open");
}

function cloneLive(live) {
  if (!live) return null;
  return {
    text: live.text || "",
    tools: (live.tools || []).map((tool) => ({ ...tool })),
    time: live.time || new Date().toISOString(),
    thinking: live.thinking || "",
    thinkingDone: Boolean(live.thinkingDone),
    el: null,
  };
}

function ensureViewLive(view) {
  if (!view.live) {
    view.live = {
      text: "",
      tools: [],
      time: new Date().toISOString(),
      thinking: "",
      thinkingDone: false,
      el: null,
    };
  }
  return view.live;
}

function touchView(view) {
  view.revision = (view.revision || 0) + 1;
}

function finishViewLive(view) {
  const live = view.live;
  if (!live || (!live.text && !live.thinking && !live.tools.length)) {
    view.live = null;
    return false;
  }
  if (live.el && live.el.parentNode) {
    live.el.classList.remove("is-live");
    syncLiveMarkdown(live.el, live.text);
    syncThinking(live.el, live.thinking, true);
  }
  view.messages.push({
    role: "assistant",
    content: live.text,
    thinking: live.thinking,
    thinkingDone: true,
    created_at: live.time,
    tool_calls: live.tools,
  });
  view.live = null;
  return true;
}

function ensureCurrentAssistantStep(view) {
  // A new model step starts only after every tool from the previous assistant
  // message has settled. Commit that message before collecting the next
  // thinking/text/tool delta so intermediate tool steps remain distinct.
  if (view.live && view.live.tools.length && view.live.tools.every((tool) => tool.status !== "running" && tool.status !== "queued")) {
    finishViewLive(view);
  }
  return ensureViewLive(view);
}

function splitServerLiveTail(sess) {
  const raw = Array.isArray(sess.messages) ? sess.messages : [];
  const liveUi = sess.live_ui || {};
  const thinking = liveUi.thinking_text || "";
  const text = liveUi.assistant_text || "";
  if (!thinking && !text) return { messages: raw, live: null };

  // get_session includes the current streaming tail in messages for reconnect
  // safety and also exposes the same bytes in live_ui. When the final raw
  // assistant message is exactly that tail, remove it from history and keep it
  // as the live bubble; otherwise prefer the transcript and avoid duplication.
  const last = raw[raw.length - 1];
  const parts = last && last.role === "assistant" && Array.isArray(last.content) ? last.content : null;
  if (!parts || !parts.every((part) => part.type === "thinking" || part.type === "text")) {
    return { messages: raw, live: null };
  }
  const tailThinking = parts
    .filter((part) => part.type === "thinking")
    .map((part) => part.thinking || part.text || "")
    .join("");
  const tailText = parts
    .filter((part) => part.type === "text")
    .map((part) => part.text || "")
    .join("");
  if (tailThinking !== thinking || tailText !== text) return { messages: raw, live: null };
  return {
    messages: raw.slice(0, -1),
    live: {
      text,
      tools: [],
      time: new Date().toISOString(),
      thinking,
      thinkingDone: Boolean(text),
      el: null,
    },
  };
}

function mergeServerSession(id, sess, { expectedRevision } = {}) {
  const view = viewOf(id);
  updateSessionMeta(id, sess);
  const unchanged = expectedRevision === undefined || view.revision === expectedRevision;
  const mayReplaceTranscript = unchanged || view.messages.length === 0;
  let transcriptChanged = false;
  if (mayReplaceTranscript) {
    const server = splitServerLiveTail(sess);
    view.messages = mergeTransientMessages(parseTranscript(server.messages), view.messages);
    if (unchanged || !view.live) view.live = server.live;
    transcriptChanged = true;
    touchView(view);
  }
  const liveUi = sess.live_ui || {};
  if (liveUi.thinking_text || liveUi.assistant_text || (sess.status && sess.status !== "idle")) {
    view.running = true;
  } else if (unchanged) {
    view.running = false;
  }
  if (unchanged) {
    view.pendingApproval = sess.pending_approval || null;
    view.pendingQuestion = sess.pending_question || null;
  }
  if (Array.isArray(sess.todos) && (unchanged || !Array.isArray(view.todos))) {
    view.todos = sess.todos;
  }
  return transcriptChanged;
}

function paintSessionView(id, { jump = true } = {}) {
  const view = viewOf(id);
  renderLog(view.messages, { jump: false, parsed: true });
  // Sync the todo popover with the session being shown: render its list, or
  // collapse the popover entirely when this session has no todos (prevents
  // stale content from a previously viewed session).
  const todoPanel = document.getElementById("todoPanel");
  if (Array.isArray(view.todos) && view.todos.length) {
    renderTodos(view.todos);
  } else if (todoPanel) {
    todoPanel.classList.remove("open");
  }
  const planPanel = document.getElementById("planPanel");
  if (view.plan && (view.plan.content || view.plan.path)) renderPlanPanel(view.plan);
  else if (planPanel) planPanel.classList.remove("open");
  const subagentPanel = document.getElementById("subagentPanel");
  if (view.subagents && view.subagents.size) renderSubagents(view);
  else if (subagentPanel) subagentPanel.classList.remove("open");
  state.live = cloneLive(view.live);
  if (state.live && (state.live.text || state.live.thinking || state.live.tools.length)) {
    replaceLiveMessage();
    setRunning(true);
  } else {
    setRunning(Boolean(view.running || view.live || view.pendingApproval || view.pendingQuestion));
    if (view.running && !view.pendingApproval && !view.pendingQuestion) showTyping();
  }
  if (view.pendingApproval) showApprovalModal(view.pendingApproval);
  else if (view.pendingQuestion) showQuestionModal(view.pendingQuestion);
  else hidePromptModal();
  if (jump) jumpToLatest();
  else updateJumpButton();
}

function applyUsageUpdate(sessionId, event) {
  if (!sessionId) return;
  const view = viewOf(sessionId);
  const usage = {
    ...(view.meta?.usage || (sessionId === state.sessionId ? state.usage : {}) || {}),
  };
  const delta = event.usage || {};
  // Token values are per-call deltas; steps/turns/context are authoritative
  // session snapshots. Keep the accumulation on the owning session only.
  usage.input_tokens = (usage.input_tokens || 0) + (delta.input_tokens || 0);
  usage.output_tokens = (usage.output_tokens || 0) + (delta.output_tokens || 0);
  usage.cache_creation_tokens =
    (usage.cache_creation_tokens || 0) + (delta.cache_creation_input_tokens || 0);
  usage.cache_read_tokens =
    (usage.cache_read_tokens || 0) + (delta.cache_read_input_tokens || 0);
  if (delta.input_includes_cache !== undefined && delta.input_includes_cache !== null) {
    usage.input_includes_cache = delta.input_includes_cache;
  }
  if (event.steps !== undefined) usage.steps = event.steps;
  if (event.turns !== undefined) usage.turns = event.turns;
  if (event.context) usage.context = event.context;
  updateSessionMeta(sessionId, { usage });
  if (sessionId === state.sessionId) state.usage = usage;
}

function applyEventToView(sessionId, event) {
  if (!sessionId) return;
  const view = viewOf(sessionId);
  const type = event.type;
  if (type === "session_config_changed") {
    updateSessionMeta(sessionId, event);
    return;
  }
  if (type === "turn_start") {
    view.running = true;
    view.live = {
      text: "",
      tools: [],
      time: new Date().toISOString(),
      thinking: "",
      thinkingDone: false,
      el: null,
    };
    touchView(view);
    return;
  }
  if (type === "thinking_delta" && event.text) {
    const live = ensureCurrentAssistantStep(view);
    live.thinking += event.text;
    live.thinkingDone = false;
    view.running = true;
    touchView(view);
    return;
  }
  if (type === "message_delta" && event.text) {
    const live = ensureCurrentAssistantStep(view);
    live.text += event.text;
    if (live.thinking) live.thinkingDone = true;
    view.running = true;
    touchView(view);
    return;
  }
  if (type === "tool_call") {
    const live = ensureCurrentAssistantStep(view);
    if (live.thinking) live.thinkingDone = true;
    live.tools.push({
      _id: event.tool_call_id,
      name: event.tool_name,
      input: event.input || {},
      status: "running",
    });
    view.running = true;
    touchView(view);
    return;
  }
  if (type === "tool_execution_status") {
    const tool = view.live?.tools.find((item) => item._id === event.tool_call_id);
    if (tool) {
      tool.status = event.status || tool.status;
      tool.queued_behind = event.queued_behind || null;
      touchView(view);
    }
    return;
  }
  if (type === "tool_result") {
    const tool = view.live?.tools.find((item) => item._id === event.tool_call_id)
      || [...view.messages].reverse().flatMap((message) => message.tool_calls || []).find((item) => item._id === event.tool_call_id);
    if (tool) {
      tool.output = event.output;
      tool.status = event.is_error ? "error" : "success";
      if (event.is_error) tool.error = event.output;
      touchView(view);
    }
    return;
  }
  if (type === "tool_cancelled") {
    const tool = view.live?.tools.find((item) => item._id === event.tool_call_id)
      || [...view.messages].reverse().flatMap((message) => message.tool_calls || []).find((item) => item._id === event.tool_call_id);
    if (tool) {
      tool.status = "cancelled";
      tool.error = event.reason || "cancelled";
      touchView(view);
    }
    return;
  }
  if (type === "turn_end" && sessionId) {
    finishViewLive(view);
    view.running = false;
    view.pendingApproval = null;
    view.pendingQuestion = null;
    touchView(view);
    return;
  }
  if (type === "error") {
    finishViewLive(view);
    view.running = false;
    cacheSessionMessage(sessionId, "system", event.message || "turn error", new Date().toISOString(), [], {
      localOnly: true,
    });
    return;
  }
  if (type === "status_update") {
    if (event.status === "idle") view.running = false;
    else if (event.status) view.running = true;
    touchView(view);
    return;
  }
  if (type === "compact_completed") {
    if (event.messages) {
      view.messages = mergeTransientMessages(parseTranscript(event.messages), view.messages);
    }
    view.live = null;
    view.running = false;
    const note = event.error
      ? `Compact 失败: ${event.error}`
      : event.messages
        ? `上下文已压缩，保留 ${event.kept_user_message_count || 0} 条用户消息。`
        : "上下文压缩已完成。";
    cacheSessionMessage(sessionId, "system", note, new Date().toISOString(), [], { localOnly: true });
    return;
  }
  if (type === "usage_update") {
    applyUsageUpdate(sessionId, event);
    return;
  }
  if (type === "approval_requested") {
    view.pendingApproval = { ...(event.request || {}), session_id: event.request?.session_id || sessionId };
    touchView(view);
    return;
  }
  if (type === "question_asked") {
    view.pendingQuestion = { ...(event.question || {}), session_id: event.question?.session_id || sessionId };
    touchView(view);
    return;
  }
  if (type === "todo_updated") {
    view.todos = Array.isArray(event.items) ? event.items : [];
    // Only render the popover for the session the user is looking at —
    // background sessions updating their list must not hijack the view.
    if (sessionId === state.sessionId) renderTodos(view.todos);
    touchView(view);
    return;
  }
  if (type === "plan_file_updated") {
    view.plan = { path: event.path || "", content: event.content || "" };
    // Same guard as todo_updated: only the displayed session may repaint UI.
    if (sessionId === state.sessionId) {
      if (view.plan.path || view.plan.content) renderPlanPanel(view.plan);
      else document.getElementById("planPanel")?.classList.remove("open");
    }
    touchView(view);
    return;
  }
  if (type === "plan_mode_changed") {
    const meta = updateSessionMeta(sessionId, { plan_mode: Boolean(event.enabled) });
    if (sessionId === state.sessionId) applySessionMeta(meta);
    return;
  }
  if (type === "goal_updated") {
    view.goal = { goal: event.goal || null, budget: event.budget || null, change: event.change || "" };
    return;
  }
  if (type === "skill_activated") {
    view.systemNotes = view.systemNotes || [];
    view.systemNotes.push(`技能 ${event.skill_name || ""} 已激活`);
    return;
  }
  if (type === "mcp_auth_required") {
    const server = event.server_name || "MCP server";
    view.mcpAuthPending = server;
    const actions = [{ label: "知道了", onclick: () => hideNotice() }];
    if (event.authorization_url) {
      actions.unshift({
        label: "打开授权页",
        primary: true,
        onclick: () => {
          window.open(event.authorization_url, "_blank", "noopener");
          hideNotice();
        },
      });
    }
    if (sessionId === state.sessionId) showNotice(`MCP 服务器 ${server} 需要授权`, actions);
    return;
  }
  if (type === "subagent_spawned" || type === "subagent_started") {
    view.subagents = view.subagents || new Map();
    const key = event.subagent_id || event.agent_id || event.id || String(view.subagents.size);
    view.subagents.set(key, {
      id: key,
      description: event.description || event.subagent_name || "",
      status: "running",
      detail: "",
    });
    if (sessionId === state.sessionId) renderSubagents(view);
    return;
  }
  if (type === "subagent_child_event") {
    view.subagents = view.subagents || new Map();
    const key = event.subagent_id || event.agent_id || event.id;
    if (!key) return;
    const entry = view.subagents.get(key) || { id: key, description: "", status: "running", detail: "" };
    const child = event.event || {};
    if (child.type === "message_delta" && child.text) {
      entry.detail = (entry.detail + child.text).slice(-500);
    } else if (child.type === "status_update" && child.status) {
      entry.detail = child.status;
    }
    view.subagents.set(key, entry);
    if (sessionId === state.sessionId) renderSubagents(view);
    return;
  }
  if (type === "subagent_completed" || type === "subagent_failed") {
    view.subagents = view.subagents || new Map();
    const key = event.subagent_id || event.agent_id || event.id;
    if (key) {
      const entry = view.subagents.get(key) || { id: key, description: "", status: "running", detail: "" };
      entry.status = type === "subagent_completed" ? "done" : "failed";
      entry.detail = type === "subagent_failed" ? event.error || "failed" : entry.detail;
      view.subagents.set(key, entry);
      if (sessionId === state.sessionId) renderSubagents(view);
    }
    return;
  }
  if (type === "steer_input") {
    view.steerNotes = view.steerNotes || [];
    view.steerNotes.push(event.text || "");
    return;
  }
  if (type === "llm_retry") {
    view.retryNotes = view.retryNotes || [];
    view.retryNotes.push(`第 ${event.retry_number || "?"} 次重试${event.reason ? `：${event.reason}` : ""}`);
    return;
  }
  if (type === "btw_retry") {
    if (sessionId === state.sessionId) {
      showNotice(`后台问答重试（第 ${event.retry_number || "?"} 次）${event.reason ? `：${event.reason}` : ""}`, [{ label: "知道了", onclick: () => hideNotice() }]);
    }
    return;
  }
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
    const preview = s.preview && s.preview !== title ? s.preview : "";
    div.innerHTML = `
      <div class="session-row">
        <div class="session-title">
          ${expander}${(s.depth || 0) > 0 ? '<span class="session-fork-icon">↳</span>' : ""}${escapeHtml(title)}
        </div>
        <div class="session-item-actions">
          <button type="button" class="session-icon-btn" data-act="rename" title="重命名">${ICONS.edit}</button>
          <button type="button" class="session-icon-btn danger" data-act="delete" title="删除">${ICONS.trash}</button>
        </div>
      </div>
      ${path ? `<div class="session-path">${path}</div>` : ""}
      ${preview ? `<div class="session-meta">${escapeHtml(preview)}</div>` : ""}
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
    const titleEl = div.querySelector(".session-title");
    titleEl.ondblclick = (e) => {
      e.stopPropagation();
      startInlineRename(id, title, titleEl);
    };
    div.querySelectorAll(".session-item-actions button").forEach((btn) => {
      btn.onclick = (e) => {
        e.stopPropagation();
        if (btn.dataset.act === "rename") startInlineRename(id, title, titleEl);
        else if (btn.dataset.act === "delete") deleteSession(id, title);
      };
    });
    sessionsEl.appendChild(div);
  }
}

async function refreshSessions() {
  const refreshVersion = ++state.sessionsRefreshVersion;
  const body = await api("/api/v1/sessions");
  if (refreshVersion !== state.sessionsRefreshVersion) return;
  const live = body.sessions || [];
  const archived = body.transcript || [];
  const seen = new Set(live.map(sessionIdOf));
  state.sessions = live.concat(archived.filter((item) => !seen.has(sessionIdOf(item))));
  renderSessions();
}

async function selectSession(id, { jump = true } = {}) {
  hidePromptModal();
  if (state.sessionId && state.sessionId !== id) saveComposerState(state.sessionId);
  const selectionVersion = ++state.selectionVersion;
  state.sessionId = id;
  restoreComposerState(id);
  if (jump) state.followBottom = true;
  renderSessions();
  const cached = viewOf(id);
  if (cached.meta) {
    applySessionMeta(cached.meta);
  } else {
    const summary = state.sessions.find((session) => sessionIdOf(session) === id);
    applySessionMeta({ title: summary?.title || "" });
  }
  if (cached.messages.length || cached.live || cached.running) {
    paintSessionView(id, { jump });
  } else {
    logEl.innerHTML = "";
  }
  const expectedRevision = cached.revision;
  let sess;
  try {
    sess = await api(`/api/v1/sessions/${id}`);
  } catch (err) {
    if (state.sessionId !== id || state.selectionVersion !== selectionVersion) return;
    appendSessionMessage(id, "system", `加载会话失败: ${err.message || err}`, new Date().toISOString());
    return;
  }
  if (state.sessionId !== id || state.selectionVersion !== selectionVersion) return;
  const current = state.sessions.find((s) => sessionIdOf(s) === id);
  if (current) {
    if (sess.forked_from) {
      current.forked_from = sess.forked_from;
      current.parent_id = sess.forked_from;
    }
    if (sess.title) current.title = sess.title;
  }
  mergeServerSession(id, sess, { expectedRevision });
  applySessionMeta(viewOf(id).meta || sess);
  paintSessionView(id, { jump });
}

async function ensureSession() {
  if (state.sessionId) return state.sessionId;
  const selectionVersion = state.selectionVersion;
  const created = await api("/api/v1/sessions", {
    method: "POST",
    body: JSON.stringify({ workspace: state.cwd || "." }),
  });
  const createdId = created.session_id;
  const meta = updateSessionMeta(createdId, created);
  // Creating a session may take long enough for the user to select another
  // one. Keep the new session, but do not steal the selection back.
  if (!state.sessionId && state.selectionVersion === selectionVersion) {
    state.sessionId = createdId;
    restoreComposerState(createdId);
    applySessionMeta(meta);
  }
  await refreshSessions();
  return createdId;
}

async function forkCurrentSession(title) {
  const sessionId = state.sessionId;
  if (!sessionId) return;
  const selectionVersion = state.selectionVersion;
  try {
    const forked = await api(`/api/v1/sessions/${sessionId}/fork`, {
      method: "POST",
      body: JSON.stringify({ title: title || undefined }),
    });
    await refreshSessions();
    const forkedId = forked.session_id || forked.id;
    if (state.sessionId === sessionId && state.selectionVersion === selectionVersion) {
      await selectSession(forkedId);
      appendSessionMessage(forkedId, "system", "已 fork 新会话。", new Date().toISOString());
    }
  } catch (err) {
    appendSessionMessage(sessionId, "system", `fork 失败: ${err.message || err}`, new Date().toISOString());
  }
}

function startInlineRename(id, current, titleEl) {
  const input = document.createElement("input");
  input.className = "session-rename-input";
  input.value = current;
  titleEl.replaceWith(input);
  input.focus();
  input.select();
  let done = false;
  const finish = async (commit) => {
    if (done) return;
    done = true;
    const next = input.value.trim();
    if (commit && next && next !== current) {
      await renameSession(id, next);
    } else {
      renderSessions();
    }
  };
  input.onkeydown = (e) => {
    e.stopPropagation();
    if (e.key === "Enter") {
      e.preventDefault();
      finish(true);
    } else if (e.key === "Escape") {
      e.preventDefault();
      finish(false);
    }
  };
  input.onblur = () => finish(true);
  input.onclick = (e) => e.stopPropagation();
}

async function renameSession(id, title) {
  const next = String(title || "").trim();
  if (!id || !next) return;
  try {
    const sess = await api(`/api/v1/sessions/${id}`, {
      method: "PATCH",
      body: JSON.stringify({ title: next }),
    });
    const meta = updateSessionMeta(id, { ...sess, title: sess.title || next });
    const current = state.sessions.find((s) => sessionIdOf(s) === id);
    if (current) current.title = sess.title || next;
    if (id === state.sessionId) applySessionMeta(meta);
    renderSessions();
  } catch (err) {
    appendSessionMessage(id, "system", `改名失败: ${err.message || err}`, new Date().toISOString());
  }
}

async function deleteSession(id, title) {
  if (!id) return;
  const ok = await confirmAction({
    title: "删除会话",
    body: `永久删除「${title || id.slice(0, 8)}」及其历史？此操作不可撤销。`,
    ok: "删除",
    danger: true,
  });
  if (!ok) return;
  try {
    await api(`/api/v1/sessions/${id}`, { method: "DELETE" });
    state.sessions = state.sessions.filter((s) => sessionIdOf(s) !== id);
    if (state.sessionId === id) {
      state.sessionId = null;
      applySessionMeta({});
      const next = filterSessions()[0] || state.sessions[0];
      if (next) await selectSession(sessionIdOf(next));
      else {
        showWelcome();
        updateJumpButton();
      }
    }
    await refreshSessions();
  } catch (err) {
    appendSessionMessage(id, "system", `删除失败: ${err.message || err}`, new Date().toISOString());
  }
}

async function patchCurrentSession(body, sessionId = state.sessionId) {
  if (!sessionId) return null;
  const sess = await api(`/api/v1/sessions/${sessionId}`, {
    method: "PATCH",
    body: JSON.stringify(body),
  });
  const meta = updateSessionMeta(sessionId, sess);
  if (state.sessionId === sessionId) applySessionMeta(meta);
  const current = state.sessions.find((s) => sessionIdOf(s) === sessionId);
  if (current && sess.title) current.title = sess.title;
  renderSessions();
  return sess;
}

async function interruptCurrent() {
  const sessionId = state.sessionId;
  if (!sessionId) return;
  try {
    await api(`/api/v1/sessions/${sessionId}/interrupt`, { method: "POST", body: "{}" });
    appendSessionMessage(sessionId, "system", "已请求中断当前 turn。", new Date().toISOString());
  } catch (err) {
    appendSessionMessage(sessionId, "system", `中断失败: ${err.message || err}`, new Date().toISOString());
  }
}

async function compactCurrent(instruction) {
  const sessionId = state.sessionId;
  if (!sessionId) return;
  try {
    await api(`/api/v1/sessions/${sessionId}/compact`, {
      method: "POST",
      body: JSON.stringify({ instruction: instruction || undefined }),
    });
    appendSessionMessage(sessionId, "system", "正在压缩上下文…", new Date().toISOString());
  } catch (err) {
    appendSessionMessage(sessionId, "system", `Compact 失败: ${err.message || err}`, new Date().toISOString());
  }
}

async function undoCurrent(count) {
  const sessionId = state.sessionId;
  if (!sessionId) return;
  try {
    const result = await api(`/api/v1/sessions/${sessionId}/undo`, {
      method: "POST",
      body: JSON.stringify({ count: count || 1 }),
    });
    if (result.messages) {
      const view = viewOf(sessionId);
      view.messages = mergeTransientMessages(parseTranscript(result.messages), view.messages);
      view.live = null;
      view.running = false;
      touchView(view);
      if (state.sessionId === sessionId) paintSessionView(sessionId, { jump: state.followBottom });
    } else if (state.sessionId === sessionId) {
      await selectSession(sessionId, { jump: state.followBottom });
    } else {
      reconcileCompletedSession(sessionId, viewOf(sessionId).revision);
    }
    appendSessionMessage(sessionId, "system", `已撤销 ${result.undone || count || 1} 轮。`, new Date().toISOString());
  } catch (err) {
    appendSessionMessage(sessionId, "system", `Undo 失败: ${err.message || err}`, new Date().toISOString());
  }
}

async function archiveCurrent() {
  if (!state.sessionId) return;
  const id = state.sessionId;
  try {
    await api(`/api/v1/sessions/${id}/archive`, {
      method: "POST",
      body: JSON.stringify({ archived: true }),
    });
    state.sessions = state.sessions.filter((s) => sessionIdOf(s) !== id);
    const wasActive = state.sessionId === id;
    if (wasActive) {
      state.sessionId = null;
      applySessionMeta({});
    }
    await refreshSessions();
    if (!wasActive || state.sessionId) return;
    const next = filterSessions()[0] || state.sessions[0];
    if (next) await selectSession(sessionIdOf(next));
    else {
      showWelcome();
      updateJumpButton();
    }
  } catch (err) {
    appendSessionMessage(id, "system", `归档失败: ${err.message || err}`, new Date().toISOString());
  }
}

async function exportCurrent() {
  const sessionId = state.sessionId;
  if (!sessionId) return;
  const title = state.title;
  try {
    const body = await api(`/api/v1/sessions/${sessionId}/export`);
    const blob = new Blob([JSON.stringify(body, null, 2)], { type: "application/json" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = `${title || sessionId}.json`;
    a.click();
    URL.revokeObjectURL(url);
  } catch (err) {
    appendSessionMessage(sessionId, "system", `导出失败: ${err.message || err}`, new Date().toISOString());
  }
}

async function setPermissionMode(mode) {
  const sessionId = state.sessionId;
  if (!sessionId) return;
  try {
    await patchCurrentSession({ permission_mode: mode }, sessionId);
    appendSessionMessage(sessionId, "system", `权限模式：${mode}`, new Date().toISOString());
  } catch (err) {
    appendSessionMessage(sessionId, "system", `切换权限失败: ${err.message || err}`, new Date().toISOString());
  }
}

async function togglePlanMode(enabled) {
  const sessionId = state.sessionId;
  if (!sessionId) return;
  const next = typeof enabled === "boolean" ? enabled : !state.planMode;
  try {
    await patchCurrentSession({ plan_mode: next }, sessionId);
    appendSessionMessage(sessionId, "system", next ? "已进入 Plan 模式。" : "已退出 Plan 模式。", new Date().toISOString());
  } catch (err) {
    appendSessionMessage(sessionId, "system", `Plan 模式失败: ${err.message || err}`, new Date().toISOString());
  }
}

async function setModel(model, { quiet = false } = {}) {
  const sessionId = state.sessionId;
  if (!sessionId) return;
  const next = String(model || "").trim();
  if (!next || next === state.model) {
    renderModelSelect();
    return;
  }
  try {
    await patchCurrentSession({ model: next }, sessionId);
    if (!quiet) appendSessionMessage(sessionId, "system", `模型：${next}`, new Date().toISOString());
  } catch (err) {
    if (state.sessionId === sessionId) renderModelSelect();
    appendSessionMessage(sessionId, "system", `切换模型失败: ${err.message || err}`, new Date().toISOString());
  }
}

function modelId(item) {
  return item.alias || item.id || item.model || "";
}

function renderModelSelect() {
  if (!modelSelect) return;
  const names = [];
  for (const item of state.models) {
    const id = typeof item === "string" ? item : modelId(item);
    if (id && !names.includes(id)) names.push(id);
  }
  if (state.model && !names.includes(state.model)) names.unshift(state.model);
  const current = modelSelect.value;
  modelSelect.innerHTML = names.length
    ? names.map((name) => `<option value="${escapeHtml(name)}">${escapeHtml(name)}</option>`).join("")
    : '<option value="">模型</option>';
  modelSelect.value = state.model || current || names[0] || "";
}

async function loadModels() {
  try {
    const body = await api("/api/v1/models");
    state.models = body.models || [];
  } catch {
    state.models = [];
  }
  renderModelSelect();
}

function progressBar(used, max, width = 24) {
  if (!max || max <= 0) return "░".repeat(width);
  const filled = Math.min(width, Math.round((used / max) * width));
  return "█".repeat(filled) + "░".repeat(width - filled);
}

function cacheHitRatio(inTok, cacheC, cacheR) {
  if (!cacheR) return null;
  const total = cacheC > 0 ? inTok + cacheC + cacheR : inTok;
  if (!total) return null;
  return cacheR / total;
}

function formatUsage(usage) {
  if (!usage) return "暂无用量";
  const inTok = usage.input_tokens ?? usage.input ?? 0;
  const outTok = usage.output_tokens ?? usage.output ?? 0;
  const cacheC = usage.cache_creation_tokens ?? 0;
  const cacheR = usage.cache_read_tokens ?? 0;
  const steps = usage.steps ?? 0;
  const turns = usage.turns ?? 0;
  // Provider-native buckets: Anthropic input excludes cache, OpenAI/Gemini
  // input already includes it. Normalize so "total" is comparable across
  // providers (mirrors Rust TokenUsage::total_input_tokens semantics).
  const includesCache =
    usage.input_includes_cache !== undefined && usage.input_includes_cache !== null
      ? usage.input_includes_cache
      : cacheC > 0; // legacy heuristic: Anthropic always reports cache_creation
  const effectiveInput = includesCache ? inTok : inTok + cacheC + cacheR;
  const total = effectiveInput + outTok;
  const hit = cacheHitRatio(inTok, cacheC, cacheR);

  const lines = [
    `input ${inTok.toLocaleString()} · output ${outTok.toLocaleString()} · total ${total.toLocaleString()}`,
    `cache create ${cacheC.toLocaleString()} · cache read ${cacheR.toLocaleString()}${hit !== null ? ` · 命中 ${(hit * 100).toFixed(1)}%` : ""}`,
    `steps ${steps} · turns ${turns}`,
  ];

  const c = usage.context;
  if (c) {
    const used = (c.system ?? 0) + (c.conversation ?? 0) + (c.tools ?? 0) + (c.media ?? 0);
    const reserved = c.reserved_output ?? 0;
    // Without a server-provided window, scale bars against the used total so
    // relative proportions stay meaningful.
    const max = Math.max(used, reserved, 1);
    const rows = [
      ["System", c.system ?? 0],
      ["Conversation", c.conversation ?? 0],
      ["Tools", c.tools ?? 0],
      ["Media", c.media ?? 0],
      ["Reserved", reserved],
    ];
    lines.push("", `── 上下文分解 (${used.toLocaleString()} tokens${c.estimated ? ", 估算" : ""}) ──`);
    for (const [name, tokens] of rows) {
      lines.push(`${name.padEnd(14, " ")} ${progressBar(tokens, max)} ${tokens.toLocaleString()}`);
    }
  }
  return lines.join("\n");
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

function hidePromptModal() {
  if (!promptModal) return;
  promptModal.classList.remove("active");
  if (promptActions) promptActions.innerHTML = "";
  if (promptExtra) promptExtra.innerHTML = "";
  if (promptBody) promptBody.innerHTML = "";
}

function setPromptActions(actions = []) {
  promptActions.innerHTML = "";
  for (const action of actions) {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = action.danger ? "btn-danger" : action.primary ? "btn-primary" : "btn-secondary";
    btn.textContent = action.label;
    btn.onclick = action.onclick;
    promptActions.appendChild(btn);
  }
}

async function runPromptRequest(request) {
  for (const button of promptActions.querySelectorAll("button")) button.disabled = true;
  promptExtra.querySelector(".prompt-error")?.remove();
  try {
    await request();
    return true;
  } catch (err) {
    const error = document.createElement("div");
    error.className = "prompt-error";
    error.textContent = String(err.message || err);
    promptExtra.prepend(error);
    return false;
  } finally {
    if (promptModal.classList.contains("active")) {
      for (const button of promptActions.querySelectorAll("button")) button.disabled = false;
    }
  }
}

function showApprovalModal(request = {}) {
  if (!promptModal) return;
  const id = request.approval_id;
  const sessionId = request.session_id || state.sessionId;
  const display = request.tool_input_display;
  // Plan review (ExitPlanMode): dedicated panel with execute / revise / reject semantics.
  if (display && typeof display === "object" && display.kind === "plan_review") {
    showPlanReviewModal(request, display);
    return;
  }
  const tool = request.tool_name || "tool";
  const action = request.action ? ` · ${request.action}` : "";
  promptTitle.textContent = "需要批准工具";
  const input = display ?? request.input;
  const inputStr = typeof input === "string" ? input : JSON.stringify(input, null, 2);
  const isLong = inputStr && inputStr.length > 2000;
  const inputHtml = inputStr
    ? `<details class="prompt-input"${isLong ? "" : " open"}><summary>工具输入</summary><pre>${escapeHtml(inputStr)}</pre></details>`
    : "";
  promptBody.innerHTML = `<div class="prompt-kicker">${escapeHtml(tool)}${escapeHtml(action)}</div>
    <div>Agent 想要调用该工具，请确认是否允许。</div>${inputHtml}`;
  promptExtra.innerHTML = `<label class="prompt-scope"><input type="checkbox" id="approvalScopeSession"> 本会话内不再询问该工具</label>`;
  const post = (decision) => {
    const scope = document.getElementById("approvalScopeSession")?.checked ? "session" : null;
    return api(`/api/v1/approvals/${id}`, { method: "POST", body: JSON.stringify({ decision, scope }) });
  };
  const done = () => {
    const view = viewOf(sessionId);
    view.pendingApproval = null;
    hidePromptModal();
  };
  setPromptActions([
    {
      label: "拒绝",
      danger: true,
      onclick: async () => {
        if (await runPromptRequest(() => post("rejected"))) done();
      },
    },
    {
      label: "批准",
      primary: true,
      onclick: async () => {
        if (await runPromptRequest(() => post("approved"))) done();
      },
    },
  ]);
  promptModal.classList.add("active");
}

function showPlanReviewModal(request, display) {
  const id = request.approval_id;
  const sessionId = request.session_id || state.sessionId;
  promptTitle.textContent = "计划待确认";
  const plan = typeof display.plan === "string" ? display.plan : JSON.stringify(display.plan ?? "", null, 2);
  const pathHtml = display.path
    ? `<div class="prompt-kicker">计划文件：${escapeHtml(display.path)}</div>`
    : "";
  const options = Array.isArray(display.options) ? display.options : [];
  const optionsHtml = options.length
    ? `<div class="plan-options">${options
        .map(
          (o, i) => `
        <label class="plan-option"><input type="radio" name="plan-option" value="${escapeHtml(o.label || `方案 ${i + 1}`)}" ${i === 0 ? "checked" : ""}>
          <span><strong>${escapeHtml(o.label || `方案 ${i + 1}`)}</strong>${o.description ? `<small>${escapeHtml(o.description)}</small>` : ""}</span>
        </label>`,
        )
        .join("")}</div>`
    : "";
  promptBody.innerHTML = `${pathHtml}
    <div class="prompt-plan">${markdownToHtml(plan)}</div>
    ${options.length ? '<div class="prompt-plan-label">选择要执行的方案：</div>' : ""}${optionsHtml}`;
  promptExtra.innerHTML = `<textarea id="planFeedback" rows="3" placeholder="修改意见（选择「修改意见」时填写）…"></textarea>`;
  const post = (decision, feedback, selectedLabel) => {
    const body = { decision };
    if (feedback) body.feedback = feedback;
    if (selectedLabel) body.selected_label = selectedLabel;
    return api(`/api/v1/approvals/${id}`, { method: "POST", body: JSON.stringify(body) });
  };
  const done = (note) => {
    const view = viewOf(sessionId);
    view.pendingApproval = null;
    hidePromptModal();
    if (note) appendSessionMessage(sessionId, "system", note, new Date().toISOString());
  };
  const selectedLabel = () => promptBody.querySelector("input[name='plan-option']:checked")?.value || null;
  const feedbackValue = () => document.getElementById("planFeedback")?.value?.trim() || "";
  setPromptActions([
    {
      label: "拒绝并退出",
      danger: true,
      onclick: async () => {
        const feedback = feedbackValue();
        if (await runPromptRequest(() => post("rejected", feedback || "用户拒绝了该计划。", null))) {
          done("已拒绝计划。");
        }
      },
    },
    {
      label: "修改意见",
      onclick: async () => {
        const feedback = feedbackValue();
        if (!feedback) {
          document.getElementById("planFeedback")?.focus();
          return;
        }
        if (await runPromptRequest(() => post("approved", feedback, selectedLabel()))) {
          done("已提交修改意见，Agent 将按反馈修改计划。");
        }
      },
    },
    {
      label: "执行",
      primary: true,
      onclick: async () => {
        if (await runPromptRequest(() => post("approved", null, selectedLabel()))) {
          done("已确认计划，开始执行。");
        }
      },
    },
  ]);
  promptModal.classList.add("active");
}

function showQuestionModal(question = {}) {
  if (!promptModal) return;
  const qid = question.question_id;
  const sessionId = question.session_id || state.sessionId;
  promptTitle.textContent = "需要你的回答";
  promptBody.innerHTML = `<div>${escapeHtml(question.text || question.prompt || question.header || "请选择或输入回答")}</div>`;
  const options = question.options || [];
  const extra = [];
  if (options.length) {
    extra.push('<div class="option-list">');
    for (const option of options) {
      const inputType = question.allow_multiple ? "checkbox" : "radio";
      extra.push(`<label><input type="${inputType}" name="prompt-option" value="${escapeHtml(option.id)}"> ${escapeHtml(option.label)}</label>`);
    }
    extra.push("</div>");
  }
  if (question.allow_free_text) {
    extra.push('<textarea id="promptFreeText" placeholder="补充说明…"></textarea>');
  }
  promptExtra.innerHTML = extra.join("");
  const submit = async (cancelled = false) => {
    const selected = [...promptExtra.querySelectorAll("input[name='prompt-option']:checked")].map((el) => el.value);
    const freeText = document.getElementById("promptFreeText")?.value?.trim() || "";
    const submitted = await runPromptRequest(() => api(`/api/v1/questions/${qid}`, {
      method: "POST",
      body: JSON.stringify({
        selected_option_ids: selected,
        free_text: freeText || null,
        cancelled,
      }),
    }));
    if (!submitted) return;
    const view = viewOf(sessionId);
    view.pendingQuestion = null;
    hidePromptModal();
  };
  const actions = [
    { label: "跳过", onclick: () => submit(true) },
    { label: "提交", primary: true, onclick: () => submit(false) },
  ];
  setPromptActions(actions);
  promptModal.classList.add("active");
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
      const example = selected?.dataset.example;
      const alreadyComplete = e.key === "Enter"
        && example
        && !example.endsWith(" ")
        && promptEl.value.trim() === example.trim();
      if (alreadyComplete) {
        closeCmdPalette();
        // Continue below and submit the complete command on the first Enter.
      } else {
        if (example) insertCommand(example);
        return;
      }
    }
    if (e.key === "Escape") {
      closeCmdPalette();
      return;
    }
  }
  if (e.key === "Enter" && !e.shiftKey) {
    e.preventDefault();
    // Running + empty input + Enter = stop (mirrors the button morph).
    // Running + text = steer, which the submit handler sends normally.
    if (state.running && !promptEl.value.trim() && state.attachments.length === 0) {
      interruptCurrent();
      return;
    }
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
  btwSend.disabled = true;
  let sid = null;
  let typing = null;
  try {
    sid = await ensureSession();
    state.btwLive = { sessionId: sid, agentId: null, text: "", el: null };
    typing = document.createElement("div");
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
    const result = await api(`/api/v1/sessions/${sid}/btw`, {
      method: "POST",
      body: JSON.stringify({ text: `关于以下内容：\n${btwContextText}\n\n${text}` }),
    });
    if (result.agent_id && state.btwLive?.sessionId === sid) state.btwLive.agentId = result.agent_id;
    if (result.answer) {
      typing?.remove();
      appendBtwMessage("assistant", result.answer, new Date().toISOString());
      if (state.btwLive?.sessionId === sid) state.btwLive = null;
    }
  } catch (err) {
    typing?.remove();
    if (!sid || state.btwLive?.sessionId === sid) state.btwLive = null;
    appendBtwMessage("system", String(err.message || err), new Date().toISOString());
  } finally {
    // A streaming BTW request stays exclusive until its matching btw_end.
    if (!state.btwLive) btwSend.disabled = false;
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
  const sessionId = state.sessionId;
  if (!sessionId) {
    timelineList.innerHTML = '<div class="timeline-empty">当前没有会话。</div>';
    return;
  }
  try {
    const tl = await api(`/api/v1/sessions/${sessionId}/timeline`);
    if (state.sessionId !== sessionId) return;
    const events = tl.events || [];
    if (!events.length) {
      timelineList.innerHTML = '<div class="timeline-empty">当前会话还没有可回看的改动。</div>';
      return;
    }
    for (const item of events) {
      const el = document.createElement("div");
      el.className = "timeline-item " + (item.kind || "");
      const changes = (item.changes || []).map((c) => {
        const add = c.additions != null ? `<span class="change-add">+${c.additions}</span>` : "";
        const del = c.deletions != null ? `<span class="change-del">−${c.deletions}</span>` : "";
        const diff = c.diff ? renderDiffHtml(String(c.diff)) : "";
        return `<div class="timeline-change-row"><div class="timeline-file">${escapeHtml(String(c.path || ""))} ${add} ${del}</div>${diff}</div>`;
      }).join("");
      const stats = item.kind === "turn"
        ? `<div class="timeline-stats"><span class="change-add">+${item.additions || 0}</span> <span class="change-del">−${item.deletions || 0}</span></div>`
        : "";
      const restore = item.can_restore
        ? `<button type="button" class="btn-secondary timeline-restore" data-turn="${item.turn_index}">恢复到此状态</button>`
        : (item.kind === "turn" ? `<div class="timeline-desc">当前状态</div>` : "");
      const label = item.label || (item.kind === "turn" ? `第 ${(item.turn_index ?? 0) + 1} 轮` : "");
      el.innerHTML = `
        <div class="timeline-dot"></div>
        <div class="timeline-body">
          <div class="timeline-time">${escapeHtml([formatTime(item.time), label].filter(Boolean).join(" · "))}</div>
          <div class="timeline-title">${escapeHtml(item.title || item.kind || "")}</div>
          ${stats}
          ${changes ? `<div class="timeline-changes">${changes}</div>` : (item.kind === "turn" ? `<div class="timeline-desc">这一轮没有文件改动。</div>` : "")}
          ${restore}
        </div>
      `;
      const restoreBtn = el.querySelector(".timeline-restore");
      if (restoreBtn) {
        restoreBtn.onclick = () => restoreTurn(Number(restoreBtn.dataset.turn), label || item.title || `第 ${Number(restoreBtn.dataset.turn) + 1} 轮`);
      }
      timelineList.appendChild(el);
    }
  } catch (err) {
    if (state.sessionId !== sessionId) return;
    timelineList.innerHTML = `<div class="timeline-empty">${escapeHtml(String(err.message || err))}</div>`;
  }
}

async function restoreTurn(turnIndex, label) {
  const sessionId = state.sessionId;
  if (!sessionId || !Number.isFinite(turnIndex)) return;
  const ok = await confirmAction({
    title: "恢复代码状态",
    body: `撤销「${label}」之后的对话和文件改动，恢复到该轮结束时的代码？`,
    ok: "恢复",
    danger: true,
  });
  if (!ok) return;
  try {
    const result = await api(`/api/v1/sessions/${sessionId}/restore`, {
      method: "POST",
      body: JSON.stringify({ turn_index: turnIndex }),
    });
    if (result.messages) {
      const view = viewOf(sessionId);
      view.messages = mergeTransientMessages(parseTranscript(result.messages), view.messages);
      view.live = null;
      view.running = false;
      touchView(view);
      if (state.sessionId === sessionId) paintSessionView(sessionId, { jump: true });
    } else if (state.sessionId === sessionId) {
      await selectSession(sessionId);
    }
    if (state.sessionId === sessionId) await openTimelinePanel();
    appendSessionMessage(sessionId, "system", result.restored === false ? "已经是这一轮的状态。" : `已恢复到 ${label} 结束时的代码。`, new Date().toISOString());
  } catch (err) {
    appendSessionMessage(sessionId, "system", `恢复失败: ${err.message || err}`, new Date().toISOString());
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
  if (e.target.closest("#confirmModal") || e.target.closest("#promptModal") || e.target.closest("#sessionMenu")) return;
  document.body.classList.remove("timeline-open");
});

document.getElementById("newSession").onclick = async () => {
  const previousSessionId = state.sessionId;
  if (previousSessionId) saveComposerState(previousSessionId);
  try {
    state.sessionId = null;
    state.live = null;
    const id = await ensureSession();
    if (state.sessionId === id) await selectSession(id);
    appendSessionMessage(id, "system", "新会话已创建。", new Date().toISOString());
  } catch (err) {
    if (!state.sessionId && previousSessionId) {
      state.sessionId = previousSessionId;
      restoreComposerState(previousSessionId);
      applySessionMeta(viewOf(previousSessionId).meta || {});
      paintSessionView(previousSessionId, { jump: state.followBottom });
      appendSessionMessage(previousSessionId, "system", `创建会话失败: ${err.message || err}`, new Date().toISOString());
    } else if (!state.sessionId) {
      appendMessage("system", `创建会话失败: ${err.message || err}`, new Date().toISOString());
    }
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
  const sessionId = state.sessionId;
  const selectionVersion = state.selectionVersion;
  const added = [];
  for (const f of files) added.push(await fileToAttachment(f));
  if (sessionId && state.sessionId !== sessionId) {
    viewOf(sessionId).attachments.push(...added);
    return;
  }
  // A file picker opened on the welcome screen must not attach its result to
  // an unrelated session selected while FileReader was still working.
  if (!sessionId && state.selectionVersion !== selectionVersion) return;
  state.attachments.push(...added);
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
  if (name === "new" || name === "clear") {
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
  if (name === "timeline" || name === "tl") {
    await openTimelinePanel();
    return true;
  }
  if (name === "title" || name === "rename") {
    if (!args) {
      appendMessage("system", `当前标题：${state.title || "(未设置)"}。用法：/title <name>`, new Date().toISOString());
      return true;
    }
    await renameSession(state.sessionId || await ensureSession(), args);
    return true;
  }
  if (name === "delete") {
    const id = state.sessionId;
    if (id) await deleteSession(id, state.title);
    return true;
  }
  if (name === "yolo" || name === "yes") {
    await setPermissionMode(state.permissionMode === "yolo" ? "manual" : "yolo");
    return true;
  }
  if (name === "auto") {
    await setPermissionMode(state.permissionMode === "auto" ? "manual" : "auto");
    return true;
  }
  if (name === "permission") {
    const mode = args.toLowerCase();
    if (!["manual", "yolo", "auto"].includes(mode)) {
      appendMessage("system", `当前权限：${state.permissionMode}。用法：/permission manual|yolo|auto`, new Date().toISOString());
      return true;
    }
    await setPermissionMode(mode);
    return true;
  }
  if (name === "plan") {
    if (args === "clear") await togglePlanMode(false);
    else await togglePlanMode();
    return true;
  }
  if (name === "compact") {
    await compactCurrent(args);
    return true;
  }
  if (name === "undo") {
    const count = args ? Number.parseInt(args, 10) : 1;
    await undoCurrent(Number.isFinite(count) && count > 0 ? count : 1);
    return true;
  }
  if (name === "interrupt" || name === "stop") {
    await interruptCurrent();
    return true;
  }
  if (name === "model") {
    if (!args) {
      try {
        const body = await api("/api/v1/models");
        const names = (body.models || [])
          .map((item) => item.alias || item.id || item.model)
          .filter(Boolean);
        appendMessage(
          "system",
          `当前模型：${state.model || "(默认)"}\n可选：${names.slice(0, 20).join(", ") || "(无)"}`,
          new Date().toISOString(),
        );
      } catch (err) {
        appendMessage("system", `当前模型：${state.model || "(默认)"}。${err.message || err}`, new Date().toISOString());
      }
      return true;
    }
    await setModel(args);
    return true;
  }
  if (name === "status" || name === "info") {
    appendMessage(
      "system",
      [
        `session: ${state.sessionId || "(无)"}`,
        `title: ${state.title || "(未设置)"}`,
        `model: ${state.model || "(默认)"}`,
        `permission: ${state.permissionMode}`,
        `plan: ${state.planMode ? "on" : "off"}`,
        formatUsage(state.usage),
      ].join("\n"),
      new Date().toISOString(),
    );
    return true;
  }
  if (name === "usage") {
    appendMessage("system", formatUsage(state.usage), new Date().toISOString());
    return true;
  }
  if (name === "export") {
    await exportCurrent();
    return true;
  }
  return false;
}

document.getElementById("composer").onsubmit = async (e) => {
  e.preventDefault();
  const submittedSessionId = state.sessionId;
  const wasRunning = state.running;
  // While the agent loop is running the send button is in "stop" mode:
  // submit becomes an interrupt request instead of sending a new message.
  // (Typing text and pressing Enter still steers, see below.)
  if (state.running && !promptEl.value.trim() && state.attachments.length === 0) {
    interruptCurrent();
    return;
  }
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
  const sid = submittedSessionId || await ensureSession();
  const imagesForLog = images.map((img) => ({ media_type: img.media_type, data: img.data }));
  const view = viewOf(sid);
  view.running = true;
  touchView(view);
  const sentAt = new Date().toISOString();
  appendSessionMessage(sid, "user", payloadText || " ", sentAt, [], {
    images: imagesForLog,
    optimistic: true,
  });
  if (wasRunning) {
    // Turn is active: post_message delivers this as steer input.
    appendSessionMessage(sid, "system", "已发送，将作为引导输入注入当前运行中的任务。", new Date().toISOString());
  } else if (state.sessionId === sid) {
    showTyping();
  }
  if (state.sessionId === sid) setRunning(true);
  try {
    await api(`/api/v1/sessions/${sid}/messages`, {
      method: "POST",
      body: JSON.stringify({ text: payloadText, images }),
    });
  } catch (err) {
    view.running = false;
    touchView(view);
    if (state.sessionId === sid) {
      removeTyping();
      setRunning(false);
    }
    appendSessionMessage(sid, "system", String(err.message || err), new Date().toISOString());
  }
};

function toolsKey(tools) {
  return (tools || []).map((tool) => `${tool._id || ""}:${tool.status || ""}:${tool.output ? "1" : "0"}`).join("|");
}

function syncThinking(el, thinking, done) {
  const header = el.querySelector(".message-header");
  let details = el.querySelector("details.thinking");
  if (!thinking) {
    if (details) details.remove();
    return;
  }
  if (!details) {
    const wrap = document.createElement("div");
    wrap.innerHTML = renderThinking(thinking, { done });
    details = wrap.firstElementChild;
    header.after(details);
  }
  const body = details.querySelector(".thinking-body");
  if (body && body.textContent !== thinking) body.textContent = thinking;
  // Auto-scroll the thinking body so the latest content is visible when the
  // box is open (especially during live streaming where content overflows the
  // 280px max-height and new lines get pushed below the fold).
  if (body && details.hasAttribute("open")) {
    body.scrollTop = body.scrollHeight;
  }
  const label = details.querySelector(".thinking-label");
  if (label) label.textContent = done ? "思考过程" : "思考中…";
  const wasLive = details.classList.contains("is-live");
  details.classList.toggle("is-live", !done);
  if (!done && !wasLive) details.setAttribute("open", "");
  if (done && wasLive) details.removeAttribute("open");
}

function syncLiveMarkdown(el, text) {
  const md = el.querySelector(".bubble-md");
  if (!md) return;
  if (md.dataset.text === text) return;
  md.dataset.text = text;
  md.innerHTML = text ? markdownToHtml(text) : "";
  bindCodeCopies(md);
}

function scheduleLiveMarkdown(el, text) {
  state.pendingMd = { el, text };
  if (state.mdRaf) return;
  state.mdRaf = requestAnimationFrame(() => {
    state.mdRaf = 0;
    const job = state.pendingMd;
    state.pendingMd = null;
    if (job && job.el && job.el.parentNode) syncLiveMarkdown(job.el, job.text);
  });
}

function flushLiveMarkdown() {
  if (state.mdRaf) {
    cancelAnimationFrame(state.mdRaf);
    state.mdRaf = 0;
  }
  const job = state.pendingMd;
  state.pendingMd = null;
  if (job && job.el && job.el.parentNode) syncLiveMarkdown(job.el, job.text);
}

function syncLiveTools(el, tools) {
  const bubble = el.querySelector(".bubble");
  if (!bubble) return;
  const key = toolsKey(tools);
  if (bubble.dataset.toolsKey === key) return;
  const openIds = new Set([...bubble.querySelectorAll(".tool-call.open")].map((node) => node.dataset.id));
  const group = bubble.querySelector(".tool-group");
  const html = renderToolGroup(tools);
  if (!html) {
    if (group) group.remove();
    bubble.dataset.toolsKey = key;
    return;
  }
  const wrap = document.createElement("div");
  wrap.innerHTML = html;
  const next = wrap.firstElementChild;
  if (group) group.replaceWith(next);
  else bubble.appendChild(next);
  for (const id of openIds) {
    if (!id) continue;
    const safe = window.CSS && CSS.escape ? CSS.escape(id) : id;
    const tc = bubble.querySelector(`.tool-call[data-id="${safe}"]`);
    if (tc) {
      tc.classList.add("open");
      const row = tc.querySelector(".tool-row");
      if (row) row.setAttribute("aria-expanded", "true");
    }
  }
  bindToolToggles(el, tools);
  bubble.dataset.toolsKey = key;
}

function replaceLiveMessage() {
  if (!state.live) return;
  removeTyping();
  const live = state.live;
  if (!live.el || !live.el.parentNode) {
    live.el = renderMessage("assistant", live.text, live.time, live.tools, {
      thinking: live.thinking,
      thinkingDone: Boolean(live.thinkingDone),
      live: true,
    });
    logEl.appendChild(live.el);
  } else {
    live.el.classList.add("is-live");
    syncThinking(live.el, live.thinking, Boolean(live.thinkingDone));
    scheduleLiveMarkdown(live.el, live.text);
    syncLiveTools(live.el, live.tools);
    const bubble = live.el.querySelector(".bubble");
    if (bubble) bubble.style.display = (live.text || live.tools.length) ? "" : "none";
  }
  const view = state.sessionId ? viewOf(state.sessionId) : null;
  if (view) view.live = live;
  maybeScrollLog();
}

function finalizeLiveMessage() {
  if (!state.live) return;
  flushLiveMarkdown();
  if (state.live.el) {
    state.live.el.classList.remove("is-live");
    syncLiveMarkdown(state.live.el, state.live.text);
    syncThinking(state.live.el, state.live.thinking, true);
  }
}

function reconcileCompletedSession(sessionId, expectedRevision) {
  api(`/api/v1/sessions/${sessionId}`).then((sess) => {
    const changed = mergeServerSession(sessionId, sess, { expectedRevision });
    if (state.sessionId !== sessionId) return;
    applySessionMeta(viewOf(sessionId).meta || sess);
    if (!changed) return;
    const follow = state.followBottom;
    const scrollTop = logEl.scrollTop;
    paintSessionView(sessionId, { jump: follow });
    if (!follow) {
      logEl.scrollTop = scrollTop;
      updateJumpButton();
    }
  }).catch(() => {});
}

function handleAgentEvent(event) {
  const type = event.type;
  const sessionId = event.session_id;
  if (type === "plugins.changed") {
    const panel = document.getElementById("pluginsPanel");
    if (panel && panel.classList.contains("open")) refreshPluginsPanel().catch(() => {});
    return;
  }
  if (type === "session.created" || type === "session.forked" || type === "session.updated" || type === "session.archived" || type === "session.restored") {
    refreshSessions().catch(() => {});
    return;
  }
  if (type === "session.deleted") {
    refreshSessions().catch(() => {});
    if (event.session_id) delete state.views[event.session_id];
    if (event.session_id && event.session_id === state.sessionId) {
      state.sessionId = null;
      state.live = null;
      applySessionMeta({});
      showWelcome();
      updateJumpButton();
    }
    return;
  }
  if (sessionId) applyEventToView(sessionId, event);
  if (type === "turn_end" && sessionId) {
    const active = sessionId === state.sessionId;
    if (active) {
      setRunning(false);
      finalizeLiveMessage();
      state.live = null;
      refreshSessions().catch(() => {});
    }
    reconcileCompletedSession(sessionId, viewOf(sessionId).revision);
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
      setRunning(true);
      state.live = viewOf(sessionId).live;
    }
    return;
  }
  if (type === "thinking_delta" || type === "message_delta" || type === "tool_call" || type === "tool_execution_status" || type === "tool_result" || type === "tool_cancelled") {
    if (sessionId === state.sessionId) {
      state.live = viewOf(sessionId).live;
      if (state.live) replaceLiveMessage();
      setRunning(true);
    }
    return;
  }
  if (type === "error") {
    setRunning(false);
    removeTyping();
    state.live = null;
    appendMessage("system", event.message || "turn error", new Date().toISOString(), [], {
      cache: !sessionId,
    });
    return;
  }
  if (type === "status_update") {
    if (sessionId === state.sessionId) {
      const status = event.status;
      if (status === "idle") setRunning(false);
      else if (status) setRunning(true);
    }
    return;
  }
  if (type === "session_config_changed") {
    if (sessionId === state.sessionId) {
      applySessionMeta(viewOf(sessionId).meta || {});
    }
    return;
  }
  if (type === "compact_completed") {
    if (sessionId === state.sessionId) {
      setRunning(false);
      paintSessionView(sessionId, { jump: state.followBottom });
    }
    return;
  }
  if (type === "usage_update") {
    if (!sessionId && state.sessionId) applyUsageUpdate(state.sessionId, event);
    return;
  }
  if (type === "approval_requested") {
    if (sessionId === state.sessionId) {
      showApprovalModal({ ...(event.request || {}), session_id: event.request?.session_id || sessionId });
    }
    return;
  }
  if (type === "question_asked") {
    if (sessionId === state.sessionId) {
      showQuestionModal({ ...(event.question || {}), session_id: event.question?.session_id || sessionId });
    }
    return;
  }
  if (type === "btw_delta" && event.text) {
    if (state.btwLive?.sessionId && sessionId && state.btwLive.sessionId !== sessionId) return;
    if (state.btwLive?.agentId && event.agent_id && state.btwLive.agentId !== event.agent_id) return;
    const typing = btwLog.querySelector(".typing-msg");
    if (typing) typing.remove();
    if (!state.btwLive) state.btwLive = { sessionId, agentId: event.agent_id, text: "", el: null };
    state.btwLive.sessionId = sessionId || state.btwLive.sessionId;
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
    if (state.btwLive?.sessionId && sessionId && state.btwLive.sessionId !== sessionId) return;
    if (state.btwLive?.agentId && event.agent_id && state.btwLive.agentId !== event.agent_id) return;
    const typing = btwLog.querySelector(".typing-msg");
    if (typing) typing.remove();
    btwSend.disabled = false;
    state.btwLive = null;
    if (event.error) appendBtwMessage("system", event.error, new Date().toISOString());
  }
}

let wsLastSeq = 0;

function connectWs() {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  const qs = new URLSearchParams();
  if (token) qs.set("token", token);
  if (wsLastSeq > 0) qs.set("since", String(wsLastSeq));
  const ws = new WebSocket(`${proto}//${location.host}${base}/api/v1/ws?${qs.toString()}`);
  ws.onmessage = (ev) => {
    let frame;
    try {
      frame = JSON.parse(ev.data);
    } catch {
      /* ignore malformed frames */
      return;
    }
    if (!frame || typeof frame.type !== "string") return;
    if (frame.type === "hello") {
      if (wsLastSeq === 0 && typeof frame.latest_event_seq === "number") {
        wsLastSeq = frame.latest_event_seq;
      }
      return;
    }
    if (frame.type === "resync_required") {
      // Replay from the last event we actually processed. Advancing straight
      // to latest_event_seq here skips exactly the events the server reported
      // as lost.
      resyncFromHistory(wsLastSeq);
      return;
    }
    if (frame.type === "event" && frame.data) {
      // Enveloped variant (kept for compatibility).
      if (typeof frame.seq === "number") wsLastSeq = Math.max(wsLastSeq, frame.seq);
      handleAgentEvent(frame.data);
      return;
    }
    // Raw agent events carry event_seq directly.
    if (typeof frame.event_seq === "number") wsLastSeq = Math.max(wsLastSeq, frame.event_seq);
    handleAgentEvent(frame);
  };
  ws.onclose = () => setTimeout(connectWs, 1500);
}

async function resyncFromHistory(since = wsLastSeq) {
  try {
    const body = await api(`/api/v1/events?since=${since}`);
    const events = Array.isArray(body?.events) ? body.events : [];
    for (const event of events) {
      if (typeof event?.event_seq === "number") wsLastSeq = Math.max(wsLastSeq, event.event_seq);
      if (event && event.type && event.type !== "hello") handleAgentEvent(event);
    }
  } catch {
    /* offline; next reconnect retries */
  }
}

function openSidebar() { sidebar.classList.add("open"); }
function closeSidebar() { sidebar.classList.remove("open"); }
menuToggle.onclick = () => sidebar.classList.toggle("open");

function setSidebarCollapsed(collapsed) {
  document.body.classList.toggle("sidebar-collapsed", collapsed);
  localStorage.setItem("kkagent_sidebar_collapsed", collapsed ? "1" : "0");
}
const sidebarCollapse = document.getElementById("sidebarCollapse");
const sidebarExpand = document.getElementById("sidebarExpand");
if (sidebarCollapse) sidebarCollapse.onclick = () => setSidebarCollapsed(true);
if (sidebarExpand) sidebarExpand.onclick = () => setSidebarCollapsed(false);
if (localStorage.getItem("kkagent_sidebar_collapsed") === "1") setSidebarCollapsed(true);
document.addEventListener("click", (e) => {
  if (window.innerWidth > 760) return;
  if (!sidebar.contains(e.target) && e.target !== menuToggle && sidebar.classList.contains("open")) closeSidebar();
});

logEl.addEventListener("scroll", () => {
  if (state.suppressScroll) return;
  state.followBottom = isNearBottom();
  updateJumpButton();
});
if (jumpBottom) jumpBottom.onclick = () => jumpToLatest();

function closeSessionMenu() {
  sessionMenu?.classList.remove("active");
}

if (moreBtn && sessionMenu) {
  moreBtn.onclick = (e) => {
    e.stopPropagation();
    sessionMenu.classList.toggle("active");
  };
  sessionMenu.onclick = async (e) => {
    const btn = e.target.closest("button");
    if (!btn) return;
    closeSessionMenu();
    const action = btn.dataset.action;
    if (action === "rename") {
      sessionTitle?.focus();
      sessionTitle?.select();
    } else if (action === "fork") await forkCurrentSession();
    else if (action === "compact") await compactCurrent();
    else if (action === "undo") await undoCurrent(1);
    else if (action === "export") await exportCurrent();
    else if (action === "archive") await archiveCurrent();
    else if (action === "delete" && state.sessionId) await deleteSession(state.sessionId, state.title);
  };
}

if (sessionTitle) {
  sessionTitle.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      sessionTitle.blur();
    } else if (e.key === "Escape") {
      sessionTitle.value = state.title;
      sessionTitle.blur();
    }
  });
  sessionTitle.addEventListener("blur", async () => {
    const next = sessionTitle.value.trim();
    if (!state.sessionId) return;
    if (next && next !== state.title) await renameSession(state.sessionId, next);
    else sessionTitle.value = state.title;
  });
}

if (permissionModeEl) {
  permissionModeEl.onchange = async () => {
    const mode = permissionModeEl.value;
    if (mode && mode !== state.permissionMode) await setPermissionMode(mode);
  };
}
if (modelSelect) {
  modelSelect.onchange = async () => {
    const model = modelSelect.value;
    if (model) await setModel(model, { quiet: true });
  };
}
if (planBtn) planBtn.onclick = () => togglePlanMode();
const pluginsBtn = document.getElementById("pluginsBtn");
if (pluginsBtn) pluginsBtn.onclick = () => openPluginsPanel();

sendBtn.onclick = (e) => {
  if (!state.running) return;
  // Clicking the visible stop button always interrupts and keeps any draft in
  // the composer. Pressing Enter with text still submits steer input through
  // the keydown handler above.
  e.preventDefault();
  interruptCurrent();
};

const todoBtn = document.getElementById("todoBtn");
if (todoBtn) {
  todoBtn.onclick = () => {
    const el = ensureTodoPopover();
    if (el.classList.contains("open")) {
      el.classList.remove("open");
      return;
    }
    // Re-render the last known todos for the active session so reopening
    // shows fresh content instead of whatever was left in the DOM.
    const view = state.sessionId ? viewOf(state.sessionId) : null;
    renderTodos(view && Array.isArray(view.todos) ? view.todos : []);
    el.classList.add("open");
  };
}
// Stop is handled by the send button morphing into a stop button while
// running (see setRunning / composer submit handler).

if (confirmModal) {
  confirmModal.addEventListener("click", (e) => {
    if (e.target === confirmModal) confirmCancel?.click();
  });
}

document.addEventListener("click", (e) => {
  if (!sessionMenu?.classList.contains("active")) return;
  if (e.target.closest("#sessionMenu") || e.target.closest("#moreBtn")) return;
  closeSessionMenu();
});

document.addEventListener("keydown", (e) => {
  if (e.key !== "Escape") return;
  if (promptModal?.classList.contains("active")) return;
  if (confirmModal?.classList.contains("active")) {
    confirmCancel?.click();
    return;
  }
  if (sessionMenu?.classList.contains("active")) {
    closeSessionMenu();
    return;
  }
  if (cmdPalette.classList.contains("active")) return;
  if (state.running && document.activeElement !== promptEl) {
    interruptCurrent();
  } else if (state.running && promptEl.value.trim() === "") {
    interruptCurrent();
  }
});

async function boot() {
  try {
    const meta = await api("/api/v1/meta");
    const workspace = await api("/api/v1/workspaces").catch(() => ({}));
    state.cwd = workspace.cwd || "";
    statusEl.textContent = `已连接 · ${meta.name || "kkagent"}`;
    statusEl.className = "online";
    connectWs();
    const cfg = await api("/api/v1/config").catch(() => ({}));
    const defaultMode = String(cfg.default_permission_mode || "manual").toLowerCase();
    state.defaultPermissionMode = ["manual", "yolo", "auto"].includes(defaultMode) ? defaultMode : "manual";
    state.defaultModel = cfg.default_model || "";
    if (!state.sessionId) state.permissionMode = state.defaultPermissionMode;
    if (!state.sessionId) state.model = state.defaultModel;
    await loadModels();
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

// --- Plugins panel -----------------------------------------------------------

async function refreshPluginsPanel() {
  const body = document.querySelector("#pluginsPanel .side-panel-body");
  if (!body) return;
  body.innerHTML = `<div class="plugin-loading">加载中…</div>`;
  const [installed, marketplaces] = await Promise.all([
    api("/api/v1/plugins").catch(() => null),
    api("/api/v1/plugins/marketplaces").catch(() => null),
  ]);
  const installedList = Array.isArray(installed?.plugins) ? installed.plugins : Array.isArray(installed) ? installed : [];
  const mktList = Array.isArray(marketplaces?.marketplaces)
    ? marketplaces.marketplaces
    : Array.isArray(marketplaces) ? marketplaces : [];

  const rows = installedList
    .map((p) => {
      const id = p.name || p.id;
      const diag = (p.diagnostics || []).map((d) => d.message || d).join("; ");
      return `<li class="plugin-item${p.enabled ? "" : " disabled"}">
        <div class="plugin-info">
          <div class="plugin-name">${escapeHtml(p.display_name || id)} <small>v${escapeHtml(p.version || "?")}${p.enabled ? "" : " · 已停用"}</small></div>
          <div class="plugin-desc">${escapeHtml(p.description || "")}${diag ? `<div class="plugin-diag">${escapeHtml(diag)}</div>` : ""}</div>
        </div>
        <div class="plugin-actions">
          ${p.managed ? `<button type="button" class="btn-mini" data-plugin-update="${escapeHtml(id)}">更新</button>` : ""}
          <button type="button" class="btn-mini" data-plugin-toggle="${escapeHtml(id)}" data-enabled="${p.enabled ? "1" : ""}">${p.enabled ? "停用" : "启用"}</button>
          ${p.managed ? `<button type="button" class="btn-mini btn-mini-danger" data-plugin-remove="${escapeHtml(id)}">卸载</button>` : ""}
        </div>
      </li>`;
    })
    .join("");

  const mktRows = mktList
    .map(
      (m) => `<li class="plugin-mkt">
        <span>${escapeHtml(m.name || m.id)} <small>${escapeHtml(m.source || "")}</small></span>
        <button type="button" class="btn-mini btn-mini-danger" data-mkt-remove="${escapeHtml(m.id || m.name)}">移除</button>
      </li>`,
    )
    .join("");

  body.innerHTML = `
    <div class="plugin-section-title">已安装（${installedList.length}）</div>
    <ul class="plugin-list">${rows || '<li class="plugin-empty">暂无插件</li>'}</ul>
    <div class="plugin-section-title">市场源</div>
    <ul class="plugin-mkt-list">${mktRows || '<li class="plugin-empty">未配置</li>'}</ul>
    <div class="plugin-add-mkt">
      <input id="pluginMktInput" placeholder="市场源 URL 或 GitHub 仓库…" />
      <button type="button" class="btn-mini" id="pluginMktAdd">添加市场源</button>
    </div>
    <div class="plugin-section-title">浏览市场</div>
    <div class="plugin-browse-bar">
      <button type="button" class="btn-mini" id="pluginBrowseBtn">浏览插件</button>
    </div>
    <ul class="plugin-list" id="pluginMarketList"></ul>
    <div class="plugin-install-src">
      <input id="pluginInstallInput" placeholder="直接安装：git URL / 本地路径…" />
      <button type="button" class="btn-mini" id="pluginInstallBtn">安装</button>
    </div>`;

  body.querySelector("#pluginMktAdd").onclick = async () => {
    const input = body.querySelector("#pluginMktInput");
    const source = input.value.trim();
    if (!source) return;
    try {
      await api("/api/v1/plugins/marketplaces", { method: "POST", body: JSON.stringify({ source }) });
      await refreshPluginsPanel();
    } catch (err) {
      showNotice(`添加市场源失败: ${err.message || err}`, [{ label: "知道了", onclick: () => hideNotice() }]);
    }
  };
  body.querySelector("#pluginBrowseBtn").onclick = async () => {
    const list = body.querySelector("#pluginMarketList");
    list.innerHTML = `<div class="plugin-loading">加载中…</div>`;
    try {
      const mkt = await api("/api/v1/plugins/marketplace");
      const entries = Array.isArray(mkt?.plugins) ? mkt.plugins : [];
      list.innerHTML =
        entries
          .map(
            (e) => `<li class="plugin-item">
          <div class="plugin-info">
            <div class="plugin-name">${escapeHtml(e.display_name || e.id)} <small>v${escapeHtml(e.version || "?")}${e.tier ? ` · ${escapeHtml(e.tier)}` : ""}</small></div>
            <div class="plugin-desc">${escapeHtml(e.description || "")}</div>
          </div>
          <div class="plugin-actions">
            <button type="button" class="btn-mini" data-mkt-install="${escapeHtml(e.id)}">安装</button>
          </div>
        </li>`,
          )
          .join("") || `<li class="plugin-empty">市场为空</li>`;
      list.querySelectorAll("[data-mkt-install]").forEach((btn) => {
        btn.onclick = async () => {
          btn.disabled = true;
          btn.textContent = "安装中…";
          try {
            await api("/api/v1/plugins/install", {
              method: "POST",
              body: JSON.stringify({ source: btn.dataset.mktInstall, marketplace: "default" }),
            });
            await refreshPluginsPanel();
          } catch (err) {
            showNotice(`安装失败: ${err.message || err}`, [{ label: "知道了", onclick: () => hideNotice() }]);
            btn.disabled = false;
            btn.textContent = "安装";
          }
        };
      });
    } catch (err) {
      list.innerHTML = `<li class="plugin-empty">加载失败: ${escapeHtml(err.message || String(err))}</li>`;
    }
  };
  body.querySelector("#pluginInstallBtn").onclick = async () => {
    const input = body.querySelector("#pluginInstallInput");
    const source = input.value.trim();
    if (!source) return;
    const btn = body.querySelector("#pluginInstallBtn");
    btn.disabled = true;
    try {
      await api("/api/v1/plugins/install", { method: "POST", body: JSON.stringify({ source }) });
      await refreshPluginsPanel();
    } catch (err) {
      showNotice(`安装失败: ${err.message || err}`, [{ label: "知道了", onclick: () => hideNotice() }]);
      btn.disabled = false;
    }
  };
  body.querySelectorAll("[data-plugin-toggle]").forEach((btn) => {
    btn.onclick = async () => {
      const enabled = !btn.dataset.enabled;
      try {
        await api(`/api/v1/plugins/${encodeURIComponent(btn.dataset.pluginToggle)}`, {
          method: "PATCH",
          body: JSON.stringify({ enabled }),
        });
        await refreshPluginsPanel();
      } catch (err) {
        showNotice(`操作失败: ${err.message || err}`, [{ label: "知道了", onclick: () => hideNotice() }]);
      }
    };
  });
  body.querySelectorAll("[data-plugin-update]").forEach((btn) => {
    btn.onclick = async () => {
      btn.disabled = true;
      btn.textContent = "更新中…";
      try {
        await api(`/api/v1/plugins/${encodeURIComponent(btn.dataset.pluginUpdate)}`, { method: "POST" });
        await refreshPluginsPanel();
      } catch (err) {
        showNotice(`更新失败: ${err.message || err}`, [{ label: "知道了", onclick: () => hideNotice() }]);
        btn.disabled = false;
        btn.textContent = "更新";
      }
    };
  });
  body.querySelectorAll("[data-plugin-remove]").forEach((btn) => {
    btn.onclick = async () => {
      if (!confirm(`确定卸载插件 ${btn.dataset.pluginRemove}？`)) return;
      try {
        await api(`/api/v1/plugins/${encodeURIComponent(btn.dataset.pluginRemove)}`, { method: "DELETE" });
        await refreshPluginsPanel();
      } catch (err) {
        showNotice(`卸载失败: ${err.message || err}`, [{ label: "知道了", onclick: () => hideNotice() }]);
      }
    };
  });
  body.querySelectorAll("[data-mkt-remove]").forEach((btn) => {
    btn.onclick = async () => {
      try {
        await api(`/api/v1/plugins/marketplaces/${encodeURIComponent(btn.dataset.mktRemove)}`, { method: "DELETE" });
        await refreshPluginsPanel();
      } catch (err) {
        showNotice(`移除失败: ${err.message || err}`, [{ label: "知道了", onclick: () => hideNotice() }]);
      }
    };
  });
}

function openPluginsPanel() {
  const el = ensurePanel("pluginsPanel", "插件");
  el.classList.add("open");
  refreshPluginsPanel().catch((err) => {
    const body = el.querySelector(".side-panel-body");
    if (body) body.innerHTML = `<div class="plugin-empty">加载失败: ${escapeHtml(err.message || String(err))}</div>`;
  });
}

const params = new URLSearchParams(location.search);
const base = params.get("base") || "";

let token = params.get("token") || localStorage.getItem("kkagent_token") || "";
if (params.get("token")) {
  localStorage.setItem("kkagent_token", token);
  params.delete("token");
  const rest = params.toString();
  history.replaceState(null, "", location.pathname + (rest ? `?${rest}` : ""));
}

const state = { sessionId: null, sessions: [] };
const logEl = document.getElementById("log");
const sessionsEl = document.getElementById("sessions");
const statusEl = document.getElementById("status");
const promptEl = document.getElementById("prompt");
const sendBtn = document.getElementById("sendBtn");
const sidebar = document.getElementById("sidebar");
const menuToggle = document.getElementById("menuToggle");

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
  if (!res.ok) throw new Error(`${path} ${res.status}`);
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
  const d = new Date(iso);
  if (isNaN(d)) return "";
  return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

function markdownToHtml(text) {
  let html = escapeHtml(text);

  html = html.replace(/\n/g, "<br>");

  // Fenced code blocks
  html = html.replace(/```(\w*)\n?([\s\S]*?)```/g, (_, lang, code) => {
    const clean = code.replace(/<br>/g, "\n").replace(/\n$/, "");
    const highlighted = highlightCode(escapeHtml(clean));
    return `<pre><code class="lang-${lang || "text"}">${highlighted}</code></pre>`;
  });
  // Inline code
  html = html.replace(/`([^`]+)`/g, "<code>$1</code>");

  // Headings
  html = html.replace(/^######\s+(.+)$/gm, "<h6>$1</h6>");
  html = html.replace(/^#####\s+(.+)$/gm, "<h5>$1</h5>");
  html = html.replace(/^####\s+(.+)$/gm, "<h4>$1</h4>");
  html = html.replace(/^###\s+(.+)$/gm, "<h3>$1</h3>");
  html = html.replace(/^##\s+(.+)$/gm, "<h2>$1</h2>");
  html = html.replace(/^#\s+(.+)$/gm, "<h1>$1</h1>");

  // Bold / italic
  html = html.replace(/\*\*\*(.+?)\*\*\*/g, "<strong><em>$1</em></strong>");
  html = html.replace(/\*\*(.+?)\*\*/g, "<strong>$1</strong>");
  html = html.replace(/\*(.+?)\*/g, "<em>$1</em>");

  // Blockquotes
  html = html.replace(/^>\s+(.+)$/gm, "<blockquote>$1</blockquote>");

  // Lists
  html = html.replace(/^((?:\s*[-*]\s+.+?<br>)+)/gm, (_, list) => {
    const items = list.replace(/\s*[-*]\s+(.+?)(?:<br>|$)/g, "<li>$1</li>");
    return `<ul>${items}</ul>`;
  });
  html = html.replace(/^((?:\s*\d+\.\s+.+?<br>)+)/gm, (_, list) => {
    const items = list.replace(/\s*\d+\.\s+(.+?)(?:<br>|$)/g, "<li>$1</li>");
    return `<ol>${items}</ol>`;
  });

  // Links
  html = html.replace(/\[([^\]]+)\]\(([^)]+)\)/g, '<a href="$2" target="_blank" rel="noopener">$1</a>');

  // Tables (simple)
  html = html.replace(/((?:^[^\n|<br>]+\|[^\n|<br>]+\n)+)/gm, (block) => {
    const rows = block.split(/<br>|\n/).filter(Boolean);
    if (rows.length < 2) return block;
    let out = "<table>";
    rows.forEach((row, i) => {
      if (/^\s*:?-+:?(?:\s*\|\s*:?-+:?)\s*$/.test(row)) return;
      const cells = row.split("|").map((c) => c.trim());
      const tag = i === 0 ? "th" : "td";
      out += "<tr>" + cells.map((c) => `<${tag}>${c}</${tag}>`).join("") + "</tr>";
    });
    return out + "</table>";
  });

  // Paragraphs
  html = html.replace(/([^>])\n\n/g, "$1</p><p>");

  return html;
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

function renderMessage(role, content, timestamp) {
  const div = document.createElement("div");
  div.className = `message ${role}`;
  const roleLabel = role === "assistant" ? "kkagent" : role === "user" ? "你" : "system";
  const roleIcon = role === "assistant" ? "K" : role === "user" ? "U" : "!";
  const htmlContent = markdownToHtml(content);
  div.innerHTML = `
    <div class="avatar ${role}">${roleIcon}</div>
    <div class="message-body">
      <div class="message-header">
        <span class="role-name">${roleLabel}</span>
        <span class="timestamp">${formatTime(timestamp)}</span>
      </div>
      <div class="bubble">${htmlContent}</div>
    </div>
  `;
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

function appendMessage(role, content, timestamp) {
  removeTyping();
  logEl.appendChild(renderMessage(role, content, timestamp));
  logEl.scrollTop = logEl.scrollHeight;
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

function renderSessions() {
  sessionsEl.innerHTML = "";
  for (const s of state.sessions) {
    const id = s.session_id || s.id;
    const div = document.createElement("div");
    div.className = "session" + (id === state.sessionId ? " active" : "");
    const title = s.title || id.slice(0, 8);
    const updated = s.updated_at ? new Date(s.updated_at).toLocaleDateString() : "";
    const preview = s.preview || "";
    div.innerHTML = `
      <div class="session-title">${escapeHtml(title)}</div>
      <div class="session-meta">${updated ? updated + " · " : ""}${escapeHtml(preview)}</div>
    `;
    div.onclick = () => { closeSidebar(); selectSession(id); };
    sessionsEl.appendChild(div);
  }
}

async function refreshSessions() {
  const body = await api("/api/v1/sessions");
  state.sessions = body.sessions || [];
  renderSessions();
}

async function selectSession(id) {
  state.sessionId = id;
  renderSessions();
  logEl.innerHTML = "";
  const sess = await api(`/api/v1/sessions/${id}`);
  const messages = sess.messages || [];
  if (messages.length === 0) {
    showWelcome();
    return;
  }
  for (const m of messages) {
    const text = Array.isArray(m.content)
      ? m.content.map((c) => c.text || "").join("\n")
      : m.content || m.text || "";
    const trimmed = text.trim();
    if (!trimmed && m.role !== "system") continue;
    appendMessage(m.role || "assistant", trimmed || text, m.created_at);
  }
}

async function ensureSession() {
  if (state.sessionId) return state.sessionId;
  const created = await api("/api/v1/sessions", {
    method: "POST",
    body: JSON.stringify({ workspace: "." }),
  });
  state.sessionId = created.session_id;
  await refreshSessions();
  return state.sessionId;
}

document.getElementById("newSession").onclick = async () => {
  state.sessionId = null;
  await ensureSession();
  showWelcome();
  appendMessage("system", "新会话已创建。", new Date().toISOString());
};

document.getElementById("timelineBtn").onclick = async () => {
  if (!state.sessionId) return;
  try {
    const tl = await api(`/api/v1/sessions/${state.sessionId}/timeline`);
    appendMessage("system", "timeline/v1\n" + JSON.stringify(tl, null, 2), new Date().toISOString());
  } catch (err) {
    appendMessage("system", String(err), new Date().toISOString());
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

function adjustPromptHeight() {
  promptEl.style.height = "auto";
  const h = Math.min(promptEl.scrollHeight, 180);
  promptEl.style.height = h + "px";
}

promptEl.addEventListener("input", adjustPromptHeight);
promptEl.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !e.shiftKey) {
    e.preventDefault();
    document.getElementById("composer").dispatchEvent(new Event("submit"));
  }
});

document.getElementById("composer").onsubmit = async (e) => {
  e.preventDefault();
  const text = promptEl.value.trim();
  if (!text) return;
  promptEl.value = "";
  promptEl.style.height = "auto";
  const sid = await ensureSession();

  // Remove welcome screen on first message
  const welcome = logEl.querySelector(".welcome");
  if (welcome) welcome.remove();

  appendMessage("user", text, new Date().toISOString());
  const typing = showTyping();
  sendBtn.disabled = true;
  try {
    await api(`/api/v1/sessions/${sid}/messages`, {
      method: "POST",
      body: JSON.stringify({ text }),
    });
    removeTyping();
    appendMessage("system", "消息已发送。请等待响应或刷新会话查看结果。", new Date().toISOString());
  } catch (err) {
    removeTyping();
    appendMessage("system", String(err), new Date().toISOString());
  } finally {
    sendBtn.disabled = false;
  }
};

function openSidebar() { sidebar.classList.add("open"); }
function closeSidebar() { sidebar.classList.remove("open"); }
menuToggle.onclick = () => sidebar.classList.toggle("open");

document.addEventListener("click", (e) => {
  if (window.innerWidth > 760) return;
  if (!sidebar.contains(e.target) && e.target !== menuToggle && sidebar.classList.contains("open")) {
    closeSidebar();
  }
});

async function boot() {
  try {
    const meta = await api("/api/v1/meta");
    statusEl.textContent = `已连接 · ${meta.name || "kkagent"}`;
    statusEl.className = "online";
    await refreshSessions();
    if (state.sessions[0]) {
      await selectSession(state.sessions[0].session_id || state.sessions[0].id);
    } else {
      showWelcome();
    }
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

if (!token) {
  showTokenPrompt();
} else {
  boot();
}

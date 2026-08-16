const params = new URLSearchParams(location.search);
const token = params.get("token") || localStorage.getItem("kkagent_token") || "";
const base = params.get("base") || "";

async function api(path, opts = {}) {
  const headers = { ...(opts.headers || {}) };
  if (token) headers.Authorization = `Bearer ${token}`;
  if (opts.body && !headers["content-type"]) headers["content-type"] = "application/json";
  const res = await fetch(base + path, { ...opts, headers });
  if (!res.ok) throw new Error(`${path} ${res.status}`);
  return res.json();
}

const state = { sessionId: null, sessions: [] };
const logEl = document.getElementById("log");
const sessionsEl = document.getElementById("sessions");
const statusEl = document.getElementById("status");

function renderSessions() {
  sessionsEl.innerHTML = "";
  for (const s of state.sessions) {
    const id = s.session_id || s.id;
    const div = document.createElement("div");
    div.className = "session" + (id === state.sessionId ? " active" : "");
    div.textContent = s.title || id.slice(0, 8);
    div.onclick = () => selectSession(id);
    sessionsEl.appendChild(div);
  }
}

function appendMessage(role, text) {
  const div = document.createElement("div");
  div.className = "msg " + role;
  const highlighted = text
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/```diff([\s\S]*?)```/g, '<pre class="diff"><code>$1</code></pre>')
    .replace(/```([\s\S]*?)```/g, "<pre><code>$1</code></pre>");
  div.innerHTML = `<div class="role">${role}</div><div>${highlighted}</div>`;
  logEl.appendChild(div);
  logEl.scrollTop = logEl.scrollHeight;
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
  for (const m of sess.messages || []) {
    const text = Array.isArray(m.content)
      ? m.content.map((c) => c.text || "").join("\n")
      : m.content || m.text || JSON.stringify(m);
    appendMessage(m.role || "assistant", text);
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
  logEl.innerHTML = "";
  appendMessage("system", "New session created.");
};

document.getElementById("timelineBtn").onclick = async () => {
  if (!state.sessionId) return;
  try {
    const tl = await api(`/api/v1/sessions/${state.sessionId}/timeline`);
    appendMessage("system", "timeline/v1\n" + JSON.stringify(tl, null, 2));
  } catch (err) {
    appendMessage("system", String(err));
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
        div.textContent = `${hit.title || hit.session_id?.slice(0, 8) || "?"} · ${hit.preview || ""}`.slice(0, 80);
        div.onclick = () => selectSession(hit.session_id);
        sessionsEl.appendChild(div);
      }
    } catch (err) {
      statusEl.textContent = `search: ${err}`;
    }
  }, 200);
};

document.getElementById("composer").onsubmit = async (e) => {
  e.preventDefault();
  const input = document.getElementById("prompt");
  const text = input.value.trim();
  if (!text) return;
  input.value = "";
  const sid = await ensureSession();
  appendMessage("user", text);
  try {
    await api(`/api/v1/sessions/${sid}/messages`, {
      method: "POST",
      body: JSON.stringify({ text }),
    });
    appendMessage("system", "Message accepted. Watch WS/events or refresh session.");
  } catch (err) {
    appendMessage("system", String(err));
  }
};

(async function boot() {
  try {
    const meta = await api("/api/v1/meta");
    statusEl.textContent = `connected · ${meta.name || "kkagent"}`;
    await refreshSessions();
    if (state.sessions[0]) await selectSession(state.sessions[0].session_id || state.sessions[0].id);
  } catch (err) {
    statusEl.textContent = `offline: ${err}`;
  }
})();

/* /ui/usage — token usage dashboard. Same auth conventions as app.js. */
const params = new URLSearchParams(location.search);
const base = params.get("base") || "";

let token = params.get("token") || localStorage.getItem("kkagent_token") || "";
if (params.get("token")) {
  localStorage.setItem("kkagent_token", token);
  params.delete("token");
  const rest = params.toString();
  history.replaceState(null, "", location.pathname + (rest ? `?${rest}` : ""));
}

async function api(path) {
  const headers = {};
  if (token) headers.Authorization = `Bearer ${token}`;
  const res = await fetch(base + path, { headers });
  if (res.status === 401) {
    document.getElementById("authOverlay").style.display = "flex";
    throw new Error("unauthorized");
  }
  if (!res.ok) throw new Error(`${path} ${res.status}`);
  return res.json();
}

const LOCATION_LABELS = {
  main: "主对话",
  subagent: "子智能体",
  compaction: "压缩",
  judge: "目标裁判",
};
function locLabel(loc) {
  return LOCATION_LABELS[loc] || loc || "-";
}

function fmt(n) {
  if (n == null) return "-";
  if (n >= 1e9) return (n / 1e9).toFixed(2) + "B";
  if (n >= 1e6) return (n / 1e6).toFixed(2) + "M";
  if (n >= 1e3) return (n / 1e3).toFixed(1) + "k";
  return String(n);
}
function fmtFull(n) {
  return (n ?? 0).toLocaleString("en-US");
}

function dayTotals(rows) {
  let input = 0, output = 0, calls = 0, cacheRead = 0, cacheCreate = 0;
  for (const r of rows) {
    input += r.input_tokens || 0;
    output += r.output_tokens || 0;
    calls += r.calls || 0;
    cacheRead += r.cache_read_input_tokens || 0;
    cacheCreate += r.cache_creation_input_tokens || 0;
  }
  return { input, output, calls, cacheRead, cacheCreate };
}

function renderCards(rows) {
  const t = dayTotals(rows);
  const total = t.input + t.output;
  document.getElementById("cards").innerHTML = `
    <div class="card"><div class="label">总消耗</div>
      <div class="value">${fmt(total)}</div>
      <div class="sub">tokens（输入+输出）</div></div>
    <div class="card"><div class="label">输入 tokens</div>
      <div class="value">${fmt(t.input)}</div>
      <div class="sub">含缓存读 ${fmt(t.cacheRead)}</div></div>
    <div class="card"><div class="label">输出 tokens</div>
      <div class="value">${fmt(t.output)}</div>
      <div class="sub"></div></div>
    <div class="card"><div class="label">LLM 调用次数</div>
      <div class="value">${fmtFull(t.calls)}</div>
      <div class="sub"></div></div>`;
}

/* Stacked bar chart: per-day input+output, SVG, no dependencies. */
function renderChart(byDay) {
  const body = document.getElementById("chartBody");
  const legend = document.getElementById("chartLegend");
  if (!byDay.length) {
    body.innerHTML = `<div class="empty">所选时间范围内暂无数据</div>`;
    legend.innerHTML = "";
    return;
  }
  // Fill missing days so gaps are visible.
  const byDayMap = new Map(byDay.map((d) => [d.day, d]));
  const days = [];
  const end = new Date();
  const start = new Date(end);
  start.setDate(start.getDate() - (Number(document.getElementById("range").value) - 1));
  for (let d = new Date(start); d <= end; d.setDate(d.getDate() + 1)) {
    const key = `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
    days.push(key);
  }
  const series = days.map(
    (day) => byDayMap.get(day) || { day, input_tokens: 0, output_tokens: 0 }
  );
  const maxTotal = Math.max(
    1,
    ...series.map((d) => (d.input_tokens || 0) + (d.output_tokens || 0))
  );

  const W = 900, H = 240, padL = 56, padB = 26, padT = 10;
  const plotW = W - padL - 8, plotH = H - padB - padT;
  const bw = Math.max(4, Math.min(48, (plotW / series.length) * 0.6));
  const step = plotW / series.length;

  const gridLines = [];
  for (let i = 0; i <= 4; i++) {
    const y = padT + (plotH * i) / 4;
    const val = fmt(Math.round((maxTotal * (4 - i)) / 4));
    gridLines.push(
      `<line x1="${padL}" y1="${y}" x2="${W - 8}" y2="${y}" stroke="#2a2f3d" stroke-width="1"/>` +
      `<text x="${padL - 6}" y="${y + 4}" fill="#8a91a5" font-size="10" text-anchor="end">${val}</text>`
    );
  }

  const bars = series
    .map((d, i) => {
      const x = padL + step * i + (step - bw) / 2;
      const total = (d.input_tokens || 0) + (d.output_tokens || 0);
      const hIn = ((d.input_tokens || 0) / maxTotal) * plotH;
      const hOut = ((d.output_tokens || 0) / maxTotal) * plotH;
      const yIn = padT + plotH - hIn;
      const yOut = yIn - hOut;
      const label =
        d.day.slice(5).replace("-", "/") +
        (total ? `\n${fmt(total)} tok` : "");
      return `<g>
        <title>${d.day}: in ${fmtFull(d.input_tokens)} / out ${fmtFull(d.output_tokens)}</title>
        <rect x="${x}" y="${yIn}" width="${bw}" height="${hIn}" rx="2" fill="#5b8cff">
          <title>${label}</title></rect>
        <rect x="${x}" y="${yOut}" width="${bw}" height="${hOut}" rx="2" fill="#38c9b0">
          <title>${label}</title></rect>
        ${
          series.length <= 31 && (i % Math.ceil(series.length / 12) === 0)
            ? `<text x="${x + bw / 2}" y="${H - 8}" fill="#8a91a5" font-size="10" text-anchor="middle">${d.day.slice(5)}</text>`
            : ""
        }
      </g>`;
    })
    .join("");

  body.innerHTML = `<svg viewBox="0 0 ${W} ${H}" preserveAspectRatio="xMidYMid meet">
    ${gridLines.join("")}${bars}</svg>`;
  legend.innerHTML = `
    <span><i style="background:#5b8cff"></i>输入</span>
    <span><i style="background:#38c9b0"></i>输出</span>
    <span>峰值 ${fmt(maxTotal)} tokens/天</span>`;
}

function renderBreakdown(tableId, rows, nameKey) {
  const tbody = document.querySelector(`#${tableId} tbody`);
  if (!rows.length) {
    tbody.innerHTML = `<tr><td colspan="6" class="empty">暂无数据</td></tr>`;
    return;
  }
  const totals = rows.map((r) => (r.input_tokens || 0) + (r.output_tokens || 0));
  const max = Math.max(1, ...totals);
  tbody.innerHTML = rows
    .map((r, i) => {
      const name = nameKey === "location" ? locLabel(r.location) : r[nameKey];
      const total = totals[i];
      const pct = ((total / max) * 100).toFixed(1);
      return `<tr>
        <td>${escapeHtml(name)}</td>
        <td class="num">${fmtFull(r.calls)}</td>
        <td class="num">${fmtFull(r.input_tokens)}</td>
        <td class="num">${fmtFull(r.output_tokens)}</td>
        <td class="num">${fmtFull(total)}</td>
        <td class="bar-cell"><div class="bar" style="width:${pct}%"></div><span>${pct}%</span></td>
      </tr>`;
    })
    .join("");
}

function renderSessions(rows) {
  const panel = document.getElementById("sessionPanel");
  const tbody = document.querySelector("#bySession tbody");
  if (!rows.length) {
    panel.style.display = "none";
    return;
  }
  panel.style.display = "";
  tbody.innerHTML = rows
    .slice(0, 50)
    .map(
      (r) => `<tr>
        <td title="${escapeHtml(r.session_id)}">${escapeHtml(r.session_id.slice(0, 18))}${r.session_id.length > 18 ? "…" : ""}</td>
        <td class="num">${fmtFull(r.calls)}</td>
        <td class="num">${fmtFull(r.input_tokens)}</td>
        <td class="num">${fmtFull(r.output_tokens)}</td>
        <td class="num">${fmtFull((r.input_tokens || 0) + (r.output_tokens || 0))}</td>
      </tr>`
    )
    .join("");
}

function escapeHtml(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  }[c]));
}

async function load() {
  const days = document.getElementById("range").value;
  try {
    const data = await api(`/api/v1/usage/tokens?days=${days}`);
    renderCards(data.by_day || []);
    renderChart(data.by_day || []);
    renderBreakdown("byModel", data.by_model || [], "model");
    renderBreakdown("byLocation", data.by_location || [], "location");
    renderSessions(data.by_session || []);
    document.getElementById("updated").textContent =
      `更新于 ${new Date().toLocaleTimeString()}` +
      (data.available === false ? " · 历史未启用" : "");
  } catch (err) {
    if (err.message !== "unauthorized") {
      document.getElementById("chartBody").innerHTML =
        `<div class="error">加载失败：${escapeHtml(err.message)}</div>`;
    }
  }
}

document.getElementById("range").addEventListener("change", load);
document.getElementById("refresh").addEventListener("click", load);
document.getElementById("tokenInput").addEventListener("keydown", (e) => {
  if (e.key === "Enter") {
    token = e.target.value.trim();
    localStorage.setItem("kkagent_token", token);
    document.getElementById("authOverlay").style.display = "none";
    load();
  }
});
load();

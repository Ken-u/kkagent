#!/usr/bin/env python3
"""kkagent 工具使用统计:频率、种类、成功率、耗时、输出体积、往返与上下文成本。

数据源是会话日志 `<home>/sessions/*/wire.jsonl`(`<home>` 默认 `$KKAGENT_HOME`,
否则 `~/.kkagent`),只依赖 Python 3.9+ 标准库。会话所用模型取自
`<home>/transcripts.db` 的 `sessions` 表(不存在时跳过模型维度)。

口径说明(解读数字前请先读):
  * `turn_start` 事件是每一次 LLM 请求触发一次(不是每个用户回合),所以脚本把
    “同一个 turn_start 之后发出的工具调用”视为同一次 LLM 请求,用来统计并行度。
  * 子 agent 的内嵌事件(`subagent_child_event`)没有时间戳,耗时统计只覆盖主 agent;
    调用次数、失败原因对主 / 子 agent 都有效,分别用 `--scope` 选择。
  * `tool_result` 不带工具名,按 `tool_call_id` 回填。
  * 外部 ACP agent(`cursor/*`、`Read File` 之类带空格或斜杠的名字)默认剔除,
    用 `--include-external` 保留。
  * 失败 = `tool_result.is_error`。Bash 的非 0 退出码也算失败,这通常是命令自身的
    结果(测试不通过等),不代表工具缺陷;`errors` 一节会把已知的工具缺陷类别单独列出。
  * 上下文“滞留成本”是估算:输出字符数 / 3.5 ≈ token,乘以其后仍留在上下文里的
    LLM 请求数(上限 40,近似压缩前的存活期),只用于工具之间的相对比较。

用法:
  python3 scripts/tool_usage_report.py                 # 全部章节、全部历史
  python3 scripts/tool_usage_report.py --days 30       # 只看最近 30 天(相对日志中最新一条)
  python3 scripts/tool_usage_report.py --since 2026-10-01 --only overview,errors
  python3 scripts/tool_usage_report.py --only errors --scope main
章节: overview bash errors models trend read parallel context tasks
"""
from __future__ import annotations

import argparse
import collections
import datetime as dt
import glob
import json
import os
import re
import sqlite3
import sys
from dataclasses import dataclass, field
from typing import Any, Dict, Iterable, List, Optional, Tuple

SECTIONS = ["overview", "bash", "errors", "models", "trend", "read", "parallel", "context", "tasks"]
DAY_MS = 86_400_000


@dataclass
class Rec:
    sid: str
    sub: Optional[str]  # 子 agent id;主 agent 为 None
    tool: str
    input: Dict[str, Any]
    call_id: str
    t_call: Optional[int]
    turn: int  # 该流内第几次 LLM 请求
    seq: int
    t_run: Optional[int] = None
    t_res: Optional[int] = None
    is_error: Optional[bool] = None  # None = 没有收到结果
    out_len: Optional[int] = None
    out_head: str = ""

    @property
    def latency_s(self) -> Optional[float]:
        start = self.t_run or self.t_call
        if self.t_res and start:
            return (self.t_res - start) / 1000.0
        return None


@dataclass
class Stream:
    turn: int = 0
    seq: int = 0
    open: Dict[str, Rec] = field(default_factory=dict)


# ---------------------------------------------------------------- 解析


def _feed(ev: Dict[str, Any], st: Stream, out: List[Rec], sid: str, sub: Optional[str], ctxs: List[dict]) -> None:
    kind = ev.get("type")
    if kind == "turn_start":
        st.turn += 1
    elif kind == "usage_update":
        if sub is None and len(ctxs) < 3 and isinstance(ev.get("context"), dict):
            ctxs.append(ev["context"])
    elif kind == "tool_call":
        raw = ev.get("input")
        rec = Rec(
            sid=sid, sub=sub, tool=ev.get("tool_name") or "", input=raw if isinstance(raw, dict) else {},
            call_id=ev.get("tool_call_id") or "", t_call=ev.get("time"), turn=st.turn, seq=st.seq,
        )
        st.seq += 1
        st.open[rec.call_id] = rec
        out.append(rec)
    elif kind == "tool_execution_status":
        rec = st.open.get(ev.get("tool_call_id"))
        if rec is not None and ev.get("status") == "running" and rec.t_run is None:
            rec.t_run = ev.get("time")
    elif kind == "tool_result":
        rec = st.open.get(ev.get("tool_call_id"))
        if rec is not None:
            body = ev.get("output")
            if not isinstance(body, str):
                body = json.dumps(body, ensure_ascii=False) if body is not None else ""
            rec.t_res = ev.get("time")
            rec.is_error = bool(ev.get("is_error"))
            rec.out_len = len(body)
            rec.out_head = body[:400]


_MARKERS = ('"tool_call"', '"tool_result"', '"tool_execution_status"', '"usage_update"', '"turn_start"')


def parse_session(path: str) -> Tuple[str, List[Rec], List[dict]]:
    sid = os.path.basename(os.path.dirname(path))
    recs: List[Rec] = []
    ctxs: List[dict] = []
    main = Stream()
    subs: Dict[str, Stream] = collections.defaultdict(Stream)
    with open(path, "r", errors="replace") as fh:
        for line in fh:
            if not any(m in line for m in _MARKERS):
                continue
            try:
                ev = json.loads(line)
            except ValueError:
                continue
            if ev.get("type") == "subagent_child_event":
                sub_id = ev.get("subagent_id") or "?"
                _feed(ev.get("event") or {}, subs[sub_id], recs, sid, sub_id, ctxs)
            else:
                _feed(ev, main, recs, sid, None, ctxs)
    return sid, recs, ctxs


def load_models(home: str) -> Dict[str, str]:
    db = os.path.join(home, "transcripts.db")
    if not os.path.exists(db):
        return {}
    try:
        con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
        return {sid: (m or "?") for sid, m in con.execute("select session_id, model from sessions")}
    except sqlite3.Error:
        return {}


# ---------------------------------------------------------------- 工具函数


def pct(values: Iterable[float], p: float) -> float:
    xs = sorted(values)
    return xs[min(len(xs) - 1, int(p * (len(xs) - 1)))] if xs else 0.0


def month(ms: int) -> str:
    return dt.datetime.fromtimestamp(ms / 1000).strftime("%Y-%m")


def day(ms: int) -> str:
    return dt.datetime.fromtimestamp(ms / 1000).strftime("%Y-%m-%d")


def show(title: str, headers: List[str], rows: List[List[Any]], note: str = "") -> None:
    print(f"\n=== {title} ===")
    cells = [[str(c) for c in r] for r in rows]
    widths = [max(len(h), *(len(r[i]) for r in cells)) if cells else len(h) for i, h in enumerate(headers)]
    print("  ".join(h.ljust(widths[i]) if i == 0 else h.rjust(widths[i]) for i, h in enumerate(headers)))
    for r in cells:
        print("  ".join(c.ljust(widths[i]) if i == 0 else c.rjust(widths[i]) for i, c in enumerate(r)))
    if note:
        print(note)


def strip_cd(command: str) -> str:
    c = command.strip()
    while True:
        m = re.match(r"^\(?\s*cd\s+(\"[^\"]*\"|'[^']*'|\S+)\s*(&&|;)\s*", c)
        if not m:
            return c
        c = c[m.end():]


def bash_command(r: Rec) -> str:
    return strip_cd(str(r.input.get("command") or r.input.get("cmd") or ""))


_READ = {"cat", "head", "tail", "nl", "less", "more", "bat", "wc"}


def classify_bash(command: str) -> str:
    segs = [s.strip() for s in re.split(r"\|\||&&|;|\||\n", command) if s.strip()]
    words = [re.sub(r"^(\w+=\S+\s+)+", "", s).split()[0].split("/")[-1] for s in segs if re.sub(r"^(\w+=\S+\s+)+", "", s)]
    if not words:
        return "empty"
    w0 = words[0]
    if re.search(r"\b(sed\s+-i|perl\s+-pi|perl\s+-i)\b", command) or (
        w0 in ("cat", "printf", "echo", "tee") and re.search(r">\s*\S|tee\s", command)
    ):
        return "write via shell (sed -i / heredoc / echo >)"
    if w0 == "sed" and re.search(r"\bsed\s+-n\b", command):
        return "read via sed -n"
    if w0 in _READ:
        return "read via cat/head/tail/nl"
    if w0 in ("rg", "grep", "egrep", "fgrep", "ag"):
        return "search via rg/grep"
    if w0 in ("find", "fd", "ls", "tree"):
        return "list via find/ls"
    groups = {
        "git": ("git",),
        "cargo/rust": ("cargo", "rustc", "rustfmt"),
        "node/js": ("npm", "npx", "pnpm", "yarn", "node", "tsc", "vitest", "jest", "bun", "deno", "ts-node"),
        "python": ("python", "python3", "pytest", "pip", "pip3", "uv", "ruff"),
        "sleep": ("sleep",),
        "network/gh": ("curl", "wget", "gh", "ssh", "scp", "rsync"),
        "fs ops": ("mkdir", "rm", "mv", "cp", "touch", "chmod", "ln"),
    }
    for name, firsts in groups.items():
        if w0 in firsts:
            return name
    return "other"


READ_LIKE = {"read via sed -n", "read via cat/head/tail/nl", "search via rg/grep", "list via find/ls",
             "write via shell (sed -i / heredoc / echo >)"}


def is_read_only_call(r: Rec) -> bool:
    if r.tool in ("Read", "Grep", "Glob", "ReadMediaFile"):
        return True
    if r.tool != "Bash":
        return False
    c = bash_command(r)
    return classify_bash(c) in {"read via sed -n", "read via cat/head/tail/nl", "search via rg/grep", "list via find/ls"}


def is_external(name: str) -> bool:
    return (not name) or bool(re.search(r"[\s/]", name)) or name in ("tool", "grep")


# ---------------------------------------------------------------- 章节


def sec_overview(a, main: List[Rec], allr: List[Rec]) -> None:
    rows = []
    total = len(main)
    by = collections.defaultdict(list)
    for r in main:
        by[r.tool].append(r)
    for tool, lst in sorted(by.items(), key=lambda kv: -len(kv[1]))[: a.top]:
        errs = sum(1 for r in lst if r.is_error)
        lat = [x for x in (r.latency_s for r in lst) if x is not None]
        outs = [r.out_len or 0 for r in lst]
        rows.append([
            tool, len(lst), f"{len(lst) / total * 100:.1f}%", errs, f"{errs / len(lst) * 100:.1f}%",
            f"{pct(lat, .5):.2f}", f"{pct(lat, .9):.1f}", f"{sum(lat) / 60:.0f}",
            f"{sum(outs) / len(outs):.0f}", pct(outs, .9),
        ])
    show(f"overview — {total} calls ({a.scope_label})",
         ["tool", "calls", "share", "errors", "err%", "p50 s", "p90 s", "sum min", "avg out", "p90 out"], rows,
         "耗时仅主 agent 有效(子 agent 事件无时间戳);avg/p90 out 单位为字符。")


def sec_bash(a, main: List[Rec], allr: List[Rec]) -> None:
    bash = [r for r in main if r.tool == "Bash"]
    by = collections.defaultdict(list)
    for r in bash:
        by[classify_bash(bash_command(r))].append(r)
    rows = []
    for k, lst in sorted(by.items(), key=lambda kv: -len(kv[1])):
        errs = sum(1 for r in lst if r.is_error)
        outs = [r.out_len or 0 for r in lst]
        rows.append([k, len(lst), f"{len(lst) / len(bash) * 100:.1f}%", f"{errs / len(lst) * 100:.1f}%", f"{sum(outs) / len(outs):.0f}"])
    covered = sum(len(v) for k, v in by.items() if k in READ_LIKE)
    show(f"Bash 用途分类 — {len(bash)} calls", ["class", "calls", "share", "err%", "avg out"], rows,
         f"专用工具已覆盖的 shell 用法(读/搜/列/写):{covered} 次 = {covered / max(1, len(bash)) * 100:.1f}%")
    flags: collections.Counter = collections.Counter()
    piped = total = 0
    for r in bash:
        c = bash_command(r)
        if classify_bash(c) != "search via rg/grep":
            continue
        total += 1
        flags.update(re.findall(r"(?<!\S)(-[A-Za-z]{1,4}\b|--[a-z\-]+)", c))
        piped += 1 if re.search(r"\|\s*(head|tail|wc|sort|uniq|awk|sed|xargs|cut)\b", c) else 0
    print(f"shell 搜索 {total} 次,其中带管道后处理 {piped} 次;最常用参数:{flags.most_common(15)}")


_ERR_CATEGORIES: List[Tuple[str, str, str]] = [
    # (tool, 正则, 类别名) —— 已知的工具缺陷类别,用于看“是否还在发生”
    ("Bash", r"Blocked dangerous", "Bash: 危险命令拦截(rm -rf /tmp/... 误杀)"),
    ("Edit", r"modified externally|changed externally|deleted externally", "Edit: 文件过期硬拦截"),
    ("Write", r"modified externally|changed externally|deleted externally", "Write: 文件过期硬拦截"),
    ("Edit", r"old_string not found", "Edit: old_string 未找到"),
    ("Edit", r"matches in", "Edit: 多处匹配"),
    ("TodoList", r"is not one of", "TodoList: 枚举值被 schema 拦截"),
    ("TodoList", r"Unknown todo id|unknown task id", "TodoList: id 不存在"),
    ("TodoList", r"no id or matching|may omit id", "TodoList: 缺 id / 多条无 id"),
    ("TodoList", r"Multiple tasks are in_progress|No task is in_progress", "TodoList: in_progress 歧义"),
    ("TodoList", r"Duplicate update", "TodoList: 同一次调用重复更新同一 id"),
    ("Grep", r"unrecognized file type", "Grep: type 参数非法(含空字符串)"),
    ("Grep", r"not installed", "Grep: 系统没有 rg"),
    ("Grep", r"IO error|No such file", "Grep: 路径不存在"),
    ("Read", r"File not found", "Read: 文件不存在"),
    ("Web", r"blocked address", "Web: SSRF 防护拦截(含 fake-ip 198.18.x.x)"),
]


def _normalize_error(text: str) -> str:
    t = text.strip().replace("\n", " ")[:160]
    t = re.sub(r"[0-9a-f]{8,}", "<id>", t)
    t = re.sub(r"\d+", "N", t)
    t = re.sub(r"(/[\w.\-@+~]+)+", "<path>", t)
    return t[:90]


def sec_errors(a, main: List[Rec], allr: List[Rec]) -> None:
    errs = [r for r in allr if r.is_error]
    total = sum(1 for r in allr if r.is_error is not None)
    print(f"\n=== errors — {len(errs)} / {total} ({len(errs) / max(1, total) * 100:.2f}%) ({a.scope_label}) ===")
    cnt = collections.Counter(r.tool for r in errs)
    print("按工具:", ", ".join(f"{t} {n}" for t, n in cnt.most_common(10)))

    # 已知缺陷类别:按月计数 + 首次 / 最近一次出现,判断是否已修复
    months = sorted({month(r.t_call) for r in allr if r.t_call})
    rows = []
    for tool, pattern, name in _ERR_CATEGORIES:
        hits = [r for r in errs if r.tool == tool and re.search(pattern, r.out_head) and r.t_call]
        if not hits:
            continue
        by_m = collections.Counter(month(r.t_call) for r in hits)
        rows.append([name, len(hits), *[by_m.get(m, 0) for m in months], day(min(r.t_call for r in hits)), day(max(r.t_call for r in hits))])
    show("已知工具缺陷类别(按月 + 首次/最近一次出现)", ["category", "total", *months, "first", "last"], rows,
         "“last” 距今很久说明已修复或随模型变化而消失;判断优先级时请以最近一个月为准。")

    for tool, _ in cnt.most_common(a.error_tools):
        buckets: collections.Counter = collections.Counter()
        sample: Dict[str, str] = {}
        for r in errs:
            if r.tool != tool:
                continue
            k = _normalize_error(r.out_head)
            buckets[k] += 1
            sample.setdefault(k, r.out_head.strip().replace("\n", " ")[:150])
        print(f"\n--- {tool}: top error messages ---")
        for k, n in buckets.most_common(a.error_top):
            print(f"{n:>5}  {sample[k]}")


def sec_models(a, main: List[Rec], allr: List[Rec]) -> None:
    if not a.models:
        print("\n=== models — 没有 transcripts.db,跳过 ===")
        return
    by_model = collections.defaultdict(list)
    for r in main:
        by_model[a.models.get(r.sid, "?")].append(r)
    rows = []
    for m, lst in sorted(by_model.items(), key=lambda kv: -len(kv[1]))[: a.top]:
        c = collections.Counter(r.tool for r in lst)
        n = len(lst)
        errs = sum(1 for r in lst if r.is_error)
        rows.append([m[:34], n, *[f"{c[t] / n * 100:.1f}%" for t in ("Bash", "Read", "Edit", "Grep", "TodoList")], f"{errs / n * 100:.1f}%"])
    show("按模型的工具习惯(主 agent)", ["model", "calls", "Bash", "Read", "Edit", "Grep", "Todo", "err%"], rows)


def sec_trend(a, main: List[Rec], allr: List[Rec]) -> None:
    by = collections.defaultdict(list)
    for r in main:
        if r.t_call:
            by[month(r.t_call)].append(r)
    rows = []
    for m in sorted(by):
        lst = by[m]
        c = collections.Counter(r.tool for r in lst)
        n = len(lst)
        errs = sum(1 for r in lst if r.is_error)
        rows.append([m, n, *[f"{c[t] / n * 100:.1f}%" for t in ("Bash", "Read", "Edit", "Grep", "TodoList")], f"{errs / n * 100:.1f}%"])
    show("按月趋势(主 agent)", ["month", "calls", "Bash", "Read", "Edit", "Grep", "Todo", "err%"], rows)


def _by_session(recs: List[Rec]) -> Dict[str, List[Rec]]:
    out: Dict[str, List[Rec]] = collections.defaultdict(list)
    for r in recs:
        out[r.sid].append(r)
    for lst in out.values():
        lst.sort(key=lambda r: (r.turn, r.seq))
    return out


def sec_read(a, main: List[Rec], allr: List[Rec]) -> None:
    reads = full = big_full = rep = 0
    wasted = 0
    big = [r.out_len or 0 for r in main if r.tool == "Read" and (r.out_len or 0) > 20000]
    total_chars = sum(r.out_len or 0 for r in main if r.tool == "Read")
    for lst in _by_session(main).values():
        seen: Dict[Tuple[Any, ...], int] = {}
        for r in lst:
            path = r.input.get("path") or r.input.get("file_path")
            if r.tool in ("Edit", "Write") and path and not r.is_error:
                seen = {k: v for k, v in seen.items() if k[0] != path}  # 自己改过,重读是合理的
            if r.tool != "Read" or not path:
                continue
            reads += 1
            ranged = any(r.input.get(k) is not None for k in ("offset", "limit", "line_offset", "n_lines"))
            if not ranged:
                full += 1
                big_full += 1 if (r.out_len or 0) > 20000 else 0
            key = (path, r.input.get("offset") or r.input.get("line_offset"), r.input.get("limit") or r.input.get("n_lines"))
            if key in seen:
                rep += 1
                wasted += r.out_len or 0
            seen[key] = r.seq
    if not reads:
        return
    print(f"\n=== Read 行为 ({a.scope_label}) ===")
    print(f"Read {reads} 次;无范围整文件读取 {full} ({full / reads * 100:.0f}%),其中超 2 万字符 {big_full} 次")
    print(f"所有超 2 万字符的读取 {len(big)} 次,占 Read 输出量 {sum(big) / max(1, total_chars) * 100:.0f}%")
    print(f"完全相同的重复读取(同路径同范围,期间自己没改过){rep} 次 ({rep / reads * 100:.1f}%),约 {wasted / 1e6:.1f}M 字符 ≈ {wasted / 3.5 / 1e6:.1f}M token")


def sec_parallel(a, main: List[Rec], allr: List[Rec]) -> None:
    steps: Dict[Tuple[str, int], List[Rec]] = collections.defaultdict(list)
    for r in main:
        steps[(r.sid, r.turn)].append(r)
    n = len(steps)
    if not n:
        return
    size = collections.Counter(len(v) for v in steps.values())
    single_ro = sum(1 for v in steps.values() if len(v) == 1 and is_read_only_call(v[0]))
    only_todo = sum(1 for v in steps.values() if all(x.tool == "TodoList" for x in v))
    chains: List[int] = []
    for sid, lst in _by_session(main).items():
        run, prev = 0, None
        by_turn: Dict[int, List[Rec]] = collections.defaultdict(list)
        for r in lst:
            by_turn[r.turn].append(r)
        for t in sorted(by_turn):
            calls = by_turn[t]
            contiguous = prev is not None and t == prev + 1
            if len(calls) == 1 and is_read_only_call(calls[0]):
                run = run + 1 if contiguous and run else 1
            else:
                if run >= 2:
                    chains.append(run)
                run = 0
            prev = t
        if run >= 2:
            chains.append(run)
    print(f"\n=== 往返与并行度 — {n} 次带工具调用的 LLM 请求 ({a.scope_label}) ===")
    print("每次请求的调用数:", ", ".join(f"{k}个 {size[k]}({size[k] / n * 100:.1f}%)" for k in sorted(size)[:6]))
    print(f"只含 1 个只读调用的请求:{single_ro} ({single_ro / n * 100:.1f}%)")
    print(f"连续 >=2 次单个只读调用的链:{len(chains)} 条,平均长 {sum(chains) / max(1, len(chains)):.1f},全部合并上限可省 {sum(c - 1 for c in chains)} 次请求 ({sum(c - 1 for c in chains) / n * 100:.0f}%)")
    print(f"只含 TodoList 的请求:{only_todo} ({only_todo / n * 100:.1f}%)(纯状态更新往返)")
    by_month = collections.defaultdict(lambda: [0, 0])
    for r in main:
        if r.tool == "TodoList" and r.t_call:
            by_month[month(r.t_call)][0] += 1
            by_month[month(r.t_call)][1] += 1 if r.is_error else 0
    if by_month:
        print("TodoList 按月 (调用, 失败):", {m: tuple(v) for m, v in sorted(by_month.items())})


def sec_context(a, main: List[Rec], allr: List[Rec], ctx: Dict[str, List[dict]]) -> None:
    carry: Dict[str, float] = collections.defaultdict(float)
    raw: Dict[str, float] = collections.defaultdict(float)
    for lst in _by_session(main).values():
        last = max(r.turn for r in lst)
        for r in lst:
            tok = (r.out_len or 0) / 3.5
            carry[r.tool] += tok * min(40, last - r.turn)
            raw[r.tool] += tok
    total = sum(carry.values()) or 1.0
    rows = [[t, f"{raw[t] / 1e6:.2f}M", f"{v / 1e6:.1f}M", f"{v / total * 100:.1f}%"] for t, v in sorted(carry.items(), key=lambda kv: -kv[1])[:8]]
    show("上下文滞留成本(估算)", ["tool", "output tokens", "token x requests", "share"], rows)
    tools_tok = [c["tools"] for cs in ctx.values() for c in cs if isinstance(c, dict) and c.get("tools")]
    sys_tok = [c["system"] for cs in ctx.values() for c in cs if isinstance(c, dict) and c.get("system")]
    if tools_tok:
        print(f"每次请求固定带的工具定义 token(estimated):p50 {pct(tools_tok, .5):.0f} p90 {pct(tools_tok, .9):.0f} max {max(tools_tok)};system p50 {pct(sys_tok, .5):.0f}")


def sec_tasks(a, main: List[Rec], allr: List[Rec]) -> None:
    to = [r for r in main if r.tool == "TaskOutput"]
    if to:
        lat = [x for x in (r.latency_s for r in to) if x is not None]
        running = sum(1 for r in to if re.search(r"still running|running", r.out_head[:200], re.I))
        per_task = collections.Counter((r.sid, r.input.get("task_id")) for r in to)
        print(f"\n=== 后台任务 / 长命令 ({a.scope_label}) ===")
        print(f"TaskOutput {len(to)} 次,累计阻塞 {sum(lat) / 60:.0f} 分钟,p90 {pct(lat, .9):.0f}s;返回仍在运行 {running} 次;被轮询 >=5 次的任务 {sum(1 for v in per_task.values() if v >= 5)} 个")
    bash = [r for r in main if r.tool == "Bash" and r.latency_s is not None]
    if bash:
        lat = [r.latency_s for r in bash]
        slow = [x for x in lat if x > 10]
        print(f"Bash p50 {pct(lat, .5):.2f}s p90 {pct(lat, .9):.1f}s p99 {pct(lat, .99):.0f}s;超 10s 的 {len(slow)} 次({len(slow) / len(lat) * 100:.1f}%)占 Bash 总耗时 {sum(slow) / max(1, sum(lat)) * 100:.0f}%")
    sleeps = sum(1 for r in main if r.tool == "Bash" and re.search(r"(^|[;&|]\s*)sleep\s+\d", str(r.input.get("command") or "")))
    bg = sum(1 for r in main if r.tool == "Bash" and r.input.get("run_in_background"))
    print(f"Bash 里含 sleep 的调用 {sleeps} 次;run_in_background {bg} 次")


# ---------------------------------------------------------------- 入口


def main_cli() -> int:
    ap = argparse.ArgumentParser(description="kkagent 工具使用统计", formatter_class=argparse.RawDescriptionHelpFormatter, epilog=__doc__)
    ap.add_argument("--home", default=os.environ.get("KKAGENT_HOME") or os.path.expanduser("~/.kkagent"))
    ap.add_argument("--days", type=int, help="只看最近 N 天(相对日志中最新一条调用)")
    ap.add_argument("--since", help="只看该日期(含)之后,格式 YYYY-MM-DD")
    ap.add_argument("--scope", choices=["main", "all"], default="main",
                    help="errors 章节统计范围:main=只主 agent(默认),all=含子 agent")
    ap.add_argument("--only", default=",".join(SECTIONS), help="逗号分隔的章节")
    ap.add_argument("--top", type=int, default=15, help="表格最多行数")
    ap.add_argument("--error-tools", type=int, default=5, help="errors 章节展开几个工具的 top 报错")
    ap.add_argument("--error-top", type=int, default=8, help="每个工具展开几条报错")
    ap.add_argument("--include-external", action="store_true", help="保留外部 ACP agent 的工具事件")
    a = ap.parse_args()

    only = [s.strip() for s in a.only.split(",") if s.strip()]
    bad = [s for s in only if s not in SECTIONS]
    if bad:
        ap.error(f"未知章节 {bad};可选 {SECTIONS}")

    files = sorted(glob.glob(os.path.join(a.home, "sessions", "*", "wire.jsonl")))
    if not files:
        print(f"没有找到会话日志:{a.home}/sessions/*/wire.jsonl", file=sys.stderr)
        return 1

    allr: List[Rec] = []
    ctx: Dict[str, List[dict]] = {}
    for f in files:
        try:
            sid, recs, ctxs = parse_session(f)
        except OSError as e:
            print(f"跳过 {f}: {e}", file=sys.stderr)
            continue
        allr.extend(recs)
        ctx[sid] = ctxs
    if not a.include_external:
        allr = [r for r in allr if not is_external(r.tool)]
    stamped = [r.t_call for r in allr if r.t_call]
    if not stamped:
        print("日志中没有带时间戳的工具调用", file=sys.stderr)
        return 1
    newest = max(stamped)
    cutoff = None
    if a.days:
        cutoff = newest - a.days * DAY_MS
    if a.since:
        cutoff = max(cutoff or 0, int(dt.datetime.strptime(a.since, "%Y-%m-%d").timestamp() * 1000))
    if cutoff:
        allr = [r for r in allr if r.t_call and r.t_call >= cutoff]
    a.models = load_models(a.home)
    mains = [r for r in allr if r.sub is None and r.is_error is not None]
    scoped = allr if a.scope == "all" else [r for r in allr if r.sub is None]
    a.scope_label = "主 agent" if a.scope == "main" else "主+子 agent"
    window = f"{day(min(r.t_call for r in allr if r.t_call))} → {day(max(r.t_call for r in allr if r.t_call))}"
    print(f"home={a.home}  会话 {len(files)} 个  调用 {len(allr)}(主 {sum(1 for r in allr if r.sub is None)} / 子 {sum(1 for r in allr if r.sub)})  窗口 {window}")

    dispatch = {
        "overview": lambda: sec_overview(a, mains, allr),
        "bash": lambda: sec_bash(a, mains, allr),
        "errors": lambda: sec_errors(a, mains, scoped),
        "models": lambda: sec_models(a, mains, allr),
        "trend": lambda: sec_trend(a, mains, allr),
        "read": lambda: sec_read(a, mains, allr),
        "parallel": lambda: sec_parallel(a, mains, allr),
        "context": lambda: sec_context(a, mains, allr, ctx),
        "tasks": lambda: sec_tasks(a, mains, allr),
    }
    for name in only:
        dispatch[name]()
    return 0


if __name__ == "__main__":
    sys.exit(main_cli())

#!/usr/bin/env python3
"""Summarize lash runtime/UI/Lashlang perf reports, guard reports, and dhat heap profiles.

Usage:
  perfreport.py REPORT.json                  # human summary
  perfreport.py REPORT.json --diff BASELINE  # before/after comparison
  perfreport.py PROFILE.dhat.json --top 25   # top heap consumers
  perfreport.py GUARD.json                    # perf guard summary
"""

from __future__ import annotations

import argparse
import json
import sys
from collections.abc import Callable
from pathlib import Path
from typing import Any


def fmt_bytes(n: float | int) -> str:
    n = float(n)
    sign = "-" if n < 0 else ""
    n = abs(n)
    for unit in ("B", "KiB", "MiB", "GiB"):
        if n < 1024 or unit == "GiB":
            if unit == "B":
                return f"{sign}{int(n)}{unit}"
            return f"{sign}{n:.2f}{unit}"
        n /= 1024
    return f"{sign}{n:.2f}GiB"


def fmt_kb(n: float | int | None) -> str:
    if n is None:
        return "n/a"
    return fmt_bytes(float(n) * 1024)


def fmt_ms(n: float | None) -> str:
    if n is None:
        return "n/a"
    return f"{n:.2f}ms"


def fmt_ns(n: float | None) -> str:
    if n is None:
        return "n/a"
    return f"{n:.1f}ns"


def fmt_metric_percentiles(metric: dict[str, Any]) -> str:
    parts = [f"median={fmt_ms(metric.get('median'))}"]
    if metric.get("p95") is not None:
        parts.append(f"p95={fmt_ms(metric['p95'])}")
    return "  ".join(parts)


# Guard classes the runtime report marks as advisory: they are measured and
# reported, but never fail the gate. The class is decided by the harness
# (`RuntimePerfGuardClass`), not re-derived from metric spelling here.
ADVISORY_GUARD_CLASSES = frozenset({"duration"})


def is_advisory_guard(result: dict[str, Any]) -> bool:
    return result.get("class") in ADVISORY_GUARD_CLASSES


def guard_failure_status(result: dict[str, Any]) -> str:
    return "ADVISORY" if is_advisory_guard(result) else "FAIL"


def fmt_stack_profile(profile: Any) -> str | None:
    if not isinstance(profile, dict):
        return None
    measured = profile.get("measured_stack_bytes")
    budget = profile.get("stack_budget_bytes")
    source = profile.get("measured_stack_source") or "unknown"
    within = profile.get("within_stack_budget")
    parts = []
    if isinstance(measured, int | float):
        parts.append(f"stack={fmt_bytes(measured)}")
    if isinstance(budget, int | float):
        parts.append(f"budget={fmt_bytes(budget)}")
    if parts:
        parts.append(f"source={source}")
    if isinstance(within, bool):
        parts.append(f"within_budget={'yes' if within else 'no'}")
    return "  " + "  ".join(parts) if parts else None


def summarize_runtime(report: dict[str, Any]) -> str:
    lines: list[str] = []
    lines.append(
        f"# runtime-perf report  ({report.get('created_at', '?')[:19]}, {report.get('runs')} runs × {report.get('chat_turns')} turns)"
    )
    lines.append(f"version: {report.get('version', '?')}  scenarios: {', '.join(report.get('scenarios', []))}")
    if report.get("dhat_out"):
        lines.append(f"dhat profile: {report['dhat_out']}")
    stack_text = fmt_stack_profile(report.get("stack_profile"))
    if stack_text:
        lines.append(stack_text.strip())
    lines.append("")

    budget_results = report.get("budget_results", [])
    timing_guards = [
        result
        for result in budget_results
        if str(result.get("metric", "")).startswith("phase:")
        and str(result.get("metric", "")).endswith(":duration_ms")
    ]
    if timing_guards:
        lines.append("phase timing guards (median across runs, advisory — not gating):")
        for result in timing_guards:
            status = "pass" if result.get("passed") else guard_failure_status(result)
            lines.append(
                f"  {result.get('scenario', '?'):28s} "
                f"{result.get('metric', '?')[6:-12]:44s} "
                f"actual={fmt_ms(result.get('actual')):>9s}  "
                f"budget={fmt_ms(result.get('budget')):>9s}  {status}"
            )
        lines.append("")

    advisories = [
        result
        for result in budget_results
        if not result.get("passed") and is_advisory_guard(result)
    ]
    if advisories:
        lines.append("advisory wall-clock exceedances (reported, do not fail the gate):")
        for result in advisories:
            lines.append(
                f"  {result.get('scenario', '?'):28s} "
                f"{result.get('metric', '?'):50s} "
                f"actual={fmt_ms(result.get('actual')):>9s}  "
                f"budget={fmt_ms(result.get('budget')):>9s}"
            )
        lines.append("")

    for s in report.get("summary", []):
        scenario = s["scenario"]
        lines.append(f"## scenario: {scenario}  ({s['runs']} runs × {s['chat_turns']} turns)")
        lines.append("")
        lines.append("phase totals (median/p95 across runs):")
        lines.append(
            f"  build_runtime         {fmt_metric_percentiles(s['build_runtime_ms'])}  "
            f"alloc={fmt_bytes(s['build_runtime_alloc_bytes']['median']):>10s}  "
            f"live={fmt_bytes(s['build_runtime_live_bytes']['median']):>10s}"
        )
        lines.append(
            f"  seed_state            {fmt_metric_percentiles(s['seed_state_ms'])}  "
            f"alloc={fmt_bytes(s['seed_state_alloc_bytes']['median']):>10s}  "
            f"live={fmt_bytes(s['seed_state_live_bytes']['median']):>10s}"
        )
        lines.append(
            f"  run_turn (sum)        {fmt_metric_percentiles(s['run_turn_ms'])}  "
            f"alloc={fmt_bytes(s['run_turn_alloc_bytes']['median']):>10s}  "
            f"live={fmt_bytes(s['run_turn_live_bytes']['median']):>10s}"
        )
        lines.append(
            f"  await_background      {fmt_metric_percentiles(s['await_background_work_ms'])}  "
            f"alloc={fmt_bytes(s['await_background_work_alloc_bytes']['median']):>10s}  "
            f"live={fmt_bytes(s['await_background_work_live_bytes']['median']):>10s}"
        )
        lines.append(
            f"  export_state          {fmt_metric_percentiles(s['export_state_ms'])}  "
            f"alloc={fmt_bytes(s['export_state_alloc_bytes']['median']):>10s}  "
            f"live={fmt_bytes(s['export_state_live_bytes']['median']):>10s}"
        )
        lines.append(
            f"  TOTAL                 {fmt_metric_percentiles(s['total_ms'])}  "
            f"alloc={fmt_bytes(s['total_alloc_bytes']['median']):>10s}  "
            f"live={fmt_bytes(s['total_live_bytes']['median']):>10s}"
        )
        lines.append("")

        rss = s.get("rss_after_export_kb")
        if rss is not None:
            growth = s.get("rss_growth_kb")
            hwm = s.get("hwm_growth_kb")
            lines.append(
                f"memory: rss_after={fmt_kb(rss['median'])}  "
                f"rss_growth={fmt_kb(growth['median']) if growth else 'n/a'}  "
                f"hwm_growth={fmt_kb(hwm['median']) if hwm else 'n/a'}"
            )
            lines.append("")

        sample = (
            f"session_nodes={s['sample_session_nodes']}  "
            f"active_path_messages={s['sample_active_path_messages']}"
        )
        lines.append(sample)
        lines.append("")

        ps = s.get("phase_summary", {})
        if ps:
            lines.append("hot phases by median/p95 duration:")
            ranked = sorted(ps.items(), key=lambda kv: -kv[1]["duration_ms"]["median"])
            for name, m in ranked:
                samples = m.get("samples", {}).get("median")
                sample_text = f"n={int(samples):>4d}" if samples is not None else "n=   ?"
                lines.append(
                    f"  {name:30s}  {sample_text}  dur={fmt_metric_percentiles(m['duration_ms'])}  "
                    f"alloc={fmt_bytes(m['alloc_bytes']['median']):>10s}  "
                    f"live={fmt_bytes(m['live_bytes']['median']):>10s}"
                )
            lines.append("")
            lines.append("hot phases by median allocation bytes:")
            ranked = sorted(ps.items(), key=lambda kv: -kv[1]["alloc_bytes"]["median"])
            for name, m in ranked:
                samples = m.get("samples", {}).get("median")
                sample_text = f"n={int(samples):>4d}" if samples is not None else "n=   ?"
                lines.append(
                    f"  {name:30s}  {sample_text}  alloc={fmt_bytes(m['alloc_bytes']['median']):>10s}  "
                    f"dur={fmt_ms(m['duration_ms']['median']):>9s}"
                )
            lines.append("")

        first = s.get("first_turn") or {}
        last = s.get("last_turn") or {}
        steady = s.get("steady_state_turn") or {}
        if first and last:
            d_total = last["total_ms"]["median"] - first["total_ms"]["median"]
            d_alloc = last["total_alloc_bytes"]["median"] - first["total_alloc_bytes"]["median"]
            d_live = last["total_live_bytes"]["median"] - first["total_live_bytes"]["median"]
            lines.append("turn growth (last vs first, median across runs):")
            lines.append(
                f"  total_ms     first={fmt_ms(first['total_ms']['median']):>9s}  "
                f"steady={fmt_ms(steady.get('total_ms', {}).get('median')) if steady else 'n/a':>9s}  "
                f"last={fmt_ms(last['total_ms']['median']):>9s}  Δ={d_total:+.2f}ms"
            )
            lines.append(
                f"  alloc_bytes  first={fmt_bytes(first['total_alloc_bytes']['median']):>10s}  "
                f"steady={fmt_bytes(steady.get('total_alloc_bytes', {}).get('median', 0)):>10s}  "
                f"last={fmt_bytes(last['total_alloc_bytes']['median']):>10s}  Δ={fmt_bytes(d_alloc)}"
            )
            lines.append(
                f"  live_bytes   first={fmt_bytes(first['total_live_bytes']['median']):>10s}  "
                f"steady={fmt_bytes(steady.get('total_live_bytes', {}).get('median', 0)):>10s}  "
                f"last={fmt_bytes(last['total_live_bytes']['median']):>10s}  Δ={fmt_bytes(d_live)}"
            )
            lines.append("")

        # per-turn drift across the run (signal of O(n) or O(n²) regressions)
        results_for_scenario = [r for r in report.get("results", []) if r["scenario"] == scenario]
        if results_for_scenario:
            r0 = results_for_scenario[0]
            turns = r0.get("turns", [])
            if len(turns) >= 4:
                lines.append("per-turn drift (run #1, picks signs of O(n) growth):")
                lines.append(
                    f"  {'turn':>4}  {'run_ms':>8}  {'alloc':>11}  {'live_Δ':>11}  {'rss_kb':>8}"
                )
                for t in turns:
                    a = t["allocations"]["total"]
                    rss = t["memory"].get("rss_after_await_kb")
                    lines.append(
                        f"  {t['turn_index']:>4}  "
                        f"{t['run_turn_ms']:>6.2f}ms  "
                        f"{fmt_bytes(a['bytes_allocated']):>11s}  "
                        f"{fmt_bytes(a['net_live_bytes']):>11s}  "
                        f"{rss if rss is not None else 'n/a':>8}"
                    )
                lines.append("")

        lines.append("")
    return "\n".join(lines)


def summarize_runtime_stack(report: dict[str, Any]) -> str:
    lines = ["# runtime stack envelope", ""]
    first_success = report.get("first_success_stack_bytes", {})
    budgets = report.get("stack_budgets", {})
    budget_results = report.get("budget_results", {})
    for scenario in report.get("scenarios", sorted(first_success)):
        measured = first_success.get(scenario)
        budget = budgets.get(scenario)
        passed = budget_results.get(scenario)
        status = "pass" if passed is True else "FAIL" if passed is False else "not checked"
        measured_text = fmt_bytes(measured) if isinstance(measured, int | float) else "n/a"
        budget_text = fmt_bytes(budget) if isinstance(budget, int | float) else "n/a"
        lines.append(
            f"  {scenario:28s} minimum_passing={measured_text:>9s}  "
            f"budget={budget_text:>9s}  {status}"
        )
    failures = sum(1 for sample in report.get("samples", []) if sample.get("status") != "ok")
    unaccounted = sum(
        1
        for sample in report.get("samples", [])
        if sample.get("status") == "ok" and not sample.get("stack_accounted", False)
    )
    lines.append("")
    lines.append(f"failed_or_timeout_samples={failures}  unaccounted_stack_samples={unaccounted}")
    return "\n".join(lines)


def summarize_lashlang(report: dict[str, Any]) -> str:
    params = report.get("parameters", {})
    lines: list[str] = []
    lines.append(
        f"# lashlang-perf report  ({report.get('created_at', '?')[:19]}, "
        f"{params.get('iterations', '?')} iterations)"
    )
    git = report.get("git", {})
    dirty = "dirty" if git.get("dirty") else "clean"
    lines.append(
        f"build={report.get('build_mode', '?')}  git={git.get('sha', '?')} ({dirty})  "
        f"scenarios={', '.join(params.get('scenarios', []))}  "
        f"modes={', '.join(params.get('modes', []))}"
    )
    stack_text = fmt_stack_profile(report.get("stack_profile"))
    if stack_text:
        lines.append(stack_text.strip())
    lines.append("")

    budget_results = report.get("budget_results", [])
    if budget_results:
        lines.append("## guard maxima")
        for metric in (
            "allocated_bytes_per_iter",
            "allocations_per_iter",
            "instructions_per_iter",
        ):
            matching = [
                result
                for result in budget_results
                if result.get("metric") == metric
                and isinstance(result.get("actual"), int | float)
            ]
            if not matching:
                continue
            worst = max(matching, key=lambda result: float(result["actual"]))
            status = "pass" if all(result.get("passed") for result in matching) else "FAIL"
            lines.append(
                f"  {metric:28s} max={worst.get('actual')}  "
                f"budget={worst.get('budget')}  {status}"
            )
        lines.append("")

    perf_results = report.get("perf_results", [])
    if perf_results:
        lines.append("## perf sweep")
        for row in sorted(perf_results, key=lambda r: (r.get("mode_arg", ""), r.get("scenario_arg", ""))):
            mode = row.get("mode_arg", "?")
            scenario = row.get("scenario_arg", "?")
            lines.append(
                f"  {mode:22s} {scenario:20s} "
                f"avg={fmt_ns(row.get('ns_per_iter')):>10s}  "
                f"allocs={row.get('allocations_per_iter', 0):>8}  "
                f"bytes={fmt_bytes(row.get('allocated_bytes_per_iter', 0)):>10s}"
            )
            if "phase_total_ns_per_iter" in row:
                lines.append(
                    f"    {'phase_total':12s} "
                    f"avg={fmt_ns(row.get('phase_total_ns_per_iter')):>10s}  "
                    f"allocs={row.get('phase_total_allocations_per_iter', 0):>8}  "
                    f"bytes={fmt_bytes(row.get('phase_total_allocated_bytes_per_iter', 0)):>10s}"
                )
                for phase in ("parse", "link", "compile", "execute"):
                    lines.append(
                        f"    {phase:12s} "
                        f"avg={fmt_ns(row.get(f'{phase}_ns_per_iter')):>10s}  "
                        f"allocs={row.get(f'{phase}_allocations_per_iter', 0):>8}  "
                        f"bytes={fmt_bytes(row.get(f'{phase}_allocated_bytes_per_iter', 0)):>10s}"
                    )
            if (
                "process_cache_hits" in row
                or "program_cache_hits" in row
                or "linked_cache_hits" in row
                or "artifact_bytes" in row
            ):
                extras = []
                if "artifact_bytes" in row:
                    extras.append(f"artifact={fmt_bytes(row.get('artifact_bytes', 0))}")
                if "process_cache_hits" in row:
                    extras.append(
                        "process_cache="
                        f"{row.get('process_cache_hits', 0)}h/"
                        f"{row.get('process_cache_misses', 0)}m/"
                        f"{row.get('process_cache_evictions', 0)}e"
                    )
                if "program_cache_hits" in row:
                    extras.append(
                        "program_cache="
                        f"{row.get('program_cache_hits', 0)}h/"
                        f"{row.get('program_cache_misses', 0)}m/"
                        f"{row.get('program_cache_evictions', 0)}e"
                    )
                if "linked_cache_hits" in row:
                    extras.append(
                        "linked_cache="
                        f"{row.get('linked_cache_hits', 0)}h/"
                        f"{row.get('linked_cache_misses', 0)}m/"
                        f"{row.get('linked_cache_evictions', 0)}e"
                    )
                lines.append(f"    {' '.join(extras)}")
        lines.append("")

    profile_results = report.get("profile_results", [])
    if profile_results:
        lines.append("## hotspot profiles")
        for profile in profile_results:
            scenario = profile.get("scenario_arg", profile.get("scenario", "?"))
            lines.append(f"### {scenario}")
            instructions = profile.get("instruction_hotspots", [])
            if instructions:
                lines.append("  instruction hotspots:")
                for row in instructions[:12]:
                    lines.append(
                        f"    {row.get('name', '?'):24s} "
                        f"total={fmt_ms(row.get('total_ms')):>9s}  "
                        f"avg={fmt_ns(row.get('avg_ns')):>10s}  "
                        f"count={row.get('count', 0)}"
                    )
            builtins = profile.get("builtin_hotspots", [])
            if builtins:
                lines.append("  builtin hotspots:")
                for row in builtins[:12]:
                    lines.append(
                        f"    {row.get('name', '?'):24s} "
                        f"total={fmt_ms(row.get('total_ms')):>9s}  "
                        f"avg={fmt_ns(row.get('avg_ns')):>10s}  "
                        f"count={row.get('count', 0)}"
                    )
            lines.append("")

    return "\n".join(lines)


def metric_pairs(name: str, baseline: dict[str, Any], current: dict[str, Any]) -> list[str]:
    rows: list[str] = []

    def cmp(metric: str, b: float | None, c: float | None, fmt) -> str:
        if b is None or c is None:
            return f"  {metric:30s} baseline={'n/a':>12s}  current={'n/a':>12s}"
        delta = c - b
        pct = (delta / b * 100.0) if b else 0.0
        delta_str = fmt(delta)
        if delta >= 0 and not delta_str.startswith("+"):
            delta_str = "+" + delta_str
        return (
            f"  {metric:30s} baseline={fmt(b):>12s}  current={fmt(c):>12s}  "
            f"Δ={delta_str:>12s} ({pct:+.1f}%)"
        )

    rows.append(f"### {name}")
    rows.append(
        cmp("run_turn_ms",
            baseline["run_turn_ms"]["median"], current["run_turn_ms"]["median"], lambda v: fmt_ms(v))
    )
    rows.append(
        cmp("total_ms",
            baseline["total_ms"]["median"], current["total_ms"]["median"], lambda v: fmt_ms(v))
    )
    rows.append(
        cmp("run_turn_alloc_bytes",
            baseline["run_turn_alloc_bytes"]["median"], current["run_turn_alloc_bytes"]["median"], fmt_bytes)
    )
    rows.append(
        cmp("total_alloc_bytes",
            baseline["total_alloc_bytes"]["median"], current["total_alloc_bytes"]["median"], fmt_bytes)
    )
    rows.append(
        cmp("total_live_bytes",
            baseline["total_live_bytes"]["median"], current["total_live_bytes"]["median"], fmt_bytes)
    )
    if baseline.get("rss_growth_kb") and current.get("rss_growth_kb"):
        rows.append(
            cmp("rss_growth",
                baseline["rss_growth_kb"]["median"], current["rss_growth_kb"]["median"], fmt_kb)
        )

    bp = baseline.get("phase_summary", {})
    cp = current.get("phase_summary", {})
    if bp and cp:
        rows.append("  phase deltas (median duration):")
        for ph in sorted(set(bp) | set(cp)):
            b = bp.get(ph, {}).get("duration_ms", {}).get("median")
            c = cp.get(ph, {}).get("duration_ms", {}).get("median")
            if b is None or c is None:
                continue
            delta = c - b
            pct = (delta / b * 100.0) if b else 0.0
            rows.append(
                f"    {ph:28s} baseline={fmt_ms(b):>9s}  current={fmt_ms(c):>9s}  "
                f"Δ={delta:+.2f}ms ({pct:+.1f}%)"
            )
    return rows


def diff_runtime(baseline: dict[str, Any], current: dict[str, Any]) -> str:
    lines = ["# runtime-perf diff", ""]
    lines.append(f"baseline: {baseline.get('created_at', '?')[:19]}  scenarios: {', '.join(baseline.get('scenarios', []))}")
    lines.append(f"current:  {current.get('created_at', '?')[:19]}  scenarios: {', '.join(current.get('scenarios', []))}")
    lines.append("")
    bs = {s["scenario"]: s for s in baseline.get("summary", [])}
    cs = {s["scenario"]: s for s in current.get("summary", [])}
    for name in sorted(set(bs) & set(cs)):
        lines.extend(metric_pairs(name, bs[name], cs[name]))
        lines.append("")
    return "\n".join(lines)


def diff_lashlang(baseline: dict[str, Any], current: dict[str, Any]) -> str:
    lines = ["# lashlang-perf diff", ""]
    lines.append(
        f"baseline: {baseline.get('created_at', '?')[:19]}  "
        f"build={baseline.get('build_mode', '?')}"
    )
    lines.append(
        f"current:  {current.get('created_at', '?')[:19]}  "
        f"build={current.get('build_mode', '?')}"
    )
    lines.append("")

    bs = {
        (r.get("mode_arg"), r.get("scenario_arg")): r
        for r in baseline.get("perf_results", [])
        if r.get("mode_arg") and r.get("scenario_arg")
    }
    cs = {
        (r.get("mode_arg"), r.get("scenario_arg")): r
        for r in current.get("perf_results", [])
        if r.get("mode_arg") and r.get("scenario_arg")
    }

    def cmp(label: str, metric: str, b: float | int | None, c: float | int | None, fmt) -> str:
        if b is None or c is None:
            return f"  {label:45s} {metric:26s} baseline={'n/a':>12s}  current={'n/a':>12s}"
        bf = float(b)
        cf = float(c)
        delta = cf - bf
        pct = (delta / bf * 100.0) if bf else 0.0
        delta_str = fmt(delta)
        if delta >= 0 and not delta_str.startswith("+"):
            delta_str = "+" + delta_str
        return (
            f"  {label:45s} {metric:26s} baseline={fmt(bf):>12s}  "
            f"current={fmt(cf):>12s}  Δ={delta_str:>12s} ({pct:+.1f}%)"
        )

    for mode, scenario in sorted(set(bs) & set(cs)):
        b = bs[(mode, scenario)]
        c = cs[(mode, scenario)]
        label = f"{mode}/{scenario}"
        lines.append(cmp(label, "ns_per_iter", b.get("ns_per_iter"), c.get("ns_per_iter"), fmt_ns))
        lines.append(
            cmp(
                label,
                "allocations_per_iter",
                b.get("allocations_per_iter"),
                c.get("allocations_per_iter"),
                lambda v: f"{v:.2f}",
            )
        )
        lines.append(
            cmp(
                label,
                "allocated_bytes_per_iter",
                b.get("allocated_bytes_per_iter"),
                c.get("allocated_bytes_per_iter"),
                fmt_bytes,
            )
        )
        lines.append("")

    return "\n".join(lines)


def summarize_dhat(payload: dict[str, Any], top: int) -> str:
    ftbl: list[str] = payload["ftbl"]
    pps: list[dict[str, Any]] = payload["pps"]
    cmd = payload.get("cmd", "?")
    total_bytes = sum(p["tb"] for p in pps)
    total_blocks = sum(p["tbk"] for p in pps)
    total_max_bytes = sum(p["mb"] for p in pps)

    def frame_label(idx: int) -> str:
        s = ftbl[idx]
        # Strip leading hex address and any " (path:line:col)" tail.
        if s.startswith("0x"):
            sp = s.find(": ")
            if sp != -1:
                s = s[sp + 2 :]
        if " (" in s:
            s = s.split(" (")[0]
        return s

    def pretty_stack(fs: list[int], depth: int = 6) -> list[str]:
        # dhat stacks are root-first; reverse to user-first then keep top.
        labels = [frame_label(i) for i in fs]
        # Skip the dhat allocator hook frames at the top.
        skip = 0
        while skip < len(labels) and (
            "dhat::Alloc" in labels[skip]
            or "__rust_alloc" in labels[skip]
            or "RawVecInner" in labels[skip]
            or "raw_vec" in labels[skip]
            or "alloc::alloc" in labels[skip]
        ):
            skip += 1
        labels = labels[skip:]
        return labels[:depth]

    def fmt_block(p: dict[str, Any]) -> list[str]:
        blocks = pretty_stack(p["fs"], depth=8)
        out = [
            f"  total={fmt_bytes(p['tb']):>10s}  blocks={p['tbk']:>7d}  "
            f"max_live={fmt_bytes(p['mb']):>10s}  end_live={fmt_bytes(p['gb']):>10s}"
        ]
        for label in blocks:
            out.append(f"    {label}")
        return out

    lines = ["# dhat heap summary", ""]
    lines.append(f"command: {cmd}")
    lines.append(
        f"total alloc={fmt_bytes(total_bytes)}  blocks={total_blocks}  "
        f"max_live(sum-of-pps)={fmt_bytes(total_max_bytes)}  pps={len(pps)}"
    )
    lines.append("")

    lines.append(f"## top {top} call stacks by total bytes allocated")
    for p in sorted(pps, key=lambda p: -p["tb"])[:top]:
        lines.extend(fmt_block(p))
        lines.append("")

    lines.append(f"## top {top} call stacks by max live bytes")
    for p in sorted(pps, key=lambda p: -p["mb"])[:top]:
        lines.extend(fmt_block(p))
        lines.append("")

    lines.append(f"## top {top} call stacks by block count")
    for p in sorted(pps, key=lambda p: -p["tbk"])[:top]:
        lines.extend(fmt_block(p))
        lines.append("")

    return "\n".join(lines)


def summarize_dhat_report(payload: dict[str, Any], top: int) -> str:
    return summarize_dhat(payload, top)


REPORT_DISPATCH: dict[
    str,
    tuple[
        Callable[[dict[str, Any], int], str],
        Callable[[dict[str, Any], dict[str, Any]], str] | None,
    ],
] = {
    "runtime-perf": (lambda payload, _top: summarize_runtime(payload), diff_runtime),
    "runtime-stack": (lambda payload, _top: summarize_runtime_stack(payload), None),
    "lashlang-perf": (lambda payload, _top: summarize_lashlang(payload), diff_lashlang),
}


def dispatch_entry(payload: dict[str, Any], path: Path) -> tuple[str, Callable, Callable | None]:
    # dhat is the one report family lash does not author.
    if "kind" not in payload and "dhatFileVersion" in payload:
        return "dhat", summarize_dhat_report, None
    kind = payload.get("kind", "<missing>")
    entry = REPORT_DISPATCH.get(kind) if isinstance(kind, str) else None
    if entry is None:
        raise ValueError(f"unknown report kind {kind!r} in {path}")
    return kind, entry[0], entry[1]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    parser.add_argument(
        "report",
        type=Path,
        help="runtime-perf JSON, perf guard JSON, ui-perf JSON, lashlang-perf JSON, or *.dhat.json",
    )
    parser.add_argument("--diff", type=Path, help="baseline JSON of the same report kind to diff against")
    parser.add_argument("--top", type=int, default=20, help="top-N call stacks for dhat output (default 20)")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    payload = json.loads(args.report.read_text())
    try:
        kind, summarize, diff = dispatch_entry(payload, args.report)
    except ValueError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    if args.diff:
        baseline = json.loads(args.diff.read_text())
        try:
            baseline_kind, _baseline_summarize, baseline_diff = dispatch_entry(
                baseline, args.diff
            )
        except ValueError as error:
            print(f"error: {error}", file=sys.stderr)
            return 2
        if baseline_kind != kind or diff is None or baseline_diff is None:
            print(
                "error: --diff expects matching report kinds with a diff handler: "
                f"current={kind!r}, baseline={baseline_kind!r}",
                file=sys.stderr,
            )
            return 2
        print(diff(baseline, payload))
        return 0
    print(summarize(payload, args.top))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

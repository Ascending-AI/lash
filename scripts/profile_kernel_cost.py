"""FIG-6622 kernel phase receipt producer and strict offline text reader.

Physical instructions, implementation work units and sampled CPU phases retain
separate populations. This is an advisory experiment, with no charge budget.
"""
from __future__ import annotations

from collections import Counter
import hashlib
import json
import os
from pathlib import Path
import resource
import statistics
import subprocess
import uuid

from perf_artifacts import artifacts
from perf_capture import checked, instruction_window, run_profiled

LABEL = "//crates/lash-dialect-typescript:kernel_perf__example"
IDENTITY_FIELDS = ("experiment_id", "host_id", "program", "workload_sha256", "artifact_sha256", "result_sha256",
                   "statement_path_sha256", "charged", "backend", "worker", "window",
                   "terminal", "compiler", "build_profile", "allocator")


def admit(reference: dict, row: dict, observer: str, iterations: int, *, acknowledged: bool = True):
    """Missing or mixed identities cannot enter a qualified phase comparison."""
    identity = row["identity"]
    for key in IDENTITY_FIELDS:
        if key not in reference or key not in identity or reference[key] in (None, ""):
            raise ValueError(f"missing experiment identity: {key}")
        if identity[key] != reference[key]:
            raise ValueError(f"mixed experiment identity: {key}")
    if identity["terminal"] != "finished":
        raise ValueError("unsuccessful terminal")
    if row["observer"] != observer or row["iterations"] != iterations or row["pid"] <= 0:
        raise ValueError("observer/process/population mismatch")
    expected = 0 if observer == "zero" else iterations
    if row["completed_runs"] != expected:
        raise ValueError("incomplete useful-work population")
    if acknowledged and row["acknowledged_windows"] != 1:
        raise ValueError("missing acknowledged instruction window")
    if observer == "on" and row["phase_counts_total"] is None:
        raise ValueError("enabled counters missing")
    if observer != "on" and row["phase_counts_total"] is not None:
        raise ValueError("unexpected observer")


def sampled_phases(folded: str, top: int) -> dict:
    """Exclusive first matching phase per stack; unmatched samples stay visible.

    Inlining and missing symbols prevent complete attribution. The priority is
    explicit: publication > refinement > accounting > tree copies > resolution.
    """
    phases = Counter()
    symbols = (
        ("publication", ("native_heap::Roots", "set_execution_roots", "publish_holdings", "publish_roots")),
        ("refinement", ("census_impl::trace", "::refine", "::collect")),
        ("accounting", ("census_impl::Census", "replace_holding", "release_statement_roots")),
        ("tree_copy", ("tree::make_dirty", "tree::Node<T> as core::clone::Clone", "tree::Tree<T>::load")),
        ("frame_resolution", ("frames::Frames::last_mut", "tasks::Tasks::get_mut")),
        ("registers", ("Vec<T,A>::resize", "Vec<T,A>::clear")),
    )
    for line in folded.splitlines():
        stack, count = line.rsplit(" ", 1)
        phase = next((phase for phase, names in symbols if any(name in stack for name in names)),
                     "other_or_unresolved")
        phases[phase] += int(count)
    if not phases or sum(phases.values()) <= 0:
        raise ValueError("empty sampled phase population")
    return {"unit": "CPU cycle samples; process envelope including setup/reference passes",
            "aggregation": "exclusive first matching symbol family per stack; heuristic, inlining can hide phases",
            "exact_phase_instructions": False, "population": sum(phases.values()),
            "top": [{"phase": phase, "samples": count} for phase, count in phases.most_common(top)],
            "omitted_samples": sum(count for _, count in phases.most_common()[top:])}


def read_one(directory: Path) -> dict:
    rows = json.loads((directory / "phase-run.json").read_text())
    if len(rows) != 1:
        raise ValueError("phase experiment requires exactly one selected program")
    return rows[0]


def render(report: dict) -> str:
    identity = report["identity"]
    lines = [f"kernel-cost experiment={report['experiment_id']} program={identity['program']}",
             f"backend={identity['backend']} worker={identity['worker']} terminal={identity['terminal']}",
             f"charge/run={identity['charged']} vm_steps/run={report['phase_counts']['vm_steps']}",
             f"guest_statements/run={report['guest_statements_per_run']} iterations/window={report['iterations']}",
             "instructions:u are acknowledged run-only windows; RSS/load cover each process.",
             "repeat observer instructions running_fraction load1_before load1_after peak_rss_kib"]
    for row in report["samples"]:
        perf = row["perf"]
        lines.append(f"{row['repeat']} {row['producer']['observer']} {perf['instructions']} "
                     f"{perf['running_fraction']:.6f} {perf['load1_before']:.2f} "
                     f"{perf['load1_after']:.2f} {perf['peak_rss_kib']}")
    lines += [f"paired on/off instruction ratios={report['paired_instruction_ratios']}",
              f"median on/off={statistics.median(report['paired_instruction_ratios']):.6f}",
              "No zero-work subtraction; raw controls retained. No wall-time or universal instructions/charge claim.",
              "Fixed phase counts: one separate counted run (calls, rows, work units and scratch peaks):",
              json.dumps(report["phase_counts"], sort_keys=True),
              "Sampled phase top-N: CPU cycle samples over process envelope; NOT exact phase instructions:"]
    lines += [f"{row['samples']} {row['phase']}" for row in report["sampled_phases"]["top"]]
    return "\n".join(lines) + "\n"


def measure(root: Path, out: Path, program: str, iterations: int, repeats: int,
            build_report: Path | None, *, build: bool, top: int) -> str:
    if out.exists():
        raise ValueError("kernel-cost output must be a fresh directory")
    out.mkdir(parents=True)
    experiment = str(uuid.uuid4())
    binary = artifacts(root, [LABEL], build=build, report=build_report or out / "build.json",
                       optimized=True, symbolized=True)[LABEL]
    ceiling = 8 * 1024**3
    _, hard = resource.getrlimit(resource.RLIMIT_AS)
    if hard != resource.RLIM_INFINITY:
        ceiling = min(ceiling, hard)
    resource.setrlimit(resource.RLIMIT_AS, (ceiling, ceiling))
    environment = {key: value for key, value in os.environ.items()
                   if key not in ("LASH_PERF_CONTROL", "LASH_PERF_ACK", "KERNEL_PERF_OBSERVE")}
    environment["KERNEL_PHASE_ITERATIONS"] = str(iterations)
    environment["LASH_PERF_EXPERIMENT_ID"] = experiment
    host = (Path("/etc/machine-id").read_text().strip(), os.uname().release,
            os.uname().machine, Path("/proc/cpuinfo").read_text().split("model name", 1)[-1].splitlines()[0])
    environment["LASH_PERF_HOST_ID"] = hashlib.sha256(";".join(host).encode()).hexdigest()
    # This pass is separate from physical instructions and the on/off comparison.
    counted = out / "counts"
    proc = checked(["/usr/bin/time", "-v", "-o", str(out / "counts.resources"),
                    str(binary), "work", str(counted), program], cwd=root, env=environment)
    (out / "counts.stdout").write_text(proc.stdout)
    (out / "counts.stderr").write_text(proc.stderr)
    work = json.loads((counted / "kernel-work.json").read_text())
    if len(work["programs"]) != 1:
        raise ValueError("kernel-cost requires one program")
    reference = work["programs"][0]
    identity = reference["identity"]
    if identity["artifact_sha256"] != hashlib.sha256(binary.read_bytes()).hexdigest():
        raise ValueError("producer artifact mismatch")
    work["experiment_id"] = experiment
    (out / "kernel-work.json").write_text(json.dumps(work, indent=2) + "\n")
    samples = []
    for repeat in range(repeats):
        for observer in (("zero", "off", "on") if repeat % 2 == 0 else ("zero", "on", "off")):
            directory = out / f"{repeat}-{observer}"
            producer_dir = directory / "producer"
            perf = instruction_window([str(binary), "phase", str(producer_dir), program], directory,
                                      env=dict(environment, KERNEL_PHASE_OBSERVER=observer), cwd=root)
            row = read_one(producer_dir)
            admit(identity, row, observer, iterations)
            if observer == "on":
                # Every deterministic additive counter must conserve its one-run population.
                def conserve(one, total, key=""):
                    for field, value in one.items():
                        if isinstance(value, dict):
                            conserve(value, total[field], field)
                        elif total[field] != (value if "_peak_" in field else value * iterations):
                            raise ValueError(f"counter population mismatch: {key}.{field}")
                conserve(reference["phase_counts"], row["phase_counts_total"])
            samples.append({"experiment_id": experiment, "repeat": repeat, "producer": row, "perf": perf})
    directory = out / "sampled"
    command = [str(binary), "phase", str(directory), program]
    proc = run_profiled(["/usr/bin/time", "-v", "-o", str(out / "sampled.resources"), *command],
                        receipt=out / "sampled.json", cpu=True, cwd=root,
                        env=dict(environment, KERNEL_PHASE_OBSERVER="off"))
    (out / "sampled.stdout").write_text(proc.stdout)
    (out / "sampled.stderr").write_text(proc.stderr)
    sampled = read_one(directory)
    admit(identity, sampled, "off", iterations, acknowledged=False)
    phases = sampled_phases((out / "sampled.profiles" / "cpu.folded").read_text(), top)
    phases.update(experiment_id=experiment, producer=sampled)
    (out / "sampled-phase-top.json").write_text(json.dumps(phases, indent=2) + "\n")
    ratios = []
    for repeat in range(repeats):
        pair = {row["producer"]["observer"]: row["perf"]["instructions"]
                for row in samples if row["repeat"] == repeat}
        ratios.append(pair["on"] / pair["off"])
    report = {"instrument": "statement-phase-cost", "experiment_id": experiment,
              "identity": identity, "observers": {"off": "fixed counters disabled", "on": "fixed counters enabled",
              "zero": "same loop/control, no VM drive", "counts": reference["observer"]},
              "observation_unit": "one process window summing completed run-to-terminal calls",
              "iterations": iterations, "repeats": repeats, "samples": samples,
              "guest_statements_per_run": sum(function["statements"] for function in reference["functions"]),
              "phase_counts": reference["phase_counts"], "paired_instruction_ratios": ratios,
              "sampled_phases": phases}
    (out / "statement-phase-cost.json").write_text(json.dumps(report, indent=2) + "\n")
    text = render(report)
    (out / "statement-phase-cost.txt").write_text(text)
    return text

#!/usr/bin/env python3
"""Build source-bound Kiln artifacts, then enforce every tracked instruction ceiling."""

import argparse
import ast
import concurrent.futures
import fcntl
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import queue
import re
import shutil
import signal
import statistics
import subprocess
import sys
import threading
import time

import perf_land_ceilings as policy

LABEL = "//crates/lash-perf:lash-perf__bin"
BUILD_ARGS = ["kiln", "build", "--config=optimized", "-c", "kiln.perf_iteration=false",
              LABEL, "--materializations", "final"]
ROOT = Path(subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True).strip())
CACHE = Path(os.environ.get("LASH_BUCK2_STORE", str(Path.home() / ".cache/lash/buck2-store"))).parent / "perf-land-cache"


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def git(*args):
    return subprocess.check_output(["git", *args], cwd=ROOT, text=True).strip()


def write(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + f".{os.getpid()}.tmp")
    tmp.write_text(json.dumps(value, indent=2, allow_nan=False) + "\n")
    os.replace(tmp, path)


def read(path):
    return json.loads(Path(path).read_text())


def require(ok, message):
    if not ok:
        raise ValueError(message)


# What an instrument-only land may touch: the measuring crate's own sources,
# and paths outside every Cargo package. Nothing that decides what is
# measured, what it is held to, or how the binary is built.
PERF_CRATE = "crates/lash-perf"
WORKLOADS = ("crates/lash-perf/src/compare/workloads/", "crates/lash-perf/src/compare/workloads.rs",
             "crates/lash-perf/src/boundary/kernel_cold_workload.rs",
             *(row["source"] for row in policy.ROWS.values()), policy.BOUNDARY["source"])
PINS = ("scripts/perf_land_ceilings.py", "scripts/build-compare-references.py")
BUILD_CONFIGURATION = ("tools/", "third-party/", ".cargo/", ".buckconfig", ".github/actions/rust-toolchain",
                       "env.sh", "rust-toolchain")
BUILD_FILES = ("Cargo.toml", "Cargo.lock", "BUCK", "PACKAGE")


def instrument_only(paths, packages):
    """Whether a land changes the instrument and nothing it measures. `paths`
    is the land's diff; `packages` the root of every Cargo package on either
    side ("" for a package at the repository root)."""
    def inside(path, root):
        return root == "" or path == root or path.startswith(root + "/")

    def fixed(path):
        name = path.rsplit("/", 1)[-1]
        return (any(path == item or (item.endswith("/") and path.startswith(item)) for item in WORKLOADS)
                or "baseline" in name or "ceiling" in name or path in PINS
                or name in BUILD_FILES or name.endswith(".bzl")
                or any(path.startswith(item) for item in BUILD_CONFIGURATION))

    return bool(paths) and all(
        not fixed(path) and (inside(path, PERF_CRATE) or not any(inside(path, root) for root in packages))
        for path in paths)


def land_paths(base, candidate):
    """The land's changed paths and the Cargo package roots on either side."""
    # Without --no-renames a move lists only its destination, so a product
    # file moved into the perf crate would read as instrument-only.
    paths = [path for path in git("diff", "--no-renames", "--name-only", f"{base}..{candidate}").splitlines() if path]
    packages = set()
    for commit in (base, candidate):
        listing = subprocess.run(["git", "grep", "-l", "-e", r"^\[package\]", commit, "--", "*Cargo.toml"],
                                 cwd=ROOT, capture_output=True, text=True)
        require(listing.returncode in (0, 1), f"package listing failed at {commit}")
        for line in listing.stdout.splitlines():
            manifest = line.split(":", 1)[1]
            packages.add(manifest.rsplit("/", 1)[0] if "/" in manifest else "")
    return paths, packages


def check_instruction_ceiling(side, key, median, ceiling, monty=None, calibrate=False):
    """Refuse a row above its ceiling or a candidate PY row above the binding
    Monty ratio unless it is listed exempt; answer the notes of an exempt row
    above it and of any PY row above the report ratio."""
    if not calibrate:
        require(median <= ceiling, f"{side}/{key}: median {median} exceeds ceiling {ceiling}")
    notes = []
    if side == "candidate" and key.startswith("python/") and policy.MONTY_BINDING:
        require(monty is not None and math.isfinite(monty) and monty > 0,
                f"{side}/{key}: missing qualified Monty secant")
        if median > policy.MONTY_MAX_RATIO * monty:
            require(key in policy.MONTY_EXEMPT_ROWS,
                    f"{side}/{key}: {median / monty:.6f}x Monty exceeds {policy.MONTY_MAX_RATIO:.2f}x")
            notes.append(f"{side}/{key}: {median / monty:.6f}x Monty, exempt from the binding "
                         f"{policy.MONTY_MAX_RATIO:.2f}x: {policy.MONTY_EXEMPT_ROWS[key]['reason']}")
        if median > policy.MONTY_REPORT_RATIO * monty:
            notes.append(f"{side}/{key}: {median / monty:.6f}x Monty above the report-only "
                         f"{policy.MONTY_REPORT_RATIO:.2f}x")
    return "; ".join(notes) or None


def check_cold_access(key, row):
    """Hold one cold row to contract section 6 for its fixture: reads d + 2
    cold, heap writes within the fixture ceiling, all writes per seal within
    that plus s + 2. Every failing term is named."""
    require(row["elements"] == policy.BOUNDARY["elements"], f"{key}: cold workload size differs")
    fixture = policy.BOUNDARY["fixtures"][key]
    reads = fixture["directory_depth"] + 2
    heap = fixture["heap_writes"]
    total = heap + fixture["run_segments"] + 2
    errors = []
    if row.get("directory_depth") != fixture["directory_depth"]:
        errors.append(f"{key}: directory depth {row.get('directory_depth')} differs from the fixture's "
                      f"d = {fixture['directory_depth']}")
    if not (type(row["cold_leaf_pages"]) is int and row["cold_leaf_pages"] > reads):
        errors.append(f"{key}: workload is not a cold collection spanning more than the read bound "
                      f"({row['cold_leaf_pages']} leaf pages measured)")
    for field, ceiling, term in (("reads", reads, "d + 2"), ("edit_reads", reads, "d + 2"),
                                 ("writes", heap, "C + I + E + D"), ("total_writes", total, "W")):
        if not (type(row[field]) is int and 0 < row[field] <= ceiling):
            errors.append(f"{key}: cold {field} {row[field]} exceeds {term} = {ceiling} or is not counted")
    require(not errors, "; ".join(errors))


# The seal cost is its instructions:u count, held to the compare rows' rule:
# each side's repeat noise below policy.NOISE and the candidate's median below
# the base's times 1 + policy.BASE_MARGIN. Identical code spreads about 0.2%
# across processes, so an exact 1.0 would refuse it at random. Wall-clock
# seal time is recorded and gates nothing. Bytes written stay exact.
def seal_noise(side, key, rows):
    """The seal-count repeat noise failure of one side's supported rows, if any."""
    counts = [row["seal_instructions"] for row in rows if row["status"] == "supported"]
    if not counts:
        return []
    noise = (max(counts) - min(counts)) / statistics.median(counts)
    return ([f"{side}/{key}: seal instructions repeat noise {noise:.4%} >={policy.NOISE:.0%}; calibration required"]
            if noise >= policy.NOISE else [])


def checkpoint_outcome(key, old, new):
    """The checkpoint comparison of one row: base-relative seal instructions
    and bytes written, or the declared outcome of a base that cannot perform
    the cold point edit (FIG-7669), which records the candidate's cost as the
    next baseline. The candidate's seal noise is held in both."""
    errors = seal_noise("candidate", key, new)
    unsupported = [row for row in old if row["status"] == "unsupported"]
    if unsupported:
        require(len(unsupported) == len(old) and all(
            "cold point edit" in row["error"] and "canonical region reached an unintegrated boundary" in row["error"]
            for row in unsupported), f"base/{key}: unqualified failure, not FIG-7669")
        supported = [row for row in new if row["status"] == "supported"]
        baseline = {field: statistics.median(row[field] for row in supported) if supported else None
                    for field in ("bytes_written", "seal_instructions", "seal_ns")}
        return dict(outcome="base unsupported: no comparison", baseline=baseline), {}, errors
    ratios = {}
    errors += seal_noise("base", key, old)
    if all(row["status"] == "supported" for row in new):
        ratios["bytes_written"] = statistics.median([n["bytes_written"] / b["bytes_written"]
                                                     for b, n in zip(old, new, strict=True)])
        if ratios["bytes_written"] > 1:
            errors.append(f"{key}: checkpoint bytes_written {ratios['bytes_written']:.6f}x exceeds base")
        ratios["seal_instructions"] = (statistics.median(n["seal_instructions"] for n in new)
                                       / statistics.median(b["seal_instructions"] for b in old))
        if ratios["seal_instructions"] >= 1 + policy.BASE_MARGIN:
            errors.append(f"{key}: checkpoint seal_instructions {ratios['seal_instructions']:.6f}x "
                          f"exceeds base by >={policy.BASE_MARGIN:.0%}")
        ratios["seal_ns_information"] = statistics.median([n["seal_ns"] / b["seal_ns"]
                                                           for b, n in zip(old, new, strict=True)])
    return dict(outcome="base-relative"), ratios, errors


def check_rows(prior, replacement):
    """Hold the rows to the base's: only an active reviewed replacement adds
    rows or changes a ceiling, and only the ones it names. The land's ratchet
    lowers ceilings after this check, so a candidate never carries one."""
    added = set(replacement["added_rows"]) if replacement else set()
    replaced = set(replacement["replaced_ceilings"]) if replacement else set()
    require(set(policy.ROWS) == set(prior) | added and not set(prior) & added,
            "tracked rows changed; reviewed replacement required")
    require(replaced <= set(prior), "a reviewed replacement replaces ceilings of tracked rows only")
    for key, old_row in prior.items():
        row = policy.ROWS[key]
        require({k: v for k, v in row.items() if k != "ceiling"} ==
                {k: v for k, v in old_row.items() if k != "ceiling"},
                f"{key}: workload contract changed; reviewed replacement required")
        require(row["ceiling"] == old_row["ceiling"] or key in replaced,
                f"{key}: ceiling {old_row['ceiling']} -> {row['ceiling']} matches neither the land's ratchet "
                f"nor a reviewed replacement naming this row")


def exemptions(replacement):
    """The rows an active reviewed replacement exempts from the base margin,
    each with its reviewer."""
    return {key: replacement["reviewer"] for key in replacement["replaced_ceilings"]} if replacement else {}


def ratchet(rows):
    """The lowered ceiling of every row whose candidate median times the
    ceiling factor sits more than the ratchet slack below its ceiling."""
    lowered = {}
    for key, row in policy.ROWS.items():
        target = rows[f"candidate/{key}"]["median"] * policy.CEILING_FACTOR
        if row["ceiling"] is not None and target < row["ceiling"] * (1 - policy.RATCHET_SLACK):
            lowered[key] = target
    return lowered


def policy_values(source):
    return {node.targets[0].id: ast.literal_eval(node.value) for node in ast.parse(source).body
            if isinstance(node, ast.Assign) and len(node.targets) == 1 and isinstance(node.targets[0], ast.Name)}


def apply_ratchet(source, lowered):
    """The ceilings file with exactly the ratchet's ceilings replaced."""
    rows = next(node.value for node in ast.parse(source).body
                if isinstance(node, ast.Assign) and getattr(node.targets[0], "id", None) == "ROWS")
    edits = []
    for key_node, row_node in zip(rows.keys, rows.values):
        if key_node.value in lowered:
            value = row_node.values[[k.value for k in row_node.keys].index("ceiling")]
            edits.append((value.lineno, value.col_offset, value.end_col_offset, repr(float(lowered[key_node.value]))))
    require(len(edits) == len(lowered), "ratchet names an untracked row")
    lines = source.splitlines(keepends=True)
    for line, start, end, text in sorted(edits, reverse=True):
        lines[line - 1] = lines[line - 1][:start] + text + lines[line - 1][end:]
    result = "".join(lines)
    before, after = policy_values(source), policy_values(result)
    expected = {key: dict(row, ceiling=lowered[key]) if key in lowered else row for key, row in before["ROWS"].items()}
    require(after == dict(before, ROWS=expected), "ratchet changed more than the lowered ceilings")
    return result


def load_artifact(path):
    value = read(path)
    require(value["configuration"] == policy.CONFIGURATION, "artifact configuration differs from ceilings")
    require(digest(value["binary"]) == value["binary_sha256"], "artifact binary checksum differs")
    require(git("cat-file", "-t", value["tree"]) == "tree", "artifact source tree is unavailable")
    if value["commit"] is not None:
        require(value["tree"] == git("rev-parse", value["commit"] + "^{tree}"), "artifact source tree differs")
    require(value["compiler"] == policy.COMPILER, "artifact compiler differs from ceilings")
    return value


def build(commit, out, label=LABEL):
    commit = git("rev-parse", commit + "^{commit}")
    directory = CACHE / "artifacts" / commit
    if label != LABEL:
        directory /= hashlib.sha256(label.encode()).hexdigest()[:16]
    directory.mkdir(parents=True, exist_ok=True)
    build_args = [label if arg == LABEL else arg for arg in BUILD_ARGS]
    metadata = directory / "artifact.json"
    with (directory / "build.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if metadata.exists():
            artifact = load_artifact(metadata)
            require(artifact["commit"] == commit, "artifact cache source identity differs")
            print(f"perf artifact {commit}: cache hit, {artifact['binary_sha256']}", flush=True)
            return metadata
        require(not git("status", "--porcelain", "--untracked-files=all"), "artifact build requires a clean tree")
        previous = git("symbolic-ref", "--quiet", "--short", "HEAD") if subprocess.run(
            ["git", "symbolic-ref", "--quiet", "HEAD"], cwd=ROOT, capture_output=True).returncode == 0 else git("rev-parse", "HEAD")
        switched = git("rev-parse", "HEAD") != commit
        report = Path(out) / f"build-{commit}.json"
        log = report.with_suffix(".log")
        start = time.monotonic()
        compiler = subprocess.check_output(["rustc", "--version"], text=True).strip()
        require(compiler == policy.COMPILER, "build compiler differs from frozen configuration")
        try:
            if switched:
                subprocess.run(["git", "checkout", "--detach", commit], cwd=ROOT, check=True)
            with log.open("w") as output:
                subprocess.run([*build_args, "--build-report", str(report)], cwd=ROOT,
                               stdout=output, stderr=subprocess.STDOUT, check=True)
            require(git("rev-parse", "HEAD") == commit and
                    not git("status", "--porcelain", "--untracked-files=all"),
                    "source tree changed during artifact construction")
            spec = importlib.util.spec_from_file_location("kiln_outputs", ROOT / "tools/buck2/outputs.py")
            module = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(module)
            outputs = module.resolve(read(report), label)
            require(len(outputs) == 1, "optimized build did not produce one executable")
            tmp = directory / f"lash_perf.{os.getpid()}.tmp"
            shutil.copyfile(outputs[0], tmp)
            tmp.chmod(0o755)
            os.replace(tmp, directory / "lash_perf")
            artifact = dict(label=label, commit=commit, tree=git("rev-parse", "HEAD^{tree}"),
                            binary=str(directory / "lash_perf"), binary_sha256=digest(directory / "lash_perf"),
                            configuration=policy.CONFIGURATION, compiler=compiler,
                            build_command=build_args, build_seconds=time.monotonic() - start)
            write(metadata, artifact)
        finally:
            if switched:
                subprocess.run(["git", "checkout", previous], cwd=ROOT, check=True)
        print(f"perf artifact {commit}: built in {artifact['build_seconds']:.2f}s, {artifact['binary_sha256']}", flush=True)
        return metadata


def references(out):
    directory = CACHE / "references"
    directory.mkdir(parents=True, exist_ok=True)
    with (directory / "references.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        qjs = directory / ("quickjs-" + policy.QUICKJS_REVISION) / "qjs"
        monty = directory / "monty-bin"
        if not qjs.exists() or not monty.exists():
            with (Path(out) / "references-build.log").open("w") as log:
                subprocess.run(["bash", "-c", 'ulimit -v 8388608; exec "$@"', "--", sys.executable,
                                "scripts/build-compare-references.py", "--cache", str(directory),
                                "--engine", "quickjs", "--engine", "monty"], cwd=ROOT, stdout=log, stderr=subprocess.STDOUT, check=True)
        require(digest(qjs) == policy.QUICKJS_SHA256, "QuickJS executable differs from frozen reference")
        require(digest(monty) == policy.MONTY_SHA256, "Monty executable differs from frozen reference")
    python = Path(shutil.which("python3")).resolve()
    require(digest(python) == policy.PYTHON_SHA256, "CPython executable differs from frozen reference")
    version = subprocess.check_output([str(python), "--version"], text=True).strip()
    require(version == policy.PYTHON_VERSION, "CPython version differs from frozen reference")
    return {"python": dict(args=["--python", str(python), "--monty", str(monty)],
                           engines={"cpython": policy.PYTHON_SHA256, "monty": policy.MONTY_SHA256}),
            "typescript": dict(args=["--qjs", str(qjs)],
                               engines={"qjs-0": policy.QUICKJS_SHA256})}


def cpus():
    allowed = os.sched_getaffinity(0)
    selected = {}
    for cpu in sorted(allowed):
        topology = Path(f"/sys/devices/system/cpu/cpu{cpu}/topology")
        key = ((topology / "physical_package_id").read_text().strip(),
               (topology / "core_id").read_text().strip())
        selected.setdefault(key, cpu)
    require(len(selected) >= policy.WORKERS, f"measurement requires {policy.WORKERS} physical cores")
    model = next(line.split(":", 1)[1].strip() for line in Path("/proc/cpuinfo").read_text().splitlines()
                 if line.startswith("model name"))
    require(model == policy.CPU_MODEL, "CPU model differs from frozen calibration")
    return list(selected.values())[:policy.WORKERS]


def contract(base, calibrate):
    """Hold the policy to the base's copy. Answers the active reviewed
    replacement, or None."""
    # Main may advance after calibration. A bootstrap must descend from the
    # reviewed seed and cannot replace a previously published policy.
    old = subprocess.run(["git", "show", f"{base['commit']}:scripts/perf_land_ceilings.py"],
                         cwd=ROOT, capture_output=True, text=True)
    replacement = None
    if old.returncode:
        require(subprocess.run(["git", "merge-base", "--is-ancestor", policy.SEED_COMMIT, base["commit"]],
                               cwd=ROOT, capture_output=True).returncode == 0,
                f"bootstrap seed {policy.SEED_COMMIT} is not an ancestor of base {base['commit']}")
        require(not git("log", "-1", "--format=%H", base["commit"], "--", "scripts/perf_land_ceilings.py"),
                "base previously published a ceilings policy; reviewed replacement required")
    else:
        previous = policy_values(old.stdout)
        current = {name: value for name, value in vars(policy).items() if name.isupper()}
        if hashlib.sha256(old.stdout.encode()).hexdigest() == policy.REVIEWED_REPLACEMENT["previous_sha256"]:
            replacement = policy.REVIEWED_REPLACEMENT
            require(isinstance(replacement.get("reviewer"), str) and replacement["reviewer"].strip(),
                    "a reviewed replacement names its reviewer")
            print(f"perf reviewed replacement by {replacement['reviewer']}: adds {replacement['added_rows']}, "
                  f"replaces ceilings {replacement['replaced_ceilings']}, changes {replacement['changed_fields']}",
                  flush=True)
        changed = set(replacement["changed_fields"]) if replacement else set()
        missing = object()
        for name in sorted((set(previous) | set(current)) - {"ROWS", "REVIEWED_REPLACEMENT"}):
            require(previous.get(name, missing) == current.get(name, missing) or name in changed,
                    f"{name}: measurement contract changed; reviewed replacement required")
        check_rows(previous["ROWS"], replacement)
    require(sorted(policy.COST_ORDER) == sorted(policy.ROWS), "COST_ORDER must list every row once")
    for path in ("tools/buck2/lash_rust.bzl", "tools/buck2/third_party.bzl"):
        flags = re.search(r"_OPTIMIZED_FLAGS = (\[[\s\S]*?\n\])", (ROOT / path).read_text())
        require(flags is not None and ast.literal_eval(flags[1]) == policy.BUILD_FLAGS,
                f"{path}: optimized flags differ from frozen configuration")
    for key, row in policy.ROWS.items():
        require(digest(ROOT / row["source"]) == row["sha256"], f"{key}: frozen workload checksum differs")
        if not calibrate:
            require(isinstance(row["ceiling"], (float, int)) and math.isfinite(row["ceiling"]) and row["ceiling"] > 0,
                    f"{key}: no qualified ceiling")
    require(digest(ROOT / policy.BOUNDARY["source"]) == policy.BOUNDARY["sha256"],
            "boundary workload checksum differs")
    require(policy.MONTY_BINDING is True and policy.MONTY_MAX_RATIO == 3.0,
            "the switch gate requires a binding 3.00x Monty ceiling")
    require(policy.MONTY_REPORT_RATIO == 2.0, "the switch gate reports PY rows above 2.00x Monty")
    require(set(policy.MONTY_EXEMPT_ROWS) <= {key for key in policy.ROWS if key.startswith("python/")},
            "only tracked PY rows can be exempt from the Monty rule")
    return replacement


def boundary_pair(checkpoint_base, candidate, instrument):
    """The boundary rows' sides. Their base is the pinned checkpoint base, not
    the land's base, so the instrument-only mode does not apply to them."""
    return checkpoint_base, candidate


def boundary(base, candidate, out):
    receipts = []
    captures = {"base": {"list": [], "map": []}, "candidate": {"list": [], "map": []}}
    # Each adjacent base/candidate pair uses the same physical core. There is
    # no concurrent boundary work; alternate order to avoid a fixed time bias.
    cpu = cpus()[0]
    for repeat in range(policy.BOUNDARY["samples"]):
        sides = (("base", base), ("candidate", candidate))
        for side, artifact in sides if repeat % 2 == 0 else reversed(sides):
            stem = Path(out) / f"boundary-{side}-{repeat}"
            with stem.with_suffix(".log").open("w") as log:
                subprocess.run(["bash", "-c", 'ulimit -v 8388608; exec "$@"', "--",
                                "taskset", "-c", str(cpu), artifact["binary"], "boundary",
                                "--case", "kernel-cold", "--operations", "1",
                                "--out", str(stem.with_suffix(".json")),
                                "--store-dir", str(stem.with_suffix(".store"))],
                               cwd=ROOT, stdout=log, stderr=subprocess.STDOUT, check=True, timeout=600)
            receipt = read(stem.with_suffix(".json"))
            receipts.append(str(stem.with_suffix(".json")))
            evidence = receipt["evidence"]
            require(receipt["population"] == 1 and evidence["source_sha256"] == policy.BOUNDARY["sha256"] and
                    evidence["charge"] == "exact" and evidence["memory_meter"] == "on" and
                    set(evidence["rows"]) == {"list", "map"}, "boundary geometry or work differs")
            for key, rows in evidence["rows"].items():
                require(len(rows) == 1, f"{side}/{key}: incomplete boundary population")
                row = rows[0]
                require(row["elements"] == 200_000, f"{side}/{key}: wrong boundary population size")
                require(row["status"] in ("supported", "unsupported"), f"{side}/{key}: missing completion status")
                if row["status"] == "supported":
                    require(row["results"] == [100_003, -1], f"{side}/{key}: wrong boundary equal-work result")
                    for field in ("seal_instructions", "seal_ns", "bytes_written"):
                        require(type(row.get(field)) is int and row[field] > 0,
                                f"{side}/{key}: missing checkpoint measurement {field}")
                captures[side][key].append(row)
    report, errors = {}, []
    for key in ("list", "map"):
        old, new = captures["base"][key], captures["candidate"][key]
        for row in new:
            try:
                require(row["status"] == "supported", f"candidate/{key}: {row.get('error', 'incomplete point edit')}")
                check_cold_access(key, row)
            except ValueError as error:
                errors.append(str(error))
        checkpoint, ratios, failed = checkpoint_outcome(key, old, new)
        errors.extend(failed)
        print(f"perf boundary {key}: {checkpoint['outcome']}"
              + (f"; baseline {checkpoint['baseline']}" if "baseline" in checkpoint else f"; ratios {ratios}"),
              flush=True)
        report[key] = dict(base=old, candidate=new, checkpoint=checkpoint, checkpoint_ratios=ratios,
                           base_receipts=[path for path in receipts if "boundary-base-" in path])
    write(Path(out) / "boundary.json", dict(base=base, candidate=candidate, rows=report, failures=errors))
    require(not errors, "; ".join(errors))
    return report


def capture(artifact, side, key, row, repeat, cpu, refs, out, deadline):
    stem = Path(out) / f"{side}-{key.replace('/', '-')}-{repeat}"
    command = ["bash", "-c", 'ulimit -v 8388608; exec "$@"', "--",
               *policy.REFERENCE_ENVIRONMENT["personality"], artifact["binary"], "compare", "--out", str(stem.with_suffix(".json")), "--instructions",
               "--runs", "1", "--warmups", "1", "--factor", "20", "--dialect", key.split("/")[0],
               "--workload", key.split("/")[1], "--cpu", str(cpu), *refs["args"]]
    remaining = deadline - time.monotonic()
    require(remaining > 0, f"{side}/{key}/{repeat}: measurement deadline exceeded")
    start = time.monotonic()
    with stem.with_suffix(".log").open("w") as log:
        process = subprocess.Popen(command, cwd=ROOT, stdout=log, stderr=subprocess.STDOUT,
                                   env=policy.REFERENCE_ENVIRONMENT["env"], start_new_session=True)
        try:
            process.wait(timeout=remaining)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
            raise ValueError(f"{side}/{key}/{repeat}: measurement deadline exceeded") from None
    require(process.returncode == 0, f"{side}/{key}/{repeat}: compare failed ({process.returncode}); {stem.with_suffix('.log')}")
    receipt = read(stem.with_suffix(".json"))
    require(not receipt["errors"] and not receipt["unsupported"], f"{side}/{key}: incomplete comparison")
    require(receipt["cpu"] == f"pinned:{cpu}", f"{side}/{key}: CPU pin failed")
    geometry = receipt["geometry"]
    require(geometry["runs"] == 1 and geometry["warmups"] == 1 and geometry["factor"] == 20 and
            geometry["charge"] == "exact" and geometry["memory_meter"] == "on" and
            geometry["observation"] == "off" and geometry["memory_bytes"] == 512 << 20 and
            geometry["charge_limit"] == 10_000_000_000, f"{side}/{key}: measurement geometry differs")
    engines = {r["engine"]: r for r in receipt["rows"]}
    require(len(receipt["rows"]) == 1 + len(refs["engines"]) and set(engines) == {"lash", *refs["engines"]}, f"{side}/{key}: missing equal-work reference")
    lash = engines["lash"]
    trips = [row["lo"], 20 * row["lo"]]
    operations = [n * row["ops"] for n in trips]
    for engine, identity in {"lash": artifact["binary_sha256"], **refs["engines"]}.items():
        result = engines[engine]
        require(result["dialect"] == key.split("/")[0] and result["workload"] == key.split("/")[1] and
                result["source_hash"] == row["sha256"] and result["trips"] == trips and
                result["operations"] == operations and result["engine_identity"] == identity and
                len(result["samples"]) == 1 and len(result["samples"][0]) == 2,
                f"{side}/{key}: source, work, engine or population differs")
    pair = lash["samples"][0]
    for engine in refs["engines"]:
        require([s["result"] for s in pair] == [s["result"] for s in engines[engine]["samples"][0]],
                f"{side}/{key}/{engine}: wrong equal-work result")
    for sample in pair:
        window = sample["instruction_window"]
        require(window["time_enabled"] > 0 and .99 <= window["time_running"] / window["time_enabled"] <= 1 and
                window["instructions"] == sample["instructions"] and sample["instructions"] > 0,
                f"{side}/{key}: unqualified instructions:u window")
    secants = {}
    for engine, result in engines.items():
        low, high = result["samples"][0]
        require(all(type(s["instructions"]) is int and s["instructions"] > 0 for s in (low, high)),
                f"{side}/{key}/{engine}: missing instruction count")
        secant = (high["instructions"] - low["instructions"]) / (19 * row["lo"] * row["ops"])
        require(math.isfinite(secant) and secant > 0, f"{side}/{key}/{engine}: invalid secant")
        secants[engine] = secant
    return dict(side=side, row=key, repeat=repeat, cpu=cpu, secant=secants["lash"], reference_secants={k: v for k, v in secants.items() if k != "lash"},
                results=[s["result"] for s in pair], wall_seconds=time.monotonic() - start,
                capture=str(stem.with_suffix(".json")))


def judge(samples, calibrate, sides=("base", "candidate"), exempt=None):
    """Hold every row of each gated side to its repeat-noise bound, its
    ceiling and the reference ratio, and each candidate row with equal work
    to its base's median times 1 + BASE_MARGIN unless a reviewed replacement
    exempts it (`exempt`: row -> reviewer). Answers the rows, the failures
    and the report-only notes."""
    rows, errors, notes = {}, [], []
    for side in sides:
        for key, row in policy.ROWS.items():
            population = [s for s in samples if s["side"] == side and s["row"] == key]
            if len(population) != 5:
                errors.append(f"{side}/{key}: incomplete population ({len(population)}/5)")
                continue
            values = [s["secant"] for s in population]
            median = statistics.median(values)
            noise = (max(values) - min(values)) / median
            result = dict(median=median, noise=noise, ceiling=row["ceiling"], samples=values)
            rows[f"{side}/{key}"] = result
            print(f"perf {side}/{key}: median={median:.6f}, ceiling={row['ceiling']}, noise={noise:.4%}", flush=True)
            if noise >= policy.NOISE:
                errors.append(f"{side}/{key}: repeat noise {noise:.4%} >={policy.NOISE:.0%}; calibration required")
            if not all(s["results"] == population[0]["results"] for s in population):
                errors.append(f"{side}/{key}: nondeterministic equal-work result")
            reference_medians = {}
            for engine in population[0]["reference_secants"]:
                counts = [s["reference_secants"][engine] for s in population]
                center = statistics.median(counts)
                reference_medians[engine] = center
                if (max(counts) - min(counts)) / center >= policy.NOISE:
                    errors.append(f"{side}/{key}/{engine}: repeat noise >={policy.NOISE:.0%}; calibration required")
            result["references"] = reference_medians
            result["ratios"] = {engine: median / count for engine, count in reference_medians.items()}
            try:
                note = check_instruction_ceiling(side, key, median, row["ceiling"],
                                                 reference_medians.get("monty"), calibrate)
            except ValueError as error:
                errors.append(str(error))
            else:
                if note:
                    notes.append(note)
                    print(f"perf report-only: {note}", flush=True)
    for key in policy.MONTY_EXEMPT_ROWS:
        ratios = [rows.get(f"{side}/{key}", {}).get("ratios", {}).get("monty") for side in sides]
        if all(ratio is not None and ratio <= policy.MONTY_MAX_RATIO for ratio in ratios):
            errors.append(f"{key}: exemption no longer needed: remove it "
                          f"(at or under {policy.MONTY_MAX_RATIO:.2f}x on both sides)")
    for key in policy.ROWS:
        populations = [[s["results"] for s in samples if s["side"] == side and s["row"] == key]
                       for side in sides]
        if all(populations) and any(population[0] != populations[0][0] for population in populations):
            errors.append(f"{key}: base and candidate equal-work results differ")
            continue
        base, candidate = rows.get(f"base/{key}"), rows.get(f"candidate/{key}")
        if base is None or candidate is None:
            continue
        candidate["base_ratio"] = ratio = candidate["median"] / base["median"]
        if key in (exempt or {}):
            candidate["exempt_reviewer"] = exempt[key]
            note = (f"candidate/{key}: {ratio:.6f}x base, exempt from the {policy.BASE_MARGIN:.0%} base margin "
                    f"by the reviewed ceiling replacement of {exempt[key]}")
            notes.append(note)
            print(f"perf {note}", flush=True)
        elif ratio >= 1 + policy.BASE_MARGIN:
            errors.append(f"candidate/{key}: median {candidate['median']:.6f} is {ratio:.6f}x the base's "
                          f"{base['median']:.6f}, not below {1 + policy.BASE_MARGIN:.2f}x; "
                          f"a reviewed replacement of its ceiling is required")
    return rows, errors, notes


def record_instrument(old_samples, rows):
    """The base's instrument on every row, beside the candidate's: recorded,
    never gating. Answers the per-row record and the rows that shift >0.5%."""
    record, shifted = {}, []
    for key in policy.ROWS:
        values = [s["secant"] for s in old_samples if s["row"] == key]
        new = rows.get(f"candidate/{key}")
        if not values or new is None:
            record[key] = dict(old_samples=values, incomplete=True)
            continue
        median = statistics.median(values)
        shift = (new["median"] - median) / median
        record[key] = dict(old_median=median, old_noise=(max(values) - min(values)) / median,
                           old_samples=values, new_median=new["median"], shift=shift,
                           old_results=sorted({json.dumps(s["results"]) for s in old_samples if s["row"] == key}))
        if abs(shift) > .005:
            shifted.append(key)
    return dict(rows=record, shifted=shifted)


def instrument_verdict(samples, old_samples, calibrate=False, exempt=None):
    """An instrument-only land: both gated sides ran the candidate's binary
    and are judged exactly as any land; the base's instrument is recorded
    and adds no failure."""
    rows, errors, notes = judge(samples, calibrate, exempt=exempt)
    return rows, errors, notes, record_instrument(old_samples, rows)


def collect(tasks, out, deadline):
    """Run every (side, artifact, row key, row, repeat) capture on the pinned
    cores. Answers the samples and the capture failures."""
    refs, cores = references(out), cpus()
    assigned = queue.Queue()
    for cpu in cores:
        assigned.put(cpu)
    local = threading.local()

    def initialize():
        local.cpu = assigned.get_nowait()

    def run(task):
        side, artifact, key, row, repeat = task
        return capture(artifact, side, key, row, repeat, local.cpu, refs[key.split("/")[0]], out, deadline)
    samples, errors = [], []
    with concurrent.futures.ThreadPoolExecutor(max_workers=len(cores), initializer=initialize) as pool:
        futures = {pool.submit(run, task): task for task in sorted(tasks, key=lambda t: policy.COST_ORDER.index(t[2]))}
        for future in concurrent.futures.as_completed(futures):
            task = futures[future]
            try:
                samples.append(future.result())
            except (ValueError, OSError, KeyError) as error:
                errors.append(f"{task[0]}/{task[2]}/{task[4]}: {error}")
    return samples, errors, cores


def measure(base_path, candidate_path, out, calibrate=False, instrument=False):
    """Gate the candidate against the base. In an instrument-only land both
    sides run the candidate's binary, and the base's binary is measured once
    more, recorded beside them, gating nothing."""
    base, candidate = load_artifact(base_path), load_artifact(candidate_path)
    exempt = exemptions(contract(base, calibrate))
    measured = candidate if instrument else base
    tasks = [(side, artifact, key, row, repeat) for key, row in policy.ROWS.items()
             for repeat in range(5) for side, artifact in (("base", measured), ("candidate", candidate))]
    start = time.monotonic()
    samples, errors, cores = collect(tasks, out, start + 600)
    report = dict(base=base, candidate=candidate, configuration=policy.CONFIGURATION, cpus=cores,
                  samples=samples, failures=errors, rows={},
                  mode="instrument-only" if instrument else "product",
                  base_relative=("instrument-only land: both sides ran the candidate's binary, so the "
                                 "base-relative check ran and passes trivially" if instrument else
                                 "candidate's binary against the base's, every row with equal work"),
                  exemptions=exempt,
                  base_side_binary=measured,
                  producer_commit_field="working checkout only; artifact source identities are recorded above",
                  policy=dict(scope=policy.SCOPE, build_flags=policy.BUILD_FLAGS, rows=policy.ROWS,
                              compiler=policy.COMPILER, engine=policy.ENGINE_VERSION,
                              references=dict(quickjs=policy.QUICKJS_REVISION, python=policy.PYTHON_VERSION,
                                              monty=policy.MONTY_VERSION, monty_binding=policy.MONTY_BINDING,
                                              environment=policy.REFERENCE_ENVIRONMENT,
                                              monty_max_ratio=policy.MONTY_MAX_RATIO,
                                              monty_report_ratio=policy.MONTY_REPORT_RATIO)),
                  monty_report=[])
    report["wall_seconds"] = time.monotonic() - start
    if instrument:
        old_start = time.monotonic()
        old_samples, old_errors, _ = collect(
            [("old", base, key, row, repeat) for key, row in policy.ROWS.items() for repeat in range(5)],
            out, old_start + 600)
        rows, failed, notes, old = instrument_verdict(samples, old_samples, calibrate, exempt)
        old["collection_errors"] = old_errors
        old["wall_seconds"] = time.monotonic() - old_start
        old["note"] = ("perf instrument-only land: both sides measured with the candidate's instrument; "
                       "the base's instrument is recorded and gates nothing")
        report["old_instrument"] = old
    else:
        rows, failed, notes = judge(samples, calibrate, exempt=exempt)
    report["rows"], report["monty_report"] = rows, notes
    errors.extend(failed)
    if report["wall_seconds"] > 600:
        errors.append(f"base plus candidate measurement exceeded 600s: {report['wall_seconds']:.2f}s")
    expected = len(policy.ROWS) * 10
    require(len(samples) == expected or errors, "incomplete gate population")
    write(Path(out) / "receipt.json", report)
    kind = "calibration" if calibrate else "gate"
    print(f"perf {kind}: {report['wall_seconds']:.2f}s, {len(samples)}/{expected} pairs, receipt={Path(out) / 'receipt.json'}", flush=True)
    print(f"perf base-relative: {report['base_relative']}", flush=True)
    if instrument:
        print(report["old_instrument"]["note"], flush=True)
        for key, entry in report["old_instrument"]["rows"].items():
            if "shift" in entry:
                print(f"perf old instrument {key}: median={entry['old_median']:.6f}, noise={entry['old_noise']:.4%}, "
                      f"shift={entry['shift']:+.3%}", flush=True)
        shifted = report["old_instrument"]["shifted"]
        print("perf instrument-only land: rows shifting more than 0.5%: " + (", ".join(
            f"{key} {report['old_instrument']['rows'][key]['shift']:+.3%}" for key in shifted) or "none"), flush=True)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", default="origin/main")
    parser.add_argument("--out", type=Path)
    parser.add_argument("--base-artifact", type=Path)
    parser.add_argument("--candidate-artifact", type=Path)
    parser.add_argument("--calibrate", action="store_true", help="report medians without enforcing unseeded ceilings")
    args = parser.parse_args()
    out = (args.out or ROOT / ".kiln/perf-land" / f"{time.time_ns()}-{os.getpid()}").resolve()
    out.mkdir(parents=True, exist_ok=True)
    require(bool(args.base_artifact) == bool(args.candidate_artifact), "supply both artifact receipts or neither")
    if args.base_artifact:
        base, candidate = args.base_artifact, args.candidate_artifact
    else:
        require(not git("status", "--porcelain", "--untracked-files=all"), "land measurement requires a clean tree")
        head = git("rev-parse", "HEAD")
        base = build(args.base, out)
        candidate = build(head, out)
        require(git("rev-parse", "HEAD") == head, "candidate checkout was not restored")
    require(policy.CHECKPOINT_BASE is not None, "checkpoint base has not been pinned")
    commits = (read(base)["commit"], read(candidate)["commit"])
    instrument = None not in commits and instrument_only(*land_paths(*commits))
    checkpoint_base = load_artifact(build(policy.CHECKPOINT_BASE, out))
    result = measure(base, candidate, out, args.calibrate, instrument)
    try:
        result["boundary"] = boundary(*boundary_pair(checkpoint_base, load_artifact(candidate), instrument), out)
    except (ValueError, OSError, KeyError, subprocess.SubprocessError) as error:
        result["failures"].append(f"boundary: {error}")
    result["status"] = "failed" if result["failures"] else ("calibrated" if args.calibrate else "qualified")
    write(out / "receipt.json", result)
    require(not result["failures"], "; ".join(result["failures"]))
    if not args.base_artifact:
        current = git("rev-parse", "HEAD")
        require(current == head, f"candidate changed during measurement: {head} -> {current}")
        require(not git("status", "--porcelain", "--untracked-files=all"),
                "candidate working tree changed during measurement; rerun scripts/land")
    # scripts/land folds this into the land as its own commit.
    result["ratchet"] = {} if args.calibrate else ratchet(result["rows"])
    for key, ceiling in result["ratchet"].items():
        print(f"perf ratchet {key}: ceiling {policy.ROWS[key]['ceiling']} -> {ceiling}", flush=True)
    write(out / "receipt.json", result)


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, KeyError, subprocess.SubprocessError) as error:
        sys.exit(f"perf land gate: FAIL {error}")

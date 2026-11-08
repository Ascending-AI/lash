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


def load_artifact(path):
    value = read(path)
    require(value["configuration"] == policy.CONFIGURATION, "artifact configuration differs from ceilings")
    require(digest(value["binary"]) == value["binary_sha256"], "artifact binary checksum differs")
    require(git("cat-file", "-t", value["tree"]) == "tree", "artifact source tree is unavailable")
    if value["commit"] is not None:
        require(value["tree"] == git("rev-parse", value["commit"] + "^{tree}"), "artifact source tree differs")
    require(value["compiler"] == policy.COMPILER, "artifact compiler differs from ceilings")
    return value


def build(commit, out):
    commit = git("rev-parse", commit + "^{commit}")
    directory = CACHE / "artifacts" / commit
    directory.mkdir(parents=True, exist_ok=True)
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
                subprocess.run([*BUILD_ARGS, "--build-report", str(report)], cwd=ROOT,
                               stdout=output, stderr=subprocess.STDOUT, check=True)
            require(git("rev-parse", "HEAD") == commit and
                    not git("status", "--porcelain", "--untracked-files=all"),
                    "source tree changed during artifact construction")
            spec = importlib.util.spec_from_file_location("kiln_outputs", ROOT / "tools/buck2/outputs.py")
            module = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(module)
            outputs = module.resolve(read(report), LABEL)
            require(len(outputs) == 1, "optimized build did not produce one executable")
            tmp = directory / f"lash_perf.{os.getpid()}.tmp"
            shutil.copyfile(outputs[0], tmp)
            tmp.chmod(0o755)
            os.replace(tmp, directory / "lash_perf")
            artifact = dict(commit=commit, tree=git("rev-parse", "HEAD^{tree}"),
                            binary=str(directory / "lash_perf"), binary_sha256=digest(directory / "lash_perf"),
                            configuration=policy.CONFIGURATION, compiler=compiler,
                            build_command=BUILD_ARGS, build_seconds=time.monotonic() - start)
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
        if not qjs.exists():
            with (Path(out) / "references-build.log").open("w") as log:
                subprocess.run(["bash", "-c", 'ulimit -v 8388608; exec "$@"', "--", sys.executable,
                                "scripts/build-compare-references.py", "--cache", str(directory),
                                "--engine", "quickjs"], cwd=ROOT, stdout=log, stderr=subprocess.STDOUT, check=True)
        require(digest(qjs) == policy.QUICKJS_SHA256, "QuickJS executable differs from frozen reference")
    python = Path(shutil.which("python3")).resolve()
    require(digest(python) == policy.PYTHON_SHA256, "CPython executable differs from frozen reference")
    version = subprocess.check_output([str(python), "--version"], text=True).strip()
    require(version == policy.PYTHON_VERSION, "CPython version differs from frozen reference")
    return {"python": ("--python", str(python), "cpython", policy.PYTHON_SHA256),
            "typescript": ("--qjs", str(qjs), "qjs-0", policy.QUICKJS_SHA256)}


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
    # Main may advance after calibration. A bootstrap must descend from the
    # reviewed seed and cannot replace a previously published policy.
    old = subprocess.run(["git", "show", f"{base['commit']}:scripts/perf_land_ceilings.py"],
                         cwd=ROOT, capture_output=True, text=True)
    if old.returncode:
        require(subprocess.run(["git", "merge-base", "--is-ancestor", policy.SEED_COMMIT, base["commit"]],
                               cwd=ROOT, capture_output=True).returncode == 0,
                f"bootstrap seed {policy.SEED_COMMIT} is not an ancestor of base {base['commit']}")
        require(not git("log", "-1", "--format=%H", base["commit"], "--", "scripts/perf_land_ceilings.py"),
                "base previously published a ceilings policy; reviewed replacement required")
    else:
        namespace = {node.targets[0].id: ast.literal_eval(node.value)
                     for node in ast.parse(old.stdout).body
                     if isinstance(node, ast.Assign) and len(node.targets) == 1 and isinstance(node.targets[0], ast.Name)}
        for field in ("CONFIGURATION", "COMPILER", "BUILD_FLAGS", "ENGINE_VERSION", "QUICKJS_REVISION", "QUICKJS_SHA256",
                      "PYTHON_SHA256", "PYTHON_VERSION", "MONTY_VERSION", "MONTY_SHA256", "WORKERS", "CPU_MODEL", "SCOPE", "COST_ORDER"):
            require(namespace[field] == getattr(policy, field), f"{field}: measurement contract changed; reviewed replacement required")
        prior = namespace["ROWS"]
        require(set(prior) == set(policy.ROWS), "tracked rows changed; reviewed replacement required")
        for key, row in policy.ROWS.items():
            old_row = prior[key]
            require({k: v for k, v in row.items() if k != "ceiling"} ==
                    {k: v for k, v in old_row.items() if k != "ceiling"},
                    f"{key}: workload contract changed; reviewed replacement required")
            require(row["ceiling"] <= old_row["ceiling"], f"{key}: ordinary lands cannot raise ceilings")
    for path in ("tools/buck2/lash_rust.bzl", "tools/buck2/third_party.bzl"):
        flags = re.search(r"_OPTIMIZED_FLAGS = (\[[\s\S]*?\n\])", (ROOT / path).read_text())
        require(flags is not None and ast.literal_eval(flags[1]) == policy.BUILD_FLAGS,
                f"{path}: optimized flags differ from frozen configuration")
    for key, row in policy.ROWS.items():
        require(digest(ROOT / row["source"]) == row["sha256"], f"{key}: frozen workload checksum differs")
        if not calibrate:
            require(isinstance(row["ceiling"], (float, int)) and math.isfinite(row["ceiling"]) and row["ceiling"] > 0,
                    f"{key}: no qualified ceiling")


def capture(artifact, side, key, row, repeat, cpu, refs, out, deadline):
    stem = Path(out) / f"{side}-{key.replace('/', '-')}-{repeat}"
    command = ["bash", "-c", 'ulimit -v 8388608; exec "$@"', "--", artifact["binary"], "compare", "--out", str(stem.with_suffix(".json")), "--instructions",
               "--runs", "1", "--warmups", "1", "--factor", "20", "--dialect", key.split("/")[0],
               "--workload", key.split("/")[1], "--cpu", str(cpu), *refs[:2]]
    remaining = deadline - time.monotonic()
    require(remaining > 0, f"{side}/{key}/{repeat}: measurement deadline exceeded")
    start = time.monotonic()
    with stem.with_suffix(".log").open("w") as log:
        process = subprocess.Popen(command, cwd=ROOT, stdout=log, stderr=subprocess.STDOUT,
                                   start_new_session=True)
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
    require(len(receipt["rows"]) == 2 and set(engines) == {"lash", refs[2]}, f"{side}/{key}: missing equal-work reference")
    lash, reference = engines["lash"], engines[refs[2]]
    trips = [row["lo"], 20 * row["lo"]]
    operations = [n * row["ops"] for n in trips]
    for result, identity in ((lash, artifact["binary_sha256"]), (reference, refs[3])):
        require(result["dialect"] == key.split("/")[0] and result["workload"] == key.split("/")[1] and
                result["source_hash"] == row["sha256"] and result["trips"] == trips and
                result["operations"] == operations and result["engine_identity"] == identity and
                len(result["samples"]) == 1, f"{side}/{key}: source, work, engine or population differs")
    pair, oracle = lash["samples"][0], reference["samples"][0]
    require([s["result"] for s in pair] == [s["result"] for s in oracle], f"{side}/{key}: wrong equal-work result")
    for sample in pair:
        window = sample["instruction_window"]
        require(window["time_enabled"] > 0 and .99 <= window["time_running"] / window["time_enabled"] <= 1 and
                window["instructions"] == sample["instructions"] and sample["instructions"] > 0,
                f"{side}/{key}: unqualified instructions:u window")
    secant = (pair[1]["instructions"] - pair[0]["instructions"]) / (19 * row["lo"] * row["ops"])
    require(math.isfinite(secant) and secant > 0, f"{side}/{key}: invalid secant")
    return dict(side=side, row=key, repeat=repeat, cpu=cpu, secant=secant,
                results=[s["result"] for s in pair], wall_seconds=time.monotonic() - start,
                capture=str(stem.with_suffix(".json")))


def measure(base_path, candidate_path, out, calibrate=False):
    base, candidate = load_artifact(base_path), load_artifact(candidate_path)
    contract(base, calibrate)
    refs, cores = references(out), cpus()
    tasks = [(side, artifact, key, row, repeat) for key, row in policy.ROWS.items()
             for repeat in range(5) for side, artifact in (("base", base), ("candidate", candidate))]
    tasks.sort(key=lambda t: policy.COST_ORDER.index(t[2]))
    assigned = queue.Queue()
    for cpu in cores:
        assigned.put(cpu)
    local = threading.local()
    def initialize():
        local.cpu = assigned.get_nowait()
    start = time.monotonic()
    deadline = start + 600
    samples, errors = [], []
    report = dict(base=base, candidate=candidate, configuration=policy.CONFIGURATION, cpus=cores,
                  samples=samples, failures=errors, rows={},
                  producer_commit_field="working checkout only; artifact source identities are recorded above",
                  policy=dict(scope=policy.SCOPE, build_flags=policy.BUILD_FLAGS, rows=policy.ROWS,
                              compiler=policy.COMPILER, engine=policy.ENGINE_VERSION,
                              references=dict(quickjs=policy.QUICKJS_REVISION, python=policy.PYTHON_VERSION,
                                              monty_recorded_only=policy.MONTY_VERSION)))
    def run(task):
        side, artifact, key, row, repeat = task
        return capture(artifact, side, key, row, repeat, local.cpu, refs[key.split("/")[0]], out, deadline)
    with concurrent.futures.ThreadPoolExecutor(max_workers=len(cores), initializer=initialize) as pool:
        futures = {pool.submit(run, task): task for task in tasks}
        for future in concurrent.futures.as_completed(futures):
            task = futures[future]
            try:
                samples.append(future.result())
            except (ValueError, OSError, KeyError) as error:
                errors.append(f"{task[0]}/{task[2]}/{task[4]}: {error}")
            write(Path(out) / "receipt.json", report)
    report["wall_seconds"] = time.monotonic() - start
    for side in ("base", "candidate"):
        for key, row in policy.ROWS.items():
            population = [s for s in samples if s["side"] == side and s["row"] == key]
            if len(population) != 5:
                errors.append(f"{side}/{key}: incomplete population ({len(population)}/5)")
                continue
            values = [s["secant"] for s in population]
            median = statistics.median(values)
            noise = (max(values) - min(values)) / median
            result = dict(median=median, noise=noise, ceiling=row["ceiling"], samples=values)
            report["rows"][f"{side}/{key}"] = result
            print(f"perf {side}/{key}: median={median:.6f}, ceiling={row['ceiling']}, noise={noise:.4%}", flush=True)
            if noise >= .01:
                errors.append(f"{side}/{key}: repeat noise {noise:.4%} >=1%; calibration required")
            if not all(s["results"] == population[0]["results"] for s in population):
                errors.append(f"{side}/{key}: nondeterministic equal-work result")
            if not calibrate and median > row["ceiling"]:
                errors.append(f"{side}/{key}: median {median:.6f} exceeds ceiling {row['ceiling']:.6f}")
    report["wall_seconds"] = time.monotonic() - start
    if report["wall_seconds"] > 600:
        errors.append(f"base plus candidate measurement exceeded 600s: {report['wall_seconds']:.2f}s")
    require(len(samples) == 110 or errors, "incomplete gate population")
    write(Path(out) / "receipt.json", report)
    kind = "calibration" if calibrate else "gate"
    print(f"perf {kind}: {report['wall_seconds']:.2f}s, {len(samples)}/110 pairs, receipt={Path(out) / 'receipt.json'}", flush=True)
    require(not errors, "; ".join(errors))
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
    result = measure(base, candidate, out, args.calibrate)
    if not args.base_artifact:
        current = git("rev-parse", "HEAD")
        require(current == head, f"candidate changed during measurement: {head} -> {current}")
        require(not git("status", "--porcelain", "--untracked-files=all"),
                "candidate working tree changed during measurement; rerun scripts/land")
    if not args.calibrate:
        result["qualified_ceilings"] = {key: min(row["ceiling"], result["rows"][f"candidate/{key}"]["median"] * 1.03)
                                        for key, row in policy.ROWS.items()}
        write(out / "receipt.json", result)


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, KeyError, subprocess.SubprocessError) as error:
        sys.exit(f"perf land gate: FAIL {error}")

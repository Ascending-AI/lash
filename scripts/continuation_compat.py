#!/usr/bin/env python3
"""Gate continuation interpretation across two source-bound production builds.

Version 2: equal actual witnesses select resume equivalence. Changed witnesses
select typed predecessor refusal plus L5's completed deployment evidence.
"""
import argparse
from contextlib import contextmanager
import functools
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time

from perf_land_gate import ROOT, build, digest, git, load_artifact, read, require, write

LABEL = "//crates/lash-kernel-conformance:continuation_compat__bin"
FORMAT_NAMES = {"PAGE_FORMAT", "KERNEL_PARKED_STATE_VERSION", "KERNEL_SAVED_FUNCTION_VERSION",
                "KERNEL_DOCUMENT_SCHEMA_VERSION", "LASH_KERNEL_VERSION", "WORKER_PROTOCOL_VERSION",
                "PAGE_DIGEST_DOMAIN"}
OBSERVATION_FIELDS = {"prints", "end", "trace", "charged", "charge_tape", "steps"}


@contextmanager
def revision_checkout(revision):
    branch = subprocess.run(["git", "symbolic-ref", "--quiet", "--short", "HEAD"],
                            cwd=ROOT, capture_output=True, text=True)
    previous = branch.stdout.strip() if branch.returncode == 0 else git("rev-parse", "HEAD")
    switched = git("rev-parse", "HEAD") != revision
    if switched:
        subprocess.run(["git", "checkout", "--detach", revision], cwd=ROOT, check=True)
    try:
        yield
    finally:
        if switched:
            subprocess.run(["git", "checkout", previous], cwd=ROOT, check=True)


def has_runner(revision):
    result = subprocess.run(["git", "show", f"{revision}:crates/lash-kernel-conformance/BUCK"],
                            cwd=ROOT, capture_output=True, text=True)
    return result.returncode == 0 and re.search(r'name\s*=\s*"continuation_compat__bin"', result.stdout) is not None


@functools.lru_cache(maxsize=8)
def dependency_sources(revision, label=LABEL):
    with revision_checkout(revision):
        client = subprocess.check_output([sys.executable, "tools/buck2/bootstrap.py"], cwd=ROOT, text=True).strip()
        # uquery follows an unconfigured prelude branch to the nonexistent
        # toolchains//:cxx_no_default_deps. The lead authorized this configured
        # closure, matching BUILD_ARGS's production optimized configuration.
        command = [client, "--isolation-dir", os.environ.get("BUCK_ISOLATION_DIR", "kiln"),
                   "cquery", "--target-platforms", "//tools/buck2:optimized",
                   "-c", "kiln.execution_mode=remote", "-c", "kiln.rust_profile=optimized",
                   "-c", "kiln.perf_iteration=false", f"inputs(deps({label}))", "--json"]
        inputs = json.loads(subprocess.check_output(command, cwd=ROOT, text=True))
        tracked = set(git("ls-files", "-z").split("\0"))
        require(isinstance(inputs, list) and all(isinstance(path, str) for path in inputs),
                "unexpected Buck dependency-input response")
        sources = {path.removeprefix("root//") for path in inputs} & tracked
        require(sources, f"{label}: empty dependency source closure")
        return frozenset(sources)


def build_configuration(path):
    return (Path(path).name in {"BUCK", "Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".buckconfig"}
            or path.startswith(("tools/buck2/", ".cargo/", ".buckconfig.")))


def triggered(changed, sources):
    return sorted(path for path in changed if path in sources or build_configuration(path))


def witnesses(revision):
    from check_version_bumps import RevisionView
    from discover_version_surfaces import discover
    from release_baseline import resolve
    view = RevisionView(ROOT, revision)
    view.preload(view.matching_paths(("crates/*.rs", "examples/*.rs")))
    surfaces, _ = discover(view)
    result = {}
    for surface in surfaces:
        name = surface["constant"]
        if name in FORMAT_NAMES:
            require(name not in result, f"ambiguous format witness {name}")
            result[name] = resolve(view.content(surface["constant_path"]), name, False)
    require(set(result) == FORMAT_NAMES, f"missing format witnesses: {FORMAT_NAMES - set(result)}")
    return result


def source_mismatches(artifact, revision, bootstrap=False):
    from check_version_bumps import RevisionView
    label = artifact.get("label", LABEL)
    require(artifact["build_command"].count(label) == 1, "artifact has no unique build target")
    paths = dependency_sources(artifact["commit"], label)
    if bootstrap:
        # The only permitted overlay is the test-owned exporter. Its source
        # cannot replace any production dependency, including native helpers.
        paths = frozenset(path for path in paths if not path.startswith("crates/lash-kernel-conformance/"))
    previous, candidate = RevisionView(ROOT, artifact["commit"]), RevisionView(ROOT, revision)
    previous.preload(paths)
    candidate.preload(paths)
    mismatches = {path for path in paths if previous.content(path) != candidate.content(path)}
    if not bootstrap:
        mismatches.update(path for path in git("diff", "--name-only", artifact["commit"], revision).splitlines()
                          if build_configuration(path))
    return sorted(mismatches)


def corpus_cases(corpus):
    cases = {}
    for path in sorted(corpus.rglob("*.json")):
        shard = read(path)
        require(shard["rule"] == path.stem and shard["cases"], f"{path}: malformed corpus shard")
        for index, case in enumerate(shard["cases"]):
            key = f"{path.relative_to(corpus).as_posix()}#{index}"
            require(case["name"] and case["document"], f"{key}: empty corpus case")
            cases[key] = case["name"]
    require(cases, "empty conformance corpus")
    return cases


def files(directory):
    result = {}
    for path in directory.rglob("*"):
        require(not path.is_symlink(), f"export contains symlink: {path}")
        if path.is_file():
            result[path.relative_to(directory).as_posix()] = digest(path)
    return result


def inventory(directory, cases):
    manifest = read(directory / "manifest.json")
    require(manifest["version"] == 2, "unknown export protocol")
    seen, pauses, images = set(), set(), set()
    for case in manifest["cases"]:
        key = case["id"]
        require(key in cases and key not in seen and case["name"] == cases[key],
                f"{key}: unknown, duplicate or renamed case")
        seen.add(key)
        for pause in case["pauses"]:
            identity = (key, pause["id"])
            require(isinstance(pause["id"], str) and pause["id"] and pause["kind"] and identity not in pauses,
                    f"{identity}: invalid or duplicate pause")
            image = directory / pause["image"]
            require(image.resolve().is_relative_to(directory.resolve()) and image.is_file() and
                    image.resolve() not in images, f"{identity}: missing, escaped or shared image")
            images.add(image.resolve())
            pauses.add(identity)
    require(seen == set(cases), f"unaccounted corpus cases: {sorted(set(cases) - seen)}")
    require(pauses, "no legal-pause seals exported")
    return pauses


def runs(path, expected, resumed):
    receipt = read(path)
    require(receipt["version"] == 2, "unknown observation protocol")
    result = {}
    for run in receipt["runs"]:
        key = (run["case"], run["pause"]) if resumed else run["case"]
        require(key in expected and key not in result, f"{key}: unknown or duplicate observation")
        result[key] = run
    require(set(result) == set(expected), f"incomplete observations: {len(result)}/{len(expected)}")
    return result


def observations(path, expected, resumed):
    result = {}
    for key, run in runs(path, expected, resumed).items():
        value = run["observation"]
        require(isinstance(value, dict) and set(value) == OBSERVATION_FIELDS,
                f"{key}: incomplete actual observation")
        require(all(isinstance(value[f], list) for f in ("prints", "trace", "charge_tape", "steps")),
                f"{key}: incomplete event or step observations")
        if value["end"] == "refused":
            # An authored admission refusal is an observation of a machine
            # that never started: nothing ran, so nothing was metered.
            require(not resumed and value["charged"] is None and not value["charge_tape"] and
                    not value["steps"] and not value["trace"], f"{key}: refusal after execution")
        else:
            require(type(value["charged"]) is int and value["charged"] >= 0 and value["steps"],
                    f"{key}: incomplete event or step observations")
            require(value["charged"] == 0 or value["charge_tape"], f"{key}: missing paid-event tape")
        result[key] = value
    return result


def refusals(path, expected, before, after):
    result = runs(path, expected, True)
    for key, run in result.items():
        refusal = run["refusal"]
        require(run["opened"] is False and isinstance(refusal, dict) and
                refusal.get("error") == "OpenError" and refusal.get("kind") == "format",
                f"{key}: predecessor opened or did not produce a typed format OpenError")
        surface = refusal["surface"]
        require(surface in FORMAT_NAMES and refusal["expected"] == after[surface] and
                refusal["observed"] == before[surface] and before[surface] != after[surface],
                f"{key}: refusal is not bound to the changed actual format")
        require(run["mutations"] == [] and type(run["executed_steps"]) is int and run["executed_steps"] == 0,
                f"{key}: mutation or execution before predecessor refusal")
    return result


def deployment(path, base, head):
    receipt = read(path)
    require(receipt["version"] == 2 and receipt["base_commit"] == base and
            re.match(r"https://linear\.app/[^/]+/issue/FIG-\d+(?:/|$)", receipt["ticket_url"]),
            "deployment evidence is not linked to the change's ticket and predecessor")
    previous = load_artifact(receipt["base_artifact"])
    candidate = load_artifact(receipt["candidate_artifact"])
    require(previous["commit"] == base, "deployment evidence used a different predecessor")
    require(not source_mismatches(candidate, head), "deployment candidate production sources differ")
    evidence = receipt["evidence"]
    require(digest(evidence["path"]) == evidence["sha256"], "deployment raw evidence checksum differs")
    raw = read(evidence["path"])
    require(raw["completed"] is True and raw["base_binary_sha256"] == previous["binary_sha256"] and
            raw["candidate_binary_sha256"] == candidate["binary_sha256"] and raw["checks"] == receipt["checks"],
            "deployment run is incomplete or does not bind the observed checks to both builds")
    checks = {check["name"]: check for check in receipt["checks"]}
    require(len(checks) == len(receipt["checks"]) == 4 and
            set(checks) == {"parked_actor", "session_root", "saved_function", "started_once"},
            "incomplete base-store deployment evidence")
    for name, check in checks.items():
        require(check["result"] == "typed_refusal" and type(check["mutations"]) is int and
                check["mutations"] == 0 and type(check["reexecutions"]) is int and check["reexecutions"] == 0,
                f"{name}: deployment did not prove refusal before mutation or re-execution")
    return receipt


def execute(binary, args, log):
    with log.open("w") as output:
        subprocess.run(["bash", "-c", 'ulimit -v 8388608; exec "$@"', "--", binary, *args],
                       cwd=ROOT, stdout=output, stderr=subprocess.STDOUT, check=True, timeout=600)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", default="origin/main")
    parser.add_argument("--out", type=Path)
    parser.add_argument("--inputs", type=Path, default=ROOT / ".kiln/continuation-compat-inputs.json")
    parser.add_argument("--always", action="store_true", help="qualify even if no dependency changed")
    args = parser.parse_args()
    require(not git("status", "--porcelain", "--untracked-files=all"), "compatibility requires a clean tree")
    base, head = git("rev-parse", args.base + "^{commit}"), git("rev-parse", "HEAD")
    require(has_runner(head), "candidate has no continuation_compat runner target")
    previous_has_runner = has_runner(base)
    sources = dependency_sources(head)
    if previous_has_runner:
        sources |= dependency_sources(base)
    changed = triggered(git("diff", "--name-only", base, head).splitlines(), sources)
    # Several contract §9 witnesses live outside the runner's closure
    # (lash-vm-runtime, lash-vm-protocol). A changed witness alone still
    # selects the refusal branch and its deployment evidence.
    before, after = witnesses(base), witnesses(head)
    changed += [f"witness {name}" for name in sorted(FORMAT_NAMES) if before[name] != after[name]]
    if not changed and not args.always:
        print("continuation compatibility: no runner dependency, build configuration or format witness changed")
        return
    mode = "resume" if previous_has_runner and before == after else "refusal"
    inputs = read(args.inputs) if args.inputs.exists() else {}
    out = (args.out or ROOT / ".kiln/continuation-compat" / str(time.time_ns())).resolve()
    out.mkdir(parents=True, exist_ok=False)
    corpus = out / "corpus"
    shutil.copytree(ROOT / "crates/lash-kernel-conformance/corpus", corpus)
    cases, corpus_hashes = corpus_cases(corpus), files(corpus)
    if previous_has_runner:
        previous = load_artifact(build(base, out, LABEL))
    else:
        require("previous_artifact" in inputs, "first switch needs L1's source-bound base exporter artifact")
        previous = load_artifact(inputs["previous_artifact"])
        require(previous["engine_commit"] == base and not source_mismatches(previous, base, True),
                "bootstrap exporter's production dependency sources differ from the base")
    candidate = load_artifact(build(head, out, LABEL))
    exports = out / "exports"
    execute(previous["binary"], ["export", "--corpus", str(corpus), "--out", str(exports)], out / "export.log")
    pauses, hashes = inventory(exports, cases), files(exports)
    if mode == "resume":
        execute(candidate["binary"], ["uninterrupted", "--corpus", str(corpus), "--out", str(out / "uninterrupted.json")], out / "uninterrupted.log")
        execute(candidate["binary"], ["resume", "--corpus", str(corpus), "--imports", str(exports), "--out", str(out / "resumed.json")], out / "resume.log")
        baseline = observations(out / "uninterrupted.json", cases, False)
        resumed = observations(out / "resumed.json", pauses, True)
        failures = [dict(case=case, pause=pause) for (case, pause), value in resumed.items() if value != baseline[case]]
    else:
        execute(candidate["binary"], ["resume", "--expect-refusal",
                  "--expected", str(after["KERNEL_PARKED_STATE_VERSION"]),
                  "--observed", str(before["KERNEL_PARKED_STATE_VERSION"]), "--corpus", str(corpus), "--imports", str(exports), "--out", str(out / "refusals.json")], out / "refusal.log")
        refusals(out / "refusals.json", pauses, before, after)
        require("deployment_receipt" in inputs, "changed formats require L5's ticket-linked deployment receipt")
        deployment(inputs["deployment_receipt"], base, head)
        failures = []
    require(files(exports) == hashes and files(corpus) == corpus_hashes, "runner changed frozen inputs")
    report = dict(previous=previous, candidate=candidate, mode=mode, witnesses=dict(previous=before, candidate=after),
                  changed=changed, dependency_inputs=sorted(sources), corpus=corpus_hashes,
                  exports=hashes, cases=len(cases), pauses=len(pauses), failures=failures)
    write(out / "receipt.json", report)
    require(git("rev-parse", "HEAD") == head and not git("status", "--porcelain", "--untracked-files=all"),
            "candidate changed during compatibility qualification")
    require(not failures, f"cross-compiler continuation mismatch: {failures}")
    print(f"continuation compatibility: PASS {mode}, {len(cases)} cases, {len(pauses)} pauses; {out / 'receipt.json'}")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, KeyError, TypeError, RuntimeError, subprocess.SubprocessError) as error:
        sys.exit(f"continuation compatibility: FAIL {error}")

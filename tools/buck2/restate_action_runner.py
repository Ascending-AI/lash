#!/usr/bin/env python3
"""Run one registered Restate suite shard with its declared server input."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import sys
import xml.etree.ElementTree as ET


def select(names: list[str], arguments: list[str], count: int, index: int) -> list[str]:
    filters, skips = [], []
    options = iter(arguments)
    for option in options:
        if option == "--skip":
            skips.append(next(options))
        elif option.startswith("--skip="):
            skips.append(option.removeprefix("--skip="))
        elif option in {"--exact", "--ignored", "--include-ignored", "--nocapture"}:
            continue
        elif option.startswith("-"):
            raise ValueError(f"unsupported Restate suite selector: {option}")
        elif option:
            filters.append(option)
    exact = "--exact" in arguments
    matches = lambda name, pattern: name == pattern if exact else pattern in name
    chosen = [name for name in names if not any(matches(name, skip) for skip in skips)]
    for pattern in filters:
        if not any(matches(name, pattern) for name in chosen):
            raise ValueError(f"no registered law matches {pattern!r}")
    chosen = [name for name in chosen if not filters or any(matches(name, pattern) for pattern in filters)]
    if not chosen:
        raise ValueError("no registered laws matched")
    return [name for name in chosen if int.from_bytes(hashlib.sha256(name.encode()).digest()[:8]) % count == index]


def write_report(summary: dict, artifacts: Path, code: int) -> None:
    report = ET.Element("testsuite", name=os.environ["TEST_TARGET"])
    passed = failed = skipped = 0
    print(f"running {len(summary['tests'])} tests")
    for outcome in summary["tests"]:
        name = outcome["name"]
        case = ET.SubElement(report, "testcase", name=name, classname=os.environ["TEST_TARGET"], time=str(outcome["seconds"]))
        output = (artifacts / f"{name.replace('::', '__')}.log").read_text(errors="replace")
        ET.SubElement(case, "system-out").text = output
        held = name in summary["held"] and outcome["status"] not in {"ok", "leftovers", "teardown_failed"}
        if held:
            skipped += 1
            ET.SubElement(case, "skipped", message="registered replay divergence")
            verdict = "ignored"
        elif outcome["status"] == "ok" and name not in summary["healed"]:
            passed += 1
            verdict = "ok"
        else:
            failed += 1
            ET.SubElement(case, "failure", message=outcome["status"]).text = output
            verdict = "FAILED"
        print(f"test {name} ... {verdict}")
    if code and not failed:
        case = ET.SubElement(report, "testcase", name=os.environ["TEST_TARGET"], classname=os.environ["TEST_TARGET"])
        ET.SubElement(case, "error", message="suite did not complete")
    report.set("tests", str(len(report.findall("testcase"))))
    report.set("failures", str(failed))
    report.set("skipped", str(skipped))
    report.set("errors", str(len(report.findall("testcase/error"))))
    destination = Path(os.environ["XML_OUTPUT_FILE"])
    destination.parent.mkdir(parents=True, exist_ok=True)
    ET.ElementTree(report).write(destination, encoding="utf-8", xml_declaration=True)
    print(f"test result: {'ok' if not code else 'FAILED'}. {passed} passed; {failed} failed; {skipped} ignored; 0 measured; 0 filtered out")


def main(argv: list[str]) -> int:
    inputs, server, name, leg, count, index, *command = argv
    inputs = Path(inputs).resolve()
    sys.path.insert(0, str(inputs / "scripts/ci"))
    import restate_suite as runner

    runner.ROOT = Path.cwd()
    runner.REGISTRY = inputs / "scripts/restate-suites.toml"
    runner.DIVERGENCE_DIR = inputs / "scripts/restate-divergences"
    os.environ["LASH_RESTATE_SERVER_BIN"] = str(Path(server).resolve())
    # A remote action must never build or download undeclared inputs.
    if not os.environ.get("LASH_VM_WORKER"):
        raise ValueError("the suite action is missing its declared VM worker")
    os.environ["LASH_VM_WORKER"] = str((runner.ROOT / os.environ["LASH_VM_WORKER"]).resolve())
    marker = command.index("--lash-libtest-args")
    if marker != 1:
        raise ValueError("a Restate suite expects one native test binary before its selectors")
    binary = Path(command[0]).resolve()
    suite = runner.load_suite(name)
    every = runner.list_tests(binary, runner.ROOT / suite.cwd, suite.filters, suite.skips)
    selected = select(every, command[marker + 1:], int(count), int(index))
    artifacts_root = Path(os.environ["TEST_UNDECLARED_OUTPUTS_DIR"]).resolve()
    artifacts = artifacts_root / f"{name}-{leg}"
    if not selected:
        artifacts.mkdir(parents=True, exist_ok=True)
        write_report({"tests": [], "held": [], "healed": []}, artifacts, 0)
        return 0

    def terminated(number, _frame):
        raise SystemExit(128 + number)

    for number in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        signal.signal(number, terminated)
    code = runner.run_suite(suite, leg, argparse.Namespace(
        binary=str(binary), artifacts=str(artifacts_root), selected_tests=selected,
        only=[], shards=1, timeout=None, include_divergent=False,
        keep_test_logs=True, server_env=[], tail_lines=60,
    ))
    summary_path = artifacts / "summary.json"
    if summary_path.exists():
        write_report(json.loads(summary_path.read_text()), artifacts, code)
    return code


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1:]))
    except (OSError, ValueError) as error:
        print(f"restate_action_runner: {error}", file=sys.stderr)
        sys.exit(70)

#!/usr/bin/env python3
"""Validate selected libtest cases and count observed, non-ignored execution."""

from collections import Counter
import json
import os
from pathlib import Path
import re
import subprocess
import sys
from urllib.parse import unquote, urlparse
import xml.etree.ElementTree as ET

from junit_xml import CASE, LIBTEST, read_log

SUMMARY = re.compile(r"^test result: .*? (\d+) passed; (\d+) failed;", re.MULTILINE)
LIST_CASE = re.compile(r"^(.+): test$", re.MULTILINE)
VALUE_FLAGS = {"--skip", "--format", "--color", "--test-threads", "--logfile", "--shuffle-seed", "-Z"}


def execution_count(text):
    outcomes = [CASE.match(line) for line in text.splitlines()]
    cases = sum(case is not None and case.group(2) in ("ok", "FAILED") for case in outcomes)
    summaries = sum(int(passed) + int(failed) for passed, failed in SUMMARY.findall(text))
    return max(cases, summaries)


def selectors(args):
    filters = []
    skip_value = False
    for arg in args:
        if skip_value:
            skip_value = False
        elif arg in VALUE_FLAGS:
            skip_value = True
        elif not arg.startswith("-") and arg != "":
            filters.append(arg)
    return filters


def selected(args):
    return bool(selectors(args)) or any(
        arg in ("--ignored", "--skip") or arg.startswith("--skip=") for arg in args
    )


def discovered(command, cargo=False):
    # Discovery describes the binary, not this shard's portion of it.
    env = dict(os.environ)
    for key in ("TEST_TOTAL_SHARDS", "TEST_SHARD_INDEX", "TEST_SHARD_STATUS_FILE"):
        env.pop(key, None)
    listing_command = [command[0]]
    arguments = iter(command[1:])
    for arg in arguments:
        if arg == "--format":
            next(arguments)
        elif arg != "--list" and not arg.startswith("--format="):
            listing_command.append(arg)
    if cargo and "--" not in listing_command:
        listing_command.append("--")
    listing_command += ["--list", "--format=terse"]

    def names(argv):
        result = subprocess.run(argv, env=env, capture_output=True, text=True, check=True)
        return Counter(LIST_CASE.findall(result.stdout))

    names_all = names(listing_command)
    if "--ignored" in command or "--include-ignored" in command:
        return names_all
    return names_all - names([*listing_command, "--ignored"])


def check_runner(log_path, command):
    args = command[1:]
    text = read_log(log_path)
    if "--help" in args or "-h" in args or not selected(args):
        return
    if not LIBTEST.search(text):
        return
    names = discovered(command)
    if not names:
        raise ValueError("no executable tests matched the runner arguments")
    validate_selectors(args, names)
    if "--list" not in args and int(os.environ.get("TEST_TOTAL_SHARDS", "0")) <= 1:
        require_execution(text)


def validate_selectors(args, names):
    for selector in selectors(args):
        matches = any(name == selector if "--exact" in args else selector in name for name in names)
        if not matches:
            raise ValueError(f"no executable tests matched selector {selector!r}")


def require_execution(text):
    count = execution_count(text)
    if count == 0:
        raise ValueError("no non-ignored test execution observed in the selected union")
    return count


def check_bazel_events(path):
    count = results = 0
    for line in Path(path).read_text().splitlines():
        event = json.loads(line)
        if "testResult" not in event:
            continue
        results += 1
        outputs = event["testResult"].get("testActionOutput", [])
        reports = [output for output in outputs if output.get("name") == "test.xml"]
        if len(reports) != 1:
            raise ValueError("a test result has no unique test.xml execution report")
        uri = urlparse(reports[0].get("uri", ""))
        if uri.scheme != "file" or uri.netloc not in ("", "localhost"):
            raise ValueError(
                "the local store gate requires a local test.xml execution report: "
                + reports[0].get("uri", "")
            )
        root = ET.parse(unquote(uri.path)).getroot()
        count += sum(execution_count(node.text or "") for node in root.iter("system-out"))
    if results == 0 or count == 0:
        raise ValueError("no non-ignored test execution observed in the selected shard union")
    print(f"PASS: {count} non-ignored test executions across {results} test results")


def main(argv):
    mode, path, *command = argv[1:]
    try:
        if mode == "runner":
            check_runner(path, command)
        elif mode == "bazel":
            check_bazel_events(path)
        elif mode == "cargo":
            if not discovered(command, cargo=True):
                raise ValueError("no executable tests matched the Cargo gate selection")
            require_execution(read_log(path))
        elif mode == "batch":
            manifest, *args = command
            if selected(args):
                names = Counter()
                for member in Path(manifest).read_text().splitlines():
                    if member:
                        binary = str(Path(os.environ["TEST_SRCDIR"]) / member)
                        names.update(discovered([binary, *args]))
                if not names:
                    raise ValueError("no executable tests matched the batch arguments")
                validate_selectors(args, names)
                root = ET.parse(path).getroot()
                require_execution("\n".join(node.text or "" for node in root.iter("system-out")))
        else:
            raise ValueError(f"unknown execution check: {mode}")
    except (ValueError, OSError, ET.ParseError, subprocess.CalledProcessError) as error:
        sys.exit(f"FAIL: {error}")


if __name__ == "__main__":
    main(sys.argv)

#!/usr/bin/env python3
"""Writes the JUnit report a Lash test action leaves at `$XML_OUTPUT_FILE`.

Bazel schedules a second remote spawn, `generate-xml.sh`, after every test
whose run did not write `$XML_OUTPUT_FILE`. That spawn inherits the test's
run request (4-8 CPU, 4-8 GiB) for about 0.1 s of work and queues for a slot
again. Every Lash test therefore writes its own report: `test_xml_runner.sh`
(the `--run_under` prefix in `.bazelrc`) calls this for a single binary, and
`test_batch_runner.sh` calls it with one suite per batch member.

    junit_xml.py OUT (NAME EXIT_CODE SECONDS LOG)...

EXIT_CODE is `?` for a batch member that died before its exit was recorded.

Each quadruple becomes one `<testsuite>`. Its cases are read from libtest's
stable `test <name> ... ok|FAILED|ignored` lines, and a failing case carries
its `---- <name> stdout ----` section. A suite whose exit code is non-zero
without a failed case (a crash, a harness abort, a non-libtest binary) gets
one error case named after the suite, the same shape `generate-xml.sh`
writes for every test. The whole log is the suite's `<system-out>`.
"""

import re
import sys
import xml.etree.ElementTree as ET

CASE = re.compile(r"^test (.+?) \.\.\. (ok|FAILED|ignored(?:, .*)?)$")
FAILURE_SECTION = re.compile(r"^---- (.+?) stdout ----$")
# Code points XML 1.0 cannot carry, even escaped.
NOT_XML = re.compile("[^\t\n\r\x20-퟿-�\U00010000-\U0010ffff]")


def read_log(path):
    try:
        with open(path, "rb") as log:
            raw = log.read()
    except OSError:
        return ""
    return NOT_XML.sub("?", raw.decode("utf-8", "replace"))


def failure_sections(lines):
    sections = {}
    current = None
    for line in lines:
        header = FAILURE_SECTION.match(line)
        if header:
            current = header.group(1)
            sections[current] = []
        elif current is not None:
            if line == "failures:":
                current = None
            else:
                sections[current].append(line)
    return {name: "\n".join(body).strip() for name, body in sections.items()}


def add_suite(root, name, code, seconds, log_path):
    text = read_log(log_path)
    lines = text.splitlines()
    outcomes = {}
    for line in lines:
        case = CASE.match(line)
        if case:
            outcomes[case.group(1)] = case.group(2)
    details = failure_sections(lines)

    suite = ET.SubElement(root, "testsuite", name=name, time=seconds)
    failures = skipped = errors = 0
    for case_name, outcome in outcomes.items():
        case = ET.SubElement(suite, "testcase", name=case_name, classname=name)
        if outcome == "FAILED":
            failures += 1
            failure = ET.SubElement(case, "failure", message="test failed")
            failure.text = details.get(case_name, "")
        elif outcome.startswith("ignored"):
            skipped += 1
            ET.SubElement(case, "skipped")

    exited_badly = code != "0"
    if not outcomes or (exited_badly and not failures):
        case = ET.SubElement(suite, "testcase", name=name, classname=name, time=seconds)
        if exited_badly:
            errors += 1
            message = (
                f"exited with error code {code}"
                if code.isdigit()
                else "exited without recording an exit code"
            )
            ET.SubElement(case, "error", message=message)

    suite.set("tests", str(len(suite)))
    suite.set("failures", str(failures))
    suite.set("errors", str(errors))
    suite.set("skipped", str(skipped))
    ET.SubElement(suite, "system-out").text = text


def main(argv):
    if len(argv) < 6 or (len(argv) - 2) % 4:
        sys.exit("usage: junit_xml.py OUT (NAME EXIT_CODE SECONDS LOG)...")
    root = ET.Element("testsuites")
    rest = argv[2:]
    for i in range(0, len(rest), 4):
        add_suite(root, *rest[i : i + 4])
    ET.ElementTree(root).write(argv[1], encoding="UTF-8", xml_declaration=True)


if __name__ == "__main__":
    main(sys.argv)

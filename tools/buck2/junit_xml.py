#!/usr/bin/env python3
"""Write the JUnit report a Lash test leaves at `$XML_OUTPUT_FILE`.

The external Buck2 test runner declares the result directory and sets
`XML_OUTPUT_FILE`. `test_launcher.sh` invokes `test_xml_runner.sh` for a single
binary, while `test_batch_runner.sh` calls this module with one suite per batch
member.

    junit_xml.py OUT (NAME EXIT_CODE SECONDS LOG)...

EXIT_CODE is `?` for a batch member that died before its exit was recorded.

Each quadruple becomes one `<testsuite>`. Its cases are read from libtest's
stable `test <name> ... ok|FAILED|ignored` records, and a failing case carries
its `---- <name> stdout ----` section. A record may be split by the output of
the tests themselves; `libtest_cases` reads it across lines. Where libtest names
its tests, the cases must add up to its own `test result:` summary. If they do
not, the suite carries an error and this exits non-zero, so the callers fail
the test. Output that names no test, as under `--format terse`, has no cases
to account for. A suite whose exit code is non-zero
without a failed case (a crash, a harness abort, a non-libtest binary) gets
one error case named after the suite. A successful libtest log without
outcomes has zero cases; successful non-libtest commands retain their one
command case. The whole log is the suite's `<system-out>`.
"""

import re
import sys
import xml.etree.ElementTree as ET

CASE = re.compile(r"^test (.+?) \.\.\. (ok|FAILED|ignored(?:, .*)?)$")
LIBTEST = re.compile(
    r"^(?:running \d+ tests?|test result:|test .+ \.\.\. |.+: (?:test|benchmark)$|\d+ tests?, \d+ benchmarks?$)",
    re.MULTILINE,
)
RESULT = r"(ok|FAILED|ignored(?:, .*)?)(?: <[0-9.]+s>)?"
START = re.compile(r"^test (.+?) \.\.\. (.*)$")
# A record that starts after another thread's unterminated output.
LOOSE_START = re.compile(r"test ((?:(?!test ).)+?) \.\.\. (.*)$")
WHOLE_RESULT = re.compile(rf"^{RESULT}$")
TRAILING_RESULT = re.compile(r"(ok|FAILED|ignored)$")
SUMMARY = re.compile(r"^test result: \S+ (\d+) passed; (\d+) failed; (\d+) ignored;", re.MULTILINE)
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


def libtest_records(lines, loose=False):
    """Return every `(name, outcome)` libtest recorded, in order.

    libtest writes `test <name> ... ` and the outcome separately. With one test
    thread the name comes before the test runs, so under `--nocapture` the
    test's output sits between the two, on the same line and on later ones.
    With several threads another test's uncaptured output can land there too.
    The outcome is the result that ends the name's line, else the last line
    that is only a result before the next record, else a result that ends
    unterminated output.
    """
    records = []
    name = found = None
    settled = False
    last = ""

    def close():
        if name is None:
            return
        outcome = found
        if outcome is None:
            trailing = TRAILING_RESULT.search(last)
            outcome = trailing.group(1) if trailing else None
        if outcome is not None:
            records.append((name, outcome))

    for line in lines:
        start = None if line.startswith("test result: ") else (LOOSE_START.search(line) if loose else START.match(line))
        if start:
            close()
            name, last = start.groups()
            whole = WHOLE_RESULT.match(last)
            found = whole.group(1) if whole else None
            settled = whole is not None
        elif line == "failures:" or line.startswith("test result: "):
            close()
            name = None
        elif name is not None and not settled:
            whole = WHOLE_RESULT.match(line)
            if whole:
                found = whole.group(1)
            if line:
                last = line
    close()
    return records


def tally(records):
    outcomes = [outcome for _, outcome in records]
    ignored = sum(outcome.startswith("ignored") for outcome in outcomes)
    return outcomes.count("ok"), outcomes.count("FAILED"), ignored


def libtest_cases(text):
    """Return each case's outcome, and how the cases contradict libtest's summary, if they do."""
    lines = text.splitlines()
    summaries = SUMMARY.findall(text)
    records = libtest_records(lines)
    mismatch = None
    if summaries and any(LOOSE_START.search(line) for line in lines):
        expected = tuple(sum(int(summary[index]) for summary in summaries) for index in range(3))
        if tally(records) != expected:
            loose = libtest_records(lines, loose=True)
            if tally(loose) == expected:
                records = loose
            else:
                mismatch = (
                    "libtest reported {} passed, {} failed, {} ignored; its output names {} passed, {} failed, {} ignored"
                    .format(*expected, *tally(records))
                )
    return dict(records), mismatch


def add_suite(root, name, code, seconds, log_path):
    text = read_log(log_path)
    lines = text.splitlines()
    outcomes, mismatch = libtest_cases(text)
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
    if (not outcomes and not LIBTEST.search(text)) or (exited_badly and not failures) or mismatch:
        case = ET.SubElement(suite, "testcase", name=name, classname=name, time=seconds)
        if exited_badly and not failures:
            errors += 1
            message = (
                f"exited with error code {code}"
                if code.isdigit()
                else "exited without recording an exit code"
            )
            ET.SubElement(case, "error", message=message)
        if mismatch:
            errors += 1
            ET.SubElement(case, "error", message=mismatch)

    suite.set("tests", str(len(suite)))
    suite.set("failures", str(failures))
    suite.set("errors", str(errors))
    suite.set("skipped", str(skipped))
    ET.SubElement(suite, "system-out").text = text
    return f"{name}: {mismatch}" if mismatch else None


def main(argv):
    if len(argv) < 6 or (len(argv) - 2) % 4:
        sys.exit("usage: junit_xml.py OUT (NAME EXIT_CODE SECONDS LOG)...")
    root = ET.Element("testsuites")
    rest = argv[2:]
    mismatches = [add_suite(root, *rest[i : i + 4]) for i in range(0, len(rest), 4)]
    ET.ElementTree(root).write(argv[1], encoding="UTF-8", xml_declaration=True)
    mismatches = [mismatch for mismatch in mismatches if mismatch]
    if mismatches:
        sys.exit("FAIL: the report does not account for every test: " + "; ".join(mismatches))


if __name__ == "__main__":
    main(sys.argv)

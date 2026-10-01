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
the test. A test's child process may print libtest output of its own into the
same stream; `libtest_runs` and `own_cases` keep the child's cases and summary
out of the binary's. Output that names no test, as under `--format terse`, has no cases
to account for. A suite whose exit code is non-zero
without a failed case (a crash, a harness abort, a non-libtest binary) gets
one error case named after the suite. A successful libtest log without
outcomes has zero cases; successful non-libtest commands retain their one
command case. The whole log is the suite's `<system-out>`.
"""

from collections import Counter
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
WHOLE_RECORD = re.compile(rf"^test (.+?) \.\.\. {RESULT}$")
TRAILING_RESULT = re.compile(r"(ok|FAILED|ignored)$")
HEADER = re.compile(r"^running (\d+) tests?$")
SUMMARY_LINE = re.compile(r"^test result: \S+ (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured;")
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


class Record:
    """One `test <name> ... <outcome>` libtest printed.

    `inside` says a child's block was open when the record began, so the text
    alone does not say whose it is; `nested` says a child's summary claimed it.
    A name whose result never came has no outcome and is not a record.
    """

    def __init__(self, name, inside):
        self.name = name
        self.inside = inside
        self.outcome = None
        self.nested = False


class Reader:
    """Read the records of one process level across the lines that split them.

    libtest writes `test <name> ... ` and the outcome separately. With one test
    thread the name comes before the test runs, so under `--nocapture` the
    test's output sits between the two, on the same line and on later ones.
    With several threads another test's uncaptured output can land there too.
    The outcome is the result that ends the name's line, else the last line
    that is only a result before the next record, else a result that ends
    unterminated output.
    """

    def __init__(self, records, inside, loose):
        self.records = records
        self.inside = inside
        self.loose = loose
        self.record = None
        self.settled = False
        self.last = ""

    def waiting(self):
        return self.record is not None and not self.settled

    def close(self):
        record, self.record = self.record, None
        if record is None:
            return
        if record.outcome is None:
            trailing = TRAILING_RESULT.search(self.last)
            record.outcome = trailing.group(1) if trailing else None

    def start(self, line):
        """Begin the record `line` names, if it names one."""
        start = LOOSE_START.search(line) if self.loose else START.match(line)
        if not start:
            return False
        self.close()
        name, self.last = start.groups()
        self.record = Record(name, self.inside)
        self.records.append(self.record)
        whole = WHOLE_RESULT.match(self.last)
        self.settled = whole is not None
        if whole:
            self.record.outcome = whole.group(1)
            return True
        # Another process's whole record can follow a name that still waits
        # for its result: `test a ... test b ... ok`.
        glued = WHOLE_RECORD.match(self.last)
        if glued:
            other = Record(glued.group(1), self.inside)
            other.outcome = glued.group(2)
            self.records.append(other)
            self.last = ""
        return True

    def text(self, line):
        if not self.waiting():
            return
        whole = WHOLE_RESULT.match(line)
        if whole:
            self.record.outcome = whole.group(1)
        if line:
            self.last = line


class Block:
    """One `running N tests` … `test result:` span and the child blocks inside it."""

    def __init__(self, count=None, opened=0):
        self.count = count
        self.opened = opened
        self.closed = None
        self.summary = None
        self.total = None


def libtest_runs(lines, loose=False):
    """Split a log into the runs of the test binary itself, apart from its children's.

    A test may start a child that prints libtest output of its own into the
    same stream, often the same binary running the same test. A `running N
    tests` seen while a run is open therefore opens a child's block. A `test
    result:` closes the innermost open block that announced as many tests as
    it totals, so a child that died without a summary does not take its
    parent's. Returns `(block, children, records, named)` per run; `named` says
    the run's output names a test.
    """
    runs = []
    top = children = records = None
    stack = []
    own = inner = None
    named = False

    def begin(count):
        nonlocal top, children, records, stack, own, inner, named
        top, children, records, named = Block(count), [], [], False
        stack = [top]
        own, inner = Reader(records, False, loose), Reader(records, True, loose)

    def end():
        nonlocal top
        if top is None:
            return
        own.close()
        inner.close()
        if top.summary is None and children:
            # Only a child that died without a summary can leave the run open
            # at the end of the log; the last summary was then the run's own.
            last = max((child for child in children if child.summary), key=lambda child: child.closed[1], default=None)
            if last is not None and top.count in (None, last.total):
                top.summary, top.total, last.summary = last.summary, last.total, None
        runs.append((top, children, records, named))
        top = None

    for order, line in enumerate(lines):
        header = HEADER.match(line)
        summary = SUMMARY_LINE.match(line)
        if top is None:
            begin(int(header.group(1)) if header else None)
            if header:
                continue
        elif header:
            if top.count is None and not children:
                top.count = int(header.group(1))
            else:
                child = Block(int(header.group(1)), len(records))
                children.append(child)
                stack.append(child)
            continue
        reader = inner if len(stack) > 1 else own
        if summary:
            reader.close()
            counts = tuple(int(number) for number in summary.groups())
            index = next((i for i in range(len(stack) - 1, -1, -1) if stack[i].count == sum(counts)), len(stack) - 1)
            stack[index].summary = counts[:3]
            stack[index].total = sum(counts)
            stack[index].closed = (len(records), order)
            del stack[index:]
            if index == 0:
                end()
        elif line == "failures:" or line.startswith("test result: "):
            reader.close()
        else:
            named = named or bool(LOOSE_START.search(line))
            if reader.start(line):
                continue
            # A result with no name waiting at this level ends the binary's own
            # record, begun before the child's block opened.
            (reader if reader.waiting() or not WHOLE_RESULT.match(line) else own).text(line)
    end()
    return runs


def own_cases(children, records):
    """Return the binary's own `{name: outcome}` among one run's records.

    A record that began with no child's block open is the binary's own. Each
    child summary claims as many of the records printed inside its block as it
    totals, first a name that is printed again, since the parent's own record
    of a test follows its child's, then the latest. What no child claims is
    the binary's own, and a name's last such record carries its outcome.
    """
    printed = Counter(record.name for record in records if record.outcome is not None)
    for child in sorted((child for child in children if child.summary), key=lambda child: child.closed[1]):
        for _ in range(child.total):
            inside = [
                (printed[records[position].name] > 1, position)
                for position in range(child.opened, child.closed[0])
                if records[position].inside and not records[position].nested and records[position].outcome is not None
            ]
            if not inside:
                break
            claimed = records[max(inside)[1]]
            claimed.nested = True
            printed[claimed.name] -= 1
    cases = {}
    for record in records:
        if record.nested or record.outcome is None:
            continue
        if record.name not in cases or not record.inside or cases[record.name][1]:
            cases[record.name] = (record.outcome, record.inside)
    return {name: outcome for name, (outcome, _) in cases.items()}


def tally(outcomes):
    outcomes = list(outcomes)
    ignored = sum(outcome.startswith("ignored") for outcome in outcomes)
    return outcomes.count("ok"), outcomes.count("FAILED"), ignored


def read_cases(lines, loose):
    """Return the binary's own cases and what its summaries and its cases each total, if they differ."""
    cases = {}
    expected, found = [0, 0, 0], [0, 0, 0]
    differs = False
    for block, children, records, named in libtest_runs(lines, loose):
        own = own_cases(children, records)
        cases.update(own)
        if block.summary is None or not named:
            continue
        counted = tally(own.values())
        differs = differs or counted != block.summary
        for index in range(3):
            expected[index] += block.summary[index]
            found[index] += counted[index]
    return cases, (expected, found) if differs else None


def libtest_cases(text):
    """Return each of the binary's own cases' outcome, and how they contradict its own summary, if they do."""
    lines = text.splitlines()
    cases, difference = read_cases(lines, loose=False)
    mismatch = None
    if difference:
        loose, still = read_cases(lines, loose=True)
        if still:
            mismatch = (
                "libtest reported {} passed, {} failed, {} ignored; its output names {} passed, {} failed, {} ignored"
                .format(*difference[0], *difference[1])
            )
        else:
            cases = loose
    return cases, mismatch


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

"""The laws that hold only on the 1.0 release tree, and how the cut runs them.

A law that asserts the release baseline is red on every tree before the cut,
so it is marked cut-only where it is written:

- a Rust test carries `#[ignore = "release cut: FIG-<n>"]` on its own line;
- a Python test carries `@unittest.skip("release cut: FIG-<n>")` on its own
  line, in a `scripts/test_*.py` module.

`release_reset.py --apply` removes each marker, so from the cut on the law runs
with its suite, and then runs every law it unmarked: each Rust law on its
owning target and on that target's synthetic-next variant, each by its exact
name through `kiln test`, which refuses a run that executes no case.
"""

from __future__ import annotations

from pathlib import Path
import re
import subprocess
import sys

import release_baseline as baseline
from fixture_regenerators import ignored_tests

RUST = re.compile(r'#\[ignore\s*=\s*"release cut: (FIG-[0-9]+)"\]')
PYTHON = re.compile(r'@unittest\.skip\("release cut: (FIG-[0-9]+)"\)')
RUST_LINE = re.compile(r'^[ \t]*#\[ignore = "release cut: FIG-[0-9]+"\][ \t]*\n', re.MULTILINE)
PYTHON_LINE = re.compile(r'^[ \t]*@unittest\.skip\("release cut: FIG-[0-9]+"\)[ \t]*\n', re.MULTILINE)
# Any spelling of the marker, so a malformed one cannot hide a law.
MENTION = re.compile(r'"release cut: ')
TARGET = re.compile(r'\n    name = "(?P<name>[^"]+)",\n(?:    build_script = None,\n)?'
                    r'    crate_features = \[(?P<features>[^\]]*)\],\n')


def features(repo: Path, package: str):
    """Each Buck2 target of `package` with the Cargo features it builds."""
    build = repo / package / "BUCK"
    text = build.read_text() if build.is_file() else ""
    return {match["name"]: frozenset(re.findall(r'"([^"]+)"', match["features"]))
            for match in TARGET.finditer(text)}


def synthetic_variant(repo: Path, target: str):
    """The feature variant of `target` that builds its features with
    `synthetic-next` added; failing that, the one that keeps most of them."""
    package, name = target.removeprefix("//").split(":")
    targets = features(repo, package)
    if name not in targets:
        raise baseline.BaselineError(f"no Buck2 target {target}")
    wanted = targets[name] | {"synthetic-next"}
    variants = [(len(built), variant) for variant, built in targets.items()
                if re.fullmatch(re.escape(name) + r"__fv_[0-9a-f]+", variant)
                and "synthetic-next" in built and built <= wanted]
    if not variants:
        return None
    best = max(variants)[0]
    found = [variant for size, variant in variants if size == best]
    if len(found) > 1:
        raise baseline.BaselineError(f"{target}: synthetic-next variants {found} are equally close")
    return f"//{package}:{found[0]}"


def python_laws(repo: Path):
    laws = []
    for path in sorted((repo / "scripts").glob("test_*.py")):
        text = path.read_text()
        for match in PYTHON.finditer(text):
            test = re.compile(r'(?:[ \t]*@[^\n]*\n)*[ \t]*def (test_\w+)').match(text, text.index("\n", match.end()) + 1)
            classes = re.findall(r'^class (\w+)', text[:match.start()], re.MULTILINE)
            if test is None or not classes:
                raise baseline.BaselineError(f"{path.relative_to(repo)}: {match[0]} marks no test method")
            laws.append(dict(kind="python", ticket=match[1], source=str(path.relative_to(repo)),
                             law=f"{classes[-1]}.{test[1]}", targets=[str(path.relative_to(repo))]))
    return laws


def discover(repo: Path):
    """Every cut-only law, Rust and Python, with the targets the cut runs it on."""
    laws = []
    for test in ignored_tests(repo, RUST):
        targets = [test["target"]]
        variant = synthetic_variant(repo, test["target"])
        if variant:
            targets.append(variant)
        laws.append(dict(kind="rust", ticket=test.pop("reason")[1], source=test["source"],
                         law=test["law"], targets=targets))
    laws += python_laws(repo)
    return laws


def undiscovered(repo: Path, laws: list[dict]):
    """Where a cut-law marker is not on its own line or marks no discovered
    test: a law the cut would leave ignored."""
    marked = {}
    for law in laws:
        marked[law["source"]] = marked.get(law["source"], 0) + 1
    errors = []
    for directory, suffix in (("crates", ".rs"), ("scripts", ".py")):
        for path in sorted((repo / directory).rglob(f"*{suffix}")):
            relative = str(path.relative_to(repo))
            if relative == "scripts/release_cut_laws.py":
                continue
            text = path.read_text()
            mentions = len(MENTION.findall(text))
            lines = len((RUST_LINE if suffix == ".rs" else PYTHON_LINE).findall(text))
            if not mentions == lines == marked.get(relative, 0):
                errors.append(f"{relative}: {mentions} cut-law markers, {lines} on their own line, "
                              f"{marked.get(relative, 0)} on a discovered test")
    return errors


def unmarked(text: str, suffix: str) -> str:
    """`text` with its cut-law markers removed."""
    return (RUST_LINE if suffix == ".rs" else PYTHON_LINE).sub("", text)


def run(repo: Path, laws: list[dict]):
    """Run each unmarked law on each of its targets; fail unless every run
    executes and passes at least one case."""
    for law in laws:
        for target in law["targets"]:
            if law["kind"] == "python":
                result = subprocess.run([sys.executable, target, law["law"], "-v"], cwd=repo,
                                        capture_output=True, text=True)
                ran = re.search(r"^Ran (\d+) tests?", result.stderr, re.MULTILINE)
                if (result.returncode or ran is None or int(ran[1]) == 0
                        or re.search(r"skipped", result.stderr.splitlines()[-1])):
                    raise baseline.BaselineError(f"cut law {law['law']} on {target}:\n{result.stderr}")
                print(f"cut law {law['law']} ({law['ticket']}) on {target}: {ran[1]} passed", file=sys.stderr)
                continue
            # A single run refuses a cached verdict and a run of zero cases.
            subprocess.run(["kiln", "test", target, "--test_arg=--exact", f"--test_arg={law['law']}",
                            "--runs_per_test=1"], cwd=repo, check=True)
            print(f"cut law {law['law']} ({law['ticket']}) on {target}: passed", file=sys.stderr)

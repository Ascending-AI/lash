#!/usr/bin/env python3
"""Tests for scripts/gate_scope.py.

The interesting assertions here are the ones that prove a family is *not*
skipped: a classifier that skips too much turns a real failure into a green
push, so every ambiguous shape (shared input, unknown path, empty set, git
failure) is pinned to "run everything". The narrow cases exist to show the
rule is still sharp enough to be worth having.
"""

from __future__ import annotations

from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))

import gate_scope  # noqa: E402

SCRIPT = Path(__file__).resolve().parent / "gate_scope.py"


def scope_of(*paths: str) -> gate_scope.Scope:
    return gate_scope.classify(list(paths))


class GlobTests(unittest.TestCase):
    def test_double_star_slash_matches_zero_directories(self) -> None:
        self.assertTrue(gate_scope.matches("**/Cargo.toml", "Cargo.toml"))
        self.assertTrue(
            gate_scope.matches("**/Cargo.toml", "crates/lash/Cargo.toml")
        )

    def test_single_star_does_not_cross_separators(self) -> None:
        self.assertTrue(gate_scope.matches("*.md", "README.md"))
        self.assertFalse(gate_scope.matches("*.md", "docs/guide.md"))

    def test_patterns_are_anchored_at_both_ends(self) -> None:
        self.assertFalse(gate_scope.matches("crates/**", "vendor/crates/x.rs"))
        self.assertFalse(gate_scope.matches("Cargo.toml", "Cargo.toml.bak"))


class DocOnlyTests(unittest.TestCase):
    def test_docs_and_root_markdown_skip_every_compile_family(self) -> None:
        scope = scope_of("docs/guide.md", "README.md", "CONTRIBUTING.md")
        self.assertEqual(scope.classification, "docs-only")
        self.assertTrue(scope.runs("docs-text"))
        for family in ("rust-compile", "scripts", "registry", "workflows"):
            self.assertFalse(scope.runs(family), family)

    def test_the_reason_names_the_paths_that_drove_it(self) -> None:
        scope = scope_of("docs/guide.md")
        self.assertIn("docs-prose", scope.reason)
        self.assertIn("docs/guide.md", scope.reason)

    def test_markdown_under_a_crate_is_not_doc_only(self) -> None:
        # `include_str!` can pull it into a doc test, so it compiles.
        scope = scope_of("crates/lash/README.md")
        self.assertEqual(scope.classification, "rust-only")
        self.assertTrue(scope.runs("rust-compile"))


class ScriptOnlyTests(unittest.TestCase):
    def test_a_script_change_runs_everything(self) -> None:
        scope = scope_of("scripts/lint_docs.py")
        self.assertEqual(scope.classification, "shared-inputs")
        self.assertEqual(scope.families, gate_scope.ALL_FAMILIES)

    def test_this_classifier_does_not_exempt_itself(self) -> None:
        scope = scope_of("scripts/gate_scope.py")
        self.assertEqual(scope.families, gate_scope.ALL_FAMILIES)


class RustSourceTests(unittest.TestCase):
    def test_a_crate_change_runs_compile_but_not_workflows(self) -> None:
        scope = scope_of("crates/lash-core/src/runtime/turn_loop.rs")
        self.assertEqual(scope.classification, "rust-only")
        self.assertTrue(scope.runs("rust-compile"))
        self.assertTrue(scope.runs("registry"))
        self.assertTrue(scope.runs("docs-text"))
        self.assertFalse(scope.runs("workflows"))
        self.assertFalse(scope.runs("scripts"))

    def test_an_example_rust_file_is_a_rust_source(self) -> None:
        scope = scope_of("examples/slack-clone/src/main.rs")
        self.assertEqual(scope.classification, "rust-only")
        self.assertTrue(scope.runs("rust-compile"))

    def test_a_non_rust_example_asset_is_unknown(self) -> None:
        scope = scope_of("examples/slack-clone/ui/index.html")
        self.assertEqual(scope.classification, "unknown-paths")
        self.assertEqual(scope.families, gate_scope.ALL_FAMILIES)


class RegistryTests(unittest.TestCase):
    def test_the_coverage_registry_also_runs_rust_compile(self) -> None:
        scope = scope_of("docs/api-example-coverage.toml")
        self.assertEqual(scope.classification, "registry-only")
        self.assertTrue(scope.runs("registry"))
        self.assertTrue(scope.runs("rust-compile"))
        self.assertFalse(scope.runs("workflows"))

    def test_the_registry_rule_wins_over_the_docs_rule(self) -> None:
        self.assertEqual(
            gate_scope.classify_path("docs/api-example-coverage.toml"),
            "registry-manifest",
        )


class MixedTests(unittest.TestCase):
    def test_docs_plus_crates_is_mixed_and_compiles(self) -> None:
        scope = scope_of("docs/guide.md", "crates/lash/src/lib.rs")
        self.assertEqual(scope.classification, "mixed")
        self.assertTrue(scope.runs("rust-compile"))
        self.assertTrue(scope.runs("docs-text"))
        self.assertFalse(scope.runs("workflows"))

    def test_docs_plus_a_shared_input_runs_everything(self) -> None:
        scope = scope_of("docs/guide.md", "Cargo.lock")
        self.assertEqual(scope.classification, "shared-inputs")
        self.assertEqual(scope.families, gate_scope.ALL_FAMILIES)


class ConservativeFallbackTests(unittest.TestCase):
    def test_cargo_lock_runs_everything(self) -> None:
        self.assertEqual(scope_of("Cargo.lock").families, gate_scope.ALL_FAMILIES)

    def test_a_nested_manifest_runs_everything(self) -> None:
        scope = scope_of("crates/lash-core/Cargo.toml")
        self.assertEqual(scope.families, gate_scope.ALL_FAMILIES)

    def test_a_build_script_runs_everything(self) -> None:
        scope = scope_of("crates/lash-ffi/build.rs")
        self.assertEqual(scope.families, gate_scope.ALL_FAMILIES)

    def test_a_toolchain_pin_runs_everything(self) -> None:
        self.assertEqual(
            scope_of("rust-toolchain.toml").families, gate_scope.ALL_FAMILIES
        )

    def test_a_workflow_change_runs_everything(self) -> None:
        scope = scope_of(".github/workflows/ci.yml")
        self.assertEqual(scope.families, gate_scope.ALL_FAMILIES)
        self.assertTrue(scope.runs("workflows"))

    def test_an_unknown_extension_runs_everything(self) -> None:
        scope = scope_of("fixtures/transcripts/turn.json")
        self.assertEqual(scope.classification, "unknown-paths")
        self.assertEqual(scope.families, gate_scope.ALL_FAMILIES)

    def test_an_empty_diff_runs_everything(self) -> None:
        scope = scope_of()
        self.assertEqual(scope.classification, "empty-diff")
        self.assertEqual(scope.families, gate_scope.ALL_FAMILIES)

    def test_a_blank_line_is_not_a_path(self) -> None:
        self.assertEqual(scope_of("", "  ").classification, "empty-diff")


class RenderTests(unittest.TestCase):
    def test_text_output_lists_every_family_and_a_reason(self) -> None:
        text = gate_scope.render_text(scope_of("docs/guide.md"))
        lines = text.splitlines()
        self.assertEqual(len(lines), len(gate_scope.FAMILIES) + 1)
        self.assertIn("rust-compile: skip", lines)
        self.assertIn("docs-text: run", lines)
        self.assertTrue(lines[-1].startswith("classification: docs-only -- "))

    def test_text_output_is_ascii_so_any_locale_can_print_it(self) -> None:
        text = gate_scope.render_text(scope_of("docs/guide.md"))
        text.encode("ascii")

    def test_env_output_is_evaluable_and_quotes_the_reason(self) -> None:
        env = gate_scope.render_env(scope_of("docs/guide.md"))
        self.assertIn("GATE_RUN_RUST_COMPILE=0", env)
        self.assertIn("GATE_RUN_DOCS_TEXT=1", env)
        printed = subprocess.run(
            ["bash", "-c", f'eval "$1"; printf "%s|%s" "$GATE_RUN_SCRIPTS" '
             f'"$GATE_SCOPE_CLASSIFICATION"', "bash", env],
            capture_output=True,
            text=True,
            check=True,
        )
        self.assertEqual(printed.stdout, "0|docs-only")


class CliTests(unittest.TestCase):
    def run_cli(self, *args: str, stdin: str = "") -> subprocess.CompletedProcess:
        return subprocess.run(
            [sys.executable, str(SCRIPT), *args],
            input=stdin,
            capture_output=True,
            text=True,
            check=False,
        )

    def test_paths_from_stdin_classifies_without_touching_git(self) -> None:
        result = self.run_cli("--paths-from", "-", stdin="docs/a.md\ndocs/b.md\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("rust-compile: skip", result.stdout)

    def test_env_format_from_stdin(self) -> None:
        result = self.run_cli(
            "--paths-from", "-", "--format", "env", stdin="Cargo.lock\n"
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("GATE_RUN_RUST_COMPILE=1", result.stdout)
        self.assertIn("GATE_SCOPE_CLASSIFICATION=shared-inputs", result.stdout)

    def test_a_missing_paths_file_exits_nonzero(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            result = self.run_cli("--paths-from", str(Path(tmp) / "absent"))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("run everything", result.stderr)

    def test_an_unresolvable_base_exits_nonzero(self) -> None:
        result = self.run_cli("--base", "refs/heads/no-such-ref-fig-1811")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("run everything", result.stderr)


class RealRepositoryTests(unittest.TestCase):
    """End-to-end against a synthetic git repository."""

    def setUp(self) -> None:
        self._temp = tempfile.TemporaryDirectory()
        self.root = Path(self._temp.name)
        self.addCleanup(self._temp.cleanup)
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.email", "gate@example.com")
        self.git("config", "user.name", "Gate")
        (self.root / "docs").mkdir()
        (self.root / "docs" / "guide.md").write_text("base\n", encoding="utf-8")
        self.git("add", "-A")
        self.git("commit", "-qm", "base")

    def git(self, *args: str) -> str:
        return subprocess.run(
            ["git", *args],
            cwd=self.root,
            capture_output=True,
            text=True,
            check=True,
        ).stdout

    def classify(self, *args: str) -> str:
        result = subprocess.run(
            [sys.executable, str(SCRIPT), "--repo", str(self.root), *args],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout

    def test_a_doc_only_branch_skips_rust_compile(self) -> None:
        self.git("checkout", "-qb", "topic")
        (self.root / "docs" / "guide.md").write_text("changed\n", encoding="utf-8")
        self.git("commit", "-qam", "docs")
        out = self.classify("--base", "main")
        self.assertIn("rust-compile: skip", out)
        self.assertIn("classification: docs-only", out)

    def test_a_dirty_worktree_widens_the_scope(self) -> None:
        self.git("checkout", "-qb", "topic")
        (self.root / "docs" / "guide.md").write_text("changed\n", encoding="utf-8")
        self.git("commit", "-qam", "docs")
        (self.root / "Cargo.lock").write_text("dirty\n", encoding="utf-8")
        self.git("add", "Cargo.lock")
        out = self.classify("--base", "main")
        self.assertIn("rust-compile: run", out)
        self.assertIn("classification: shared-inputs", out)
        out = self.classify("--base", "main", "--no-worktree")
        self.assertIn("rust-compile: skip", out)

    def test_a_renamed_source_file_reports_both_paths(self) -> None:
        self.git("checkout", "-qb", "topic")
        (self.root / "crates").mkdir()
        (self.root / "crates" / "a.rs").write_text("fn main() {}\n", encoding="utf-8")
        self.git("add", "-A")
        self.git("commit", "-qm", "add")
        self.git("mv", "crates/a.rs", "crates/b.rs")
        out = self.classify("--base", "main")
        self.assertIn("crates/a.rs", out)
        self.assertIn("crates/b.rs", out)


class RustRuntimeDocInputTests(unittest.TestCase):
    """Keep `docs-only ⇒ skip rust-compile` true as the tree evolves.

    The claim this classifier rests on is that prose cannot fail the Rust
    suite. That is a claim about the *tree*, not about the classifier, so it
    is checked against the tree: every reference to `docs/**` or `CONTEXT.md`
    found in a tracked Rust source must name a path this classifier already
    treats as affecting `rust-compile`. A new test that reads an ADR at run
    time fails here until `RUST_RUNTIME_DOC_INPUTS` catches up, rather than
    silently reopening the hole.
    """

    #: A path literal reaching the repository root: `docs/...`, or root-level
    #: prose named outright. Leading `./` and `../` are stripped first so a
    #: crate-relative `include_str!("../../../docs/x.md")` is caught too.
    LITERAL = re.compile(r'"((?:\.{1,2}/)*(?:docs/[^"\s]+|CONTEXT\.md))"')

    @classmethod
    def setUpClass(cls) -> None:
        listed = subprocess.run(
            ["git", "ls-files", "-z", "*.rs"],
            cwd=gate_scope.ROOT,
            capture_output=True,
            text=True,
            check=True,
        )
        cls.sources = [
            gate_scope.ROOT / entry
            for entry in listed.stdout.split("\0")
            if entry
        ]

    def referenced_paths(self) -> dict[str, list[str]]:
        found: dict[str, list[str]] = {}
        for source in self.sources:
            body = source.read_text(encoding="utf-8", errors="replace")
            for match in self.LITERAL.finditer(body):
                path = re.sub(r"^(?:\.{1,2}/)+", "", match.group(1))
                relative = source.relative_to(gate_scope.ROOT).as_posix()
                found.setdefault(path, []).append(relative)
        return found

    def test_the_sweep_finds_the_sources_it_is_meant_to_read(self) -> None:
        # A silent zero-hit sweep would be a guard that cannot fail.
        self.assertGreater(len(self.sources), 100)

    def test_every_doc_path_named_in_rust_is_rust_affecting(self) -> None:
        offenders = {
            path: sorted(set(sources))
            for path, sources in self.referenced_paths().items()
            if not scope_of(path).runs("rust-compile")
        }
        self.assertEqual(
            offenders,
            {},
            "these prose paths are named by Rust sources but classify as "
            "unable to affect rust-compile, so a prose-only edit to one of "
            "them would skip the Rust suite locally; add them to "
            "gate_scope.RUST_RUNTIME_DOC_INPUTS or stop reading them from "
            f"Rust: {offenders}",
        )

    def test_the_pinned_list_stays_current(self) -> None:
        referenced = self.referenced_paths()
        stale = [
            path
            for path in gate_scope.RUST_RUNTIME_DOC_INPUTS
            if path not in referenced
        ]
        self.assertEqual(
            stale,
            [],
            "no Rust source names these any more; drop them so `docs-only` "
            f"keeps its value: {stale}",
        )

    def test_a_pinned_doc_input_runs_the_rust_battery(self) -> None:
        scope = scope_of("docs/adr/0008-confidence-gate.md")
        self.assertTrue(scope.runs("rust-compile"))
        self.assertTrue(scope.runs("docs-text"))
        self.assertEqual(scope.classification, "rust-input-docs")

    def test_unrelated_prose_still_skips_the_rust_battery(self) -> None:
        scope = scope_of("docs/adr/0001-store-shape.md", "README.md")
        self.assertFalse(scope.runs("rust-compile"))
        self.assertEqual(scope.classification, "docs-only")


if __name__ == "__main__":
    unittest.main()

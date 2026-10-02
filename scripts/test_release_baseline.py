#!/usr/bin/env python3
"""Release-cut inventory and reset laws."""

import json
import re
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import release_baseline as baseline
import release_cut_laws as cut_laws
import release_reset as reset
from fixture_regenerators import without_comments

ROOT = Path(__file__).resolve().parents[1]
# The cut-law marker, spelled so that this module's own text carries only its
# real marker.
CUT = "release cut" + ": FIG-1"


class ReleaseBaselineTests(unittest.TestCase):
    def test_discovery_lexer_preserves_literals_and_removes_nested_comments(self):
        source = '''const QUOTE: u8 = b'"';
const BRACE: char = '{';
const UNICODE: char = '\\u{7b}';
const RAW: &str = r#"/* a literal */"#;
/* /* nested */ #[ignore = "regenerates false"] */
// #[ignore = "regenerates false"]
#[ignore = "regenerates fixtures/real"]
'''
        cleaned = without_comments(source)
        self.assertIn('b\'"\'', cleaned)
        self.assertIn('r#"/* a literal */"#', cleaned)
        self.assertNotIn('regenerates false', cleaned)
        self.assertIn('regenerates fixtures/real', cleaned)

    def test_every_fixture_regenerator_is_discovered(self):
        generators = reset.discover(ROOT)
        discovered = {row["source"] for row in generators}
        undiscovered = []
        for path in sorted((ROOT / "crates").rglob("*.rs")):
            source = without_comments(path.read_text())
            variables = re.findall(r'"((?:UPDATE_|LASH_UPDATE_|LASH_REGENERATE_)[A-Z0-9_]+)"', source)
            if variables:
                undiscovered.append(f"{path.relative_to(ROOT)}: {', '.join(variables)}")
            annotations = re.findall(r'#\[ignore\s*=\s*"regenerates ([^"]+)"\]', source)
            actual = [row["output"] for row in generators if row["source"] == str(path.relative_to(ROOT))]
            self.assertCountEqual(annotations, actual, str(path))
            if '"LASH_REGENERATE"' in source and str(path.relative_to(ROOT)) not in discovered:
                undiscovered.append(f"{path.relative_to(ROOT)}: LASH_REGENERATE")
        self.assertEqual(undiscovered, [], "fixture writers missing from discovery")
        self.assertIn("crates/lash-core-store/src/testdata/usage_fact_payload_v4.hex",
                      {row["output"] for row in generators})

    def test_regenerator_discovery_follows_rust_modules_and_cargo_targets(self):
        scratch_root = ROOT / ".buck2/release-baseline-tests"
        scratch_root.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch_root) as temporary:
            repo = Path(temporary)
            package = repo / "crates/demo"
            (package / "src/nested").mkdir(parents=True)
            (package / "tests").mkdir()
            (package / "Cargo.toml").write_text('[package]\nname = "demo"\nversion = "1.0.0"\n')
            (package / "src/lib.rs").write_text('''
#[path = "nested/fixtures.rs"]
pub(crate) mod renamed;
// #[ignore = "regenerates commented"]
''')
            generator = '''
#[tokio::test]
#[ignore = "regenerates fixtures/demo"]
async fn rewrite() { assert_eq!(std::env::var("LASH_REGENERATE").as_deref(), Ok("1")); }
'''
            (package / "src/nested/fixtures.rs").write_text('mod checks {' + generator + '}')
            (package / "tests/golden.rs").write_text(generator)
            rows = reset.discover(repo)
            self.assertEqual([(row["target"], row["law"]) for row in rows], [
                ("//crates/demo:demo__unit_test", "renamed::checks::rewrite"),
                ("//crates/demo:golden__test", "rewrite"),
            ])
            (package / "tests/golden.rs").write_text(generator.replace("fixtures/demo", "../outside"))
            with self.assertRaises(baseline.BaselineError):
                reset.discover(repo)

    def test_reset_runs_every_discovered_generator_with_exact_selection(self):
        rows = reset.generation_order(ROOT, reset.discover(ROOT))
        self.assertCountEqual(rows, reset.discover(ROOT))
        with patch.object(reset, "run") as run:
            reset.regenerate_fixtures(ROOT)
        self.assertEqual(run.call_count, len(rows))
        for call, row in zip(run.call_args_list, rows):
            repo, argv = call.args
            self.assertEqual(repo, ROOT)
            self.assertEqual(argv[:3], ["kiln", "test", row["target"]])
            for argument in ("--test_arg=--ignored", "--test_arg=--exact",
                             f"--test_arg={row['law']}", "--test_env=LASH_REGENERATE=1",
                             f"--test_env=BUILD_WORKSPACE_DIRECTORY={ROOT}"):
                self.assertIn(argument, argv)

    def test_an_artifact_a_build_compiles_in_is_regenerated_before_the_other_fixtures(self):
        # The durable-read fixture's test compiles the shape artifact in, and
        # refuses one stamped for another version before it writes anything.
        self.assertIn("crates/lash-postgres-store/schema-shape.txt", reset.compiled_inputs(ROOT))
        order = [row["output"] for row in reset.generation_order(ROOT, reset.discover(ROOT))]
        self.assertLess(order.index("crates/lash-postgres-store/schema-shape.txt"),
                        order.index("fixtures/durable-read/v1/postgres"))

    def command(self, *args):
        return subprocess.run(
            [sys.executable, str(ROOT / "scripts/release_baseline.py"), *args],
            capture_output=True, text=True, cwd=ROOT,
        )

    def test_identity_versions_are_discovered_from_source(self):
        rows = baseline.inventory(ROOT)
        values = {row["default"] for row in rows}
        at_cut = not baseline.mismatches(rows)
        for value in ("frame-key/v2/", "lash.agent-frame-key/v2", "frame-node/v3/",
                      "lash-frame-node/v3", "lashlang:v2:blake3:",
                      "process-env:v6:blake3:", "lash-process-env/v6",
                      "lash-stable-identity/v2"):
            self.assertIn(baseline.baseline_of(value) if at_cut else value, values)
        self.assertTrue(any(row["key"].endswith(":TOOL_INTENT_IDENTITY_FAMILY_VERSION")
                            for row in rows))

    def test_inventory_resolves_source_markers_without_surface_tables(self):
        scratch_root = ROOT / ".buck2/release-baseline-tests"
        scratch_root.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch_root) as temporary:
            repo = Path(temporary)
            (repo / "scripts").mkdir()
            (repo / "crates/demo/src").mkdir(parents=True)
            (repo / baseline.REGISTRY).write_text("# No surface registry.\n")
            (repo / "crates/demo/src/lib.rs").write_text(
                '/// version_surface = "drain"\n'
                '/// format_manifest = "Wire"\n'
                '#[cfg(not(feature = "synthetic-next"))]\nconst WIRE_VERSION: u32 = 3;\n'
                '/// version_surface = "drain"\n'
                '/// format_manifest = "Wire"\n'
                '#[cfg(feature = "synthetic-next")]\nconst WIRE_VERSION: u32 = 4;\n'
                '/// version_surface = "coexist"\nconst IDENTITY: &str = "demo/v2";\n'
            )
            self.assertEqual(baseline.inventory(repo), [
                dict(key="crates/demo/src/lib.rs:WIRE_VERSION", default=3, synthetic=4,
                     upgrade="drain", manifest="Wire"),
                dict(key="crates/demo/src/lib.rs:IDENTITY", default="demo/v2", synthetic="demo/v2",
                     upgrade="coexist", manifest=None),
            ])

    def test_new_unregistered_identity_versions_are_refused(self):
        scratch_root = ROOT / ".buck2/release-baseline-tests"
        scratch_root.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch_root) as temporary:
            repo = Path(temporary)
            (repo / "scripts").mkdir()
            (repo / "crates/demo/src").mkdir(parents=True)
            (repo / baseline.REGISTRY).write_text(
                '\n[[excluded_class]]\nsuffix = "_FAMILY_VERSION"\nreason = "hash tag"\n'
            )
            source = repo / "crates/demo/src/lib.rs"
            for addition in ('const UNDECLARED_FAMILY_VERSION: u8 = 2;',
                             'const CONTROL_INTENT_FORMAT: u32 = 1;',
                             'fn encode() { IdentityEncoder::new("demo", 2); }',
                             'fn encode() { hash("new-identity/v2"); }',
                             'const TAG: &str = r#"new-identity:v2:blake3:"#;'):
                source.write_text('/// version_surface = "coexist"\nconst KNOWN_VERSION: u8 = 1;\n' + addition)
                with self.subTest(addition=addition), self.assertRaises(baseline.BaselineError):
                    baseline.inventory(repo)

    def test_registered_surfaces_are_resolved_in_both_tiers(self):
        result = self.command("inventory")
        self.assertEqual(result.returncode, 0, result.stderr)
        rows = json.loads(result.stdout)
        self.assertEqual(len(rows), len(baseline.surfaces(ROOT)))
        by_name = {row["key"]: row for row in rows}
        at_cut = not baseline.mismatches(rows)
        # A store schema version is one constant in both tiers: the synthetic
        # build's descriptor writes the version after it.
        compat = "crates/lash-core-store/src/compat.rs"
        for path, constant, default, synthetic in [
            (compat, "POSTGRES_SCHEMA_VERSION", 141, 141),
            (compat, "SQLITE_CORE_SCHEMA_VERSION", 99, 99),
            (compat, "SQLITE_REGISTRY_SCHEMA_VERSION", 44, 44),
            (compat, "SQLITE_TRIGGERS_SCHEMA_VERSION", 12, 12),
            ("crates/lash-restate/src/process/admission.rs", "JOURNAL_LOGIC_EPOCH", 1, 2),
            ("crates/lashlang/src/workflow_graph.rs", "WORKFLOW_GRAPH_SCHEMA_VERSION", 21, 22),
        ]:
            row = by_name[f"{path}:{constant}"]
            if at_cut:
                default, synthetic = 1, 1 if default == synthetic else 2
            self.assertEqual((row["default"], row["synthetic"]), (default, synthetic))
        self.assertTrue(all(row["upgrade"] and row["default"] is not None for row in rows))

    def test_draft_table_detects_pre_cut_constants(self):
        result = self.command("check")
        if not baseline.mismatches(baseline.inventory(ROOT)):
            self.assertEqual(result.returncode, 0, result.stderr)
            return
        self.assertEqual(result.returncode, 1, result.stderr)
        for name in ["REMOTE_PROTOCOL_VERSION", "RESTATE_PROCESS_JOURNAL_VERSION",
                     "WORKFLOW_GRAPH_SCHEMA_VERSION", "SQLITE_REGISTRY_SCHEMA_VERSION",
                     "SQLITE_TRIGGERS_SCHEMA_VERSION", "compat.rs:POSTGRES_SCHEMA_VERSION"]:
            self.assertIn(name, result.stderr)

    @unittest.skip("release cut: FIG-4485")
    def test_release_values_match_declared_baseline(self):
        result = self.command("check")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_sqlite_catalogs_are_in_their_compat_versions(self):
        self.assertEqual(baseline.sqlite_stamp_mismatches(ROOT), [])

    def test_postgres_schema_and_catalog_are_in_its_compat_version(self):
        self.assertEqual(baseline.postgres_stamp_mismatches(ROOT), [])

    def test_each_store_component_has_one_schema_version_constant(self):
        versions = baseline.store_versions(ROOT)
        self.assertEqual(
            {component: constant for component, (constant, _) in versions.items()},
            {"POSTGRES": "POSTGRES_SCHEMA_VERSION", "SQLITE_CORE": "SQLITE_CORE_SCHEMA_VERSION",
             "SQLITE_REGISTRY": "SQLITE_REGISTRY_SCHEMA_VERSION",
             "SQLITE_TRIGGERS": "SQLITE_TRIGGERS_SCHEMA_VERSION"})
        registered = {f'{row["constant_path"]}:{row["constant"]}' for row in baseline.surfaces(ROOT)}
        for constant, _ in versions.values():
            self.assertIn(f"{baseline.STORE_VERSIONS}:{constant}", registered)

    def test_a_step_bound_is_an_integer_or_the_store_s_own_constant(self):
        names = {"SQLITE_CORE_SCHEMA_VERSION": 7}
        self.assertEqual(baseline.step_bound(" 3 ", names, "t"), 3)
        self.assertEqual(baseline.step_bound("compat::SQLITE_CORE_SCHEMA_VERSION", names, "t"), 7)
        self.assertEqual(baseline.step_bound("compat::SQLITE_CORE_SCHEMA_VERSION + 1", names, "t"), 8)
        for expression in ["compat::SQLITE_REGISTRY_SCHEMA_VERSION", "next()", "1 + 1"]:
            with self.subTest(expression=expression), self.assertRaises(baseline.BaselineError):
                baseline.step_bound(expression, names, "t")

    def test_resolver_rejects_ambiguous_missing_and_unsupported_constants(self):
        for text, name in [("const V: u32 = 1;\nconst V: u32 = 2;", "V"),
                           ("const V: u32 = 1;", "MISSING"),
                           ("const V: u32 = compute();", "V"),
                           ("const V: u32 = OTHER;", "V"),
                           ("const V: u32 = V;", "V"),
                           ('#[cfg(feature = "unknown")]\nconst V: u32 = 1;', "V")]:
            with self.subTest(text=text), self.assertRaises(baseline.BaselineError):
                baseline.resolve(text, name, False)

    def test_comments_cannot_supply_constants_or_hide_cfg(self):
        text = '''
// const V: u32 = 99;
/* /* nested */ const V: u32 = 88; */
#[cfg(not(feature = "synthetic-next"))]
/// const V: u32 = 77;
pub const V: u32 = BASE;
#[cfg(feature = "synthetic-next")]
pub const V: u32 = BASE + 1;
const BASE: u32 = 7u32;
const RAW: &str = r#"// const V: u32 = 66;"#;
'''
        self.assertEqual(baseline.resolve(text, "V", False), 7)
        self.assertEqual(baseline.resolve(text, "V", True), 8)

    def test_baseline_is_derived_for_every_discovered_value(self):
        for value, expected in [(7, 1), ("frame-key/v2/", "frame-key/v1/"),
                                ("lashlang:v2:blake3:", "lashlang:v1:blake3:"),
                                ("lashlang-vm-abi-v14", "lashlang-vm-abi-v1"),
                                ("wait-index/v2/wait/", "wait-index/v1/wait/")]:
            self.assertEqual(baseline.baseline_of(value), expected)
        with self.assertRaises(baseline.BaselineError):
            baseline.baseline_of("unversioned")

    def test_cut_laws_are_discovered_on_both_tiers_and_unmarked_by_the_reset(self):
        scratch_root = ROOT / ".buck2/release-baseline-tests"
        scratch_root.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch_root) as temporary:
            repo = Path(temporary)
            package = repo / "crates/demo"
            (package / "src").mkdir(parents=True)
            (repo / "scripts").mkdir()
            (package / "Cargo.toml").write_text('[package]\nname = "demo"\nversion = "1.0.0"\n')
            law = f'mod tests {{\n    #[test]\n    #[ignore = "{CUT}"]\n    fn at_baseline() {{}}\n}}\n'
            (package / "src/lib.rs").write_text(law)
            (package / "BUCK").write_text(
                'lash_rust_unit_test(\n    name = "demo__unit_test",\n    crate_features = [],\n)\n\n'
                'lash_rust_feature_test(\n    name = "demo__unit_test__fv_0123abcd",\n'
                '    crate_features = [\n        "synthetic-next",\n    ],\n)\n\n'
                'lash_rust_feature_test(\n    name = "demo__unit_test__fv_4567cdef",\n'
                '    crate_features = [\n        "otel",\n        "synthetic-next",\n    ],\n)\n')
            script = repo / "scripts/test_demo.py"
            script.write_text(f'import unittest\n\nclass DemoTests(unittest.TestCase):\n'
                              f'    @unittest.skip("{CUT}")\n    def test_at_baseline(self):\n        pass\n')
            laws = cut_laws.discover(repo)
            self.assertEqual([(row["kind"], row["law"], row["targets"]) for row in laws], [
                ("rust", "tests::at_baseline",
                 ["//crates/demo:demo__unit_test", "//crates/demo:demo__unit_test__fv_0123abcd"]),
                ("python", "DemoTests.test_at_baseline", ["scripts/test_demo.py"]),
            ])
            self.assertEqual(cut_laws.undiscovered(repo, laws), [])
            unmarked = cut_laws.unmarked(law, ".rs")
            self.assertEqual(unmarked, law.replace(f'    #[ignore = "{CUT}"]\n', ""))
            self.assertNotIn(CUT, cut_laws.unmarked(script.read_text(), ".py"))
            # A marker the reset cannot remove whole, or one on no test, would
            # leave a law ignored after the cut.
            (package / "src/lib.rs").write_text(law.replace(
                f'    #[ignore = "{CUT}"]\n    fn', f'    #[ignore = "{CUT}"] fn'))
            self.assertTrue(cut_laws.undiscovered(repo, cut_laws.discover(repo)))
            (package / "src/lib.rs").write_text(law + f'#[ignore = "{CUT}"]\nconst X: u8 = 1;\n')
            self.assertTrue(cut_laws.undiscovered(repo, cut_laws.discover(repo)))
            (package / "src/lib.rs").write_text(law)
            (package / "BUCK").write_text("")
            with self.assertRaises(baseline.BaselineError):
                cut_laws.discover(repo)

    def test_every_cut_law_is_discovered_with_its_synthetic_next_target(self):
        laws = cut_laws.discover(ROOT)
        self.assertEqual(cut_laws.undiscovered(ROOT, laws), [])
        if not baseline.mismatches(baseline.inventory(ROOT)):
            # The reset unmarked every cut-only law: it runs with its suite.
            self.assertEqual(laws, [])
            return
        targets = {row["law"]: row["targets"] for row in laws}
        for law, default in [
            ("tests::the_build_states_every_version_at_the_release_baseline", "//crates/lashctl:lashctl__unit_test"),
            ("migrate::tests::catalogs_hold_no_pre_release_transition",
             "//crates/lash-postgres-store:lash-postgres-store__unit_test"),
            ("a_fresh_1_0_store_records_only_its_version_1_bootstrap_and_opens",
             "//crates/lash-postgres-store:migrate__test"),
            ("migration::release_catalog_tests::catalog_holds_no_pre_release_transition",
             "//crates/lash-sqlite-store:lash-sqlite-store__unit_test"),
        ]:
            self.assertEqual(targets[law][0], default, law)
            self.assertEqual(len(targets[law]), 2, f"{law} runs on its synthetic-next variant too")
        self.assertEqual(targets["ReleaseBaselineTests.test_release_values_match_declared_baseline"],
                         ["scripts/test_release_baseline.py"])

    def test_the_reset_runs_every_cut_law_on_each_of_its_targets(self):
        laws = [dict(kind="rust", ticket="FIG-1", source="crates/demo/src/lib.rs", law="tests::at_baseline",
                     targets=["//crates/demo:demo__unit_test", "//crates/demo:demo__unit_test__fv_0123abcd"]),
                dict(kind="python", ticket="FIG-1", source="scripts/test_demo.py",
                     law="DemoTests.test_at_baseline", targets=["scripts/test_demo.py"])]
        with patch.object(cut_laws.subprocess, "run") as run:
            run.return_value = subprocess.CompletedProcess([], 0, "", "Ran 1 test in 0.1s\n\nOK\n")
            cut_laws.run(ROOT, laws)
        calls = [call.args[0] for call in run.call_args_list]
        self.assertEqual(len(calls), sum(len(row["targets"]) for row in laws))
        for row in laws:
            for target in row["targets"]:
                if row["kind"] == "rust":
                    self.assertIn(["kiln", "test", target, "--test_arg=--exact", f"--test_arg={row['law']}",
                                   "--runs_per_test=1"], calls)
                else:
                    self.assertIn([sys.executable, target, row["law"], "-v"], calls)
        # A red law fails the run only after every other law has run.
        with patch.object(cut_laws.subprocess, "run") as run:
            run.side_effect = lambda argv, **_: subprocess.CompletedProcess(
                argv, 32 if argv[2] == laws[0]["targets"][0] else 0, "", "Ran 1 test in 0.1s\n\nOK\n")
            with self.assertRaises(cut_laws.CutLawFailure) as raised:
                cut_laws.run(ROOT, laws)
        self.assertEqual(run.call_count, len(calls))
        self.assertIn(f"{laws[0]['law']} on {laws[0]['targets'][0]}", str(raised.exception))
        # A Python law the run skipped or never selected did not pass.
        for stderr in ("Ran 1 test in 0.1s\n\nOK (skipped=1)\n", "Ran 0 tests in 0.0s\n\nOK\n"):
            with patch.object(cut_laws.subprocess, "run") as run, self.assertRaises(baseline.BaselineError):
                run.return_value = subprocess.CompletedProcess([], 0, "", stderr)
                cut_laws.run(ROOT, [row for row in laws if row["kind"] == "python"])

    def test_production_catalogs_carry_no_step_and_a_predecessor_step_is_red(self):
        self.assertEqual(baseline.production_catalog_mismatches(ROOT), [])
        scratch_root = ROOT / ".buck2/release-baseline-tests"
        scratch_root.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch_root) as temporary:
            repo = Path(temporary)
            self.scratch(repo)
            expand = repo / baseline.POSTGRES_CATALOG
            current = expand.read_text()
            table = "static EXPAND_MIGRATIONS: &[ExpandMigration] = &["
            step = ("\n    ExpandMigration {\n        id: \"0141-predecessor\",\n        from_version: 140,\n"
                    "        to_version: 141,\n        statements: \"\",\n    },")
            expand.write_text(current.replace(table, table + '\n    #[cfg(feature = "synthetic-next")]' + step, 1))
            self.assertEqual(baseline.production_catalog_mismatches(repo), [])
            expand.write_text(current.replace(table, table + step, 1))
            errors = baseline.production_catalog_mismatches(repo)
            self.assertEqual(len(errors), 1, errors)
            self.assertIn("ExpandMigration", errors[0])
            # The reset refuses it rather than deleting it.
            with self.assertRaises(baseline.BaselineError):
                reset.plan(repo)
            expand.write_text(current)
            catalog = repo / baseline.SQLITE_CATALOG
            catalog.write_text(catalog.read_text().replace(
                '    #[cfg(feature = "synthetic-next")]\n    SqliteMigration {', "    SqliteMigration {", 1))
            errors = baseline.production_catalog_mismatches(repo)
            self.assertEqual(len(errors), 1, errors)
            self.assertIn("SqliteMigration", errors[0])

    def scratch(self, repo: Path):
        """Copy every file the reset reads into `repo`; return their bytes."""
        paths = {row["constant_path"] for row in baseline.surfaces(ROOT)}
        paths.update(["scripts/versioned-surfaces.toml",
                      "crates/lash-core-store/src/store/state_version.rs",
                      "crates/lash-core-store/src/store/synthetic_next.rs",
                      str(baseline.BLAKE3_DOMAIN_TABLE), str(baseline.PREDECESSOR_TABLE),
                      "crates/lash-postgres-store/src/postgres/migrate.rs",
                      "crates/lash-sqlite-store/src/migration.rs",
                      "crates/lash-sqlite-store/src/schema.rs",
                      "crates/lash-postgres-store/src/lib.rs",
                      "crates/lash-postgres-store/schema.sql",
                      "crates/lash-core-store/src/compat.rs",
                      "crates/lash-typescript/tests/workflow_graph_schema.rs",
                      "examples/workflow-graph-roundtrip/frontend/scripts/generate-contract-types.mjs",
                      "examples/workflow-graph-roundtrip/CONTRACT.md"])
        for relative in paths:
            destination = repo / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(ROOT / relative, destination)
        return {path: (repo / path).read_bytes() for path in paths}

    def test_the_reset_edits_rust_only_inside_registered_constants(self):
        # A pin spelled as a literal beside the code that reads it is reached
        # only by a pattern written for that file, and a renamed function or a
        # reformatted call escapes the pattern. The reset may change a Rust
        # source only inside the value of a constant the inventory resolves,
        # an alias it is defined by, or a floor the registry ties to one.
        scratch_root = ROOT / ".buck2/release-baseline-tests"
        scratch_root.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch_root) as temporary:
            repo = Path(temporary)
            self.scratch(repo)
            rows = baseline.inventory(repo)
            public, edits = reset.plan(repo)
            registered = {}
            for row in rows:
                path, name = row["key"].rsplit(":", 1)
                registered.setdefault(path, set()).add(name)
            for path, floor, _ in baseline.floors(repo):
                registered.setdefault(path, set()).add(floor)
            references = dict(public["schema_renames"])
            references.update({old.removesuffix(".schema.json"): new.removesuffix(".schema.json")
                               for old, new in public["schema_renames"].items()})
            exempt = {str(baseline.BLAKE3_DOMAIN_TABLE), str(baseline.PREDECESSOR_TABLE)}

            def masked(text, names):
                found = baseline.definitions(text)
                names = set(names)
                while True:
                    aliases = {word for match in found if match["name"] in names
                               for word in re.findall(r"[A-Z][A-Z0-9_]*", match["value"])} - names
                    if not aliases:
                        break
                    names |= aliases
                for match in reversed(found):
                    if match["name"] in names:
                        text = text[:match.start("value")] + "<registered>" + text[match.end("value"):]
                return text

            outside = []
            for path, text in edits.items():
                relative = str(path.relative_to(repo))
                if path.suffix != ".rs" or relative in exempt:
                    continue
                before = path.read_text()
                for old, new in references.items():
                    before = before.replace(old, new)
                names = registered.get(relative, set())
                if masked(before, names) != masked(text, names):
                    outside.append(relative)
            self.assertEqual(outside, [], "the reset edits these outside any registered constant")
            # The code that reads a table is untouched; the table is what the
            # reset inventory generates.
            self.assertNotIn(repo / "crates/lash-core-store/src/store/synthetic_next.rs", edits)
            for path, text in edits.items():
                path.write_text(text)
            after = baseline.inventory(repo)
            self.assertEqual(baseline.table_mismatches(repo, after), [])
            pins = (repo / baseline.PREDECESSOR_TABLE).read_text()
            for name in ("LASHLANG_SNAPSHOT_VERSION", "RLM_SNAPSHOT_VERSION", "NATIVE_DRIVER_STATE_VERSION",
                         "SCOPE_STORAGE_PAYLOAD_VERSION", "WORKFLOW_GRAPH_SCHEMA_VERSION"):
                self.assertIn(f'("{name}", 1),', pins)
            by_name = {row["key"].rsplit(":", 1)[1]: row["default"] for row in after}
            self.assertEqual(by_name["CELL_GRAMMAR_INSTRUCTION_ACCOUNTING_VERSION"],
                             by_name["INSTRUCTION_ACCOUNTING_VERSION"])
            floor = repo / "crates/lash-core-store/src/store/state_version.rs"
            self.assertEqual(baseline.resolve(floor.read_text(), "OLDEST_SUPPORTED_SESSION_STATE_VERSION", False), 1)
            # A constant that moves without its table is red.
            source = repo / "crates/lashlang/src/runtime/state.rs"
            source.write_text(source.read_text().replace("LASHLANG_SNAPSHOT_VERSION: u32 = 1;",
                                                         "LASHLANG_SNAPSHOT_VERSION: u32 = 7;"))
            errors = baseline.table_mismatches(repo, baseline.inventory(repo))
            self.assertEqual(len(errors), 1, errors)
            self.assertIn(str(baseline.PREDECESSOR_TABLE), errors[0])

    def test_generated_tables_are_current(self):
        self.assertEqual(baseline.table_mismatches(ROOT, baseline.inventory(ROOT)), [])
        # Every literal the reset used to patch in place is gone from the code
        # that reads the tables.
        pins = (ROOT / "crates/lash-core-store/src/store/synthetic_next.rs").read_text()
        self.assertEqual(re.findall(r"[(,]\s*\d+\s*[,)]", pins), [], "a version passed as a literal")
        # A domain in use cannot be retired.
        rows = baseline.inventory(ROOT)
        with self.assertRaises(baseline.BaselineError):
            baseline.generated_tables(rows, baseline.hash_domains(rows)[:1])

    def test_scratch_reset_plan_covers_changes_and_retained_old_value_is_red(self):
        scratch_root = ROOT / ".buck2/release-baseline-tests"
        scratch_root.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch_root) as temporary:
            repo = Path(temporary)
            before = self.scratch(repo)
            paths = set(before)
            public, edits = reset.plan(repo)
            self.assertEqual(public["hardcoded_workflow_schema_paths_after_reset"], [])
            self.assertEqual(before, {path: (repo / path).read_bytes() for path in paths}, "dry-run writes nothing")
            for path, text in edits.items():
                path.write_text(text)
            changed = {path for path in paths if before[path] != (repo / path).read_bytes()}
            self.assertEqual(changed, set(public["source_edits"]))
            self.assertEqual(baseline.mismatches(baseline.inventory(repo)), [])
            domains = (repo / baseline.BLAKE3_DOMAIN_TABLE).read_text()
            self.assertIn('"lash-process-env/v1"', domains)
            self.assertIn('"lash-process-env/v6"', domains.split('const RETIRED_BLAKE3_DOMAINS:')[1])
            self.assertIn("lash-process-env/v6", baseline.retired_hash_domains(repo))
            source = repo / "crates/lash-remote-protocol/src/lib.rs"
            source.write_text(source.read_text().replace("REMOTE_PROTOCOL_VERSION: u32 = 1;",
                                                       "REMOTE_PROTOCOL_VERSION: u32 = 100;"))
            errors = baseline.mismatches(baseline.inventory(repo))
            self.assertTrue(any("REMOTE_PROTOCOL_VERSION" in error for error in errors))

            # After the reset every store is at version 1 in compat.rs alone,
            # schema.sql states it, and the catalogs are numbered in it. An
            # artifact that keeps an old number, a backend that restates the
            # version, and a catalog step numbered past it are red.
            self.assertEqual(baseline.sqlite_stamp_mismatches(repo), [])
            self.assertEqual(baseline.postgres_stamp_mismatches(repo), [])
            self.assertEqual({value for _, value in baseline.store_versions(repo).values()}, {1})
            check = subprocess.run(
                [sys.executable, str(ROOT / "scripts/release_baseline.py"), "--repo", str(repo), "check"],
                capture_output=True, text=True, cwd=ROOT,
            )
            self.assertIn("REMOTE_PROTOCOL_VERSION", check.stderr)
            self.assertNotIn("stamp", check.stderr)
            artifact = repo / "crates/lash-postgres-store/schema.sql"
            reset_artifact = artifact.read_text()
            self.assertTrue(reset_artifact.startswith("-- lash-postgres-store schema, component version 1.\n"))
            self.assertIn("VALUES ('lash-postgres-store', 1, 1)", reset_artifact)
            artifact.write_text(reset_artifact.replace("VALUES ('lash-postgres-store', 1, 1)",
                                                       "VALUES ('lash-postgres-store', 141, 141)"))
            errors = baseline.postgres_stamp_mismatches(repo)
            self.assertTrue(any("seed stamp 141/141" in error and "POSTGRES_SCHEMA_VERSION = 1" in error
                                for error in errors), errors)
            check = subprocess.run(
                [sys.executable, str(ROOT / "scripts/release_baseline.py"), "--repo", str(repo), "check"],
                capture_output=True, text=True, cwd=ROOT,
            )
            self.assertIn("seed stamp 141/141", check.stderr)
            artifact.write_text(reset_artifact)
            lib = repo / "crates/lash-postgres-store/src/lib.rs"
            reset_lib = lib.read_text()
            alias = "const SCHEMA_VERSION: i32 = lash_core_execution::compat::POSTGRES_SCHEMA_VERSION as i32;"
            self.assertIn(alias, reset_lib)
            lib.write_text(reset_lib.replace(alias, "const SCHEMA_VERSION: i32 = 1;"))
            errors = baseline.postgres_stamp_mismatches(repo)
            self.assertTrue(any("must be the i32 alias" in error for error in errors), errors)
            lib.write_text(reset_lib)
            expand = repo / "crates/lash-postgres-store/src/postgres/migrate.rs"
            reset_expand = expand.read_text()
            self.assertIn("static EXPAND_MIGRATIONS: &[ExpandMigration] = &[", reset_expand)
            # The reset removes the production chain and keeps the synthetic
            # successor's row, cfg and all.
            self.assertNotIn('id: "0141-begin-session-close"', reset_expand)
            self.assertIn('    #[cfg(feature = "synthetic-next")]\n    ExpandMigration {\n'
                          '        id: "synthetic-next-expand",\n'
                          "        from_version: SCHEMA_VERSION,\n"
                          "        to_version: SCHEMA_VERSION + 1,", reset_expand)
            expand.write_text(reset_expand.replace(
                "static EXPAND_MIGRATIONS: &[ExpandMigration] = &[",
                "static EXPAND_MIGRATIONS: &[ExpandMigration] = &[\n"
                "    ExpandMigration {\n"
                "        id: \"0002-past-the-stamp\",\n"
                "        from_version: 1,\n"
                "        to_version: 9,\n"
                "        statements: \"-- past the stamp\",\n"
                "    },", 1))
            errors = baseline.postgres_stamp_mismatches(repo)
            self.assertTrue(any("expand step 1 to 9 is outside" in error for error in errors), errors)
            expand.write_text(reset_expand)
            schema = repo / "crates/lash-sqlite-store/src/schema.rs"
            reset_schema = schema.read_text()
            schema.write_text(reset_schema + "\nconst PROCESS_SCHEMA_VERSION: i32 = 44;\n")
            errors = baseline.sqlite_stamp_mismatches(repo)
            self.assertTrue(errors and all("PROCESS_SCHEMA_VERSION" in error and "restated" in error
                                           for error in errors), errors)
            schema.write_text(reset_schema)
            catalog = repo / "crates/lash-sqlite-store/src/migration.rs"
            reset_catalog = catalog.read_text()
            step = ("from: compat::SQLITE_CORE_SCHEMA_VERSION,\n"
                    "        to: compat::SQLITE_CORE_SCHEMA_VERSION + 1,")
            self.assertIn(step, reset_catalog)
            catalog.write_text(reset_catalog.replace(step, "from: 2,\n        to: 3,", 1))
            errors = baseline.sqlite_stamp_mismatches(repo)
            self.assertTrue(any("DurableCore step 2 to 3 is outside" in error for error in errors), errors)
            self.assertTrue(any("DurableCore has no step chain from the default stamp 1" in error
                                for error in errors), errors)
            catalog.write_text(reset_catalog.replace(
                step, "from: compat::SQLITE_REGISTRY_SCHEMA_VERSION,\n"
                      "        to: compat::SQLITE_REGISTRY_SCHEMA_VERSION + 1,", 1))
            with self.assertRaises(baseline.BaselineError):
                baseline.sqlite_stamp_mismatches(repo)


if __name__ == "__main__":
    unittest.main()

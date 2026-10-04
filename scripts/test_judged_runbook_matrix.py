import importlib.util
import json
import pathlib
import re
import subprocess
import sys
import unittest

import yaml


ROOT = pathlib.Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "judged_runbook_matrix", ROOT / "scripts" / "judged_runbook_matrix.py"
)
assert SPEC and SPEC.loader
MATRIX = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MATRIX)

COVERAGE_MATRIX_SECTION = "## Example coverage matrix"
MATRIX_EXPRESSION = re.compile(r"\$\{\{\s*matrix\.([A-Za-z0-9_]+)\s*\}\}")


def cited_ci_names(rules_text: str) -> set:
    """Capitalized code spans in the RULES.md coverage-matrix section.

    A capitalized code span in the matrix cites a CI display name — a job,
    step, or workflow — by convention; lowercase spans are recipes, labels,
    and other identifiers the workflows would never name.
    """
    section = rules_text.split(COVERAGE_MATRIX_SECTION, 1)[1].split("\n## ", 1)[0]
    return {
        span
        for span in re.findall(r"`([^`]+)`", section)
        if span[:1].isupper()
    }


def expanded_job_names(template: str, matrix: dict) -> set:
    """A job's display-name template plus every `${{ matrix.<key> }}`
    expansion the declared matrix produces. The literal template stays a
    valid citation: it is the name as the workflow file writes it."""
    expanded = {template}
    keys = MATRIX_EXPRESSION.findall(template)
    if keys and isinstance(matrix, str):
        if matrix != "${{ fromJSON(needs.plan.outputs.restate_matrix) }}":
            raise ValueError(f"unresolved dynamic CI matrix: {matrix}")
        matrix = json.loads(subprocess.check_output(
            [sys.executable, str(ROOT / "scripts/ci/restate_matrix.py"), "matrix"], text=True
        ))
    for key in keys:
        values = set(matrix.get(key) or [])
        values.update(
            entry[key]
            for entry in matrix.get("include") or []
            if isinstance(entry, dict) and key in entry
        )
        if not values:
            continue
        expanded = {
            re.sub(
                r"\$\{\{\s*matrix\." + re.escape(key) + r"\s*\}\}",
                str(value),
                name,
            )
            for name in expanded
            for value in values
        }
    return expanded | {template}


def workflow_ci_names() -> set:
    """Every display name `.github/workflows/` defines: workflow names, job
    ids and names (with matrix expansions), and step names."""
    names = set()
    workflows = sorted((ROOT / ".github" / "workflows").glob("*.yml"))
    workflows += sorted((ROOT / ".github" / "workflows").glob("*.yaml"))
    for workflow in workflows:
        document = yaml.safe_load(workflow.read_text())
        names.add(document.get("name"))
        for job_id, job in (document.get("jobs") or {}).items():
            names.add(job_id)
            names.add(job.get("name"))
            if job.get("name"):
                names.update(
                    expanded_job_names(
                        job["name"],
                        (job.get("strategy") or {}).get("matrix") or {},
                    )
                )
            for step in job.get("steps") or []:
                if isinstance(step, dict):
                    names.add(step.get("name"))
    names.discard(None)
    return names


class JudgedRunbookMatrixTests(unittest.TestCase):
    def test_typescript_host_flows_keep_their_cells_and_judged_row(self) -> None:
        with MATRIX.MATRIX.open("rb") as handle:
            config = MATRIX.tomllib.load(handle)
        emitted = [
            row for row in MATRIX.rows(config)
            if row["scenario"] == "typescript-host-flows"
        ]
        self.assertEqual(len(emitted), 1)
        self.assertEqual(emitted[0]["label"], "typescript")
        self.assertTrue((ROOT / emitted[0]["runbook"]).is_file())
        for cell in ("turn.ts", "durable-process.ts"):
            self.assertTrue((ROOT / "examples" / "typescript-host-flows" / cell).is_file())
        tests = ROOT / "crates" / "lash-typescript" / "tests"
        self.assertIn("mod host_flow_examples;", (tests / "main.rs").read_text())
        self.assertIn("typescript-host-flows", (tests / "host_flow_examples.rs").read_text())
        self.assertEqual(MATRIX.MATRIX.name, "judged-matrix.toml")

    def test_every_existing_runbook_has_exactly_one_typescript_row(self) -> None:
        # Discovery plus the row shape in one test: a runbook directory that
        # nobody classified is as invisible as a scenario that quietly emits a
        # second paid row under a language the tree no longer has.
        with MATRIX.MATRIX.open("rb") as handle:
            config = MATRIX.tomllib.load(handle)
        rows = MATRIX.rows(config)
        ordinary = set(config["scenarios"])
        excluded = set().union(
            *(
                set(config[group])
                for group in config["groups"]
                if group != "scenarios"
            )
        )
        discovered = {
            path.parent.name
            for path in (ROOT / "runbooks").glob("*/runbook.md")
            if path.parent.name not in excluded
        }
        self.assertEqual(discovered, ordinary)
        self.assertEqual(config["language"], "typescript")
        for scenario in ordinary:
            self.assertEqual(
                [row["label"] for row in rows if row["scenario"] == scenario],
                ["typescript"],
                f"`{scenario}` must emit exactly one row, labelled typescript",
            )

    def test_no_row_carries_a_retired_language_id(self) -> None:
        # ADR 0096 leaves one language id. A row labelled with the retired
        # surface would claim a session pin no host can serve, and the artifact
        # directory it names would be evidence of nothing.
        with MATRIX.MATRIX.open("rb") as handle:
            config = MATRIX.tomllib.load(handle)
        labels = {row["label"] for row in MATRIX.rows(config)}
        self.assertEqual(labels, {"typescript", "standard"})
        self.assertNotIn("dialects", config)
        self.assertNotIn("lashlang", MATRIX.MATRIX.read_text())

    def test_the_matrix_lists_no_scenario_twice(self) -> None:
        # A scenario in two groups is invisible to a per-group check while the
        # shard set silently grows, and every extra row is a paid judged run.
        # (TOML already refuses a repeated key inside one group.)
        with MATRIX.MATRIX.open("rb") as handle:
            config = MATRIX.tomllib.load(handle)
        listed = [
            name for group in config["groups"] for name in config.get(group, {})
        ]
        duplicates = sorted({name for name in listed if listed.count(name) > 1})
        self.assertEqual(duplicates, [], f"the matrix classifies {duplicates} twice")
        rows = MATRIX.rows(config)
        keys = [(row["scenario"], row["label"]) for row in rows]
        repeated = sorted({key for key in keys if keys.count(key) > 1})
        self.assertEqual(repeated, [], f"the matrix emits {repeated} more than once")

    def test_no_rlm_session_scenarios_get_one_mode_labelled_row(self) -> None:
        # A scenario that opens no RLM session has no language to pin, so its
        # row is labelled with the mode. Labelling it `typescript` would claim a
        # session pin the evidence cannot show.
        with MATRIX.MATRIX.open("rb") as handle:
            config = MATRIX.tomllib.load(handle)
        rows = MATRIX.rows(config)
        for scenario in config["no_rlm_session_only"]:
            emitted = [row for row in rows if row["scenario"] == scenario]
            self.assertEqual(
                [row["label"] for row in emitted],
                ["standard"],
                f"`{scenario}` must emit exactly one mode-labelled row",
            )
            self.assertNotIn(scenario, config["scenarios"])

    def test_scripted_live_model_scenarios_name_their_runner_and_no_language(
        self,
    ) -> None:
        # These rows are owned by a shell oracle, not by the judged shard, so
        # the runner is the only thing the matrix can be trusted on. A
        # per-entry language list would be a second source of truth for a
        # choice ADR 0096 removed.
        with MATRIX.MATRIX.open("rb") as handle:
            config = MATRIX.tomllib.load(handle)
        emitted = {row["scenario"] for row in MATRIX.rows(config)}
        for scenario, entry in config["scripted_live_model"].items():
            self.assertEqual(entry["runner"], "just rlm-smoke-e2e")
            self.assertNotIn("dialects", entry)
            self.assertNotIn(scenario, emitted)

    def test_the_row_total_is_the_stated_arithmetic(self) -> None:
        # The count is cited in the report, the runbook rules and the shard
        # plan. Deriving it here means a reclassification cannot silently leave
        # those citations stale.
        with MATRIX.MATRIX.open("rb") as handle:
            config = MATRIX.tomllib.load(handle)
        expected = sum(
            len(config[group])
            for group, policy in config["groups"].items()
            if policy["emits"]
        )
        self.assertEqual(len(MATRIX.rows(config)), expected)
        self.assertEqual(expected, 32)

    def test_every_scenario_declares_a_valid_tier_and_its_tier_model(self) -> None:
        # The tier word is what a reader trusts; the slug is what the bill is
        # for. A row whose model does not match its tier is a funding claim the
        # evidence cannot support, and nothing else in the repository looks.
        with MATRIX.MATRIX.open("rb") as handle:
            config = MATRIX.tomllib.load(handle)
        self.assertEqual(MATRIX.tier_violations(config), [])
        self.assertEqual(
            sorted(config["tiers"]), ["deterministic", "economy", "frontier"]
        )
        for item in MATRIX.rows(config):
            self.assertIn(item["tier"], config["tiers"])
            self.assertIn(item["model"], config["tiers"][item["tier"]])

    def test_a_mismatched_tier_model_is_rejected(self) -> None:
        # Drives the checker with the mutation it exists to catch, so an
        # assertion that only reads the shipped file cannot pass vacuously.
        with MATRIX.MATRIX.open("rb") as handle:
            config = MATRIX.tomllib.load(handle)
        config["no_rlm_session_only"]["workbench-valid-empty-completion"]["model"] = config[
            "tiers"
        ]["frontier"][0]
        self.assertNotEqual(MATRIX.tier_violations(config), [])
        config["no_rlm_session_only"]["workbench-valid-empty-completion"]["tier"] = "platinum"
        self.assertNotEqual(MATRIX.tier_violations(config), [])

    def test_every_non_emitting_row_names_something_that_exists(self) -> None:
        # `deterministic_only` and `scripted_live_model` rows are excluded from
        # discovery and never emitted, so no other assertion reads them. The
        # shipped matrix resolves today; this pins that it keeps resolving.
        with MATRIX.MATRIX.open("rb") as handle:
            config = MATRIX.tomllib.load(handle)
        self.assertEqual(MATRIX.referent_violations(config), [])

    def test_a_dangling_non_emitting_row_is_rejected(self) -> None:
        # The mutation the checker exists to catch, in both shapes: a
        # deterministic harness whose runbook is gone, and a scripted case
        # directory that was renamed out from under its row.
        with MATRIX.MATRIX.open("rb") as handle:
            config = MATRIX.tomllib.load(handle)
        config["deterministic_only"]["runbook-that-was-deleted"] = {
            "tier": "deterministic",
            "model": "scripted-provider",
        }
        self.assertNotEqual(MATRIX.referent_violations(config), [])

        with MATRIX.MATRIX.open("rb") as handle:
            config = MATRIX.tomllib.load(handle)
        config["scripted_live_model"]["rlm-smoke-case-that-was-renamed"] = {
            "tier": "economy",
            "model": "deepseek/deepseek-v4-flash",
            "runner": "just rlm-smoke-e2e",
        }
        self.assertNotEqual(MATRIX.referent_violations(config), [])

    def test_a_paid_row_may_not_be_served_entirely_by_the_dev_provider(self) -> None:
        # `tier_violations` reads only the matrix, so it cannot see a row whose
        # every phase is scripted but whose tier buys a real model. The slug is
        # then never requested and the row's evidence names a driver that
        # produced none of it.
        with MATRIX.MATRIX.open("rb") as handle:
            config = MATRIX.tomllib.load(handle)
        self.assertEqual(MATRIX.deterministic_provider_violations(config), [])

    def test_a_dev_provider_row_funded_at_a_paid_tier_is_rejected(self) -> None:
        # The red side, and the precondition that makes the green side mean
        # something: `workbench-session-resume` drives `replay-route-change`
        # through every phase, so funding it at a paid tier must be refused,
        # and declaring which phases are scripted must clear it.
        with MATRIX.MATRIX.open("rb") as handle:
            config = MATRIX.tomllib.load(handle)
        entry = config["scenarios"]["workbench-session-resume"]
        self.assertEqual(entry["tier"], "deterministic")
        self.assertIn(
            "AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO",
            (
                MATRIX.ROOT / "runbooks" / "workbench-session-resume" / "runbook.md"
            ).read_text(),
        )
        entry["tier"] = "economy"
        entry["model"] = config["tiers"]["economy"][0]
        self.assertNotEqual(MATRIX.deterministic_provider_violations(config), [])
        entry["deterministic_phases"] = "every phase"
        self.assertEqual(MATRIX.deterministic_provider_violations(config), [])

    def test_no_deterministic_scenario_names_a_paid_model(self) -> None:
        # The tier's whole claim is that the row makes no provider network
        # call. A deterministic row pointing at a real slug would spend money
        # under a label that says it cannot.
        with MATRIX.MATRIX.open("rb") as handle:
            config = MATRIX.tomllib.load(handle)
        self.assertEqual(config["tiers"]["deterministic"], ["scripted-provider"])
        for item in MATRIX.rows(config):
            if item["tier"] == "deterministic":
                self.assertNotIn("/", item["model"])

    def test_shards_are_disjoint_and_complete(self) -> None:
        # Drives the script's own selection, so a regression in the shard
        # arithmetic turns this red. Re-implementing the split here tested
        # Python, not the script: an off-by-one that dropped one row of the total kept
        # this green.
        with MATRIX.MATRIX.open("rb") as handle:
            config = MATRIX.tomllib.load(handle)
        expected = MATRIX.rows(config)
        for count in (1, 3, 7):
            with self.subTest(count=count):
                shards = [
                    MATRIX.select_shard(expected, index, count)
                    for index in range(1, count + 1)
                ]
                self.assertEqual(
                    sum(map(len, shards)),
                    len(expected),
                    "sharding must be lossless and non-overlapping",
                )
                self.assertEqual(
                    [(row["scenario"], row["label"]) for row in expected],
                    sorted(
                        (
                            (row["scenario"], row["label"])
                            for shard in shards
                            for row in shard
                        ),
                        key=lambda key: [
                            (row["scenario"], row["label"]) for row in expected
                        ].index(key),
                    ),
                    "every row must appear in exactly one shard",
                )

    def test_shard_arguments_outside_the_range_are_refused(self) -> None:
        for bad in ("0/3", "4/3", "1/0", "x/3", "3"):
            with self.subTest(shard=bad):
                with self.assertRaises(Exception):
                    MATRIX.parse_shard(bad)
        self.assertEqual(MATRIX.parse_shard("2/3"), (2, 3))

    def test_deleted_operator_pages_have_no_documentation_or_example_consumers(self) -> None:
        candidates = [ROOT / "CONTRIBUTING.md"]
        candidates.extend((ROOT / "runbooks").glob("**/*.md"))
        candidates.extend((ROOT / "docs").glob("**/*.md"))
        candidates.extend((ROOT / "examples").glob("**/*.rs"))
        violations = []
        for path in candidates:
            text = path.read_text()
            for deleted_page in ("docs/operations.html", "docs/PUBLISHING.md"):
                if deleted_page in text:
                    violations.append(f"{path.relative_to(ROOT)}: {deleted_page}")
        self.assertEqual(violations, [])

    def test_the_coverage_matrix_cites_only_ci_names_that_exist(self) -> None:
        # The matrix is the source of truth for the coverage split; a cited
        # job or step that no longer exists keeps reading as coverage while
        # nothing checks it. Matrix expressions are expanded so a per-leg
        # citation such as `Functional E2E (agent-workbench)` resolves.
        cited = cited_ci_names((ROOT / "runbooks" / "RULES.md").read_text())
        missing = sorted(cited - workflow_ci_names())
        self.assertEqual(
            missing,
            [],
            f"RULES.md cites CI names no workflow defines: {missing}",
        )

    def test_a_stale_ci_name_citation_is_rejected(self) -> None:
        # The red side: the retired `Test shard` job name the matrix used to
        # cite must still be detectable as a violation, so the check cannot
        # pass vacuously on an empty cited set.
        cited = cited_ci_names(
            f"{COVERAGE_MATRIX_SECTION}\n\n"
            "| `example` | `Test shard ${{ matrix.shard }}/4` | x | x |\n"
            "\n## Following section\n"
        )
        self.assertEqual(cited, {"Test shard ${{ matrix.shard }}/4"})
        self.assertNotIn(
            "Test shard ${{ matrix.shard }}/4", workflow_ci_names()
        )

if __name__ == "__main__":
    unittest.main()

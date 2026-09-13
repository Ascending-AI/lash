#!/usr/bin/env python3
"""Holds `tools/kiln/services.json` to the CI jobs it claims to reproduce.

`kiln test --service <name>` starts a container locally and then runs the same
`scripts/ci/store-tests.sh` suites the `Test Postgres store` and `Test S3 store`
jobs run. That claim is only worth something while the two stay in lockstep, so
this checks the properties that would silently drift:

  * every suite the manifest runs is a suite the workflow runs, in the same
    order within each service, and every workflow suite is claimed by exactly
    one service;
  * the container images match the ones the workflow starts;
  * nothing hardcodes a host port -- the runner picks a free one, so every
    connection string interpolates `{port}`;
  * `store-tests.sh` still requires its shared-cache environment inside CI and
    still supplies the `kiln build` configuration outside it;
  * the `not_covered` report names real commands, so a reader who follows it
    does not hit an unknown recipe.
"""

from __future__ import annotations

import json
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MANIFEST = ROOT / "tools" / "kiln" / "services.json"
STORE_TESTS = ROOT / "scripts" / "ci" / "store-tests.sh"
WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"


def manifest() -> dict:
    return json.loads(MANIFEST.read_text(encoding="utf-8"))


def workflow_suites() -> list:
    """Suite names in `run: bash scripts/ci/store-tests.sh <suite>` order.

    Read with a regex rather than a YAML parser so this file has no dependency
    beyond the standard library, matching the other scripts/test_*.py gates.
    """
    return re.findall(
        r"run: bash scripts/ci/store-tests\.sh ([a-z0-9-]+)\s*$",
        WORKFLOW.read_text(encoding="utf-8"),
        flags=re.MULTILINE,
    )


def workflow_images() -> str:
    return WORKFLOW.read_text(encoding="utf-8")


class KilnServiceManifest(unittest.TestCase):
    def test_every_suite_is_a_ci_suite_and_every_ci_suite_is_claimed(self) -> None:
        declared = workflow_suites()
        self.assertTrue(declared, "the workflow dispatches no store suites")
        claimed = []
        for name, service in manifest()["services"].items():
            with self.subTest(service=name):
                suites = [step["run"][-1] for step in service["steps"]]
                for suite in suites:
                    self.assertIn(suite, declared, f"{name} runs an unknown suite")
                # Order within a service is the workflow's order: the suites
                # share one database and one bucket, and pg-store truncating
                # every lash_* table after a later suite seeded it is exactly
                # the kind of reordering that only fails intermittently.
                self.assertEqual(
                    sorted(suites, key=declared.index),
                    suites,
                    f"{name} reorders the CI suites",
                )
                claimed.extend(suites)
        # A suite no service claims is a suite that only CI ever runs, which is
        # the coverage gap this manifest exists to close.
        self.assertEqual(set(declared), set(claimed))

    def test_images_match_the_workflow(self) -> None:
        workflow = workflow_images()
        services = manifest()["services"]
        for major in ("14", "16", "18"):
            self.assertEqual(
                f"postgres:{major}-alpine", services[f"pg{major}"]["image"]
            )
        self.assertIn("postgres:${{ matrix.postgres }}-alpine", workflow)
        self.assertIn(services["s3"]["image"], workflow)
        # The mc client the readiness probe and the bucket setup use is the
        # same pinned release the workflow pulls.
        mc_probe = services["s3"]["ready"]["host"]
        self.assertTrue(any(image in workflow for image in mc_probe))

    def test_no_host_port_is_hardcoded(self) -> None:
        """The runner picks a free port; a literal would collide between lanes."""
        for name, service in manifest()["services"].items():
            with self.subTest(service=name):
                rendered = json.dumps(
                    {
                        key: value
                        for key, value in service.items()
                        if key != "port"
                    }
                )
                for literal in ("5432", ":9000"):
                    self.assertNotIn(literal, rendered)
                for value in service["test_env"].values():
                    if "127.0.0.1" in value:
                        self.assertIn("{port}", value)

    def test_postgres_services_pass_the_same_image_settings_as_ci(self) -> None:
        workflow = workflow_images()
        self.assertIn("-c shared_preload_libraries=pg_stat_statements", workflow)
        for name, service in manifest()["services"].items():
            if not name.startswith("pg"):
                continue
            with self.subTest(service=name):
                self.assertEqual(
                    ["-c", "shared_preload_libraries=pg_stat_statements"],
                    service["command"],
                )
                self.assertEqual(
                    {
                        "POSTGRES_USER": "lash",
                        "POSTGRES_PASSWORD": "lash",
                        "POSTGRES_DB": "lash",
                    },
                    service["env"],
                )

    def test_store_tests_keeps_ci_strict_and_supplies_the_kiln_configuration(
        self,
    ) -> None:
        """The generalisation must not weaken CI.

        Inside GitHub Actions both shared-cache variables stay required: an
        unset value there means the credentials step did not run, and a build
        that quietly missed the shared cache is the failure this refuses. The
        default outside CI is the same `--config=shared` `kiln build` uses.
        """
        script = STORE_TESTS.read_text(encoding="utf-8")
        self.assertIn('if [ -n "${GITHUB_ACTIONS:-}" ]; then', script)
        self.assertIn('"${BAZEL_SHARED_CACHE_FLAGS:?', script)
        self.assertIn('"${BAZEL_OUTPUT_USER_ROOT:?', script)
        self.assertIn(
            ': "${BAZEL_SHARED_CACHE_FLAGS=--config=shared'
            ' --strategy=TestRunner=local}"',
            script,
        )
        # The trust decision itself is never defaulted: a run that cannot say
        # which path it is on must fail, not guess.
        self.assertIn(
            'trusted="${BAZEL_TRUSTED:?BAZEL_TRUSTED must be', script
        )
        # Every service takes the trusted, pool-built path locally.
        for name, service in manifest()["services"].items():
            with self.subTest(service=name):
                self.assertEqual("true", service["test_env"]["BAZEL_TRUSTED"])

    def test_not_covered_names_runnable_recipes(self) -> None:
        entries = manifest()["not_covered"]
        self.assertTrue(entries)
        justfile = (ROOT / "justfile").read_text(encoding="utf-8")
        for entry in entries:
            with self.subTest(case=entry["case"]):
                self.assertTrue(entry["reason"])
                recipe = entry["recipe"]
                if recipe.startswith("just "):
                    target = recipe.split()[1]
                    self.assertRegex(justfile, rf"(?m)^{re.escape(target)}[ :]")
                elif recipe.startswith("bash "):
                    self.assertTrue((ROOT / recipe.split()[1]).is_file())
                else:
                    self.assertTrue(recipe.startswith("cargo "))


if __name__ == "__main__":
    unittest.main()

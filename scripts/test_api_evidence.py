#!/usr/bin/env python3

import contextlib
import io
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import api_evidence


class FacadeConfigurationTests(unittest.TestCase):
    def test_facade_packages_are_derived_from_library_targets(self) -> None:
        self.assertEqual(
            api_evidence.facade_packages(
                {
                    "lash": "lash-runtime",
                    "lash_restate": "lash-internal-restate",
                }
            ),
            [
                (api_evidence.FACADE_SPECS[0], "lash-runtime"),
                (api_evidence.FACADE_SPECS[1], "lash-internal-restate"),
            ],
        )

    def test_scrape_builds_both_facades_and_restate_example_feature(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            snapshot = Path(directory) / "snapshot"
            target = Path(directory) / "target"
            environment = {"CARGO_TARGET_DIR": "original"}
            with (
                mock.patch.object(api_evidence, "run") as run,
                mock.patch.object(
                    api_evidence.time, "monotonic", side_effect=[10.0, 12.5]
                ),
            ):
                elapsed = api_evidence.rustdoc_scrape(
                    snapshot,
                    target,
                    ["lash-runtime", "lash-internal-restate"],
                    list(api_evidence.DEFAULT_EXAMPLE_PACKAGES),
                    api_evidence.EXAMPLE_FEATURES,
                    environment,
                )

        self.assertEqual(elapsed, 2.5)
        run.assert_called_once_with(
            [
                "cargo",
                "+nightly",
                "doc",
                "-Zunstable-options",
                "-Zrustdoc-scrape-examples",
                "--no-deps",
                "-p",
                "agent-service",
                "-p",
                "agent-workbench",
                "-p",
                "lash-internal-restate",
                "-p",
                "lash-runtime",
                "-p",
                "slack-clone",
                "--features",
                "agent-service/restate",
            ],
            cwd=snapshot,
            env={"CARGO_TARGET_DIR": str(target)},
        )


class GapGateTests(unittest.TestCase):
    PACKAGES = [
        {
            "name": name,
            "manifest_path": str(
                api_evidence.REPO / "examples" / name / "Cargo.toml"
            ),
        }
        for name in api_evidence.DEFAULT_EXAMPLE_PACKAGES
    ]
    COVERED = api_evidence.SurfaceItem(
        "lash::run", "function", "lash::run", ()
    )
    GAP = api_evidence.SurfaceItem(
        "lash_restate::RestateEffectGroupServices::new",
        "function",
        "lash_restate::effect_group::RestateEffectGroupServices::new",
        (),
    )

    def report(self, *, enforce: bool) -> tuple[int, str, str]:
        evidence = api_evidence.CallEvidence(
            "lash::run",
            "plain-function",
            tuple(
                f"examples/{package['name']}/src/main.rs"
                for package in self.PACKAGES
            ),
        )
        output = io.StringIO()
        errors = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
            status = api_evidence.print_report(
                [self.COVERED, self.GAP],
                {"lash::run": evidence},
                self.PACKAGES,
                1.0,
                1,
                enforce=enforce,
                all_gaps=True,
            )
        return status, output.getvalue(), errors.getvalue()

    def test_check_fails_and_names_every_uncovered_candidate(self) -> None:
        status, output, errors = self.report(enforce=True)

        self.assertEqual(status, 1)
        self.assertIn("lash_restate: 1 item(s), 0/1", output)
        self.assertIn(self.GAP.symbol, output)
        self.assertIn("lash_restate=1", errors)

    def test_report_remains_advisory_without_check(self) -> None:
        status, output, errors = self.report(enforce=False)

        self.assertEqual(status, 0)
        self.assertIn("UNCOVERED direct-call candidates (1; all shown)", output)
        self.assertEqual(errors, "")


if __name__ == "__main__":
    unittest.main()

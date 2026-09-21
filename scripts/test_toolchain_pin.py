#!/usr/bin/env python3
"""Contract test: the pinned Rust toolchain is one version, named in three
places.

`rust-toolchain.toml` selects the toolchain for local Cargo/rustup use,
`.github/actions/rust-toolchain/action.yml` carries the default every CI
install step resolves, and `MODULE.bazel` declares the version rules_rs
registers for the hermetic Bazel toolchain. All three must agree: a drift of
one patch release is exactly the "local green, CI red" divergence FIG-1672
shipped, except slower to notice because each site looks correct alone.

The pin lives in the action's `toolchain` input *default*, not in a per-step
input: one default is the single source of truth, while forty copies of a
literal version are forty places a bump PR can miss. Call sites therefore must
not pass `toolchain:` at all. The sole exception is `toolchain-canary.yml`,
whose whole purpose is resolving a floating channel (stable/beta) instead of
the pin.
"""

from __future__ import annotations

from pathlib import Path
import re
import tomllib
import unittest

import yaml


ROOT = Path(__file__).resolve().parents[1]
TOOLCHAIN_TOML = ROOT / "rust-toolchain.toml"
TOOLCHAIN_ACTION = ROOT / ".github" / "actions" / "rust-toolchain" / "action.yml"
MODULE_BAZEL = ROOT / "MODULE.bazel"
WORKFLOWS = ROOT / ".github" / "workflows"

# The canary deliberately floats; every other workflow resolves the pin.
FLOATING_TOOLCHAIN_WORKFLOWS = {"toolchain-canary.yml"}

RUST_TOOLCHAIN_USES = "./.github/actions/rust-toolchain"


def pin_sites() -> dict[str, str]:
    """The three places the pinned version is stated, keyed by site name."""
    channel = tomllib.loads(TOOLCHAIN_TOML.read_text(encoding="utf-8"))[
        "toolchain"
    ]["channel"]
    action_default = str(
        yaml.safe_load(TOOLCHAIN_ACTION.read_text(encoding="utf-8"))["inputs"][
            "toolchain"
        ]["default"]
    )
    block = re.search(
        r"toolchains\.toolchain\((.*?)\)",
        MODULE_BAZEL.read_text(encoding="utf-8"),
        flags=re.DOTALL,
    )
    assert block is not None, "MODULE.bazel declares no toolchains.toolchain"
    bazel_version = re.search(r'version\s*=\s*"([^"]+)"', block.group(1))
    assert bazel_version is not None
    return {
        "rust-toolchain.toml channel": channel,
        "rust-toolchain action default": action_default,
        "MODULE.bazel toolchain version": bazel_version.group(1),
    }


def workflow_steps(path: Path) -> list[dict[str, object]]:
    document = yaml.safe_load(path.read_text(encoding="utf-8"))
    steps: list[dict[str, object]] = []
    for job in (document.get("jobs") or {}).values():
        if isinstance(job, dict):
            steps.extend(
                step for step in (job.get("steps") or []) if isinstance(step, dict)
            )
    return steps


class ToolchainPinTests(unittest.TestCase):
    def test_all_three_pin_sites_agree(self) -> None:
        sites = pin_sites()
        self.assertEqual(
            1,
            len(set(sites.values())),
            f"toolchain pin sites disagree: {sites}",
        )

    def test_the_pin_is_a_version_not_a_channel(self) -> None:
        # `stable` here would silently re-float CI: the whole point of the pin
        # is that a Rust release changes nothing until a PR bumps it.
        for site, version in pin_sites().items():
            with self.subTest(site=site):
                self.assertRegex(version, r"^[0-9]+\.[0-9]+\.[0-9]+$")

    def test_call_sites_inherit_the_pin(self) -> None:
        for path in sorted(WORKFLOWS.glob("*.yml")) + sorted(
            WORKFLOWS.glob("*.yaml")
        ):
            floating = path.name in FLOATING_TOOLCHAIN_WORKFLOWS
            for step in workflow_steps(path):
                uses = str(step.get("uses", ""))
                if not uses.startswith(RUST_TOOLCHAIN_USES):
                    continue
                toolchain = (step.get("with") or {}).get("toolchain")
                with self.subTest(workflow=path.name, step=step.get("name")):
                    if floating:
                        self.assertIsNotNone(
                            toolchain,
                            "canary must name the floating channel it tests",
                        )
                    else:
                        self.assertIsNone(
                            toolchain,
                            "call sites inherit the action default; the pin "
                            "lives in exactly one place",
                        )


if __name__ == "__main__":
    unittest.main()

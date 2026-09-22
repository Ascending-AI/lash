#!/usr/bin/env python3
from __future__ import annotations

import importlib.util
import pathlib
import subprocess
import sys
import tomllib
import unittest


ROOT = pathlib.Path(__file__).resolve().parent.parent


def load_evidence():
    """Import the driver's pure helpers without the browser dependency."""
    spec = importlib.util.spec_from_file_location(
        "slack_clone_thread_evidence", ROOT / "scripts/slack_clone_thread_evidence.py"
    )
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


class SlackCloneFullHostE2eContractTest(unittest.TestCase):
    def test_runner_rejects_arguments_before_starting_services(self) -> None:
        runner = ROOT / "scripts/slack-clone-full-host-e2e.sh"
        syntax = subprocess.run(
            ["bash", "-n", runner],
            cwd=ROOT,
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(syntax.returncode, 0, syntax.stderr)

        rejected = subprocess.run(
            ["bash", runner, "unexpected"],
            cwd=ROOT,
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(rejected.returncode, 2)
        self.assertIn("usage:", rejected.stderr)

    def test_thread_root_is_selected_by_author_identity_not_by_marker_text(self) -> None:
        driver = load_evidence()
        history = [
            {"ts": "1.000001", "is_bot": False, "text": "ada: FIG1341-AMBIENT-ONE says cobalt"},
            {"ts": "1.000002", "is_bot": False, "text": "ada: FIG1341-AMBIENT-TWO says cedar"},
            # What a model actually answers when asked to recall the ambient facts.
            {"ts": "1.000004", "is_bot": True, "text": "FIG1341-AMBIENT-ONE says cobalt, and #general exists."},
        ]
        self.assertEqual(
            driver.select_thread_root(history, "FIG1341-AMBIENT-ONE")["ts"], "1.000001"
        )
        with self.assertRaises(AssertionError):
            driver.select_thread_root(history, "FIG1341-AMBIENT-THREE")
        with self.assertRaises(AssertionError):
            driver.select_thread_root(
                history + [{"ts": "1.000005", "is_bot": False, "text": "FIG1341-AMBIENT-ONE again"}],
                "FIG1341-AMBIENT-ONE",
            )

    def test_inherited_prefix_is_read_from_lineage_not_from_child_node_rows(self) -> None:
        driver = load_evidence()
        # `fork_at` writes no graph nodes into the child, so the child's own rows
        # never hold ancestor text — which is why an isolation gate written
        # against them cannot fail, and this one must be written against the
        # ancestor chain the lineage row names.
        nodes = [
            {"session_id": "channel:C1", "node_id": "n1", "parent_node_id": None, "node_json": "root marker"},
            {"session_id": "channel:C1", "node_id": "n2", "parent_node_id": "n1", "node_json": "post-fork marker"},
            {"session_id": "thread:C1:1", "node_id": "t1", "parent_node_id": None, "node_json": "child turn"},
        ]
        child_rows = [node for node in nodes if node["session_id"] == "thread:C1:1"]
        self.assertNotIn("post-fork marker", str(child_rows))

        correct = [{"session_id": "thread:C1:1", "ancestor_session_id": "channel:C1", "fork_node_id": "n1"}]
        inherited = driver.inherited_prefix_nodes(nodes, correct, "thread:C1:1", "channel:C1")
        self.assertEqual([node["node_id"] for node in inherited], ["n1"])
        self.assertNotIn("post-fork marker", str(inherited))

        late = [{"session_id": "thread:C1:1", "ancestor_session_id": "channel:C1", "fork_node_id": "n2"}]
        leaked = driver.inherited_prefix_nodes(nodes, late, "thread:C1:1", "channel:C1")
        self.assertEqual([node["node_id"] for node in leaked], ["n1", "n2"])
        self.assertIn("post-fork marker", str(leaked))

        with self.assertRaises(AssertionError):
            driver.inherited_prefix_nodes(nodes, [], "thread:C1:1", "channel:C1")

    def test_driver_and_host_agree_on_the_thread_root_seed(self) -> None:
        driver = load_evidence()
        threads = (ROOT / "examples/slack-clone/src/bot/threads.rs").read_text(encoding="utf-8")
        self.assertIn(f'"{driver.THREAD_ROOT_SEED_PREFIX}"', threads)

    def test_a_seed_label_that_starts_mid_line_is_not_counted_as_a_label(self) -> None:
        driver = load_evidence()
        prefix = driver.THREAD_ROOT_SEED_PREFIX
        # What the host writes: the break ahead of the label is its own doing.
        good = {"messages": [{"text": f"ada: an earlier line\n{prefix}ada: the root\n"}]}
        self.assertEqual(driver.seed_label_line_starts(good), (1, 1))
        # What concatenation does without it — the defect this gate exists for.
        bad = {"messages": [{"text": f"ada: an earlier line{prefix}ada: the root\n"}]}
        self.assertEqual(driver.seed_label_line_starts(bad), (1, 0))
        self.assertEqual(driver.seed_label_line_starts({"messages": []}), (0, 0))

    def test_e2e_provider_is_feature_gated(self) -> None:
        manifest = tomllib.loads(
            (ROOT / "examples/slack-clone/Cargo.toml").read_text(encoding="utf-8")
        )
        self.assertEqual(manifest["features"]["e2e"], ["lash/testing"])


if __name__ == "__main__":
    unittest.main()

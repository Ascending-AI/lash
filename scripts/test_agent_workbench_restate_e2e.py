#!/usr/bin/env python3
"""The agent-workbench Restate E2E driver's cleanup contract."""

import os
import pathlib
import socket
import subprocess
import tempfile
import unittest


REPO = pathlib.Path(__file__).resolve().parents[1]
SCRIPT = REPO / "scripts" / "agent-workbench-restate-e2e.sh"


def run_cleanup(root: pathlib.Path, run_status: int, token: str, *, expect: int) -> subprocess.CompletedProcess[str]:
    """Source the driver and run its cleanup over manifests under `root`."""
    artifact = root / "artifacts"
    source = f"""
        source "{SCRIPT}"
        artifact_dir='{artifact}'
        data_manifest="$artifact_dir/fixture-data.manifest"
        child_manifest="$artifact_dir/fixture-children.manifest"
        endpoint_manifest="$artifact_dir/fixture-endpoints.manifest"
        cleanup_log="$artifact_dir/cleanup.log"
        cleanup_token='{token}'
        set +e
        agent_workbench_cleanup {run_status}
        test "$?" -eq {expect}
    """
    env = os.environ.copy()
    env.update({"TMPDIR": str(root), "AGENT_WORKBENCH_E2E_KEEP_ARTIFACTS": "1"})
    return subprocess.run(["bash", "-c", source], cwd=REPO, env=env, text=True, capture_output=True, check=False)


class AgentWorkbenchRestateCleanupTest(unittest.TestCase):
    def fixture(self, root: pathlib.Path, *, marker: str | None, endpoints: str = "") -> pathlib.Path:
        artifact = root / "artifacts"
        artifact.mkdir()
        data = root / "fixture-data"
        data.mkdir()
        if marker is not None:
            (data / ".agent-workbench-fixture-owner").write_text(marker, encoding="utf-8")
        (artifact / "fixture-data.manifest").write_text(f"{data}\n", encoding="utf-8")
        (artifact / "fixture-children.manifest").write_text("", encoding="utf-8")
        (artifact / "fixture-endpoints.manifest").write_text(endpoints, encoding="utf-8")
        return data

    def test_tokenized_fixture_data_is_removed_and_the_run_status_kept(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            data = self.fixture(root, marker="run-token")
            result = run_cleanup(root, 3, "run-token", expect=3)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertFalse(data.exists())
            cleanup = (root / "artifacts" / "cleanup.log").read_text(encoding="utf-8")
            self.assertIn("owned_data_removed=true", cleanup)

    def test_fixture_data_with_a_foreign_token_is_kept(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            data = self.fixture(root, marker="someone-else")
            result = run_cleanup(root, 0, "run-token", expect=97)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertTrue(data.exists())
            cleanup = (root / "artifacts" / "cleanup.log").read_text(encoding="utf-8")
            self.assertIn("owned_data_preimage_valid=false", cleanup)

    def test_an_endpoint_left_listening_fails_the_cleanup(self) -> None:
        with tempfile.TemporaryDirectory() as directory, socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            listener.listen()
            port = listener.getsockname()[1]
            root = pathlib.Path(directory)
            self.fixture(root, marker="run-token", endpoints=f"127.0.0.1:{port}\n")
            result = run_cleanup(root, 0, "run-token", expect=97)
            self.assertEqual(result.returncode, 0, result.stderr)
            cleanup = (root / "artifacts" / "cleanup.log").read_text(encoding="utf-8")
            self.assertIn(f"owned_endpoint_closed=false addr=127.0.0.1:{port}", cleanup)


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
"""Launcher tests must never share the host's ownership or process namespace."""

import concurrent.futures
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]


class LauncherSandboxTests(unittest.TestCase):
    def test_concurrent_launchers_have_independent_global_locks(self):
        # The script must remain visible after the sandbox hides /tmp.
        git_dir = subprocess.check_output(
            ["git", "rev-parse", "--absolute-git-dir"], cwd=ROOT, text=True
        ).strip()
        with tempfile.TemporaryDirectory(dir=git_dir) as fixture:
            probe = Path(fixture) / "probe.sh"
            probe.write_text(
                '#!/usr/bin/env bash\nset -euo pipefail\n'
                f'source "{ROOT}/scripts/ci/launcher-test-sandbox.sh"\n'
                'launcher_test_sandbox "$@"\n'
                'root="/tmp/lash-agent-workbench-$UID"\n'
                'mkdir -m 700 "$root"\n'
                'exec 9>"$root/data-ownership.lock"\n'
                'flock -n 9\n'
                'printf "%s\\n" "$(readlink /proc/self/ns/mnt)"\n'
                'test ! -e "$1"\n'
                'test ! -e /run/docker.sock\n'
                'sleep 0.2\n'
            )
            with tempfile.NamedTemporaryFile(dir="/tmp") as host_file:
                def run():
                    return subprocess.run(
                        ["bash", str(probe), host_file.name],
                        capture_output=True, text=True, timeout=20,
                    )

                with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                    results = list(pool.map(lambda _: run(), range(2)))
            for result in results:
                self.assertEqual(result.returncode, 0, result.stderr)
            self.assertNotEqual(results[0].stdout, results[1].stdout)
            self.assertNotEqual(
                results[0].stdout.strip(),
                subprocess.check_output(
                    ["readlink", "/proc/self/ns/mnt"], text=True
                ).strip(),
            )


if __name__ == "__main__":
    unittest.main()

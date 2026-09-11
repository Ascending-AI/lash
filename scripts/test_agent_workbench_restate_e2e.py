#!/usr/bin/env python3
import os
import pathlib
import socket
import subprocess
import tempfile
import unittest


REPO = pathlib.Path(__file__).resolve().parents[1]
SCRIPT = REPO / "scripts" / "agent-workbench-restate-e2e.sh"


def run_bash(
    source: str,
    *,
    env: dict[str, str] | None = None,
    unset: tuple[str, ...] = (),
) -> subprocess.CompletedProcess[str]:
    merged = os.environ.copy()
    for name in unset:
        merged.pop(name, None)
    if env:
        merged.update(env)
    return subprocess.run(
        ["bash", "-c", source],
        cwd=REPO,
        env=merged,
        text=True,
        capture_output=True,
        check=False,
    )


class AgentWorkbenchRestateE2eTest(unittest.TestCase):
    ADDRESS_OVERRIDES = (
        "RESTATE_ADMIN_URL",
        "RESTATE_INGRESS_URL",
        "AGENT_WORKBENCH_E2E_DATABASE_URL",
        "AGENT_WORKBENCH_E2E_ENDPOINT_BIND",
        "AGENT_WORKBENCH_E2E_POSTGRES_ENDPOINT_BIND",
        "AGENT_WORKBENCH_E2E_ENDPOINT_URL",
        "AGENT_WORKBENCH_E2E_POSTGRES_ENDPOINT_URL",
    )

    def test_default_port_plan_is_distinct(self) -> None:
        result = run_bash(f'source "{SCRIPT}"; agent_workbench_default_port_plan 61000')
        self.assertEqual(result.returncode, 0, result.stderr)
        ports = [int(value) for value in result.stdout.splitlines()]
        self.assertEqual(ports, [61030, 61031, 61032, 61033, 61034, 61035])
        self.assertEqual(len(ports), len(set(ports)))

    def test_preflight_refuses_an_existing_name_without_removing_it(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            calls = root / "docker-calls"
            docker = root / "docker"
            docker.write_text(
                "#!/usr/bin/env bash\n"
                f"printf '%s\\n' \"$*\" >>'{calls}'\n"
                "if [ \"$1 $2\" = 'container inspect' ]; then exit 0; fi\n"
                "exit 1\n",
                encoding="utf-8",
            )
            docker.chmod(0o755)
            result = run_bash(
                f"""
                source "{SCRIPT}"
                restate_container=occupied-restate
                postgres_container=unused-postgres
                admin_port=65001; ingress_port=65002; node_port=65003
                endpoint_port=65004; postgres_port=65005; postgres_endpoint_port=65006
                set +e
                agent_workbench_refuse_preexisting_resources
                status=$?
                test "$status" -eq 73
                """,
                env={"PATH": f"{root}:{os.environ['PATH']}"},
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertNotIn("rm ", calls.read_text(encoding="utf-8"))

    def test_exact_owned_addresses_are_accepted(self) -> None:
        result = run_bash(
            f"""
            source "{SCRIPT}"
            admin_port=62030; ingress_port=62031; postgres_port=62034
            admin_url=http://127.0.0.1:62030
            ingress_url=http://127.0.0.1:62031
            database_url=postgres://lash:lash@127.0.0.1:62034/lash
            endpoint_bind=127.0.0.1:62033
            postgres_endpoint_bind=127.0.0.1:62035
            unset AGENT_WORKBENCH_E2E_ENDPOINT_URL
            unset AGENT_WORKBENCH_E2E_POSTGRES_ENDPOINT_URL
            agent_workbench_validate_owned_addresses
            test "$endpoint_port" = 62033
            test "$postgres_endpoint_port" = 62035
            """,
            unset=self.ADDRESS_OVERRIDES,
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_mismatched_addresses_exit_before_any_mutation(self) -> None:
        cases = {
            "RESTATE_ADMIN_URL": "http://203.0.113.1:1",
            "RESTATE_INGRESS_URL": "http://203.0.113.2:2",
            "AGENT_WORKBENCH_E2E_DATABASE_URL": "postgres://secret@203.0.113.3/db",
            "AGENT_WORKBENCH_E2E_ENDPOINT_BIND": "0.0.0.0:64000",
            "AGENT_WORKBENCH_E2E_POSTGRES_ENDPOINT_BIND": "0.0.0.0:64001",
            "AGENT_WORKBENCH_E2E_ENDPOINT_URL": "http://203.0.113.4:4",
            "AGENT_WORKBENCH_E2E_POSTGRES_ENDPOINT_URL": "http://203.0.113.5:5",
        }
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            calls = root / "mutation-calls"
            sentinel = root / "preexisting-data"
            sentinel.write_text("untouched", encoding="utf-8")
            for command in ["docker", "cargo"]:
                executable = root / command
                executable.write_text(
                    "#!/usr/bin/env bash\n"
                    f"printf '%s %s\\n' \"$(basename \"$0\")\" \"$*\" >>'{calls}'\n"
                    "exit 99\n",
                    encoding="utf-8",
                )
                executable.chmod(0o755)
            for index, (variable, value) in enumerate(cases.items()):
                with self.subTest(variable=variable):
                    artifact = root / f"artifacts-{index}"
                    result = run_bash(
                        f'bash "{SCRIPT}"',
                        env={
                            "PATH": f"{root}:{os.environ['PATH']}",
                            "AGENT_WORKBENCH_E2E_ARTIFACT_DIR": str(artifact),
                            variable: value,
                        },
                        unset=tuple(name for name in self.ADDRESS_OVERRIDES if name != variable),
                    )
                    self.assertEqual(result.returncode, 73, result.stderr)
                    self.assertIn(variable, result.stderr)
                    self.assertNotIn(value, result.stderr)
                    self.assertFalse(artifact.exists())
                    self.assertFalse(calls.exists())
                    self.assertEqual(sentinel.read_text(encoding="utf-8"), "untouched")

    def test_preflight_refuses_an_existing_listener(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            docker = root / "docker"
            docker.write_text("#!/usr/bin/env bash\nexit 1\n", encoding="utf-8")
            docker.chmod(0o755)
            with socket.socket() as listener:
                listener.bind(("127.0.0.1", 0))
                listener.listen()
                occupied = listener.getsockname()[1]
                result = run_bash(
                    f"""
                    source "{SCRIPT}"
                    restate_container=unused-restate
                    postgres_container=unused-postgres
                    admin_port={occupied}; ingress_port=65012; node_port=65013
                    endpoint_port=65014; postgres_port=65015; postgres_endpoint_port=65016
                    set +e
                    agent_workbench_refuse_preexisting_resources
                    test "$?" -eq 73
                    """,
                    env={"PATH": f"{root}:{os.environ['PATH']}"},
                )
            self.assertEqual(result.returncode, 0, result.stderr)

    def test_failed_engine_removal_retains_tokenized_data(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            artifact = root / "artifacts"
            data = root / "fixture-data"
            artifact.mkdir()
            data.mkdir()
            token = "owned-fixture-token"
            (data / ".agent-workbench-fixture-owner").write_text(token, encoding="utf-8")
            (artifact / "fixture-data.manifest").write_text(f"{data}\n", encoding="utf-8")
            for name in ["fixture-children.manifest", "fixture-endpoints.manifest"]:
                (artifact / name).write_text("", encoding="utf-8")
            docker = root / "docker"
            docker.write_text(
                "#!/usr/bin/env bash\n"
                "case \"$1\" in\n"
                "  logs) exit 0 ;;\n"
                "  rm|ps) exit 1 ;;\n"
                "esac\n"
                "exit 1\n",
                encoding="utf-8",
            )
            docker.chmod(0o755)
            result = run_bash(
                f"""
                source "{SCRIPT}"
                lash_gate_cleanup() {{ :; }}
                artifact_dir='{artifact}'
                data_manifest="$artifact_dir/fixture-data.manifest"
                child_manifest="$artifact_dir/fixture-children.manifest"
                endpoint_manifest="$artifact_dir/fixture-endpoints.manifest"
                cleanup_log="$artifact_dir/cleanup.log"
                restate_log="$artifact_dir/restate.log"
                postgres_log="$artifact_dir/postgres.log"
                cleanup_token='{token}'
                restate_id=owned-restate-id
                postgres_id=
                set +e
                agent_workbench_cleanup 1
                test "$?" -eq 97
                """,
                env={
                    "PATH": f"{root}:{os.environ['PATH']}",
                    "TMPDIR": str(root),
                    "AGENT_WORKBENCH_E2E_KEEP_ARTIFACTS": "1",
                },
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertTrue(data.exists())
            cleanup = (artifact / "cleanup.log").read_text(encoding="utf-8")
            self.assertIn("owned_engine_removal_verified=0", cleanup)
            self.assertIn("owned_data_retained_engine_unverified=true", cleanup)


if __name__ == "__main__":
    unittest.main()

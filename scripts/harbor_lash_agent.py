"""Harbor adapter for running lash headlessly inside benchmark environments."""

from __future__ import annotations

import os
import shlex
from pathlib import Path

from harbor.agents.installed.base import BaseInstalledAgent, ExecInput
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_LASH_BINARY = REPO_ROOT / "target" / "release" / "lash"
OPTIONAL_LIBS_DIR = REPO_ROOT / "bench" / "libs"
HOST_LASH_CONFIG = Path.home() / ".lash" / "config.json"

REMOTE_HOME = "/installed-agent/home"
REMOTE_LASH_CONFIG = f"{REMOTE_HOME}/.lash/config.json"


class LashAgent(BaseInstalledAgent):
    @staticmethod
    def name() -> str:
        return "lash"

    @property
    def _install_agent_template_path(self) -> Path:
        return Path(__file__).resolve().parent / "install-lash.sh.j2"

    async def setup(self, environment: BaseEnvironment) -> None:
        await environment.exec(
            command=f"mkdir -p /installed-agent/libs {REMOTE_HOME}/.lash"
        )

        # Optional host-provided libs are disabled by default because they may
        # be ABI-incompatible with task images (e.g. older glibc in benchmark containers).
        use_optional_libs = os.environ.get("LASH_BENCH_USE_OPTIONAL_LIBS") == "1"
        self._use_optional_libs = use_optional_libs

        binary_path = Path(os.environ.get("LASH_BENCH_BINARY", str(DEFAULT_LASH_BINARY)))
        if not binary_path.exists():
            raise FileNotFoundError(
                f"Expected lash binary at {binary_path}. Build it before running Harbor."
            )

        await environment.upload_file(
            source_path=str(binary_path),
            target_path="/installed-agent/lash",
        )

        if use_optional_libs and OPTIONAL_LIBS_DIR.exists():
            for lib in OPTIONAL_LIBS_DIR.iterdir():
                if lib.is_file():
                    await environment.upload_file(
                        source_path=str(lib),
                        target_path=f"/installed-agent/libs/{lib.name}",
                    )

        if HOST_LASH_CONFIG.exists():
            await environment.upload_file(
                source_path=str(HOST_LASH_CONFIG),
                target_path=REMOTE_LASH_CONFIG,
            )
        else:
            self.logger.warning(
                "No local lash config found at %s; run may require env-based provider auth.",
                HOST_LASH_CONFIG,
            )

        await super().setup(environment)

    def create_run_agent_commands(self, instruction: str) -> list[ExecInput]:
        env: dict[str, str] = {
            "HOME": REMOTE_HOME,
            # Bench tasks can involve long thinking phases with sparse stream chunks.
            # Use a higher default than interactive runs; allow override from host env.
            "LASH_LLM_STREAM_TIMEOUT_SECS": os.environ.get(
                "LASH_LLM_STREAM_TIMEOUT_SECS", "300"
            ),
        }

        if getattr(self, "_use_optional_libs", False):
            env["LD_LIBRARY_PATH"] = "/installed-agent/libs"

        for key in (
            "OPENROUTER_API_KEY",
            "ANTHROPIC_API_KEY",
            "TAVILY_API_KEY",
            "LASH_LOG",
            "LASH_ALLOW_UNKNOWN_MODELS",
            "LASH_LLM_STREAM_TIMEOUT_SECS",
        ):
            value = os.environ.get(key, "")
            if value:
                env[key] = value

        provider_flag = "--provider " if os.environ.get("LASH_PROVIDER_SETUP") == "1" else ""
        model_flag = (
            f"--model {shlex.quote(self.model_name)} " if self.model_name else ""
        )
        prompt_flags = ""
        for env_key, section in (
            ("LASH_PROMPT_REPLACE_IDENTITY", "identity"),
            ("LASH_PROMPT_REPLACE_GUIDELINES", "guidelines"),
            ("LASH_PROMPT_REPLACE_TOOL_GUIDES", "tool_guides"),
        ):
            value = os.environ.get(env_key)
            if value:
                prompt_flags += (
                    f"--prompt-replace {shlex.quote(f'{section}={value}')} "
                )

        disable_sections = os.environ.get("LASH_PROMPT_DISABLE", "").strip()
        if disable_sections:
            for section in disable_sections.split(","):
                sec = section.strip()
                if sec:
                    prompt_flags += f"--prompt-disable {shlex.quote(sec)} "
        prompt = shlex.quote(instruction)

        return [
            ExecInput(
                command=f"lash {provider_flag}{model_flag}{prompt_flags}--print {prompt}",
                env=env,
                timeout_sec=None,
            )
        ]

    async def _persist_lash_log(self, environment: BaseEnvironment) -> None:
        """Best-effort copy of lash runtime log into Harbor trial artifacts."""
        source = f"{REMOTE_HOME}/.lash/lash.log"
        target = self.logs_dir / "lash.log"
        try:
            await environment.download_file(source_path=source, target_path=target)
        except Exception as exc:  # pragma: no cover - defensive, non-fatal
            self.logger.warning("Failed to persist lash log from %s: %s", source, exc)

    async def _persist_lash_sessions(self, environment: BaseEnvironment) -> None:
        """Best-effort copy of lash session artifacts (db/jsonl/llm trace) into trial logs."""
        sessions_dir = f"{REMOTE_HOME}/.lash/sessions"
        target_dir = self.logs_dir / "sessions"
        target_dir.mkdir(parents=True, exist_ok=True)

        list_cmd = (
            "bash -lc "
            + shlex.quote(
                f"ls -1 {sessions_dir}/*.db {sessions_dir}/*.jsonl {sessions_dir}/*.llm.jsonl "
                "2>/dev/null | sort -u || true"
            )
        )

        try:
            result = await environment.exec(command=list_cmd)
        except Exception as exc:  # pragma: no cover - defensive, non-fatal
            self.logger.warning("Failed to enumerate lash session artifacts: %s", exc)
            return

        if result.return_code != 0:
            self.logger.warning(
                "Failed to enumerate lash session artifacts (rc=%s): %s",
                result.return_code,
                result.stderr or "",
            )
            return

        for remote_path in [
            line.strip() for line in (result.stdout or "").splitlines() if line.strip()
        ]:
            target = target_dir / Path(remote_path).name
            try:
                await environment.download_file(source_path=remote_path, target_path=target)
            except Exception as exc:  # pragma: no cover - defensive, non-fatal
                self.logger.warning(
                    "Failed to persist lash session artifact %s: %s", remote_path, exc
                )

    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        try:
            await super().run(instruction, environment, context)
        finally:
            await self._persist_lash_log(environment)
            await self._persist_lash_sessions(environment)

    def populate_context_post_run(self, context: AgentContext) -> None:
        pass

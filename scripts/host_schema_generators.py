#!/usr/bin/env python3
"""The host contract generators: the one list of which binaries own schemas/host.

Buck's //:host_schema_documents (emitted by tools/buck2/sync.py), the portable
CI check, release reset and the writer's complete-set admission all read this
list. The Rust binaries stay the authority for what each one emits.

Run directly, it prints `<cargo package> <binary>` per generator for shell
callers.
"""

from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True)
class Generator:
    package: str
    directory: str
    binary: str

    @property
    def label(self) -> str:
        return f"//{self.directory}:{self.binary}__bin"


GENERATORS = (
    Generator("lash-internal-trace", "crates/lash-trace", "trace_schema_generator"),
    Generator("lash-internal-vm-client", "crates/lash-vm-client", "worker_wire_schema_generator"),
    Generator(
        "lash-internal-core-execution",
        "crates/lash-core-execution",
        "process_event_schema_generator",
    ),
    Generator("lash-internal-core", "crates/lash-core", "process_workflow_schema_generator"),
    Generator("lash-kernel-doc", "crates/lash-kernel-doc", "kernel_schema_generator"),
    Generator("lash-kernel-migrate", "crates/lash-kernel-migrate", "kernel_migration_schema_generator"),
    Generator("lash-kernel-edit", "crates/lash-kernel-edit", "kernel_edit_schema_generator"),
    Generator("lash-kernel-view", "crates/lash-kernel-view", "kernel_view_schema_generator"),
    Generator(
        "lash-internal-postgres-store",
        "crates/lash-postgres-store",
        "host_config_schema_generator",
    ),
)

if len({generator.binary for generator in GENERATORS}) != len(GENERATORS):
    raise SystemExit("host schema generator binaries must be unique")


if __name__ == "__main__":
    for generator in GENERATORS:
        print(generator.package, generator.binary)

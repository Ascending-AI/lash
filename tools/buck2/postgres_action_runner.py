#!/usr/bin/env python3
"""Run one test command against a PostgreSQL server private to its action.

    postgres_action_runner.py TREE NSS_WRAPPER SCHEMA [--drop-libtest-marker] COMMAND...

The server is the pinned `native//:postgres` tree, so the action needs nothing
of its host but loopback and a writable temporary directory: it runs on the
pool like any other test and its verdict is cached. The runner initializes a
cluster, starts the server on a free loopback port, creates the `lash`
database, applies SCHEMA to it, runs COMMAND with
`LASH_POSTGRES_DATABASE_URL` naming that database, and stops the server and
deletes the cluster however COMMAND ends.

A caller that supplies `LASH_POSTGRES_DATABASE_URL` itself owns the server
(the PostgreSQL 14/18 lanes of `scripts/ci/with-service.sh`); COMMAND then
runs unchanged and nothing is started.

`initdb` looks its user up in the password database, and a pool action runs
as a user its read-only image does not list. NSS_WRAPPER, the pinned
`libnss_wrapper.so`, answers that one lookup; the server itself never asks.
"""
from __future__ import annotations

import os
from pathlib import Path
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time

URL = "LASH_POSTGRES_DATABASE_URL"
MARKER = "--lash-libtest-args"
USER = "lash"
DATABASE = "lash"
READY_SECONDS = 60
STOP_SECONDS = 5
# The settings of `scripts/ci/with-service.sh pg16`: the statement-count tests
# read pg_stat_statements, applying the schema takes more locks than the
# default table holds, and a throwaway cluster has nothing to make durable.
SETTINGS = {
    "listen_addresses": "127.0.0.1",
    "unix_socket_directories": "",
    "shared_preload_libraries": "pg_stat_statements",
    "max_connections": "100",
    "max_locks_per_transaction": "256",
    "fsync": "off",
    "synchronous_commit": "off",
    "full_page_writes": "off",
    "timezone": "UTC",
    "log_timezone": "UTC",
}


class ServerError(Exception):
    def __init__(self, fields: dict[str, str]):
        super().__init__(f"{fields.get('S', 'ERROR')} {fields.get('C', '')}: {fields.get('M', '')}")
        self.code = fields.get("C", "")


def receive(connection: socket.socket, count: int) -> bytes:
    data = bytearray()
    while len(data) < count:
        chunk = connection.recv(count - len(data))
        if not chunk:
            raise ConnectionError("the server closed the connection")
        data += chunk
    return bytes(data)


def until_ready(connection: socket.socket) -> None:
    """Read backend messages up to ReadyForQuery; an ErrorResponse raises."""
    while True:
        kind, length = struct.unpack("!cI", receive(connection, 5))
        payload = receive(connection, length - 4)
        if kind == b"E":
            fields = [field for field in payload.split(b"\0") if field]
            raise ServerError({field[:1].decode(): field[1:].decode("utf-8", "replace") for field in fields})
        if kind == b"R" and struct.unpack("!I", payload[:4])[0] != 0:
            raise ConnectionError("the server asked for a password; the cluster is initialized with trust")
        if kind == b"Z":
            return


def execute(port: int, database: str, sql: str) -> None:
    """Send `sql` as one simple query: the statements share a transaction."""
    with socket.create_connection(("127.0.0.1", port), timeout=READY_SECONDS) as connection:
        startup = struct.pack("!I", 196608) + f"user\0{USER}\0database\0{database}\0\0".encode()
        connection.sendall(struct.pack("!I", len(startup) + 4) + startup)
        until_ready(connection)
        query = sql.encode() + b"\0"
        connection.sendall(b"Q" + struct.pack("!I", len(query) + 4) + query)
        until_ready(connection)
        connection.sendall(b"X" + struct.pack("!I", 4))


def free_port() -> int:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def initialize(tree: Path, nss_wrapper: Path, work: Path) -> Path:
    data = work / "data"
    (work / "passwd").write_text(f"{USER}:x:{os.geteuid()}:{os.getegid()}::{work}:/bin/false\n")
    (work / "group").write_text(f"{USER}:x:{os.getegid()}:\n")
    environment = dict(
        os.environ,
        LD_PRELOAD=str(nss_wrapper),
        NSS_WRAPPER_PASSWD=str(work / "passwd"),
        NSS_WRAPPER_GROUP=str(work / "group"),
        LC_ALL="C",
        TZ="UTC",
    )
    # The ICU collation of the service lane, so the suites that compare key
    # order against the database's own locale have a linguistic one.
    command = [
        str(tree / "bin/initdb"), "--pgdata", str(data), "--username", USER, "--auth", "trust",
        "--encoding", "UTF8", "--locale", "C", "--locale-provider", "icu", "--icu-locale", "en-US",
        "--no-sync",
    ]
    result = subprocess.run(command, env=environment, stdin=subprocess.DEVNULL, capture_output=True, text=True)
    if result.returncode != 0:
        raise RuntimeError(f"initdb failed ({result.returncode}):\n{result.stdout}{result.stderr}")
    return data


def start(tree: Path, data: Path, log: Path) -> tuple[subprocess.Popen, int]:
    """Start the server as this process's child, in its process group, so
    whatever ends the group ends the server."""
    port = free_port()
    command = [str(tree / "bin/postgres"), "-D", str(data), "-c", f"port={port}"]
    for name, value in SETTINGS.items():
        command += ["-c", f"{name}={value}"]
    with log.open("wb") as output:
        server = subprocess.Popen(
            command, stdin=subprocess.DEVNULL, stdout=output, stderr=subprocess.STDOUT,
            env=dict(os.environ, LC_ALL="C", TZ="UTC"),
        )
    deadline = time.monotonic() + READY_SECONDS
    while True:
        if server.poll() is not None:
            raise RuntimeError(f"postgres exited with {server.returncode} while starting")
        try:
            execute(port, "postgres", "SELECT 1")
            return server, port
        except (OSError, ServerError) as error:
            # 57P03: the server accepts connections but is still starting up.
            if isinstance(error, ServerError) and error.code != "57P03":
                raise
            if time.monotonic() >= deadline:
                raise RuntimeError(f"postgres was not ready after {READY_SECONDS} s: {error}") from error
            time.sleep(0.02)


def stop(server: subprocess.Popen) -> None:
    if server.poll() is None:
        server.send_signal(signal.SIGINT)  # fast shutdown: sessions end now
        try:
            server.wait(timeout=STOP_SECONDS)
        except subprocess.TimeoutExpired:
            server.kill()
            server.wait()


def run(tree: Path, nss_wrapper: Path, schema: Path, command: list[str]) -> int:
    child: list[subprocess.Popen] = []
    interrupted: list[int] = []

    def forward(number, _frame):
        interrupted.append(number)
        for process in child:
            if process.poll() is None:
                process.send_signal(number)

    for number in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        signal.signal(number, forward)
    server = None
    with tempfile.TemporaryDirectory(prefix="lash-postgres-") as raw:
        work = Path(raw)
        log = work / "postgres.log"
        try:
            server, port = start(tree, initialize(tree, nss_wrapper, work), log)
            execute(port, "postgres", f'CREATE DATABASE "{DATABASE}"')
            execute(port, DATABASE, schema.read_text(encoding="utf-8"))
            if interrupted:
                return 128 + interrupted[0]
            environment = dict(os.environ)
            environment[URL] = f"postgres://{USER}:{USER}@127.0.0.1:{port}/{DATABASE}"
            child.append(subprocess.Popen(command, env=environment))
            code = child[0].wait()
            return 128 - code if code < 0 else code
        except (OSError, RuntimeError, ServerError) as error:
            print(f"postgres_action_runner: {error}", file=sys.stderr)
            if log.is_file():
                sys.stderr.write(log.read_text(encoding="utf-8", errors="replace")[-8192:])
            return 70
        finally:
            if server is not None:
                stop(server)


def main(argv: list[str]) -> int:
    if len(argv) < 4:
        print(__doc__, file=sys.stderr)
        return 64
    tree, nss_wrapper, schema = (Path(value).resolve() for value in argv[:3])
    command = argv[3:]
    if command[0] == "--drop-libtest-marker":
        command = command[1:]
        if MARKER in command:
            command.remove(MARKER)
    if not command:
        print(__doc__, file=sys.stderr)
        return 64
    if os.environ.get(URL):
        os.execvp(command[0], command)
    return run(tree, nss_wrapper, schema, command)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))

#!/usr/bin/env python3
"""Records what CPython does with each witness case.

Usage: python3 record.py            rewrite recorded.json from cases/*.py
       python3 record.py --check    fail if recorded.json is not what CPython gives

A case is a cell of the dialect: Python that may `await` at its top level
and call the tools `echo(x)`, which answers `x`, and `boom(x)`, which fails
with an error of class `boom` whose message is `x`. A case's first line may
script the order outcomes are delivered in:

    # deliver: echo:b0, echo:a0; sleep:50; echo:a1

Each `;` ends the batch delivered at one park. Once the script is used up
the oldest pending request is delivered, one per park.

A run is recorded as the dialect's laws record the kernel machine: one
epoch per stretch between deliveries, with the outcomes delivered into it,
the tool calls and sleeps it asked for and the lines it printed, and then
how the run ended. The event loop is asyncio's own; this script only finds
the moments it has nothing left to run, which is when a lash run parks, and
delivers there. A tool call or a sleep that its task abandoned before the
park, because the task was cancelled, was never handed to the host and is
not recorded as asked (`K-TASK-024`). A sleep longer than zero is a request
the script answers, so no case waits on a clock. When the cell ends, the
tasks still running are cancelled and awaited, as `asyncio.run` does.
"""

import ast
import asyncio
import asyncio.runners
import inspect
import json
import pathlib
import platform
import sys

HERE = pathlib.Path(__file__).parent


class boom(Exception):
    """The error the tool `boom` fails with."""


def shown(value):
    return value if isinstance(value, str) else repr(value)


class Recorder:
    def __init__(self, deliveries):
        self.script = iter(deliveries)
        self.epochs = []
        self.epoch = {"delivered": [], "asked": [], "logged": []}
        self.requested = []
        self.pending = []
        self.loop = None

    def request(self, label, settle):
        future = self.loop.create_future()
        self.requested.append((label, future, settle))
        return future

    def print(self, *args, sep=" "):
        self.epoch["logged"].append(sep.join(str(arg) for arg in args))

    async def echo(self, x):
        return await self.request(f"echo:{shown(x)}", lambda future: future.set_result(x))

    async def boom(self, x):
        return await self.request(
            f"boom:{shown(x)}", lambda future: future.set_exception(boom(shown(x)))
        )

    async def sleep(self, seconds, result=None):
        if seconds <= 0:
            await REAL_SLEEP(0)
            return result
        milliseconds = int(round(seconds * 1000))
        await self.request(f"sleep:{milliseconds}", lambda future: future.set_result(None))
        return result

    def park(self):
        """The loop has nothing ready: hand out requests, deliver a batch."""
        live = [entry for entry in self.requested if not entry[1].cancelled()]
        self.requested = []
        self.pending = [entry for entry in self.pending if not entry[1].cancelled()]
        self.epoch["asked"] = [label for label, _, _ in live]
        self.pending.extend(live)
        if not self.pending:
            raise RuntimeError("the case is stuck: nothing is ready and nothing is pending")
        self.epochs.append(self.epoch)
        batch = next(self.script, None)
        if batch is None:
            batch = [self.pending[0][0]]
        for label in batch:
            index = next(
                (i for i, entry in enumerate(self.pending) if entry[0] == label), None
            )
            if index is None:
                raise RuntimeError(f"no pending request `{label}`")
            _, future, settle = self.pending.pop(index)
            settle(future)
        self.epoch = {"delivered": list(batch), "asked": [], "logged": []}


class Loop(asyncio.SelectorEventLoop):
    """asyncio's loop, told where it runs dry."""

    recorder = None

    def _run_once(self):
        if not self._ready and not self._scheduled and not self._stopping:
            self.recorder.park()
        super()._run_once()


REAL_SLEEP = asyncio.sleep


def run_case(source, deliveries):
    recorder = Recorder(deliveries)
    loop = Loop()
    loop.recorder = recorder
    recorder.loop = loop
    namespace = {
        "__name__": "__main__",
        "print": recorder.print,
        "echo": recorder.echo,
        "boom": recorder.boom,
        "boom_error": boom,
    }
    code = compile(source, "<case>", "exec", flags=ast.PyCF_ALLOW_TOP_LEVEL_AWAIT)

    async def cell():
        result = eval(code, namespace)
        if inspect.iscoroutine(result):
            await result

    asyncio.sleep = recorder.sleep
    try:
        end = "ok"
        try:
            loop.run_until_complete(cell())
        except BaseException as error:  # noqa: BLE001 - the case's own error is the record
            end = f"error {type(error).__name__}: {error}"
        asyncio.runners._cancel_all_tasks(loop)
    finally:
        asyncio.sleep = REAL_SLEEP
        loop.close()
    recorder.epochs.append(recorder.epoch)
    return {"epochs": recorder.epochs, "end": end}


def deliveries_of(source):
    first = source.split("\n", 1)[0]
    if not first.startswith("# deliver:"):
        return []
    script = first[len("# deliver:"):]
    return [
        [label.strip() for label in batch.split(",") if label.strip()]
        for batch in script.split(";")
        if batch.strip()
    ]


def record():
    cases = []
    for path in sorted((HERE / "cases").glob("*.py")):
        source = path.read_text()
        deliveries = deliveries_of(source)
        try:
            outcome = run_case(source, deliveries)
        except Exception as error:
            raise SystemExit(f"{path.name}: {type(error).__name__}: {error}") from error
        cases.append(
            {"name": path.stem, "source": source, "deliveries": deliveries, **outcome}
        )
    return {"python": platform.python_version(), "cases": cases}


def main():
    recorded = json.dumps(record(), indent=1, ensure_ascii=False) + "\n"
    target = HERE / "recorded.json"
    if "--check" in sys.argv:
        if target.read_text() != recorded:
            raise SystemExit("recorded.json is not what CPython gives; run record.py")
        return
    target.write_text(recorded)


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Capture trusted dialect source against a deterministic scripted tool table."""

import argparse
import asyncio
import importlib.util
import json
import math
from pathlib import Path
import subprocess
import sys


def datum(value):
    if value is None:
        return "null"
    if isinstance(value, bool):
        return {"bool": value}
    if isinstance(value, int):
        return {"int": str(value)}
    if isinstance(value, float):
        text = "nan" if math.isnan(value) else "inf" if value == math.inf else "-inf" if value == -math.inf else repr(value)
        return {"float": text}
    if isinstance(value, str):
        return {"text": value}
    if isinstance(value, bytes):
        return {"bytes": value.hex()}
    if isinstance(value, tuple):
        return {"tuple": [datum(item) for item in value]}
    if isinstance(value, list):
        return {"list": [datum(item) for item in value]}
    if isinstance(value, dict):
        return {"record": [[key, datum(item)] for key, item in value.items()]}
    raise TypeError(f"unsupported witness datum {type(value).__name__}")


class ToolError(Exception):
    def __init__(self, error):
        super().__init__(error["message"])
        self.kind = error["kind"]
        self.data = error.get("data")


async def python_capture(source, script):
    spec = importlib.util.spec_from_file_location("witness_program", source)
    program = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(program)
    rows = script["tools"]
    batches = script["deliveries"]
    trace, prints, pending = [], [], []

    async def tool(name, *args):
        index = len(pending)
        if index >= len(rows) or rows[index]["name"] != name or rows[index]["args"] != list(args):
            raise AssertionError(f"unscripted tool {index}: {name}{args}")
        future = asyncio.get_running_loop().create_future()
        pending.append(future)
        trace.append({"phase": "requested", "id": index, "tool": name, "args": [datum(arg) for arg in args]})
        return await future

    def output(value):
        prints.append(datum(value))

    task = asyncio.create_task(program.main(tool, output))
    delivered = set()
    try:
        for batch in batches:
            # Drain runnable coroutines before a scripted committed delivery.
            for _ in range(100):
                await asyncio.sleep(0)
                if all(index < len(pending) for index in batch) or task.done():
                    break
            if task.done():
                raise AssertionError("program ended before delivery script was consumed")
            for index in batch:
                if index >= len(pending) or index in delivered:
                    raise AssertionError(f"unknown or repeated delivery {index}")
                delivered.add(index)
                row = rows[index]
                trace.append({"phase": "delivered", "id": index})
                if "error" in row:
                    pending[index].set_exception(ToolError(row["error"]))
                else:
                    pending[index].set_result(row["value"])
        for _ in range(100):
            if task.done():
                break
            await asyncio.sleep(0)
        if not task.done():
            raise AssertionError("program waits beyond its script")
        if len(pending) != len(rows) or delivered != set(range(len(rows))):
            raise AssertionError("unconsumed tools or outcomes")
        try:
            end = {"returned": datum(task.result())}
        except Exception as error:
            end = {"raised": {"kind": getattr(error, "kind", type(error).__name__), "message": str(error), "data": datum(getattr(error, "data", None))}}
        return {"end": end, "prints": prints, "trace": trace}
    finally:
        task.cancel()
        await asyncio.gather(task, return_exceptions=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("language", choices=["python", "javascript"])
    parser.add_argument("source", type=Path)
    parser.add_argument("script", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--node", type=Path, help="the vendored Node executable")
    args = parser.parse_args()
    if args.language == "javascript":
        root = Path(__file__).resolve().parents[3]
        node = args.node or root / ".buck2/native/node/bin/node"
        subprocess.run([str(node), str(Path(__file__).with_name("capture.mjs")),
                        str(args.source.resolve()), str(args.script.resolve()), str(args.output.resolve())], check=True)
    else:
        script = json.loads(args.script.read_text())
        result = asyncio.run(python_capture(args.source.resolve(), script))
        args.output.write_text(json.dumps(result, indent=2, ensure_ascii=False) + "\n")


if __name__ == "__main__":
    main()

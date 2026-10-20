#!/usr/bin/env python3
"""CPython oracle for FIG-6372, using exactly the Node oracle's tagged format."""
import ast
import asyncio
import json
import math
import struct
import sys
import types


def tag(value):
    if value is None:
        return {"type": "null"}
    if type(value) is bool:
        return {"type": "bool", "value": value}
    if type(value) is int:
        return {"type": "int", "value": str(value)}
    if type(value) is float:
        return {"type": "float", "bits": struct.pack(">d", math.nan if math.isnan(value) else value).hex()}
    if type(value) is str:
        return {"type": "str", "value": list(struct.unpack("<" + "H" * (len(value.encode("utf-16-le", "surrogatepass")) // 2), value.encode("utf-16-le", "surrogatepass")))}
    if type(value) is tuple:
        return {"type": "tuple", "value": [tag(item) for item in value]}
    if type(value) is list:
        return {"type": "list", "value": [tag(item) for item in value]}
    if type(value) is dict:
        return {"type": "dict", "value": [[tag(k), tag(v)] for k, v in value.items()]}
    if type(value) is set:
        return {"type": "set", "value": [tag(item) for item in value]}
    raise TypeError(f"Unrecordable {type(value).__name__}")


async def run(case):
    values = []

    async def observe(value):
        values.append(tag(value))

    namespace = {"host": types.SimpleNamespace(observe=observe), "print": lambda *args, **kwargs: None}
    try:
        code = compile(case["source"], case["id"], "exec", flags=ast.PyCF_ALLOW_TOP_LEVEL_AWAIT)
        pending = eval(code, namespace)
        if pending is not None:
            await pending
        return {"type": "observations", "value": values}
    except Exception as error:
        return {"type": "throw", "value": type(error).__name__}


async def main():
    for line in sys.stdin:
        case = json.loads(line)
        print(json.dumps({"id": case["id"], "result": await run(case)}, ensure_ascii=True), flush=True)


if __name__ == "__main__":
    asyncio.run(main())

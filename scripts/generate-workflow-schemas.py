#!/usr/bin/env python3
"""Generate or check the checked-in workflow JSON Schemas and host TypeScript declarations."""

from __future__ import annotations

import argparse
import json
import os
import re
from pathlib import Path
import subprocess
import sys


ROOT = Path(__file__).resolve().parent.parent
OUTPUT = ROOT / "schemas" / "host"

# The workflow view contracts are also committed inside the crate that owns
# the view, where TYPESCRIPT_DECLARATION and JSON_SCHEMA ship them via
# include_str!, so a host that vendors lash can check its copies hermetically.
# The crate's drift law checks both copies against the repository schemas.
CRATE_DECLARATIONS = {
    Path("kernel-workflow-view") / "v1.d.ts": Path("crates")
    / "lash-kernel-view"
    / "kernel-workflow-view-v1.d.ts",
    Path("kernel-workflow-view") / "v1.schema.json": Path("crates")
    / "lash-kernel-view"
    / "kernel-workflow-view-v1.schema.json",
}


def typescript_declarations(schema: dict[str, object]) -> str:
    """Render the host schema graph, with JSON constraints kept in the schema.

    Fail on unsupported schema forms rather than silently widening host types.
    Every local definition is exported so hosts can name nested facts.
    """
    definitions = schema.get("$defs", {})
    title = schema["title"]
    for name in [title, *definitions]:
        if not isinstance(name, str) or re.fullmatch(r"[A-Za-z_$][A-Za-z0-9_$]*", name) is None:
            raise ValueError(f"invalid TypeScript type name: {name!r}")

    def literal(value: object) -> str:
        if value is None or isinstance(value, (str, bool, int, float)):
            return json.dumps(value, ensure_ascii=True)
        raise ValueError(f"unsupported TypeScript literal: {value!r}")

    def comment(description: str, indent: int = 0) -> str:
        pad = " " * indent
        lines = description.replace("*/", "* /").splitlines()
        return pad + "/**\n" + "".join((pad + " * " + line).rstrip() + "\n" for line in lines) + pad + " */\n"

    def render(node: object, indent: int = 0) -> str:
        if isinstance(node, bool):
            return "unknown" if node else "never"
        supported = {
            "$defs", "$id", "$schema", "title", "description", "default",
            "x-lash-schema-version", "x-lash-version-constant",
            "$ref", "const", "enum", "oneOf", "anyOf", "allOf", "type",
            "properties", "required", "additionalProperties", "items",
            "format", "pattern", "minimum", "maximum", "minLength", "maxLength",
            "minItems", "maxItems", "uniqueItems", "multipleOf",
            "exclusiveMinimum", "exclusiveMaximum",
        }
        unknown = node.keys() - supported
        if unknown:
            raise ValueError(f"unsupported TypeScript schema keywords: {sorted(unknown)}")
        if "$ref" in node:
            reference = node["$ref"]
            prefix = "#/$defs/"
            if not reference.startswith(prefix) or reference[len(prefix):] not in definitions:
                raise ValueError(f"unresolved TypeScript schema reference: {reference}")
            return reference[len(prefix):]
        if "const" in node:
            return literal(node["const"])
        if "enum" in node:
            return "(" + " | ".join(literal(value) for value in node["enum"]) + ")"
        for keyword, operator in [("oneOf", " | "), ("anyOf", " | "), ("allOf", " & ")]:
            if keyword in node:
                return "(" + operator.join(render(part, indent) for part in node[keyword]) + ")"
        kind = node.get("type")
        if isinstance(kind, list):
            return "(" + " | ".join(render(dict(node, type=part), indent) for part in kind) + ")"
        if kind == "array":
            return "Array<" + render(node.get("items", True), indent) + ">"
        if kind == "object":
            required = node.get("required", [])
            rows = []
            pad = " " * (indent + 2)
            for name, field in node.get("properties", {}).items():
                if isinstance(field, dict) and field.get("description"):
                    rows.append(comment(field["description"], indent + 2).rstrip())
                optional = "" if name in required else "?"
                rows.append(pad + json.dumps(name) + optional + ": " + render(field, indent + 2) + ";")
            extra = node.get("additionalProperties", True)
            if extra is not False:
                rows.append(pad + "[key: string]: " + render(extra, indent + 2) + ";")
            if not rows:
                return "Record<string, never>"
            return "{\n" + "\n".join(rows) + "\n" + " " * indent + "}"
        primitives = {"string": "string", "integer": "number", "number": "number",
                      "boolean": "boolean", "null": "null"}
        if kind in primitives:
            return primitives[kind]
        if kind is None and not (node.keys() - {"description", "default", "title"}):
            return "unknown"
        raise ValueError(f"unsupported TypeScript schema form: {node}")

    output = [
        "// Derived from v1.schema.json by scripts/generate-workflow-schemas.py.\n",
        "// JSON Schema remains authoritative for value constraints and validation.\n\n",
    ]
    for name, definition in [(title, schema), *definitions.items()]:
        if definition.get("description"):
            output.append(comment(definition["description"]))
        output.append(f"export type {name} = {render(definition)};\n\n")
    return "".join(output).rstrip() + "\n"


def rendered_documents(payload: list[dict[str, object]]) -> dict[Path, str]:
    documents: dict[Path, str] = {}
    for document in payload:
        shape = document["shape"]
        version = document["version"]
        schema = document["schema"]
        if not isinstance(shape, str) or not isinstance(version, int):
            raise ValueError("generator returned an invalid shape registration")
        relative = Path(shape) / f"v{version}.schema.json"
        documents[relative] = json.dumps(schema, indent=2) + "\n"
        if shape in {"kernel-workflow-view", "kernel-authoring-operation"}:
            documents[Path(shape) / f"v{version}.d.ts"] = typescript_declarations(schema)
    return documents


def generate(
    check: bool,
    generators: list[Path] | None = None,
    generated: Path | None = None,
    output: Path | None = None,
) -> int:
    output = OUTPUT if output is None else output
    if generated is not None:
        documents = rendered_documents([
            {"shape": path.parent.name,
             "version": int(path.name.removeprefix("v").removesuffix(".schema.json")),
             "schema": json.loads(path.read_text(encoding="utf-8"))}
            for path in sorted(generated.glob("*/*.schema.json"))
        ])
        if not documents:
            print("generated workflow schema directory is empty", file=sys.stderr)
            return 1
    else:
        if not generators:
            raise ValueError("a generator or generated documents are required")
        payload: list[dict[str, object]] = []
        for generator in generators:
            completed = subprocess.run(
                [str(generator.resolve())], cwd=ROOT, check=False, capture_output=True, text=True
            )
            if completed.returncode != 0:
                sys.stderr.write(completed.stderr)
                return completed.returncode
            try:
                payload.extend(json.loads(completed.stdout))
            except (json.JSONDecodeError, TypeError) as error:
                print(f"invalid schema generator output: {error}", file=sys.stderr)
                return 1
        try:
            documents = rendered_documents(payload)
        except (json.JSONDecodeError, KeyError, TypeError, ValueError) as error:
            print(f"invalid workflow schema generator output: {error}", file=sys.stderr)
            return 1

    stale: list[str] = []
    for relative, expected in documents.items():
        path = output / relative
        if check:
            try:
                actual = path.read_text(encoding="utf-8")
            except OSError as error:
                stale.append(f"{path} cannot be read: {error}")
            else:
                if actual != expected:
                    stale.append(f"{path} differs")
        else:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(expected, encoding="utf-8")
            crate_relative = CRATE_DECLARATIONS.get(relative)
            if crate_relative is not None:
                crate_path = ROOT / crate_relative
                crate_path.parent.mkdir(parents=True, exist_ok=True)
                crate_path.write_text(expected, encoding="utf-8")

    expected_paths = {output / relative for relative in documents}
    if output.exists():
        for path in [*output.glob("*/*.schema.json"), *output.glob("*/*.d.ts")]:
            if path not in expected_paths:
                if check:
                    stale.append(f"{path} is obsolete")
                else:
                    path.unlink()

    if stale:
        print("workflow schema drift detected:", file=sys.stderr)
        print("\n".join(stale), file=sys.stderr)
        print("run `python3 scripts/generate-workflow-schemas.py`", file=sys.stderr)
        return 1
    if check:
        print("workflow schema documents are current")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check",
        action="store_true",
        help="fail if checked-in documents differ from fresh generation",
    )
    parser.add_argument("--generator", type=Path, action="append", dest="generators",
                        help="use an already built generator; may be repeated")
    parser.add_argument("--generated", type=Path, help="compare documents already generated by Buck2")
    parser.add_argument("--output", type=Path, default=OUTPUT)
    parser.add_argument("--stamp", type=Path, help="write a success receipt after comparison")
    args = parser.parse_args()
    if args.stamp is not None:
        args.stamp.unlink(missing_ok=True)
    if args.generators is None and args.generated is None:
        report_dir = ROOT / ".buck2" / "schema-reports"
        report_dir.mkdir(parents=True, exist_ok=True)
        report = report_dir / f"host-schema-{os.getpid()}.json"
        built = subprocess.run(
            [
                "scripts/hermetic-build.sh",
                "build",
                "--materializations",
                "final",
                "--build-report",
                str(report),
                "//:host_schema_documents",
            ],
            cwd=ROOT,
        )
        if built.returncode != 0:
            return built.returncode
        resolved = subprocess.run(
            [
                sys.executable,
                "tools/buck2/outputs.py",
                "--report",
                str(report),
                "--label",
                "//:host_schema_documents",
                "--single",
            ],
            cwd=ROOT,
            check=False,
            capture_output=True,
            text=True,
        )
        if resolved.returncode != 0:
            sys.stderr.write(resolved.stderr)
            return resolved.returncode
        args.generated = Path(resolved.stdout.strip())
    status = generate(args.check, args.generators, args.generated, args.output)
    if status == 0 and args.stamp is not None:
        args.stamp.write_text("workflow schema documents are current\n", encoding="utf-8")
    return status


if __name__ == "__main__":
    sys.exit(main())

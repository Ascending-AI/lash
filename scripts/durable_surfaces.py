"""Collect durable roots from Rust ownership declarations, using the type resolver."""
from __future__ import annotations

from dataclasses import dataclass
import re

import check_version_bumps as gate


@dataclass(frozen=True)
class Record:
    path: str
    name: str
    surface: str | None
    output: str
    kind: str | None
    offset: int
    is_step: bool
    generic: bool
    instance: str


IMPL = re.compile(
    r"\bimpl\s*(?P<generic><[^{}]*>)?\s+"
    r"(?P<trait>[\w:]+)\s+for\s+(?P<name>[\w:]+)"
    r"(?:<[^{};]*>)?\s*(?:where\s+[^{}]+)?\{"
)

SURFACE = re.compile(r"\bconst\s+SURFACE\s*:[^=]+=(?:\s*\w+::)*\s*surface_format!\(\s*([\w:]+)\s*\)")


def records(view: gate.TreeView) -> tuple[Record, ...]:
    if hasattr(view, "_durable_records"):
        return view._durable_records
    found = []
    for path in view.matching_paths(("crates/**/*.rs", "examples/**/*.rs", "runbooks/**/*.rs")):
        if gate._is_test_source(path):
            continue
        text = view.content(path) or ""
        aliases = {name: segments[-1] for name, segments, _ in gate.index_file(text).uses
                   if segments and segments[-1] in {"DurableRecord", "JournalStep"}}
        for match in IMPL.finditer(text):
            trait = match["trait"].split("::")[-1]
            trait = aliases.get(trait, trait)
            if trait not in {"DurableRecord", "JournalStep"}:
                continue
            if gate._gated_on_test(text, match.start()) or any(
                start <= match.start() < end for start, end in gate.test_only_module_ranges(text, gate.rust_outer_attribute_ranges(text))
            ):
                continue
            body = text[match.end():gate.rust_item_end(text, match.end() - 1)]
            surface = SURFACE.search(body)
            output = re.search(r"\btype\s+Output\s*=\s*([^;]+);", body)
            kind = re.search(r'\bconst\s+KIND\s*:[^=]+=\s*"([^"\n]+)"', body)
            found.append(Record(path, match.group("name"),
                                surface[1].split("::")[-1] if surface else None,
                                output[1].strip() if output else match.group("name"),
                                kind[1] if kind else None, match.start(),
                                trait == "JournalStep", bool(match["generic"]),
                                gate.named_rust_items(body, ("instance",)).get("instance", "")))
    view._durable_records = tuple(found)
    return view._durable_records


def problems(view: gate.TreeView, surfaces: set[str]) -> list[str]:
    failures = []
    kinds = {}
    for record in records(view):
        where = f"{record.path}:{record.name}"
        if record.surface is None:
            failures.append(f"{where} must declare SURFACE with surface_format!(CONSTANT)")
        elif record.surface not in surfaces:
            failures.append(f"{where} names surface {record.surface} outside every registered surface")
        if record.is_step and record.generic:
            failures.append(f"{where} JournalStep must have one concrete Output, not a generic implementation")
        if record.is_step and record.kind is None:
            failures.append(f"{where} JournalStep must declare a literal KIND")
        if record.kind is not None:
            if not record.kind or ":" in record.kind:
                failures.append(f"{where} KIND must be nonempty and contain no instance delimiter ':'")
            if record.kind in kinds:
                failures.append(f"{where} duplicates JournalStep KIND {record.kind!r}: {kinds[record.kind]}")
            kinds[record.kind] = where
    return failures


def guards(view: gate.TreeView, constant: str) -> tuple[gate.Guard, ...]:
    owned = [r for r in records(view) if r.surface == constant]
    return tuple(gate.Guard("records", (r.path,), (r.output,)) for r in owned) + tuple(
        gate.Guard("step", (r.path,), (r.name,)) for r in owned if r.kind is not None)


def closure(view: gate.TreeView, guard: gate.Guard):
    reach = gate.reachability(view)
    roots = []
    problems = []
    opaque = []
    for path in guard.paths:
        owners = [r for r in records(view) if r.path == path and r.output in guard.symbols]
        index = gate.index_file(view.content(path) or "")
        modules = tuple(name for name, start, end, _ in index.modules
                        if owners and start < owners[0].offset < end)
        origin = gate.Shape(path, "durable declaration", 0, "struct", modules)
        for expression in guard.symbols:
            for ref in gate.type_refs(gate.rust_tokens(expression)):
                try:
                    roots.extend(reach.resolve(ref, origin, opaque))
                except gate._Unresolved as error:
                    problems.append(str(error))
    result = reach.closure(roots)
    return result, problems


def step_signature(view: gate.TreeView, guard: gate.Guard):
    return tuple((r.path, r.name, f"{r.kind}:{r.output}:{gate.strip_rust_trivia(r.instance)}") for r in records(view)
                 if r.path in guard.paths and r.name in guard.symbols)

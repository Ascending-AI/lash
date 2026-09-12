#!/usr/bin/env python3
"""Validate and execute the workspace's declared Cargo feature coverage plan."""

from __future__ import annotations

import argparse
import itertools
import json
import os
import re
import signal
import subprocess
import sys
import time
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
PROCESS_GROUP_GRACE_SECONDS = 0.5
RUST_MASK_START = re.compile(
    r'//|/\*|(?<![A-Za-z0-9_])(?:br|cr|r)(?P<hash>#{0,255})"|(?:b|c)?"|b?\''
)
RUST_STRING_LITERAL = re.compile(r'(?:b|c)?"(?:\\.|[^"\\])*"', re.DOTALL)
RUST_CHAR_LITERAL = re.compile(
    r"b?'(?:\\(?:x[0-9A-Fa-f]{2}|u\{[0-9A-Fa-f_]+\}|.)|[^'\\\n])'",
)
NON_NEWLINE = re.compile(r"[^\n]")


@dataclass(frozen=True)
class Package:
    name: str
    path: Path
    features: dict[str, list[str]]
    dependencies: dict[str, str]
    dev_self_features: frozenset[str]


@dataclass(frozen=True)
class CfgExpr:
    kind: str
    name: str = ""
    value: str = ""
    children: tuple[CfgExpr, ...] = ()


@dataclass(frozen=True)
class CfgPredicateRequirement:
    source: Path
    line: int
    text: str
    expr: CfgExpr
    context: str
    truths: frozenset[bool]
    features: frozenset[str]


@dataclass(frozen=True)
class RustAttribute:
    kind: str
    start: int
    end: int
    body: str
    inner: bool


@dataclass(frozen=True)
class PackageCfgRequirements:
    feature_states: dict[str, dict[str, set[str]]]
    predicates: tuple[CfgPredicateRequirement, ...]


@dataclass(frozen=True)
class CargoArtifact:
    manifest_path: Path
    target_name: str
    target_kinds: tuple[str, ...]
    test: bool
    debug_assertions: bool
    features: frozenset[str]
    sources: frozenset[Path]


@dataclass(frozen=True)
class CommandArtifacts:
    command: tuple[str, ...]
    artifacts: tuple[CargoArtifact, ...]


CFG_TOKEN = re.compile(
    r'\s*(?:(?P<ident>[A-Za-z_][A-Za-z0-9_-]*)|'
    r'(?P<string>"(?:\\.|[^"\\])*")|(?P<punct>[(),=]))'
)
SUPPORTED_CFG_FLAGS = {"test", "unix", "windows", "debug_assertions"}
SUPPORTED_CFG_VALUES = {
    "target_arch",
    "target_endian",
    "target_env",
    "target_family",
    "target_has_atomic",
    "target_os",
    "target_pointer_width",
    "target_vendor",
}
SUPPORTED_CFG_ATTRS = {"allow", "derive", "expect", "no_std"}


def load_toml(path: Path) -> dict[str, Any]:
    with path.open("rb") as handle:
        return tomllib.load(handle)


def dependency_sections(manifest: dict[str, Any]) -> list[tuple[str, dict[str, Any]]]:
    sections = [
        (name, manifest.get(name, {}))
        for name in ("dependencies", "dev-dependencies", "build-dependencies")
    ]
    for target in manifest.get("target", {}).values():
        if not isinstance(target, dict):
            continue
        sections.extend(
            (name, target.get(name, {}))
            for name in ("dependencies", "dev-dependencies", "build-dependencies")
        )
    return sections


def workspace_packages(root: Path) -> dict[str, Package]:
    workspace = load_toml(root / "Cargo.toml")
    workspace_dependencies = workspace["workspace"].get("dependencies", {})
    packages: dict[str, Package] = {}

    for member in workspace["workspace"]["members"]:
        path = root / member
        manifest = load_toml(path / "Cargo.toml")
        package_name = manifest["package"]["name"]
        features = {
            name: list(definition)
            for name, definition in manifest.get("features", {}).items()
        }
        dependencies: dict[str, str] = {}
        dev_self_features: set[str] = set()

        for section_name, section in dependency_sections(manifest):
            for alias, raw_spec in section.items():
                spec = raw_spec if isinstance(raw_spec, dict) else {}
                inherited = workspace_dependencies.get(alias, {}) if spec.get("workspace") else {}
                inherited = inherited if isinstance(inherited, dict) else {}
                dependency_name = spec.get("package", inherited.get("package", alias))
                if section_name == "dependencies":
                    dependencies[alias] = dependency_name
                elif section_name == "dev-dependencies" and dependency_name == package_name:
                    dev_self_features.update(spec.get("features", inherited.get("features", [])))

                if spec.get("optional", inherited.get("optional", False)):
                    explicit = any(
                        f"dep:{alias}" in definition
                        for definition in features.values()
                    )
                    if not explicit:
                        features.setdefault(alias, [f"dep:{alias}"])

        packages[package_name] = Package(
            name=package_name,
            path=path,
            features=features,
            dependencies=dependencies,
            dev_self_features=frozenset(dev_self_features),
        )

    return packages


def masked_rust(source: str) -> str:
    """Mask comments and literals while preserving offsets for brace matching."""
    ranges: list[tuple[int, int]] = []
    search_from = 0
    while match := RUST_MASK_START.search(source, search_from):
        start = match.start()
        token = match.group()
        if token == "//":
            newline = source.find("\n", match.end())
            end = len(source) if newline == -1 else newline
        elif token == "/*":
            depth = 1
            end = match.end()
            while depth:
                opening = source.find("/*", end)
                closing = source.find("*/", end)
                if closing == -1:
                    end = len(source)
                    break
                if opening != -1 and opening < closing:
                    depth += 1
                    end = opening + 2
                else:
                    depth -= 1
                    end = closing + 2
        elif match.group("hash") is not None:
            delimiter = '"' + match.group("hash")
            closing = source.find(delimiter, match.end())
            end = len(source) if closing == -1 else closing + len(delimiter)
        elif token.endswith('"'):
            literal = RUST_STRING_LITERAL.match(source, start)
            end = len(source) if literal is None else literal.end()
        else:
            literal = RUST_CHAR_LITERAL.match(source, start)
            if literal is None:
                search_from = match.end()
                continue
            end = literal.end()
        ranges.append((start, end))
        search_from = end

    pieces: list[str] = []
    emitted = 0
    for start, end in ranges:
        pieces.append(source[emitted:start])
        pieces.append(NON_NEWLINE.sub(" ", source[start:end]))
        emitted = end
    pieces.append(source[emitted:])
    return "".join(pieces)


def balanced_end(source: str, start: int, opening: str, closing: str) -> int | None:
    depth = 0
    for index in range(start, len(source)):
        if source[index] == opening:
            depth += 1
        elif source[index] == closing:
            depth -= 1
            if depth == 0:
                return index
    return None


class CfgParser:
    def __init__(self, source: str) -> None:
        self.tokens: list[str] = []
        position = 0
        while position < len(source):
            match = CFG_TOKEN.match(source, position)
            if match is None:
                if source[position:].strip() == "":
                    break
                raise ValueError(f"unsupported cfg syntax near {source[position:]!r}")
            self.tokens.append(next(value for value in match.groups() if value is not None))
            position = match.end()
        self.position = 0

    def parse(self) -> CfgExpr:
        expression = self.parse_expression()
        if self.position != len(self.tokens):
            raise ValueError(f"unexpected cfg token {self.tokens[self.position]!r}")
        return expression

    def parse_expression(self) -> CfgExpr:
        name = self.take()
        if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_-]*", name):
            raise ValueError(f"expected cfg name, found {name!r}")
        if self.peek() == "(":
            if name not in {"all", "any", "not"}:
                raise ValueError(f"unsupported cfg predicate {name!r}")
            self.take("(")
            children: list[CfgExpr] = []
            if self.peek() != ")":
                while True:
                    children.append(self.parse_expression())
                    if self.peek() != ",":
                        break
                    self.take(",")
            self.take(")")
            if name == "not" and len(children) != 1:
                raise ValueError("cfg not() requires exactly one predicate")
            return CfgExpr(kind=name, children=tuple(children))
        if self.peek() == "=":
            self.take("=")
            raw_value = self.take()
            if not raw_value.startswith('"'):
                raise ValueError(f"cfg value for {name!r} must be a string")
            value = json.loads(raw_value)
            if name == "feature":
                if re.fullmatch(r"[A-Za-z0-9_.-]+", value) is None:
                    raise ValueError(f"invalid cfg feature name {value!r}")
            elif name not in SUPPORTED_CFG_VALUES:
                raise ValueError(f"unsupported valued cfg predicate {name!r}")
            return CfgExpr(kind="value", name=name, value=value)
        if name not in SUPPORTED_CFG_FLAGS:
            raise ValueError(f"unsupported flag cfg predicate {name!r}")
        return CfgExpr(kind="flag", name=name)

    def peek(self) -> str | None:
        return self.tokens[self.position] if self.position < len(self.tokens) else None

    def take(self, expected: str | None = None) -> str:
        current = self.peek()
        if current is None:
            raise ValueError("unexpected end of cfg predicate")
        if expected is not None and current != expected:
            raise ValueError(f"expected cfg token {expected!r}, found {current!r}")
        self.position += 1
        return current


def cfg_features(expression: CfgExpr) -> frozenset[str]:
    if expression.kind == "value" and expression.name == "feature":
        return frozenset((expression.value,))
    return frozenset(
        feature
        for child in expression.children
        for feature in cfg_features(child)
    )


def cfg_atoms(expression: CfgExpr) -> frozenset[tuple[str, str]]:
    if expression.kind == "flag":
        return frozenset((("flag", expression.name),))
    if expression.kind == "value" and expression.name != "feature":
        return frozenset(((expression.name, expression.value),))
    return frozenset(atom for child in expression.children for atom in cfg_atoms(child))


def evaluate_cfg(
    expression: CfgExpr,
    *,
    enabled_features: frozenset[str],
    flags: frozenset[str],
    values: frozenset[tuple[str, str]],
) -> bool:
    if expression.kind == "flag":
        return expression.name in flags
    if expression.kind == "value":
        if expression.name == "feature":
            return expression.value in enabled_features
        return (expression.name, expression.value) in values
    results = [
        evaluate_cfg(child, enabled_features=enabled_features, flags=flags, values=values)
        for child in expression.children
    ]
    if expression.kind == "all":
        return all(results)
    if expression.kind == "any":
        return any(results)
    if expression.kind == "not":
        return not results[0]
    raise AssertionError(f"unknown cfg expression kind: {expression.kind}")


def feature_sensitive_contexts(expression: CfgExpr) -> tuple[str, ...]:
    features = sorted(cfg_features(expression))
    atoms = sorted(atom for atom in cfg_atoms(expression) if atom != ("flag", "test"))
    variables = [("feature", feature) for feature in features] + atoms
    if len(variables) > 10:
        raise ValueError("feature-bearing cfg predicate exceeds the 10-variable grammar limit")
    if "test" not in {value for kind, value in cfg_atoms(expression) if kind == "flag"}:
        return ("any",)
    contexts: list[str] = []
    for context, test_enabled in (("normal", False), ("test", True)):
        sensitive = False
        for bits in itertools.product((False, True), repeat=len(variables)):
            assignments = dict(zip(variables, bits, strict=True))
            enabled = frozenset(
                feature
                for feature in features
                if assignments[("feature", feature)]
            )
            flags = {
                value
                for (kind, value), active in assignments.items()
                if kind == "flag" and active
            }
            if test_enabled:
                flags.add("test")
            values = frozenset(
                atom
                for atom, active in assignments.items()
                if atom[0] != "feature" and atom[0] != "flag" and active
            )
            original = evaluate_cfg(
                expression,
                enabled_features=enabled,
                flags=frozenset(flags),
                values=values,
            )
            for feature in features:
                toggled = enabled ^ frozenset((feature,))
                if original != evaluate_cfg(
                    expression,
                    enabled_features=toggled,
                    flags=frozenset(flags),
                    values=values,
                ):
                    sensitive = True
                    break
            if sensitive:
                break
        if sensitive:
            contexts.append(context)
    if not contexts:
        raise ValueError("cfg predicate mentions features but no feature can affect its result")
    return tuple(contexts)


def true_feature_states(
    expression: CfgExpr, feature: str, context: str
) -> set[str]:
    features = sorted(cfg_features(expression))
    atoms = sorted(atom for atom in cfg_atoms(expression) if atom != ("flag", "test"))
    variables = [("feature", current) for current in features if current != feature] + atoms
    states: set[str] = set()
    test_values = (False, True) if context == "any" else (context == "test",)
    for enabled_state in (False, True):
        for test_enabled in test_values:
            for bits in itertools.product((False, True), repeat=len(variables)):
                assignments = dict(zip(variables, bits, strict=True))
                enabled = {
                    current
                    for current in features
                    if current != feature and assignments[("feature", current)]
                }
                if enabled_state:
                    enabled.add(feature)
                flags = {
                    value
                    for (kind, value), active in assignments.items()
                    if kind == "flag" and active
                }
                if test_enabled:
                    flags.add("test")
                values = frozenset(
                    atom
                    for atom, active in assignments.items()
                    if atom[0] not in {"feature", "flag"} and active
                )
                if evaluate_cfg(
                    expression,
                    enabled_features=frozenset(enabled),
                    flags=frozenset(flags),
                    values=values,
                ):
                    states.add("on" if enabled_state else "off")
                    break
            if ("on" if enabled_state else "off") in states:
                break
    return states


def top_level_commas(source: str) -> tuple[int, ...]:
    masked = masked_rust(source)
    closing = {")": "(", "]": "[", "}": "{"}
    delimiters: list[str] = []
    commas: list[int] = []
    for index, character in enumerate(masked):
        if character in "([{":
            delimiters.append(character)
        elif character in closing:
            if delimiters and delimiters[-1] == closing[character]:
                delimiters.pop()
        elif character == "," and not delimiters:
            commas.append(index)
    return tuple(commas)


def split_cfg_attr(body: str) -> tuple[str, str]:
    commas = top_level_commas(body)
    if not commas:
        raise ValueError("cfg_attr requires a predicate and an attribute")
    index = commas[0]
    return body[:index], body[index + 1 :]


def single_cfg_attr_action(applied: str) -> str:
    commas = top_level_commas(applied)
    parts = []
    start = 0
    for index in commas:
        parts.append(applied[start:index])
        start = index + 1
    parts.append(applied[start:])
    if len(parts) > 1 and not masked_rust(parts[-1]).strip():
        parts.pop()
    if len(parts) != 1 or not masked_rust(parts[0]).strip():
        raise ValueError(
            "cfg_attr must contain exactly one action "
            "(an optional trailing comma is allowed)"
        )
    return parts[0]


def rust_attributes(source: Path, text: str, masked: str) -> tuple[RustAttribute, ...]:
    attributes: list[RustAttribute] = []
    for match in re.finditer(r"#\s*(?P<inner>!)?\s*\[", masked):
        bracket = masked.find("[", match.start())
        end = balanced_end(masked, bracket, "[", "]")
        if end is None:
            raise ValueError(f"{source.name}: unbalanced Rust attribute")
        contents = masked[bracket + 1 : end]
        cfg_match = re.match(r"\s*(cfg|cfg_attr)\s*\(", contents)
        if cfg_match is None:
            attributes.append(
                RustAttribute("", match.start(), end + 1, "", match.group("inner") is not None)
            )
            continue
        paren = bracket + 1 + cfg_match.end() - 1
        paren_end = balanced_end(masked, paren, "(", ")")
        if paren_end is None or masked[paren_end + 1 : end].strip():
            line = text.count("\n", 0, match.start()) + 1
            raise ValueError(f"{source.name}:{line}: malformed {cfg_match.group(1)} attribute")
        attributes.append(
            RustAttribute(
                cfg_match.group(1),
                match.start(),
                end + 1,
                text[paren + 1 : paren_end],
                match.group("inner") is not None,
            )
        )
    return tuple(attributes)


def attribute_stacks(
    attributes: tuple[RustAttribute, ...], masked: str
) -> tuple[tuple[int, ...], ...]:
    stacks: list[list[int]] = []
    for index, attribute in enumerate(attributes):
        if (
            stacks
            and attributes[stacks[-1][-1]].inner == attribute.inner
            and not masked[attributes[stacks[-1][-1]].end : attribute.start].strip()
        ):
            stacks[-1].append(index)
        else:
            stacks.append([index])
    return tuple(tuple(stack) for stack in stacks)


def attached_brace_region(masked: str, start: int) -> tuple[int, int] | None:
    """Return the bounded body attached to an outer attribute stack, if any."""
    parentheses = 0
    brackets = 0
    for index in range(start, len(masked)):
        character = masked[index]
        if character == "(":
            parentheses += 1
        elif character == ")":
            parentheses -= 1
        elif character == "[":
            brackets += 1
        elif character == "]":
            brackets -= 1
        elif parentheses == 0 and brackets == 0:
            if character == "{":
                close = balanced_end(masked, index, "{", "}")
                return (index, close) if close is not None else None
            if character in ";,}":
                return None
    return None


def brace_regions(masked: str) -> tuple[tuple[int, int], ...]:
    openings: list[int] = []
    regions: list[tuple[int, int]] = []
    for index, character in enumerate(masked):
        if character == "{":
            openings.append(index)
        elif character == "}" and openings:
            regions.append((openings.pop(), index))
    return tuple(regions)


def normalized_cfg(body: str) -> str:
    return re.sub(r"\s+", " ", body).strip()


def combined_cfg(expressions: list[CfgExpr]) -> CfgExpr:
    if len(expressions) == 1:
        return expressions[0]
    return CfgExpr(kind="all", children=tuple(expressions))


def combined_cfg_text(predicates: list[str]) -> str:
    if len(predicates) == 1:
        return normalized_cfg(predicates[0])
    return f"all({', '.join(normalized_cfg(predicate) for predicate in predicates)})"


def parsed_cfg_attribute(
    package: Package,
    source: Path,
    text: str,
    attribute: RustAttribute,
    *,
    composition: bool,
) -> tuple[CfgExpr, str]:
    predicate = attribute.body
    if attribute.kind == "cfg_attr":
        try:
            predicate, applied = split_cfg_attr(attribute.body)
            applied = single_cfg_attr_action(applied)
        except ValueError as error:
            line = text.count("\n", 0, attribute.start) + 1
            raise ValueError(
                f"{source.relative_to(package.path)}:{line}: {error}"
            ) from error
        if re.match(r"\s*cfg(?:_attr)?\s*\(", applied):
            line = text.count("\n", 0, attribute.start) + 1
            if "feature" in attribute.body:
                raise ValueError(
                    f"{source.relative_to(package.path)}:{line}: "
                    "nested feature-bearing cfg_attr is unsupported"
                )
            raise ValueError(
                f"{source.relative_to(package.path)}:{line}: "
                "conditional cfg_attr in a feature-gated composition is unsupported"
            )
        applied_match = re.match(
            r"\s*([A-Za-z_][A-Za-z0-9_]*)", masked_rust(applied)
        )
        applied_name = applied_match.group(1) if applied_match is not None else ""
        if applied_name not in SUPPORTED_CFG_ATTRS:
            line = text.count("\n", 0, attribute.start) + 1
            scope = "feature-bearing" if "feature" in attribute.body else "composed"
            raise ValueError(
                f"{source.relative_to(package.path)}:{line}: "
                f"unsupported {scope} cfg_attr action {applied_name!r}"
            )
    try:
        return CfgParser(predicate).parse(), predicate
    except (ValueError, json.JSONDecodeError) as error:
        line = text.count("\n", 0, attribute.start) + 1
        scope = " in feature-gated composition" if composition else ""
        raise ValueError(
            f"{source.relative_to(package.path)}:{line}: "
            f"unsupported {attribute.kind}{scope}: {error}"
        ) from error


def cfg_requirements(package: Package) -> PackageCfgRequirements:
    found: dict[str, dict[str, set[str]]] = {}
    predicate_requirements: list[CfgPredicateRequirement] = []
    for source in package.path.rglob("*.rs"):
        text = source.read_text(encoding="utf-8")
        masked = masked_rust(text)
        attributes = rust_attributes(source, text, masked)
        stacks = attribute_stacks(attributes, masked)
        stack_by_attribute = {
            attribute_index: stack_index
            for stack_index, stack in enumerate(stacks)
            for attribute_index in stack
        }
        regions = {
            stack_index: region
            for stack_index, stack in enumerate(stacks)
            if not attributes[stack[0]].inner
            and any(attributes[index].kind in {"cfg", "cfg_attr"} for index in stack)
            and (region := attached_brace_region(masked, attributes[stack[-1]].end))
            is not None
        }
        inner_stack_indices = [
            stack_index
            for stack_index, stack in enumerate(stacks)
            if attributes[stack[0]].inner
            and any(attributes[index].kind in {"cfg", "cfg_attr"} for index in stack)
        ]
        braces = brace_regions(masked) if inner_stack_indices else ()
        inner_scopes = {
            stack_index: min(
                (
                    region
                    for region in braces
                    if region[0] < attributes[stacks[stack_index][0]].start < region[1]
                ),
                key=lambda region: region[1] - region[0],
                default=(-1, len(masked)),
            )
            for stack_index in inner_stack_indices
        }

        for attribute_index, attribute in enumerate(attributes):
            if attribute.kind not in {"cfg", "cfg_attr"} or "feature" not in attribute.body:
                continue
            current_stack = stack_by_attribute[attribute_index]
            enclosing_stacks = sorted(
                {
                    stack_index
                    for stack_index, (region_start, region_end) in regions.items()
                    if region_start < attribute.start < region_end
                }
                | {
                    stack_index
                    for stack_index, (region_start, region_end) in inner_scopes.items()
                    if stack_index != current_stack
                    and attributes[stacks[stack_index][-1]].end <= attribute.start
                    and region_start < attribute.start < region_end
                },
                key=lambda stack_index: (
                    regions.get(stack_index, inner_scopes.get(stack_index, (-1, len(masked))))[0],
                    attributes[stacks[stack_index][0]].start,
                ),
            )
            controlling_indices = [
                index
                for stack_index in (*enclosing_stacks, current_stack)
                for index in stacks[stack_index]
                if attributes[index].kind == "cfg"
            ]
            relevant_stacks = set(enclosing_stacks) | {current_stack}
            for stack_index in relevant_stacks:
                for index in stacks[stack_index]:
                    candidate = attributes[index]
                    if candidate.kind == "cfg_attr" and index != attribute_index:
                        parsed_cfg_attribute(
                            package,
                            source,
                            text,
                            candidate,
                            composition=True,
                        )

            controlling: list[CfgExpr] = []
            controlling_text: list[str] = []
            for index in controlling_indices:
                expression, predicate = parsed_cfg_attribute(
                    package,
                    source,
                    text,
                    attributes[index],
                    composition=index != attribute_index,
                )
                controlling.append(expression)
                controlling_text.append(predicate)

            expression, predicate = parsed_cfg_attribute(
                package,
                source,
                text,
                attribute,
                composition=False,
            )
            if attribute.kind == "cfg":
                effective = combined_cfg(controlling)
                requirements = [
                    (
                        effective,
                        combined_cfg_text(controlling_text),
                        None,
                    )
                ]
            else:
                requirements = [
                    (
                        combined_cfg([*controlling, expression]),
                        combined_cfg_text([*controlling_text, predicate]),
                        frozenset((True,)),
                    ),
                    (
                        combined_cfg(
                            [
                                *controlling,
                                CfgExpr(kind="not", children=(expression,)),
                            ]
                        ),
                        combined_cfg_text([*controlling_text, f"not({normalized_cfg(predicate)})"]),
                        frozenset((True,)),
                    ),
                ]

            own_features = cfg_features(expression)
            if not own_features:
                raise ValueError(
                    f"{source.relative_to(package.path)}:"
                    f"{text.count(chr(10), 0, attribute.start) + 1}: "
                    f"feature-bearing {attribute.kind} contains no valid feature predicate"
                )
            for effective, effective_text, explicit_truths in requirements:
                features = cfg_features(effective)
                contexts = feature_sensitive_contexts(effective)
                for context in contexts:
                    truths = (
                        explicit_truths
                        if explicit_truths is not None
                        else frozenset((True,))
                        if context == "test"
                        else frozenset((False, True))
                    )
                    predicate_requirements.append(
                        CfgPredicateRequirement(
                            source=source.resolve(),
                            line=text.count("\n", 0, attribute.start) + 1,
                            text=effective_text,
                            expr=effective,
                            context=context,
                            truths=truths,
                            features=features,
                        )
                    )
                    for name in features:
                        feature_requirements = found.setdefault(
                            name, {"normal": set(), "test": set()}
                        )
                        legacy_context = "normal" if context == "any" else context
                        if legacy_context == "normal":
                            feature_requirements[legacy_context].update(("on", "off"))
                        else:
                            feature_requirements[legacy_context].update(
                                true_feature_states(effective, name, context)
                            )
    return PackageCfgRequirements(found, tuple(predicate_requirements))


def split_coverage(token: str) -> tuple[str, str]:
    feature_id, separator, state = token.rpartition(":")
    if separator != ":" or state not in {"on", "off"} or "/" not in feature_id:
        raise ValueError(f"invalid feature coverage token: {token!r}")
    return feature_id, state


def feature_id(package: str, feature: str) -> str:
    return f"{package}/{feature}"


def command_feature_set(command: list[str]) -> set[str]:
    if "--features" not in command:
        return set()
    index = command.index("--features")
    if index + 1 >= len(command):
        return set()
    return {item for item in command[index + 1].split(",") if item}


def command_package(command: list[str]) -> str | None:
    if "-p" not in command:
        return None
    index = command.index("-p")
    return command[index + 1] if index + 1 < len(command) else None


def cargo_subcommand(command: list[str]) -> str | None:
    if len(command) < 2 or command[0] != "cargo":
        return None
    return command[1]


def command_compiles_tests(command: list[str]) -> bool:
    return (cargo_subcommand(command) == "test" and "--doc" not in command) or any(
        target in command
        for target in ("--all-targets", "--tests", "--test", "--benches", "--bench")
    )


def command_compiles_normal_library(command: list[str]) -> bool:
    return cargo_subcommand(command) == "check" and any(
        target in command for target in ("--lib", "--all-targets")
    )


def effective_command_features(command: list[str], package: Package) -> set[str]:
    enabled = command_feature_set(command)
    if "--all-features" in command:
        enabled.update(package.features)
    if command_compiles_tests(command):
        enabled.update(package.dev_self_features)
    return enabled


def workflow_job_block(workflow: str, job: str) -> str:
    marker = f"  {job}:\n"
    start = workflow.find(marker)
    if start == -1:
        return ""
    next_job = re.search(
        r"^  [A-Za-z0-9_-]+:\n", workflow[start + len(marker) :], re.MULTILINE
    )
    if next_job is None:
        return workflow[start:]
    return workflow[start : start + len(marker) + next_job.start()]


def validate(root: Path) -> tuple[dict[str, Package], dict[str, Any]]:
    packages = workspace_packages(root)
    plan = load_toml(root / "scripts" / "feature-coverage.toml")
    failures: list[str] = []

    if plan.get("schema") != 1:
        failures.append("scripts/feature-coverage.toml must declare schema = 1")

    declared = {
        feature_id(package.name, feature)
        for package in packages.values()
        for feature in package.features
    }
    package_cfg: dict[str, PackageCfgRequirements] = {}
    for package in packages.values():
        try:
            package_cfg[package.name] = cfg_requirements(package)
        except ValueError as error:
            failures.append(f"{package.name}: {error}")
            package_cfg[package.name] = PackageCfgRequirements({}, ())
    package_requirements = {
        name: scan.feature_states for name, scan in package_cfg.items()
    }
    non_normal_forwards = {
        f"{feature_id(package.name, name)}->{edge}"
        for package in packages.values()
        for name, definition in package.features.items()
        for edge in definition
        if "/" in edge
        and edge.split("/", 1)[0].removesuffix("?") not in package.dependencies
    }
    planned_non_normal = set(plan.get("non_normal_forwards", []))
    for edge in sorted(non_normal_forwards - planned_non_normal):
        failures.append(f"unmapped non-normal feature forward: {edge}")
    for edge in sorted(planned_non_normal - non_normal_forwards):
        failures.append(f"stale non-normal feature forward: {edge}")
    baseline = set(plan.get("baseline", {}).get("features", []))
    baseline_command = plan.get("baseline", {}).get("command")
    if baseline_command != ["cargo", "check", "--workspace", "--all-targets", "--locked"]:
        failures.append("baseline command must be the locked workspace all-targets check")
    for current_feature in sorted(baseline):
        if not current_feature.endswith("/default"):
            failures.append(f"baseline may own only explicit default features: {current_feature}")
    unresolved_rows = plan.get("unresolved", [])
    unresolved = {row.get("feature") for row in unresolved_rows}
    if None in unresolved:
        failures.append("every [[unresolved]] row needs a feature")
        unresolved.discard(None)

    lanes = plan.get("lane", [])
    lane_names = [lane.get("name") for lane in lanes]
    if None in lane_names or len(set(lane_names)) != len(lane_names):
        failures.append("every [[lane]] needs a unique name")

    on_owners: dict[str, list[str]] = {}
    off_owners: dict[str, list[str]] = {}
    for lane in lanes:
        name = lane.get("name", "<unnamed>")
        commands = lane.get("commands", [])
        if not commands:
            failures.append(f"lane {name!r} has no executable commands")
        for command in commands:
            if not isinstance(command, list) or not all(isinstance(arg, str) for arg in command):
                failures.append(f"lane {name!r} has a non-argv command")
                continue
            subcommand = cargo_subcommand(command)
            if subcommand not in {"check", "test"}:
                failures.append(f"lane {name!r} has a non-Cargo check/test command")
            if "--locked" not in command:
                failures.append(f"lane {name!r} has an unlocked Cargo command")
            if command_package(command) not in packages:
                failures.append(f"lane {name!r} command does not name one workspace package")
            if "--all-features" in command:
                failures.append(f"lane {name!r} command may not use --all-features")
            if any(
                argument == "--message-format" or argument.startswith("--message-format=")
                for argument in command
            ):
                failures.append(f"lane {name!r} command may not set --message-format")
            if subcommand == "check" and not any(
                target in command for target in ("--lib", "--all-targets", "--tests")
            ):
                failures.append(f"lane {name!r} check command lacks an explicit target context")
        for token in lane.get("features", []):
            try:
                current_feature, state = split_coverage(token)
            except ValueError as error:
                failures.append(str(error))
                continue
            owners = on_owners if state == "on" else off_owners
            owners.setdefault(current_feature, []).append(name)

    for current_feature in sorted(declared):
        package_name, name = current_feature.split("/", 1)
        owners = int(current_feature in baseline) + len(on_owners.get(current_feature, []))
        if current_feature in unresolved:
            if owners:
                failures.append(f"unresolved feature {current_feature} must not claim green ownership")
            continue
        if owners == 0:
            failures.append(f"unowned declared feature: {current_feature}")
        elif owners > 1:
            failures.append(f"multiply owned declared feature: {current_feature}")

        requirements = package_requirements[package_name].get(name, {})
        if name != "default" and requirements:
            if not off_owners.get(current_feature):
                failures.append(f"cfg feature lacks an OFF witness: {current_feature}")

    for current_feature in sorted((baseline | set(on_owners) | set(off_owners) | unresolved) - declared):
        failures.append(f"coverage plan names undeclared feature: {current_feature}")

    for package_name, scan in package_cfg.items():
        declared_names = packages[package_name].features
        for requirement in scan.predicates:
            for name in sorted(requirement.features - declared_names.keys()):
                failures.append(
                    f"cfg predicate references undeclared feature: {package_name}/{name}"
                )
            owners = {
                owner
                for name in requirement.features
                for owner in (
                    ["<baseline>"]
                    if feature_id(package_name, name) in baseline
                    else on_owners.get(feature_id(package_name, name), [])
                )
            }
            if len(owners) > 1:
                failures.append(
                    f"cfg predicate spans feature coverage owners at "
                    f"{requirement.source.relative_to(root)}:{requirement.line}: "
                    + ", ".join(sorted(owners))
                )

    for current_feature, owners in sorted(off_owners.items()):
        if len(owners) > 1:
            failures.append(f"feature has multiple OFF witnesses: {current_feature}")

    for lane in lanes:
        name = lane.get("name", "<unnamed>")
        commands = lane.get("commands", [])
        for token in lane.get("features", []):
            try:
                current_feature, state = split_coverage(token)
            except ValueError:
                continue
            package_name, name_part = current_feature.split("/", 1)
            package = packages[package_name]
            package_commands = [
                command for command in commands if command_package(command) == package_name
            ]
            candidates = package_commands
            if state == "on":
                candidates = [
                    command
                    for command in candidates
                    if command_feature_set(command) == {name_part}
                ]
            else:
                candidates = [
                    command
                    for command in candidates
                    if "--no-default-features" in command
                    and name_part not in effective_command_features(command, package)
                    and "--all-features" not in command
                ]
            if not candidates:
                failures.append(
                    f"lane {name!r} lacks an exact {state.upper()} command for {current_feature}"
                )
                continue
            requirements = package_requirements[package_name].get(name_part, {})
            for context, states in requirements.items():
                if state not in states:
                    continue
                if (
                    context == "test"
                    and state == "off"
                    and name_part in package.dev_self_features
                ):
                    # Cargo unifies self dev-dependency features into every test
                    # target, so this source predicate has no reachable OFF test
                    # graph. The normal-library OFF command remains mandatory.
                    continue
                contextual = [
                    command
                    for command in package_commands
                    if (
                        command_compiles_tests(command)
                        if context == "test"
                        else command_compiles_normal_library(command)
                    )
                    and "--all-features" not in command
                    and (
                        name_part in effective_command_features(command, package)
                        if state == "on"
                        else name_part not in effective_command_features(command, package)
                    )
                ]
                if not contextual:
                    failures.append(
                        f"lane {name!r} lacks a {context}-context {state.upper()} command "
                        f"for {current_feature}"
                    )

    all_commands = {
        tuple(command)
        for lane in lanes
        for command in lane.get("commands", [])
        if isinstance(command, list)
    }
    proxy_witnesses = plan.get("proxy_witness", [])
    planned_proxy_pairs = {
        (witness.get("feature"), witness.get("dependency"))
        for witness in proxy_witnesses
    }
    required_proxy_pairs = {
        (feature_id(package.name, name), edge)
        for package in packages.values()
        for name, definition in package.features.items()
        if package_requirements[package.name].get(name, {}).get("test")
        and not package_requirements[package.name].get(name, {}).get("normal")
        for edge in definition
        if "/" in edge
        and "?/" not in edge
        and edge.split("/", 1)[0] in package.dependencies
    }
    for current_feature, dependency_edge in sorted(required_proxy_pairs - planned_proxy_pairs):
        failures.append(
            f"test-only proxy lacks local-OFF/dependency-ON witness: "
            f"{current_feature}->{dependency_edge}"
        )
    for witness in proxy_witnesses:
        current_feature = witness.get("feature", "")
        dependency_edge = witness.get("dependency", "")
        command = witness.get("command", [])
        if current_feature not in declared:
            failures.append(f"proxy witness names undeclared feature: {current_feature}")
            continue
        package_name, name_part = current_feature.split("/", 1)
        package = packages[package_name]
        if dependency_edge not in package.features[name_part]:
            failures.append(
                f"proxy witness edge is not declared by {current_feature}: {dependency_edge}"
            )
        if tuple(command) not in all_commands:
            failures.append(f"proxy witness command is not executed by a lane: {current_feature}")
        if (
            command_package(command) != package_name
            or "--no-default-features" not in command
            or name_part in effective_command_features(command, package)
            or dependency_edge not in command_feature_set(command)
        ):
            failures.append(
                f"proxy witness does not compile local OFF with dependency ON: {current_feature}"
            )
        test_states = package_requirements[package_name].get(name_part, {}).get("test", set())
        if test_states and not command_compiles_tests(command):
            failures.append(f"proxy witness misses test context: {current_feature}")

    workflow = (root / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
    feature_job = workflow_job_block(workflow, "package-feature-checks")
    workflow_lanes = set(re.findall(r"^\s*- lane: ([A-Za-z0-9_.-]+)\s*$", workflow, re.MULTILINE))
    for name in lane_names:
        if name not in workflow_lanes:
            failures.append(f"coverage lane missing from ci.yml: {name}")
        invocation = f"python3 scripts/check_feature_coverage.py run {name}"
        if invocation not in feature_job:
            failures.append(f"coverage lane has no ci.yml runner command: {name}")

    runner_step = re.compile(
        r"^      - name: Run exact package feature graph\n"
        r"        run: \$\{\{ matrix\.command \}\}\s*$",
        re.MULTILINE,
    )
    if runner_step.search(feature_job) is None:
        failures.append("package-feature-checks does not execute matrix.command")
    if "github.event_name == 'merge_group'" not in feature_job:
        failures.append("package-feature-checks is not required on merge_group")

    conclusion = workflow_job_block(workflow, "ci-conclusion")
    if re.search(r"^      - package-feature-checks\s*$", conclusion, re.MULTILINE) is None:
        failures.append("ci-conclusion does not require package-feature-checks")
    if re.search(r"^      - test-doc\s*$", conclusion, re.MULTILINE) is None:
        failures.append("ci-conclusion does not require the baseline test-doc job")

    test_doc = workflow_job_block(workflow, "test-doc")
    baseline_shell = "cargo check --workspace --all-targets --locked ${LASH_CI_FEATURES}"
    if baseline_shell not in test_doc:
        failures.append("test-doc does not execute the workspace default baseline")

    repo_gates = workflow_job_block(workflow, "repo-gates")
    for invocation in (
        "python3 scripts/test_check_feature_coverage.py",
        "python3 scripts/check_feature_coverage.py check",
    ):
        if invocation not in repo_gates:
            failures.append(f"repo-gates does not execute {invocation}")

    for row in unresolved_rows:
        current_feature = row.get("feature")
        failures.append(f"unapproved unresolved feature: {current_feature}")

    if failures:
        print("feature coverage contract failed:", file=sys.stderr)
        for failure in failures:
            print(f"- {failure}", file=sys.stderr)
        raise SystemExit(1)

    print(
        "feature coverage contract passed: "
        f"{len(declared)} declared features, {len(lanes)} executable lanes, "
        f"{len(unresolved)} explicit unresolved"
    )
    return packages, plan


def process_group_exists(process_group: int) -> bool:
    try:
        os.killpg(process_group, 0)
    except ProcessLookupError:
        return False
    return True


def terminate_process_group(process: subprocess.Popen[str]) -> None:
    process_group = process.pid
    try:
        os.killpg(process_group, signal.SIGTERM)
    except ProcessLookupError:
        process.wait()
        return

    deadline = time.monotonic() + PROCESS_GROUP_GRACE_SECONDS
    while time.monotonic() < deadline and process_group_exists(process_group):
        process.poll()
        time.sleep(0.01)
    if process_group_exists(process_group):
        try:
            os.killpg(process_group, signal.SIGKILL)
        except ProcessLookupError:
            pass
    process.wait()


def run_command(command: list[str], root: Path) -> str:
    print("+ " + " ".join(command), flush=True)
    process: subprocess.Popen[str] | None = None
    previous_sigterm = signal.getsignal(signal.SIGTERM)

    def cancel_on_sigterm(signum: int, _frame: Any) -> None:
        signal.signal(signum, signal.SIG_IGN)
        raise SystemExit(128 + signum)

    signal.signal(signal.SIGTERM, cancel_on_sigterm)
    output: list[str] = []
    try:
        process = subprocess.Popen(
            command,
            cwd=root,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            start_new_session=True,
        )
        assert process.stdout is not None
        for line in process.stdout:
            output.append(line)
            print(line, end="", flush=True)
        return_code = process.wait()
    except BaseException:
        if process is not None:
            terminate_process_group(process)
        raise
    finally:
        signal.signal(signal.SIGTERM, previous_sigterm)
    combined = "".join(output)
    if return_code != 0:
        raise subprocess.CalledProcessError(return_code, command, output=combined)
    return combined


def dep_info_paths(
    root: Path, filenames: list[str], target_source: str
) -> frozenset[Path]:
    sources: set[Path] = set()
    source_path = Path(target_source)
    sources.add(
        (root / source_path).resolve()
        if not source_path.is_absolute()
        else source_path.resolve()
    )
    dep_infos: set[Path] = set()
    for filename in filenames:
        artifact = Path(filename)
        stem = re.sub(r"\.(?:rlib|rmeta|so|dylib|dll|a|exe)$", "", artifact.name)
        if stem.startswith("lib"):
            stem = stem[3:]
        dep_infos.add(artifact.parent / f"{stem}.d")
    for dep_info in dep_infos:
        if not dep_info.is_file():
            continue
        contents = dep_info.read_text(encoding="utf-8")
        for match in re.finditer(r"(?<!\S)([^\s]+\.rs)(?=[:\s])", contents):
            raw_path = match.group(1).replace(r"\ ", " ")
            path = Path(raw_path)
            sources.add((root / path).resolve() if not path.is_absolute() else path.resolve())
    return frozenset(sources)


def parse_cargo_artifacts(output: str, root: Path) -> list[CargoArtifact]:
    artifacts: list[CargoArtifact] = []
    for line in output.splitlines():
        if not line.startswith("{"):
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if message.get("reason") != "compiler-artifact":
            continue
        target = message.get("target", {})
        profile = message.get("profile", {})
        manifest_path = Path(message["manifest_path"])
        if not manifest_path.is_absolute():
            manifest_path = root / manifest_path
        artifacts.append(
            CargoArtifact(
                manifest_path=manifest_path.resolve(),
                target_name=target.get("name", ""),
                target_kinds=tuple(target.get("kind", [])),
                test=bool(profile.get("test")),
                debug_assertions=bool(profile.get("debug_assertions")),
                features=frozenset(message.get("features", [])),
                sources=dep_info_paths(
                    root,
                    message.get("filenames", []),
                    target.get("src_path", ""),
                ),
            )
        )
    return artifacts


def command_with_json_messages(command: list[str]) -> list[str]:
    if any(
        argument == "--message-format" or argument.startswith("--message-format=")
        for argument in command
    ):
        raise SystemExit("feature coverage commands may not set --message-format")
    return [*command[:2], "--message-format=json", *command[2:]]


def package_artifacts(
    execution: CommandArtifacts, package: Package
) -> list[CargoArtifact]:
    manifest = (package.path / "Cargo.toml").resolve()
    return [
        artifact
        for artifact in execution.artifacts
        if artifact.manifest_path == manifest and "custom-build" not in artifact.target_kinds
    ]


def validate_selected_artifacts(
    command: list[str], package: Package, artifacts: list[CargoArtifact]
) -> None:
    if not artifacts:
        raise SystemExit(
            f"Cargo command emitted no compiler artifact for selected package {package.name}: "
            + " ".join(command)
        )
    subcommand = cargo_subcommand(command)
    expected: list[CargoArtifact]
    if "--doc" in command:
        expected = [
            artifact
            for artifact in artifacts
            if "lib" in artifact.target_kinds and not artifact.test
        ]
    elif "--test" in command:
        index = command.index("--test")
        target_name = command[index + 1] if index + 1 < len(command) else ""
        expected = [
            artifact
            for artifact in artifacts
            if "test" in artifact.target_kinds
            and artifact.target_name == target_name
            and artifact.test
        ]
    elif "--tests" in command:
        expected = [artifact for artifact in artifacts if artifact.test]
    elif "--bench" in command:
        index = command.index("--bench")
        target_name = command[index + 1] if index + 1 < len(command) else ""
        expected = [
            artifact
            for artifact in artifacts
            if "bench" in artifact.target_kinds
            and artifact.target_name == target_name
            and artifact.test
        ]
    elif "--benches" in command:
        expected = [
            artifact
            for artifact in artifacts
            if "bench" in artifact.target_kinds and artifact.test
        ]
    elif "--lib" in command:
        expected = [artifact for artifact in artifacts if "lib" in artifact.target_kinds]
        if subcommand == "test":
            expected = [artifact for artifact in expected if artifact.test]
    elif "--all-targets" in command:
        expected = artifacts
    elif subcommand == "test":
        expected = [artifact for artifact in artifacts if artifact.test]
    else:
        expected = artifacts
    if not expected:
        raise SystemExit(
            f"Cargo artifacts do not match the selected target/context for {package.name}: "
            + " ".join(command)
        )


def run_cargo_with_artifacts(
    command: list[str], root: Path, package: Package
) -> CommandArtifacts:
    output = run_command(command_with_json_messages(command), root)
    execution = CommandArtifacts(tuple(command), tuple(parse_cargo_artifacts(output, root)))
    validate_selected_artifacts(command, package, package_artifacts(execution, package))
    return execution


def command_target(command: tuple[str, ...]) -> str | None:
    for index, argument in enumerate(command):
        if argument == "--target" and index + 1 < len(command):
            return command[index + 1]
        if argument.startswith("--target="):
            return argument.split("=", 1)[1]
    return None


def rustc_cfg(
    command: tuple[str, ...],
    root: Path,
    cache: dict[
        str | None, tuple[frozenset[str], frozenset[tuple[str, str]]]
    ],
) -> tuple[frozenset[str], frozenset[tuple[str, str]]]:
    target = command_target(command)
    if target in cache:
        return cache[target]
    rustc_command = ["rustc", "--print", "cfg"]
    if target is not None:
        rustc_command.extend(["--target", target])
    result = subprocess.run(
        rustc_command,
        cwd=root,
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    flags: set[str] = set()
    values: set[tuple[str, str]] = set()
    for line in result.stdout.splitlines():
        if "=" in line:
            name, raw_value = line.split("=", 1)
            values.add((name, json.loads(raw_value)))
        elif line:
            flags.add(line)
    flags.discard("debug_assertions")
    cache[target] = (frozenset(flags), frozenset(values))
    return cache[target]


def artifact_context_matches(
    requirement: CfgPredicateRequirement, artifact: CargoArtifact
) -> bool:
    if requirement.context == "normal":
        return not artifact.test
    if requirement.context == "test":
        return artifact.test
    return True


def validate_lane_artifacts(
    root: Path,
    packages: dict[str, Package],
    lane: dict[str, Any],
    executions: list[CommandArtifacts],
) -> None:
    executions_by_command = {execution.command: execution for execution in executions}
    for token in lane["features"]:
        current_feature, state = split_coverage(token)
        package_name, name = current_feature.split("/", 1)
        package = packages[package_name]
        candidates = [
            command
            for command in lane["commands"]
            if command_package(command) == package_name
        ]
        if state == "on":
            candidates = [
                command for command in candidates if command_feature_set(command) == {name}
            ]
        else:
            candidates = [
                command
                for command in candidates
                if "--no-default-features" in command
                and name not in effective_command_features(command, package)
                and "--all-features" not in command
            ]
        witnessed = any(
            (name in artifact.features) == (state == "on")
            for command in candidates
            for artifact in package_artifacts(executions_by_command[tuple(command)], package)
        )
        if not witnessed:
            raise SystemExit(
                f"compiled artifacts do not witness {state.upper()} for {current_feature}"
            )

    owned_by_package: dict[str, set[str]] = {}
    for token in lane["features"]:
        current_feature, _ = split_coverage(token)
        package_name, name = current_feature.split("/", 1)
        owned_by_package.setdefault(package_name, set()).add(name)

    cfg_cache: dict[
        str | None, tuple[frozenset[str], frozenset[tuple[str, str]]]
    ] = {}
    for package_name, owned_features in owned_by_package.items():
        package = packages[package_name]
        scan = cfg_requirements(package)
        for requirement in scan.predicates:
            if requirement.features.isdisjoint(owned_features):
                continue
            for expected in sorted(requirement.truths):
                witnessed = False
                for execution in executions:
                    if command_package(list(execution.command)) != package_name:
                        continue
                    base_flags, values = rustc_cfg(execution.command, root, cfg_cache)
                    for artifact in package_artifacts(execution, package):
                        if not artifact_context_matches(requirement, artifact):
                            continue
                        if expected and requirement.source not in artifact.sources:
                            continue
                        flags = set(base_flags)
                        if artifact.test:
                            flags.add("test")
                        if artifact.debug_assertions:
                            flags.add("debug_assertions")
                        actual = evaluate_cfg(
                            requirement.expr,
                            enabled_features=artifact.features,
                            flags=frozenset(flags),
                            values=values,
                        )
                        if actual == expected:
                            witnessed = True
                            break
                    if witnessed:
                        break
                if not witnessed:
                    relative_source = requirement.source.relative_to(root)
                    expected_label = "true" if expected else "false"
                    raise SystemExit(
                        "no compiled artifact witnesses cfg predicate "
                        f"{expected_label} at {relative_source}:{requirement.line} "
                        f"in {requirement.context} context: {requirement.text}"
                    )


def resolver_witness(
    root: Path, packages: dict[str, Package], current_feature: str, state: str
) -> None:
    package_name, name = current_feature.split("/", 1)
    command = [
        "cargo",
        "tree",
        "-p",
        package_name,
        "--no-default-features",
    ]
    if state == "on":
        command.extend(["--features", name])
    command.extend(["-e", "normal,features", "-i", package_name, "--locked"])
    output = run_command(command, root)
    marker = f'{package_name} feature "{name}"'
    if state == "on" and marker not in output:
        raise SystemExit(f"resolver did not enable {current_feature}")
    if state == "off" and marker in output:
        raise SystemExit(f"resolver unexpectedly enabled {current_feature}")

    if state != "on":
        return
    definition = packages[package_name].features[name]
    for edge in definition:
        if "?/" in edge:
            continue
        if edge.startswith("dep:"):
            alias = edge.removeprefix("dep:")
            dependency = packages[package_name].dependencies.get(alias)
            if dependency is None:
                print(f"resolver note: {current_feature} has non-normal edge {edge}")
                continue
            graph = run_command(
                [
                    "cargo",
                    "tree",
                    "-p",
                    package_name,
                    "--no-default-features",
                    "--features",
                    name,
                    "-e",
                    "normal,features",
                    "-i",
                    dependency,
                    "--locked",
                ],
                root,
            )
            if dependency not in graph:
                raise SystemExit(
                    f"resolver did not activate optional dependency {dependency} for {current_feature}"
                )
        elif "/" in edge:
            alias, forwarded = edge.split("/", 1)
            dependency = packages[package_name].dependencies.get(alias)
            if dependency is None:
                print(f"resolver note: {current_feature} has non-normal edge {edge}")
                continue
            graph = run_command(
                [
                    "cargo",
                    "tree",
                    "-p",
                    package_name,
                    "--no-default-features",
                    "--features",
                    name,
                    "-e",
                    "normal,features",
                    "-i",
                    dependency,
                    "--locked",
                ],
                root,
            )
            marker = f'{dependency} feature "{forwarded}"'
            if marker not in graph:
                raise SystemExit(
                    f"resolver did not forward {dependency}/{forwarded} for {current_feature}"
                )


def resolver_proxy_witness(
    root: Path, packages: dict[str, Package], witness: dict[str, Any]
) -> None:
    current_feature = witness["feature"]
    package_name, name = current_feature.split("/", 1)
    dependency_edge = witness["dependency"]
    alias, forwarded = dependency_edge.split("/", 1)
    dependency = packages[package_name].dependencies[alias]
    base = [
        "cargo",
        "tree",
        "-p",
        package_name,
        "--no-default-features",
        "--features",
        dependency_edge,
        "-e",
        "normal,features",
    ]
    local_graph = run_command([*base, "-i", package_name, "--locked"], root)
    if f'{package_name} feature "{name}"' in local_graph:
        raise SystemExit(f"proxy witness unexpectedly enabled local feature {current_feature}")
    dependency_graph = run_command([*base, "-i", dependency, "--locked"], root)
    if f'{dependency} feature "{forwarded}"' not in dependency_graph:
        raise SystemExit(
            f"proxy witness did not independently enable {dependency}/{forwarded}"
        )


def run_lane(root: Path, lane_name: str) -> None:
    packages, plan = validate(root)
    lanes = {lane["name"]: lane for lane in plan["lane"]}
    if lane_name not in lanes:
        raise SystemExit(f"unknown feature coverage lane: {lane_name}")
    lane = lanes[lane_name]
    for token in lane["features"]:
        current_feature, state = split_coverage(token)
        resolver_witness(root, packages, current_feature, state)
    owned_features = {split_coverage(token)[0] for token in lane["features"]}
    for witness in plan.get("proxy_witness", []):
        if witness["feature"] in owned_features:
            resolver_proxy_witness(root, packages, witness)
    executions = [
        run_cargo_with_artifacts(
            command,
            root,
            packages[command_package(command)],
        )
        for command in lane["commands"]
    ]
    validate_lane_artifacts(root, packages, lane, executions)
    print(f"feature coverage lane passed: {lane_name}")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("action", choices=("check", "run"))
    parser.add_argument("lane", nargs="?")
    parser.add_argument("--root", type=Path, default=ROOT)
    args = parser.parse_args()
    root = args.root.resolve()
    if args.action == "check":
        if args.lane is not None:
            parser.error("check does not accept a lane")
        validate(root)
    else:
        if args.lane is None:
            parser.error("run requires a lane")
        run_lane(root, args.lane)


if __name__ == "__main__":
    main()

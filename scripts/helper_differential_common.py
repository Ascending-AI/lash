"""Shared live-run comparator and typed grammar for FIG-6372 tooling."""
from dataclasses import dataclass
import json
import random

@dataclass(frozen=True)
class Expression:
    kind: str
    template: str
    children: tuple = ()

    @property
    def source(self):
        return self.template.format(*(child.source for child in self.children)) if self.children else self.template

    def shrinks(self):
        for child in self.children:
            if child.kind == self.kind:
                yield child
        for index, child in enumerate(self.children):
            for smaller in child.shrinks():
                children = list(self.children)
                children[index] = smaller
                yield Expression(self.kind, self.template, tuple(children))
        simple = {"number": "0", "text": "''", "list": "[]"}[self.kind]
        if self.source != simple:
            yield Expression(self.kind, simple)


class Generator:
    """Generate trees from typed language operations, never arbitrary source."""
    def __init__(self, dialect, seed):
        self.dialect = dialect
        self.rng = random.Random(seed)

    def expression(self, kind=None, depth=3):
        kind = kind or self.rng.choice(["number", "text", "list"] + (["approximate"] if self.dialect == "typescript" else []))
        ts = self.dialect == "typescript"
        if kind == "approximate":
            # An implementation-approximated root has an exact input tree.
            # Formatting an allowed alternate libm result is not a Node
            # formatting oracle: the input bits would already differ.
            name = self.rng.choice(["exp", "log", "cos", "sin", "tanh", "cbrt"])
            return Expression("number", "Math." + name + "({0})", (self.expression("number", depth - 1),))
        if depth == 0 or self.rng.randrange(4) == 0:
            pools = {
                "number": ["-0.0", "9007199254740991", "9007199254740992", "9007199254740993", "(0.1+0.2)", "5e-324", "1e-300", "1e300", "-1.5", "0", "0.5", "1", "NaN" if ts else "float('nan')", "Infinity" if ts else "float('inf')", "-Infinity" if ts else "float('-inf')"],
                "text": [json.dumps(value, ensure_ascii=False) for value in ["", "a😀b", "e\u0301", "  é  ", "𐀀", "12.5", "\u0000"]],
                "list": ["[]", "[1,2,3]", "[[1],[],[2,3]]", "[0,-1,2]", "[{b:2,a:1}]" if ts else "[{'b':2,'a':1}]"],
            }
            return Expression(kind, self.rng.choice(pools[kind]))
        child = lambda ty: self.expression(ty, depth - 1)
        if kind == "number":
            if self.rng.randrange(4) == 0:
                value = child("text")
                return Expression(kind, "({0}).length" if ts else "len({0})", (value,))
            names = ["abs", "floor", "ceil", "trunc", "sign", "sqrt", "fround", "clz32"] if ts else ["abs", "round", "int", "float"]
            name = self.rng.choice(names)
            return Expression(kind, ("Math." if ts else "") + name + "({0})", (child("number"),))
        if kind == "text":
            if self.rng.randrange(3) == 0:
                return Expression(kind, "String({0})" if ts else "str({0})", (child("number"),))
            names = ["toLowerCase", "toUpperCase", "trim", "trimStart", "trimEnd"] if ts else ["lower", "upper", "strip", "lstrip", "rstrip"]
            name = self.rng.choice(names)
            return Expression(kind, "({0})." + name + "()", (child("text"),))
        if self.rng.randrange(3) == 0:
            return Expression(kind, "[{0},{1}]", (child("number"), child("text")))
        if ts:
            template = self.rng.choice(["({0}).slice(-2)", "({0}).concat([])", "({0}).toReversed()", "Array.from({0})"])
        else:
            template = self.rng.choice(["({0})[-2:]", "({0})[::-1]", "list({0})", "({0}).copy()"])
        return Expression(kind, template, (child("list"),))

    def case(self, expression, index):
        return {"id": f"fuzz/{self.dialect}/{index}", "dialect": self.dialect,
                "source": f"await observe({expression.source})" + (";" if self.dialect == "typescript" else "\n")}

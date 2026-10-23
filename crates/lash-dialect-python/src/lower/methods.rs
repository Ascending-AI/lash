//! The dialect's methods: the table of those it has, and a method call,
//! which a package member answers first where a package value may reach
//! its receiver (§2.4 of the host-package design).

use lash_kernel_doc::{Member, Place};

use ruff_python_ast::{self as ast, Expr as PyExpr};
use ruff_text_size::Ranged;

use super::calls::{Given, absent, arguments_error, keyword_names};
use super::{Lowerer, Lowering, Operand};
use crate::diagnostics::{self, Code};
use crate::scope::Ty;

/// A method: its name, the fewest and the most arguments it takes, and the
/// names its arguments may be passed under, in order.
pub(super) struct Method {
    pub(crate) name: &'static str,
    min: usize,
    pub(crate) packed: bool,
    kinds: &'static [&'static str],
    pub(crate) max: usize,
    keywords: &'static [&'static str],
    pub(super) result: Ty,
}

const fn method(
    name: &'static str,
    min: usize,
    max: usize,
    result: Ty,
    kinds: &'static [&'static str],
) -> Method {
    Method {
        name,
        min,
        packed: false,
        kinds,
        max,
        keywords: &[],
        result,
    }
}

const fn packed_method(name: &'static str, result: Ty, kinds: &'static [&'static str]) -> Method {
    let mut entry = method(name, 0, 1, result, kinds);
    entry.packed = true;
    entry
}

pub(super) const METHODS: &[Method] = &[
    method("append", 1, 1, Ty::None, &["list"]),
    method("extend", 1, 1, Ty::None, &["list"]),
    method("insert", 2, 2, Ty::None, &["list"]),
    method("pop", 0, 2, Ty::Unknown, &["list", "map", "set"]),
    method("remove", 1, 1, Ty::None, &["list", "set"]),
    method("index", 1, 3, Ty::Int, &["list", "tuple", "text"]),
    method("count", 1, 3, Ty::Int, &["list", "tuple", "text"]),
    Method {
        name: "sort",
        packed: false,
        kinds: &["list"],
        min: 0,
        max: 2,
        keywords: &["key", "reverse"],
        result: Ty::None,
    },
    method("reverse", 0, 0, Ty::None, &["list"]),
    method("copy", 0, 0, Ty::Unknown, &["list", "map", "set"]),
    method("clear", 0, 0, Ty::None, &["list", "map", "set"]),
    method("get", 1, 2, Ty::Unknown, &["map"]),
    method("keys", 0, 0, Ty::List, &["map"]),
    method("values", 0, 0, Ty::List, &["map"]),
    method("items", 0, 0, Ty::List, &["map"]),
    method("setdefault", 1, 2, Ty::Unknown, &["map"]),
    packed_method("update", Ty::None, &["map", "set"]),
    method("add", 1, 1, Ty::None, &["set"]),
    method("discard", 1, 1, Ty::None, &["set"]),
    packed_method("union", Ty::Set, &["set"]),
    packed_method("intersection", Ty::Set, &["set"]),
    packed_method("difference", Ty::Set, &["set"]),
    method("format_map", 1, 1, Ty::Str, &["text"]),
    method("translate", 1, 1, Ty::Str, &["text"]),
    method("maketrans", 1, 3, Ty::Dict, &["text"]),
    packed_method("format", Ty::Str, &["text"]),
    method("popitem", 0, 0, Ty::Tuple, &["map"]),
    method("fromkeys", 1, 2, Ty::Dict, &["map"]),
    method("conjugate", 0, 0, Ty::Unknown, &["integer", "float"]),
    method("join", 1, 1, Ty::Str, &["text"]),
    Method {
        packed: false,
        name: "split",
        kinds: &["text", "bytes"],
        min: 0,
        max: 2,
        keywords: &["sep", "maxsplit"],
        result: Ty::List,
    },
    method("strip", 0, 1, Ty::Str, &["text"]),
    method("lstrip", 0, 1, Ty::Str, &["text"]),
    method("rstrip", 0, 1, Ty::Str, &["text"]),
    method("upper", 0, 0, Ty::Str, &["text"]),
    method("lower", 0, 0, Ty::Str, &["text"]),
    method("startswith", 1, 3, Ty::Bool, &["text"]),
    method("endswith", 1, 3, Ty::Bool, &["text"]),
    Method {
        packed: false,
        name: "replace",
        kinds: &["text"],
        min: 2,
        max: 3,
        keywords: &["", "", "count"],
        result: Ty::Str,
    },
    method("find", 1, 3, Ty::Int, &["text"]),
    method("title", 0, 0, Ty::Str, &["text"]),
    method("capitalize", 0, 0, Ty::Str, &["text"]),
    method("casefold", 0, 0, Ty::Str, &["text"]),
    method("swapcase", 0, 0, Ty::Str, &["text"]),
    method("partition", 1, 1, Ty::Tuple, &["text"]),
    method("rpartition", 1, 1, Ty::Tuple, &["text"]),
    method("center", 1, 2, Ty::Str, &["text"]),
    method("ljust", 1, 2, Ty::Str, &["text"]),
    method("rjust", 1, 2, Ty::Str, &["text"]),
    method("zfill", 1, 1, Ty::Str, &["text"]),
    Method {
        packed: false,
        name: "expandtabs",
        kinds: &["text"],
        min: 0,
        max: 1,
        keywords: &["tabsize"],
        result: Ty::Str,
    },
    method("removeprefix", 1, 1, Ty::Str, &["text"]),
    method("removesuffix", 1, 1, Ty::Str, &["text"]),
    method("rfind", 1, 3, Ty::Int, &["text"]),
    method("rindex", 1, 3, Ty::Int, &["text"]),
    Method {
        packed: false,
        name: "rsplit",
        kinds: &["text", "bytes"],
        min: 0,
        max: 2,
        keywords: &["sep", "maxsplit"],
        result: Ty::List,
    },
    Method {
        packed: false,
        name: "splitlines",
        kinds: &["text"],
        min: 0,
        max: 1,
        keywords: &["keepends"],
        result: Ty::List,
    },
    method("symmetric_difference", 1, 1, Ty::Set, &["set"]),
    method("symmetric_difference_update", 1, 1, Ty::None, &["set"]),
    packed_method("intersection_update", Ty::None, &["set"]),
    packed_method("difference_update", Ty::None, &["set"]),
    method("issubset", 1, 1, Ty::Bool, &["set"]),
    method("issuperset", 1, 1, Ty::Bool, &["set"]),
    method("isdisjoint", 1, 1, Ty::Bool, &["set"]),
    Method {
        packed: false,
        name: "decode",
        kinds: &["bytes"],
        min: 0,
        max: 2,
        keywords: &["encoding", "errors"],
        result: Ty::Str,
    },
    Method {
        packed: false,
        name: "encode",
        kinds: &["text"],
        min: 0,
        max: 2,
        keywords: &["encoding", "errors"],
        result: Ty::Unknown,
    },
    method("is_integer", 0, 0, Ty::Bool, &["float"]),
    method("as_integer_ratio", 0, 0, Ty::Tuple, &["float", "integer"]),
    method("bit_length", 0, 0, Ty::Int, &["integer"]),
    method("bit_count", 0, 0, Ty::Int, &["integer"]),
    method("hex", 0, 0, Ty::Str, &["float"]),
    method("isalpha", 0, 0, Ty::Bool, &["text"]),
    method("isalnum", 0, 0, Ty::Bool, &["text"]),
    method("isdecimal", 0, 0, Ty::Bool, &["text"]),
    method("isdigit", 0, 0, Ty::Bool, &["text"]),
    method("isnumeric", 0, 0, Ty::Bool, &["text"]),
    method("islower", 0, 0, Ty::Bool, &["text"]),
    method("isupper", 0, 0, Ty::Bool, &["text"]),
    method("istitle", 0, 0, Ty::Bool, &["text"]),
    method("isspace", 0, 0, Ty::Bool, &["text"]),
    method("isprintable", 0, 0, Ty::Bool, &["text"]),
    method("isascii", 0, 0, Ty::Bool, &["text"]),
    method("isidentifier", 0, 0, Ty::Bool, &["text"]),
    method("cancel", 0, 0, Ty::Bool, &["task"]),
];

/// Pure value properties, authored once for both lookup and name projection.
const PROPERTIES: &[(&str, &str, &str)] = &[
    ("integer", "real", "value"),
    ("integer", "imag", "0"),
    ("integer", "numerator", "value"),
    ("integer", "denominator", "1"),
    ("float", "real", "value"),
    ("float", "imag", "0.0"),
    ("tuple", "__name__", "invoke py.name(value)"),
    ("tuple", "start", "invoke py.slice.start(value)"),
    ("tuple", "stop", "invoke py.slice.stop(value)"),
    ("tuple", "step", "invoke py.slice.step(value)"),
    ("error", "args", "invoke py.exc.args(value)"),
    ("error", "__cause__", "invoke py.exc.cause(value)"),
];

/// The names of the methods the dialect has.
fn method_names() -> Vec<&'static str> {
    METHODS.iter().map(|method| method.name).collect()
}

impl Lowerer<'_> {
    /// `receiver.name(...)`. Where a package value may reach the receiver,
    /// a package member answers before the dialect's method (§2.4).
    /// `awaited` is whether an `await` stands before the call, which the
    /// call then awaits in its own lowering.
    pub(super) fn method(
        &mut self,
        attribute: &ast::ExprAttribute,
        call: &ast::ExprCall,
        awaited: bool,
    ) -> Lowering<Operand> {
        if call.arguments.args.iter().any(PyExpr::is_starred_expr)
            || call
                .arguments
                .keywords
                .iter()
                .any(|keyword| keyword.arg.is_none())
        {
            return self.call_function(call, awaited);
        }
        let name = attribute.attr.id.as_str();
        let method = METHODS.iter().find(|method| method.name == name);
        if self.seals() && !self.is_asyncio(&attribute.value) {
            return self.package_method(attribute, call, method, awaited);
        }
        let Some(method) = method else {
            return self.call_function(call, awaited);
        };
        let Given {
            head: receiver,
            positional,
            keywords: keyword_values,
        } = self.given(Some(&attribute.value), &call.arguments)?;
        let keywords = keyword_names(&call.arguments)
            .zip(keyword_values)
            .collect::<Vec<_>>();
        let slots = self.method_slots(method, receiver, positional, &keywords, call)?;
        self.ordinary_method(method, slots, attribute, awaited)
    }

    /// A method call where a package value may reach the receiver: the
    /// receiver's package type's member, the dialect's method on a
    /// receiver of another known type, or a selection between them.
    fn package_method(
        &mut self,
        attribute: &ast::ExprAttribute,
        call: &ast::ExprCall,
        method: Option<&'static Method>,
        awaited: bool,
    ) -> Lowering<Operand> {
        let name = attribute.attr.id.as_str();
        let receiver = self.expr(&attribute.value)?;
        if let Some(brand) = self.brand_of(receiver.ty) {
            return self.call_typed_member(receiver, &brand, name, call, awaited);
        }
        let targets = if receiver.ty == Ty::Unknown {
            self.selectable(name, true)
        } else {
            Vec::new()
        };
        if targets.is_empty() && method.is_none() {
            return Err(self.method_refusal(name, call));
        }
        if receiver.ty != Ty::Unknown
            && let Some(method) = method
        {
            self.no_stars(&call.arguments)?;
            let passed = self.passed(Some(receiver), &call.arguments)?;
            let receiver = passed.receiver.unwrap_or_else(Operand::none);
            let slots =
                self.method_slots(method, receiver, passed.positional, &passed.keywords, call)?;
            return self.ordinary_method(method, slots, attribute, awaited);
        }
        self.select_call(receiver, name, method, &targets, call, awaited)
    }

    /// The refusal of a method the dialect does not have.
    pub(super) fn method_refusal(
        &self,
        name: &str,
        call: &ast::ExprCall,
    ) -> lash_kernel_dialect::Diagnostic {
        let (range, receiver) = match call.func.as_ref() {
            PyExpr::Attribute(attribute) => (attribute.range, attribute.value.range()),
            other => (other.range(), other.range()),
        };
        let repair = format!(
            "replace `{}` with a supported method of `{}`; the dialect's methods are: {}",
            self.text(range),
            self.text(receiver),
            method_names().join(", ")
        );
        diagnostics::with_repair(
            Code::MethodUnsupported,
            format!("the method `{name}` is not in the dialect"),
            range,
            &repair,
        )
    }

    /// The slots of a call of the dialect's method `method`: the receiver,
    /// then each argument at its position or under its keyword, absent
    /// for one the call leaves out.
    pub(super) fn method_slots(
        &self,
        method: &Method,
        receiver: Operand,
        positional: Vec<Operand>,
        keywords: &[(String, Operand)],
        call: &ast::ExprCall,
    ) -> Lowering<Vec<Operand>> {
        let name = method.name;
        if method.packed {
            let fields = keywords
                .iter()
                .map(|(name, value)| lash_kernel_doc::MapEntry {
                    key: Operand::text(name).expr,
                    value: value.expr.clone(),
                })
                .collect();
            return Ok(vec![
                receiver,
                Self::tuple_of(&positional),
                Operand::inline(lash_kernel_doc::Expr::Map(fields), Ty::Dict),
            ]);
        }
        let mut slots: Vec<Option<Operand>> = positional.into_iter().map(Some).collect();
        for ((keyword_name, value), keyword) in keywords.iter().zip(&call.arguments.keywords) {
            let Some(index) = method
                .keywords
                .iter()
                .position(|known| known == keyword_name)
            else {
                return Err(arguments_error(
                    format!("`{name}` takes no keyword argument `{keyword_name}`"),
                    keyword.range,
                ));
            };
            if slots.len() <= index {
                slots.resize(index + 1, None);
            }
            if slots[index].is_some() {
                return Err(arguments_error(
                    format!("`{name}` got multiple values for `{keyword_name}`"),
                    keyword.range,
                ));
            }
            slots[index] = Some(value.clone());
        }
        if slots.len() < method.min || slots.len() > method.max {
            return Err(arguments_error(
                format!(
                    "`{name}` takes {} to {} arguments here, and {} were given",
                    method.min,
                    method.max,
                    slots.len()
                ),
                call.range(),
            ));
        }
        let mut all = vec![receiver];
        all.extend(slots.into_iter().map(|slot| slot.unwrap_or_else(absent)));
        Ok(all)
    }

    /// A call of the dialect's method over its slots, the receiver first.
    fn ordinary_method(
        &mut self,
        method: &Method,
        mut slots: Vec<Operand>,
        attribute: &ast::ExprAttribute,
        awaited: bool,
    ) -> Lowering<Operand> {
        let name = method.name;
        let list_local = matches!(attribute.value.as_ref(), PyExpr::Name(local)
            if self.variable(local.id.as_str()).is_some_and(|found|
                self.local_type(&found, local.id.as_str()) == Ty::List));
        if name == "append" && slots[0].ty == Ty::List && list_local {
            let value = slots.remove(1);
            let target = slots.remove(0).expr;
            let end = self.native("list.len", vec![target.clone()])?;
            // As in comprehension collection, evaluate the argument before
            // reading the end: it may itself mutate the receiver or park.
            self.store(Place::Member(Member::Index { target, index: end }), value);
            return Ok(Operand::none());
        }
        let value = self.invoke(&format!("py.method.{name}"), &slots, method.result)?;
        if awaited {
            return self.await_via("py.await", value);
        }
        Ok(value)
    }
}

/// The runtime attribute table is a projection of the call catalogue. Guest
/// records are reachable only for private installed modules; package values
/// continue through their declared member dispatch.
pub(crate) fn attribute_helpers() -> String {
    let mut source = String::from(
        "use py.bind\nuse py.name\nuse py.slice.start\nuse py.slice.stop\nuse py.slice.step\nuse kind\nuse same\nuse bool.not\nuse record.contains\nuse py.no_attribute\nuse py.value_attribute\nuse py.getattr\nuse py.attribute\nuse py.fail\nuse py.exc.args\nuse py.exc.cause\nuse list.len\nuse record.keys\n",
    );
    for method in METHODS {
        source.push_str(&format!("use py.method.{}\n", method.name));
    }
    source.push_str("\nfunction py.value_attribute(value: Any, name: Text) -> Any\nkernel 1\ncharge 32\nbody {\nlet k = kind(value)\n");
    let property_kinds: std::collections::BTreeSet<_> =
        PROPERTIES.iter().map(|(kind, _, _)| *kind).collect();
    for kind in property_kinds {
        source.push_str(&format!("if same(k, {kind:?}) {{\n"));
        for (_, name, expression) in PROPERTIES.iter().filter(|(owner, _, _)| *owner == kind) {
            source.push_str(&format!(
                "if same(name, {name:?}) {{ return {expression} }}\n"
            ));
        }
        source.push_str("}\n");
    }
    source.push_str("return absent\n}\n\nfunction py.attribute_names(value: Any) -> List(Text)\nkernel 1\ncharge sum(128, size(result))\nbody {\nlet k = kind(value)\nlet out = []\nif same(k, \"record\") { if record.contains(value, \"__py_module\") { for name in record.keys(value) { if bool.not(same(name, \"__py_module\")) { set out[list.len(out)] = name } } return out } }\n");
    let mut kinds: std::collections::BTreeSet<_> = METHODS
        .iter()
        .flat_map(|method| method.kinds.iter().copied())
        .collect();
    kinds.extend(PROPERTIES.iter().map(|(kind, _, _)| *kind));
    for kind in kinds {
        source.push_str(&format!("if same(k, {kind:?}) {{\n"));
        for method in METHODS.iter().filter(|method| method.kinds.contains(&kind)) {
            source.push_str(&format!("set out[list.len(out)] = {:?}\n", method.name));
        }
        for (_, name, _) in PROPERTIES.iter().filter(|(owner, _, _)| *owner == kind) {
            source.push_str(&format!("set out[list.len(out)] = {name:?}\n"));
        }
        source.push_str("return out\n}\n");
    }
    source.push_str("return out\n}\n");
    source.push_str("\nfunction py.getattr(receiver: Any, name: Any, default?: Any) -> Any\nkernel 1\ncharge sum(12, size(name))\nbody {\nif bool.not(same(kind(name), \"text\")) { do invoke py.fail(\"TypeError\", \"attribute name must be string\") }\nreturn invoke py.attribute(receiver, name, default)\n}\n");
    source.push_str("\nfunction py.attribute(receiver: Any, name: Text, default: Any) -> Any\nkernel 1\ncharge sum(1024, size(name))\nbody {\nlet k = kind(receiver)\nif same(k, \"record\") { if record.contains(receiver, \"__py_module\") { if bool.not(same(name, \"__py_module\")) { if record.contains(receiver, name) { return receiver[name] } } } }\nlet property = invoke py.value_attribute(receiver, name)\nif bool.not(same(property, absent)) { return property }\n");
    let kinds: std::collections::BTreeSet<_> = METHODS
        .iter()
        .flat_map(|method| method.kinds.iter().copied())
        .collect();
    for kind in kinds {
        source.push_str(&format!("if same(k, {kind:?}) {{\n"));
        for method in METHODS.iter().filter(|method| method.kinds.contains(&kind)) {
            let names: Vec<_> = (0..method.max).map(|i| format!("arg{i}")).collect();
            let params: Vec<_> = if method.packed {
                vec![("arguments", 3, false), ("keywords", 4, false)]
            } else {
                names
                    .iter()
                    .enumerate()
                    .map(|(index, name)| {
                        let keyword = method
                            .keywords
                            .get(index)
                            .is_some_and(|name| !name.is_empty());
                        let spelling = if keyword {
                            method.keywords[index]
                        } else {
                            name.as_str()
                        };
                        let mode = if keyword && method.name == "sort" {
                            2
                        } else if keyword {
                            1
                        } else {
                            0
                        };
                        (spelling, mode, index >= method.min)
                    })
                    .collect()
            };
            let args = (0..params.len())
                .map(|index| format!("argument{index}"))
                .collect::<Vec<_>>()
                .join(", ");
            let comma = if args.is_empty() { "" } else { ", " };
            let body = format!(
                "let answer = invoke py.method.{}(receiver{comma}{args}) return answer",
                method.name
            );
            let value = super::helper_value(method.name, &params, &body);
            source.push_str(&format!(
                "if same(name, {:?}) {{ return {value} }}\n",
                method.name
            ));
        }
        source.push_str("}\n");
    }
    source.push_str("if bool.not(same(default, absent)) { return default }\ndo invoke py.no_attribute(receiver, name)\nreturn null\n}\n\nfunction py.hasattr(receiver: Any, name: Any) -> Bool\nkernel 1\ncharge sum(12, size(name))\nbody {\ntry { let value = invoke py.getattr(receiver, name) return true } catch problem { if same(problem.kind, \"AttributeError\") { return false } throw problem }\n}\n");
    source
}

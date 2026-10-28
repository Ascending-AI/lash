//! Member operations certified from source guards and registered scalar kernels.

use super::*;

impl Evaluator<'_> {
    pub(super) fn member_access(
        &self,
        definition: &FunctionDefinition,
        kinds: &[&str],
        state: &State,
        value: &Symbol,
    ) -> Option<lash_kernel_doc::plan::Element> {
        use lash_kernel_doc::plan::Element;
        use lash_kernel_value::Type;
        let [target, index] = definition.signature.params.as_slice() else {
            return None;
        };
        let steps = &state.steps;
        match (&target.ty, &index.ty, &definition.signature.result) {
            (Type::Any, Type::Any, Type::Any)
                if kinds == ["Map(_)", "Text(_)"] && map_read(steps, value) =>
            {
                Some(Element::ReadMap)
            }
            (Type::List(item), Type::Int, Type::Any)
                if **item == Type::Any && self.integer_read(kinds, steps, value) =>
            {
                Some(Element::ReadInt)
            }
            (Type::Map(map), Type::Text, Type::Any)
                if map.key == Type::Any
                    && map.value == Type::Any
                    && kinds == ["Map(_)", "Text(_)"]
                    && map_read(steps, value) =>
            {
                Some(Element::ReadMap)
            }
            (Type::Any, Type::Any, Type::Null)
                if kinds.first() == Some(&"List(_)")
                    && matches!(value.constant, Some(Value::Null))
                    && state.vars.get(&index.name)?.kind != Some(ValueKind::Absent)
                    && append(steps) =>
            {
                let kind = state.vars.get(&index.name)?.kind?;
                Some(Element::Append(1 << kind as u32))
            }
            _ => None,
        }
    }

    fn integer_read(&self, kinds: &[&str], steps: &[Step], value: &Symbol) -> bool {
        if kinds != ["List(_)", "Int(_)"] {
            return false;
        }
        let [
            Step::Entry {
                name: nonnegative,
                native: le,
                args: lower,
            },
            Step::Guard {
                test: lower_test,
                holds: true,
            },
            Step::Heap {
                name: length,
                form:
                    Form::Len {
                        collection: "List",
                        target,
                    },
            },
            Step::Entry {
                name: inside,
                native: lt,
                args: upper,
            },
            Step::Guard {
                test: upper_test,
                holds: true,
            },
            Step::Heap {
                name: answer,
                form:
                    Form::Index {
                        target: indexed,
                        index,
                    },
            },
        ] = steps
        else {
            return false;
        };
        let kernel = |native: usize| {
            self.library
                .registry
                .get(self.callees.get(native)?)?
                .native()?
                .prepare()?
                .pair?
                .ints
        };
        lower == "Value::Int(0), args[1]"
            && lower_test == nonnegative
            && target == "args[0]"
            && upper == &format!("args[1], {length}")
            && upper_test == inside
            && indexed == "args[0]"
            && index == "args[1]"
            && value.code == *answer
            && kernel(*le) == Some(lash_kernel_doc::scalar::ScalarOp::Le)
            && kernel(*lt) == Some(lash_kernel_doc::scalar::ScalarOp::Lt)
    }
}

fn append(steps: &[Step]) -> bool {
    let [
        Step::Heap {
            name,
            form:
                Form::Len {
                    collection: "List",
                    target,
                },
        },
        Step::SetIndex {
            target: written,
            index,
            value,
        },
    ] = steps
    else {
        return false;
    };
    target == "args[0]" && written == "args[0]" && index == name && value == "args[1]"
}

fn map_read(steps: &[Step], value: &Symbol) -> bool {
    let [
        Step::Heap {
            name,
            form:
                Form::Lookup {
                    table: "Map",
                    target,
                    key,
                    literal: None,
                },
        },
        Step::Guard { test, holds: true },
    ] = steps
    else {
        return false;
    };
    target == "args[0]" && key == "args[1]" && test == name && value.code == format!("{name}_found")
}

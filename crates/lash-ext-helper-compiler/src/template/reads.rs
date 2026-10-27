//! Member reads certified from their source guards and registered scalar kernels.

use super::*;

impl Evaluator<'_> {
    pub(super) fn member_read(
        &self,
        definition: &FunctionDefinition,
        kinds: &[&str],
        steps: &[Step],
        value: &Symbol,
    ) -> Option<lash_kernel_doc::plan::Element> {
        use lash_kernel_doc::plan::Element;
        use lash_kernel_value::Type;
        let [target, index] = definition.signature.params.as_slice() else {
            return None;
        };
        if definition.signature.result != Type::Any {
            return None;
        }
        match (&target.ty, &index.ty) {
            (Type::List(item), Type::Int)
                if **item == Type::Any && self.integer_read(kinds, steps, value) =>
            {
                Some(Element::ReadInt)
            }
            (Type::Map(map), Type::Text)
                if map.key == Type::Any
                    && map.value == Type::Any
                    && kinds == ["Map(_)", "Text(_)"]
                    && map_read(steps, value) =>
            {
                Some(Element::ReadMap)
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

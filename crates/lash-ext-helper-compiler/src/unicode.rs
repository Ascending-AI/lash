//! Native lookup tables projected from the dialect's Unicode source owner.
use std::fmt::Write as _;

use lash_kernel_doc::constant_pool::{ConstantPool, PoolCoordinate};
use lash_kernel_doc::{Expr, Literal, Rhs, Stmt};

use crate::library::Library;

pub(super) fn tables(library: &Library) -> Result<String, String> {
    let mut output = String::new();
    for (name, constant) in [
        ("py.text.whitespace", "WHITESPACE_RANGES"),
        ("py.text.printable", "NONPRINTABLE_RANGES"),
    ] {
        let (function, registered) = library
            .registry
            .iter()
            .find(|(_, f)| f.name() == name)
            .ok_or_else(|| format!("{name} has no Unicode source owner"))?;
        let definition = registered.definition();
        let pool = ConstantPool::for_function(definition);
        let body = definition
            .body()
            .ok_or_else(|| format!("{name} has no Unicode source body"))?;
        let (statement, ranges) = body
            .block
            .iter()
            .enumerate()
            .find_map(|(index, stmt)| match stmt {
                Stmt::Let {
                    name,
                    value: Rhs::Expr(Expr::List(ranges)),
                } if name.as_str() == "ranges" => Some((index, ranges)),
                _ => None,
            })
            .ok_or_else(|| format!("{name} has no range list"))?;
        let _ = writeln!(
            output,
            "\n/// Projected from `{name}`; the kernel body owns membership.\npub const {constant}: &[(i64, i64)] = &["
        );
        let mut previous = None;
        let mut keys = Vec::new();
        for (index, range) in ranges.iter().enumerate() {
            let coordinate = PoolCoordinate {
                path: vec![statement as u32, 0, index as u32],
                atom: None,
            };
            let entry = pool
                .at(&coordinate)
                .ok_or_else(|| format!("{name} has no exact pool range at {index}"))?;
            keys.push(&entry.key);
            let Expr::Tuple(pair) = range else {
                return Err(format!("{name} has a non-tuple range"));
            };
            let [
                Expr::Literal(Literal::Int(low)),
                Expr::Literal(Literal::Int(high)),
            ] = pair.as_slice()
            else {
                return Err(format!("{name} has non-integer range bounds"));
            };
            let (Some(low), Some(high)) = (low.word(), high.word()) else {
                return Err(format!("{name} has oversized bounds"));
            };
            if low > high || previous.is_some_and(|end| end >= low) {
                return Err(format!("{name} has unordered or overlapping ranges"));
            }
            previous = Some(high);
            let _ = writeln!(output, "    ({low}, {high}),");
        }
        output.push_str("];\n");
        let _ = writeln!(
            output,
            "\n#[allow(dead_code, reason = \"phase-one native pool bindings; runtime reads follow in phase three\")]\npub const {constant}_POOL_KEYS: &[lash_kernel_doc::constant_pool::StaticPoolKey] = &["
        );
        for key in keys {
            let _ = writeln!(
                output,
                "    lash_kernel_doc::constant_pool::StaticPoolKey {{ function: lash_kernel_value::FunctionId::from_bytes({:?}), path: &{:?}, digest: {:?} }},",
                function.as_bytes(),
                key.coordinate.path,
                key.digest
            );
        }
        output.push_str("];\n");
    }
    Ok(output)
}

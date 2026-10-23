//! Native lookup tables projected from the dialect's Unicode source owner.
use std::fmt::Write as _;

use lash_kernel_doc::{Expr, Literal, Rhs, Stmt};

use crate::library::Library;

pub(super) fn tables(library: &Library) -> Result<String, String> {
    let mut output = String::new();
    for (name, constant) in [
        ("py.text.whitespace", "WHITESPACE_RANGES"),
        ("py.text.printable", "NONPRINTABLE_RANGES"),
    ] {
        let body = library
            .registry
            .iter()
            .find(|(_, f)| f.name() == name)
            .and_then(|(_, f)| f.definition().body())
            .ok_or_else(|| format!("{name} has no Unicode source body"))?;
        let ranges = body
            .block
            .iter()
            .find_map(|stmt| match stmt {
                Stmt::Let {
                    name,
                    value: Rhs::Expr(Expr::List(ranges)),
                } if name.as_str() == "ranges" => Some(ranges),
                _ => None,
            })
            .ok_or_else(|| format!("{name} has no range list"))?;
        let _ = writeln!(
            output,
            "\n/// Projected from `{name}`; the kernel body owns membership.\npub const {constant}: &[(i64, i64)] = &["
        );
        let mut previous = None;
        for range in ranges {
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
    }
    Ok(output)
}

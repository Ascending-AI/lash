use super::{Expr, Printed, Printer, TypeScriptSourceError};

impl Printer<'_> {
    pub(super) fn sparse_array(&self, args: &[Expr]) -> Printed {
        let malformed = |reason| TypeScriptSourceError::MalformedSparseArray { reason };
        let [Expr::List(values), Expr::List(holes)] = args else {
            return Err(malformed("expected dense values and hole indexes"));
        };
        if holes.is_empty() {
            return Err(malformed("expected at least one hole"));
        }
        let mut indexes = Vec::with_capacity(holes.len());
        for hole in holes {
            let Expr::Number(index) = hole else {
                return Err(malformed("hole index is not a number"));
            };
            if !index.is_finite() || index.fract() != 0.0 || index.is_sign_negative() {
                return Err(malformed(
                    "hole index is not a canonical non-negative integer",
                ));
            }
            if *index >= values.len() as f64 {
                return Err(malformed("hole index exceeds the dense value count"));
            }
            let index = *index as usize;
            if indexes.last().is_some_and(|previous| *previous >= index) {
                return Err(malformed("hole indexes are not strictly increasing"));
            }
            if !matches!(values[index], Expr::Undefined) {
                return Err(malformed("hole placeholder is not undefined"));
            }
            indexes.push(index);
        }
        let slots = values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                if indexes.binary_search(&index).is_ok() {
                    Ok(String::new())
                } else {
                    self.expression(value)
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut literal = format!("[{}", slots.join(", "));
        // A final elision needs its own comma; a trailing separator alone
        // does not add a slot to a TypeScript array literal.
        if indexes.last() == Some(&(values.len() - 1)) {
            literal.push(',');
        }
        literal.push(']');
        Ok(literal)
    }
}

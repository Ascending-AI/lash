//! Copy-out provenance that is independent of a value's runtime kind.
use super::*;

impl Facts {
    pub(super) fn derive_forwarding_facts(&mut self, collector: &Collector<'_>) {
        // Only proven producers seed the fixed point; cycles and unfinished
        // alias chains keep forwarding. An annotation alone proves nothing.
        for _ in 0..MAX_ROUNDS {
            let before = self.non_builtin.len();
            for (binding, entry) in collector.bindings.iter().enumerate() {
                if entry.captured
                    || (entry.session_scope && self.session.contains(&Name::new(&entry.name)))
                {
                    continue;
                }
                if entry
                    .sources
                    .iter()
                    .any(|source| matches!(source, Source::Value(_) | Source::Returns(_)))
                    && entry.sources.iter().all(|source| match source {
                        Source::Annotated(_) | Source::Returns(_) => true,
                        Source::Value(value) => self.is_non_builtin_source(value),
                        _ => false,
                    })
                {
                    self.non_builtin.insert(binding);
                }
            }
            if self.non_builtin.len() == before {
                break;
            }
        }
    }

    /// Whether copy-out may need a builtin callable's source-owned wrapper.
    /// Container literals expose their supplied values; representation alone
    /// says nothing about the contents of an aliased or mutable container.
    pub(crate) fn may_carry_builtin(&self, expression: &Expr) -> bool {
        fn may(ty: &Ty) -> bool {
            match ty {
                Ty::Undefined
                | Ty::Null
                | Ty::Bool
                | Ty::Float
                | Ty::BigInt
                | Ty::Number
                | Ty::Text
                | Ty::Function(_)
                | Ty::Never => false,
                Ty::List(element) => may(element),
                Ty::Union(parts) => parts.iter().any(may),
                // A structural record type names fields without proving
                // that there are no additional callable-valued fields.
                _ => true,
            }
        }
        if self.is_non_builtin_source(expression) {
            return false;
        }
        // Follow builtin aliases before trusting an annotation on their binding.
        if self.builtin(expression) {
            return true;
        }
        match expression {
            Expr::Array(elements) => elements.iter().any(|element| match element {
                ArrayElement::Hole => false,
                ArrayElement::Value(value) | ArrayElement::Spread(value) => {
                    self.may_carry_builtin(value)
                }
            }),
            Expr::Object(properties) => properties.iter().any(|property| {
                let (ObjectProperty::KeyValue(_, value) | ObjectProperty::Spread(value)) = property;
                self.may_carry_builtin(value)
            }),
            Expr::As { value, .. } => self.may_carry_builtin(value),
            Expr::Conditional {
                consequent,
                alternate,
                ..
            }
            | Expr::Logical {
                left: consequent,
                right: alternate,
                ..
            } => self.may_carry_builtin(consequent) || self.may_carry_builtin(alternate),
            _ => may(&self.type_of(expression)),
        }
    }

    fn is_non_builtin_source(&self, expression: &Expr) -> bool {
        match expression {
            Expr::Function(_) => true,
            Expr::Ident(name, _) => self
                .binding(name)
                .is_some_and(|binding| self.non_builtin.contains(&binding)),
            Expr::As { value, .. } => self.is_non_builtin_source(value),
            Expr::Call { callee, .. } => effect_path(callee)
                .and_then(|path| self.constructors.get(&path))
                .is_some_and(|ty| {
                    matches!(
                        ty,
                        Ty::Undefined
                            | Ty::Null
                            | Ty::Bool
                            | Ty::Float
                            | Ty::BigInt
                            | Ty::Number
                            | Ty::Text
                    )
                }),
            _ => false,
        }
    }
}

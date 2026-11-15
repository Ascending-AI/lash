//! Candidate spellings tried by the readable printer.

/// How a lift spells what it may spell more than one way. [`super::print`]
/// tries the plainest style first and keeps the first one that reads back.
#[derive(Clone, Copy)]
pub(super) struct Style {
    /// Keep a singleton return in a block: lowering an arrow expression can
    /// introduce an additional scope around nested callable metadata.
    pub(super) block_bodies: bool,
    /// Captured module functions retain their `const` declaration form.
    pub(super) const_functions: bool,
    /// Front-end temporaries keep their own `const`s instead of being written
    /// where they are read.
    pub(super) preserve_temporaries: bool,
    /// Numeric natives over checked `number` operands are written as the
    /// operators they lower from (`n < 1`), not as `num.lt(num.checked_float(n), 1)`.
    pub(super) operators: bool,
    /// An unchecked list read and a method call on a receiver read in place
    /// are written as the exact operations they lower from
    /// (`list.element`, `callable.method`), not as `xs[i]` and `m.get(k)`,
    /// whose ordinary lowering takes the receiver binding's own facts.
    pub(super) exact: bool,
    /// Retain float inference from numeric declarations when erased annotations
    /// are unnecessary; the ordinary exact style first tries widened locals.
    pub(super) float_initializers: bool,
}

/// Plainest first: operators over constructor calls, then temporaries
/// written where they are read over their own `const`s.
pub(super) const STYLES: [Style; 4] = [
    Style {
        block_bodies: false,
        const_functions: false,
        operators: true,
        preserve_temporaries: false,
        exact: false,
        float_initializers: false,
    },
    Style {
        block_bodies: false,
        const_functions: false,
        operators: true,
        preserve_temporaries: true,
        exact: false,
        float_initializers: false,
    },
    Style {
        block_bodies: false,
        const_functions: false,
        operators: false,
        preserve_temporaries: false,
        exact: false,
        float_initializers: false,
    },
    Style {
        block_bodies: false,
        const_functions: false,
        operators: false,
        preserve_temporaries: true,
        exact: false,
        float_initializers: false,
    },
];

pub(super) fn attempts() -> impl Iterator<Item = Style> {
    [false, true]
        .into_iter()
        .flat_map(|float_initializers| {
            [false, true].into_iter().flat_map(move |exact| {
                [false, true].into_iter().flat_map(move |block_bodies| {
                    [false, true].into_iter().flat_map(move |const_functions| {
                        STYLES.iter().map(move |style| Style {
                            block_bodies,
                            const_functions,
                            exact,
                            float_initializers,
                            ..*style
                        })
                    })
                })
            })
        })
        .filter(|style| !style.float_initializers || (style.exact && style.operators))
}

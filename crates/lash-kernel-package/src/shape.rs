//! Nominal Brands do not freeze representation. The consumer checks its current shape.
use lash_kernel_doc::{Datum, Decoder, ErrorDatum, NumberPolicy};

use lash_vocabulary::names::Brand;
use lash_vocabulary::types::Type;
pub const STALE_VALUE: &str = "package.stale_value";
/// A value with another Brand is a type error; a matching Brand with an obsolete shape is catchable stale_value.
pub fn check_branded(
    expected: &Brand,
    actual: &Brand,
    representation: &Type,
    value: &Datum,
    numbers: NumberPolicy,
) -> Result<(), ErrorDatum> {
    let error = |kind: &str, message: String| ErrorDatum {
        kind: kind.into(),
        message,
        data: Datum::Null,
    };
    if expected != actual {
        return Err(error(
            "type_error",
            format!(
                "expected {}.{}, received {}.{}",
                expected.package, expected.name, actual.package, actual.name
            ),
        ));
    }
    let decoder = Decoder {
        policy: numbers,
        max_depth: 64,
        declared: &|_| false,
    };
    decoder
        .decode(value, representation)
        .map(|_| ())
        .map_err(|mismatch| {
            error(
                STALE_VALUE,
                format!("{}.{}: {mismatch}", expected.package, expected.name),
            )
        })
}

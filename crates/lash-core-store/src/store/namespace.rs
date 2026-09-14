//! Opaque key grammar shared by namespace-mapping backends.
pub fn is_valid_opaque_key(value: &str) -> bool {
    !value.is_empty() && !value.contains('\0')
}

// Media introspection the workbench does before handing bytes to an
// `AttachmentStore`.
//
// Lash keeps no media catalog: a `MediaType` is validated for shape, never
// interpreted. Deriving the dimensions that ride along in
// `AttachmentTypeMetadata` is therefore the host's job, and this is the whole
// of it.

/// Read a PNG's intrinsic size straight out of its IHDR chunk.
///
/// Returns `None` for anything that is not a PNG with a plausible IHDR, so an
/// upload of another media type simply carries no dimensions rather than
/// fabricated ones.
fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if bytes.len() < 24 || &bytes[..8] != PNG_SIGNATURE || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    (width > 0 && height > 0).then_some((width, height))
}

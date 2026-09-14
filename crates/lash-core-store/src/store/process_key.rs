pub(crate) fn invalid_process_key_reason(value: &str) -> Option<&'static str> {
    if value.trim().is_empty() {
        Some("process id must be a non-empty string")
    } else if value.contains('\0') {
        Some("process id must not contain NUL")
    } else if value.contains('#') {
        Some("process id contains reserved segment separator `#`")
    } else {
        None
    }
}

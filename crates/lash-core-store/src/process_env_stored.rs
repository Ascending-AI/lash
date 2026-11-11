//! How a store keeps a published environment's exact bytes: a shared body
//! and a small head (FIG-7700).
//!
//! The body is one contiguous span of the environment's top-level members
//! other than `components` and `capabilities`, stored once under a
//! content address of its bytes. The head is the remaining bytes, kept at
//! the environment's own reference with the offset the body splices back
//! at. Reads rebuild the original bytes exactly, whatever spelling the
//! publisher used; a spelling this split cannot represent is stored whole.

/// The members a head keeps. Every other top-level member is the body's.
const HEAD_MEMBERS: [&[u8]; 2] = [b"components", b"capabilities"];

/// The namespace bodies are stored under, beside the environments' own.
pub const BODY_NAMESPACE: &str = "process_env_body";

/// version_surface = "coexist"
/// version_guard(items(PROCESS_ENV_BODY_PREFIX_VERSION, body_ref))
const PROCESS_ENV_BODY_PREFIX_VERSION: &str = "process-env-body:v1:blake3:";

/// version_surface = "coexist"
/// version_guard(items(LASH_PROCESS_ENV_BODY_DOMAIN_VERSION, body_ref))
const LASH_PROCESS_ENV_BODY_DOMAIN_VERSION: &str = "lash-process-env-body/v1";

/// A published environment split into its head and its body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredSplit {
    /// The environment's bytes without the body span.
    pub head: Vec<u8>,
    /// Where in `head` the body splices back.
    pub at: usize,
    /// The body span's bytes.
    pub body: Vec<u8>,
    /// The body's content address in [`BODY_NAMESPACE`].
    pub body_ref: String,
}

/// Split `bytes` into a head and a body, or `None` when they are not one
/// top-level object whose body members form one span with no head member
/// between them.
#[must_use]
pub fn split(bytes: &[u8]) -> Option<StoredSplit> {
    let members = top_level_members(bytes)?;
    let mut body: Option<(usize, usize)> = None;
    let mut head_after_body = false;
    for (key, start, end) in members {
        if HEAD_MEMBERS.contains(&key) {
            head_after_body |= body.is_some();
            continue;
        }
        if head_after_body {
            return None;
        }
        body = Some(body.map_or((start, end), |(first, _)| (first, end)));
    }
    let (start, end) = body?;
    let body = bytes[start..end].to_vec();
    let mut head = Vec::with_capacity(bytes.len() - body.len());
    head.extend_from_slice(&bytes[..start]);
    head.extend_from_slice(&bytes[end..]);
    Some(StoredSplit {
        head,
        at: start,
        body_ref: body_ref(&body),
        body,
    })
}

/// The content address of a body's bytes.
#[must_use]
pub fn body_ref(body: &[u8]) -> String {
    format!(
        "{PROCESS_ENV_BODY_PREFIX_VERSION}{}",
        crate::stable_hash::blake3_hex(LASH_PROCESS_ENV_BODY_DOMAIN_VERSION, body)
    )
}

/// Rebuild an environment's bytes from its head, the offset and its body,
/// or `None` when the offset lies outside the head.
#[must_use]
pub fn splice(head: &[u8], at: usize, body: &[u8]) -> Option<Vec<u8>> {
    let (prefix, suffix) = (head.get(..at)?, head.get(at..)?);
    let mut bytes = Vec::with_capacity(head.len() + body.len());
    bytes.extend_from_slice(prefix);
    bytes.extend_from_slice(body);
    bytes.extend_from_slice(suffix);
    Some(bytes)
}

fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
}

fn skip_space(bytes: &[u8], mut at: usize) -> usize {
    while bytes.get(at).copied().is_some_and(is_space) {
        at += 1;
    }
    at
}

/// The end of the string opening at `at`, past its closing quote.
fn string_end(bytes: &[u8], mut at: usize) -> Option<usize> {
    at += 1;
    loop {
        match *bytes.get(at)? {
            b'"' => return Some(at + 1),
            b'\\' => at += 2,
            _ => at += 1,
        }
    }
}

/// The end of the value starting at `at`.
fn value_end(bytes: &[u8], at: usize) -> Option<usize> {
    match *bytes.get(at)? {
        b'"' => string_end(bytes, at),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut at = at;
            loop {
                match *bytes.get(at)? {
                    b'"' => {
                        at = string_end(bytes, at)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(at + 1);
                        }
                    }
                    _ => {}
                }
                at += 1;
            }
        }
        _ => {
            let mut end = at;
            while bytes
                .get(end)
                .is_some_and(|byte| !is_space(*byte) && !matches!(byte, b',' | b'}' | b']'))
            {
                end += 1;
            }
            (end > at).then_some(end)
        }
    }
}

/// Each top-level member as its raw key and the span from its key's opening
/// quote to its value's end. A key spelled with an escape is refused: its raw
/// bytes would not name the member it decodes to.
fn top_level_members(bytes: &[u8]) -> Option<Vec<(&[u8], usize, usize)>> {
    let mut at = skip_space(bytes, 0);
    if bytes.get(at) != Some(&b'{') {
        return None;
    }
    at = skip_space(bytes, at + 1);
    let mut members = Vec::new();
    if bytes.get(at) == Some(&b'}') {
        return (skip_space(bytes, at + 1) == bytes.len()).then_some(members);
    }
    loop {
        if bytes.get(at) != Some(&b'"') {
            return None;
        }
        let start = at;
        let key_end = string_end(bytes, at)?;
        let key = &bytes[start + 1..key_end - 1];
        if key.contains(&b'\\') {
            return None;
        }
        at = skip_space(bytes, key_end);
        if bytes.get(at) != Some(&b':') {
            return None;
        }
        at = skip_space(bytes, at + 1);
        let end = value_end(bytes, at)?;
        members.push((key, start, end));
        at = skip_space(bytes, end);
        match bytes.get(at)? {
            b',' => at = skip_space(bytes, at + 1),
            b'}' => {
                return (skip_space(bytes, at + 1) == bytes.len()).then_some(members);
            }
            _ => return None,
        }
    }
}

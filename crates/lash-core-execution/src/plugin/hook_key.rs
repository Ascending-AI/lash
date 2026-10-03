use super::PluginError;

/// The longest local key a hook registration may carry.
const MAX_HOOK_KEY_BYTES: usize = 64;

/// A hook registration's stable local name within its plugin and seam.
///
/// The runtime records a callback as `{seam}:{key}` under its plugin
/// revision ([`super::PluginCallbackIdentity`]), so a key names the callback
/// across processes and replays: reordering registrations never renames a
/// callback, and two registrations in one plugin and seam with the same key
/// are a registration error. A key is 1 to 64 bytes of ASCII letters,
/// digits, `_`, `-` and `.`. Changing what a keyed callback does requires
/// the plugin's behavior revision, not a new key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HookKey(&'static str);

impl HookKey {
    /// Validate `key`.
    pub fn new(key: &'static str) -> Result<Self, PluginError> {
        if Self::is_valid(key) {
            Ok(Self(key))
        } else {
            Err(PluginError::Registration(format!(
                "invalid hook key `{key}`: a key is 1 to {MAX_HOOK_KEY_BYTES} bytes of ASCII letters, digits, `_`, `-` and `.`"
            )))
        }
    }

    /// The validated key for a literal: what [`crate::hook_key!`] evaluates
    /// at compile time.
    #[doc(hidden)]
    #[expect(
        clippy::panic,
        reason = "only `hook_key!` calls this, in a const item, so an invalid literal fails compilation"
    )]
    pub const fn __literal(key: &'static str) -> Self {
        if !Self::is_valid(key) {
            panic!("invalid hook key literal");
        }
        Self(key)
    }

    const fn is_valid(key: &str) -> bool {
        let bytes = key.as_bytes();
        if bytes.is_empty() || bytes.len() > MAX_HOOK_KEY_BYTES {
            return false;
        }
        let mut index = 0;
        while index < bytes.len() {
            let byte = bytes[index];
            if !(byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' || byte == b'.') {
                return false;
            }
            index += 1;
        }
        true
    }

    pub fn as_str(self) -> &'static str {
        self.0
    }
}

impl std::fmt::Display for HookKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

/// A [`HookKey`](crate::plugin::HookKey) for a string literal, validated at
/// compile time.
///
/// ```
/// let key = lash_core_execution::hook_key!("normalize-args");
/// assert_eq!(key.as_str(), "normalize-args");
/// ```
#[macro_export]
macro_rules! hook_key {
    ($key:literal) => {{
        const KEY: $crate::plugin::HookKey = $crate::plugin::HookKey::__literal($key);
        KEY
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_validated_names() {
        assert_eq!(
            HookKey::new("normalize.args-1_x").unwrap().as_str(),
            "normalize.args-1_x"
        );
        for invalid in ["", "has space", "slot:key", "ünicode", &"k".repeat(65)] {
            let invalid: &'static str = Box::leak(invalid.to_string().into_boxed_str());
            assert!(HookKey::new(invalid).is_err(), "{invalid:?}");
        }
        assert_eq!(crate::hook_key!("literal").as_str(), "literal");
    }
}

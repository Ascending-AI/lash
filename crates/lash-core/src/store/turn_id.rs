/// Stable data-layer identity of one logical turn at lease, claim, and turn-registry
/// boundaries.
///
/// The transparent representation preserves every existing serialized and
/// database string byte while preventing unrelated strings from crossing those
/// authority seams accidentally.
#[repr(transparent)]
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct TurnId(String);

impl TurnId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl std::ops::Deref for TurnId {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl AsRef<str> for TurnId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::borrow::Borrow<str> for TurnId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl std::fmt::Display for TurnId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl From<String> for TurnId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for TurnId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl From<&String> for TurnId {
    fn from(value: &String) -> Self {
        Self(value.clone())
    }
}

impl From<TurnId> for String {
    fn from(value: TurnId) -> Self {
        value.into_inner()
    }
}

impl From<&TurnId> for String {
    fn from(value: &TurnId) -> Self {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_is_the_original_string_bytes() {
        let turn_id = TurnId::from("turn-7");
        assert_eq!(serde_json::to_string(&turn_id).unwrap(), r#""turn-7""#);
        assert_eq!(
            serde_json::from_str::<TurnId>(r#""turn-7""#).unwrap(),
            turn_id
        );
    }
}

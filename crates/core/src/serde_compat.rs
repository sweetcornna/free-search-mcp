//! Serde helpers shared by the declaration types.

/// Serialize a field natively for human-readable formats (JSON, TOML) and as
/// an embedded JSON string for compact binary formats (postcard).
///
/// `serde_json::Value`, untagged enums and other `deserialize_any` users
/// cannot be read back from non-self-describing formats; routing them through
/// a JSON string keeps manifest declarations embeddable with postcard
/// (`fsm-manifests/build.rs`, `SPEC.md` §4) while TOML/JSON stay natural.
pub mod json_compat {
    use serde::de::{DeserializeOwned, Error as _};
    use serde::ser::Error as _;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<T: Serialize, S: Serializer>(
        value: &T,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            value.serialize(serializer)
        } else {
            let text = serde_json::to_string(value).map_err(S::Error::custom)?;
            serializer.serialize_str(&text)
        }
    }

    pub fn deserialize<'de, T: DeserializeOwned, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<T, D::Error> {
        if deserializer.is_human_readable() {
            T::deserialize(deserializer)
        } else {
            let text = String::deserialize(deserializer)?;
            serde_json::from_str(&text).map_err(D::Error::custom)
        }
    }
}

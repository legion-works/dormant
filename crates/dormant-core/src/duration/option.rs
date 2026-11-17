//! Serde adapter for optional duration strings.

use std::time::Duration;

use ::serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::serde::TextDuration;

/// Serialize an optional duration as a string or null.
///
/// # Errors
/// Returns the serializer error if the serializer cannot write the value.
pub fn serialize<S: Serializer>(
    duration: &Option<Duration>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    duration.map(TextDuration).serialize(serializer)
}

/// Deserialize an optional duration string or null.
///
/// # Errors
/// Returns the deserializer error for an invalid duration or value type.
pub fn deserialize<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Duration>, D::Error> {
    Option::<TextDuration>::deserialize(deserializer).map(|value| value.map(|value| value.0))
}

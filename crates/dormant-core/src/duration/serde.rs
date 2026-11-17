//! Serde adapter for duration strings.

use std::fmt;
use std::time::Duration;

use ::serde::{Deserialize, Deserializer, Serialize, Serializer, de};

pub(super) struct TextDuration(pub Duration);

impl<'de> Deserialize<'de> for TextDuration {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl de::Visitor<'_> for Visitor {
            type Value = TextDuration;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a duration")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                super::parse_duration(value)
                    .map(TextDuration)
                    .map_err(|_| E::invalid_value(de::Unexpected::Str(value), &self))
            }
        }
        deserializer.deserialize_str(Visitor)
    }
}

impl Serialize for TextDuration {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&super::format_duration(self.0))
    }
}

/// Serialize a duration as a string.
///
/// # Errors
/// Returns the serializer error if the serializer cannot write the string.
pub fn serialize<S: Serializer>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
    TextDuration(*duration).serialize(serializer)
}

/// Deserialize a duration string.
///
/// # Errors
/// Returns the deserializer error for a non-string or invalid duration.
pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
    TextDuration::deserialize(deserializer).map(|value| value.0)
}

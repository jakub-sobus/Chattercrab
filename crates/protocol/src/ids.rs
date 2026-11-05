//! Identifier and validated-string newtypes.

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::ValidationError;

/// Maximum length of a [`RoomId`] in characters.
pub const MAX_ROOM_ID_LEN: usize = 64;
/// Maximum length of a [`DisplayName`] in characters.
pub const MAX_DISPLAY_NAME_CHARS: usize = 32;
/// Maximum length of a [`RoomName`] in characters.
pub const MAX_ROOM_NAME_CHARS: usize = 48;

/// Identifier of a room: 1 to 64 characters from `[A-Za-z0-9_-]`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RoomId(String);

impl RoomId {
    /// Validates and wraps a room id.
    pub fn new(id: impl Into<String>) -> Result<Self, ValidationError> {
        let id = id.into();
        let valid_chars = id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if id.is_empty() || id.len() > MAX_ROOM_ID_LEN || !valid_chars {
            return Err(ValidationError::RoomId);
        }
        Ok(Self(id))
    }

    /// Returns the id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RoomId {
    type Error = ValidationError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<RoomId> for String {
    fn from(value: RoomId) -> Self {
        value.0
    }
}

impl fmt::Display for RoomId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Identifier of a connected peer, assigned by the server on join (UUID v4).
///
/// Ordering is byte ordering of the UUID, which equals the string ordering of its
/// lowercase hyphenated form. Perfect negotiation relies on this ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PeerId(Uuid);

impl PeerId {
    /// Generates a new random peer id.
    pub fn random() -> Self {
        Self(Uuid::new_v4())
    }

    /// Wraps an existing UUID.
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// Returns the underlying UUID.
    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl fmt::Display for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0.hyphenated(), f)
    }
}

impl std::str::FromStr for PeerId {
    type Err = ValidationError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(s)
            .map(Self)
            .map_err(|_| ValidationError::PeerId)
    }
}

/// Trims `raw` and checks it is 1..=`max_chars` characters with no control characters.
fn validate_label(raw: &str, max_chars: usize) -> Option<String> {
    let trimmed = raw.trim();
    let count = trimmed.chars().count();
    if count == 0 || count > max_chars || trimmed.chars().any(char::is_control) {
        return None;
    }
    Some(trimmed.to_owned())
}

/// A user's display name: trimmed, 1 to 32 characters, no control characters.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct DisplayName(String);

impl DisplayName {
    /// Trims and validates a display name.
    pub fn new(raw: &str) -> Result<Self, ValidationError> {
        validate_label(raw, MAX_DISPLAY_NAME_CHARS)
            .map(Self)
            .ok_or(ValidationError::DisplayName)
    }

    /// Returns the name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for DisplayName {
    type Error = ValidationError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(&value)
    }
}

impl From<DisplayName> for String {
    fn from(value: DisplayName) -> Self {
        value.0
    }
}

impl fmt::Display for DisplayName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A human-readable room name: trimmed, 1 to 48 characters, no control characters.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RoomName(String);

impl RoomName {
    /// Trims and validates a room name.
    pub fn new(raw: &str) -> Result<Self, ValidationError> {
        validate_label(raw, MAX_ROOM_NAME_CHARS)
            .map(Self)
            .ok_or(ValidationError::RoomName)
    }

    /// Returns the name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RoomName {
    type Error = ValidationError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(&value)
    }
}

impl From<RoomName> for String {
    fn from(value: RoomName) -> Self {
        value.0
    }
}

impl fmt::Display for RoomName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

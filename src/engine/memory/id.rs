//! `MemoryId` newtype — cheap-to-clone identifier for memory records.
//!
//! Phase E D-E2: `Arc<str>` newtype matching `TenantId` / `UserId` /
//! `SessionId` precedent. Cloning is two atomic ops (16 bytes per
//! clone) — cheap enough to pass by value through deep call stacks.

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Identifier for a [`super::Memory`]. Convention: `mem-<8+ chars>`,
/// matching the `les-` prefix on `LessonFrontmatter::id`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MemoryId(Arc<str>);

impl MemoryId {
    /// Construct from any string-shaped input.
    pub fn new(s: impl Into<Arc<str>>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MemoryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for MemoryId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Serialize for MemoryId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for MemoryId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(Self::new(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_via_serde_json_uses_plain_string() {
        let id = MemoryId::new("mem-abc12345");
        let s = serde_json::to_string(&id).unwrap();
        assert_eq!(s, "\"mem-abc12345\"");
        let back: MemoryId = serde_json::from_str(&s).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn clone_is_cheap_via_arc() {
        let id = MemoryId::new("mem-clone001");
        let c1 = id.clone();
        let c2 = id.clone();
        assert_eq!(id.as_str().as_ptr(), c1.as_str().as_ptr());
        assert_eq!(id.as_str().as_ptr(), c2.as_str().as_ptr());
    }

    #[test]
    fn display_passes_through() {
        let id = MemoryId::new("mem-display1");
        assert_eq!(format!("{id}"), "mem-display1");
    }

    #[test]
    fn equality_compares_string_content() {
        let a = MemoryId::new("mem-aaaaaaaa");
        let b = MemoryId::new("mem-aaaaaaaa");
        let c = MemoryId::new("mem-bbbbbbbb");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}

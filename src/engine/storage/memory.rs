//! In-memory storage backend.
//!
//! Test fixture and reference impl. Implements the full `Storage`
//! trait including `put_if_version` (trivial under a single `Mutex`).
//! Production code uses [`super::LocalFsStorage`].

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use bytes::Bytes;

use super::error::StorageError;
use super::key::StorageKey;
use super::sealed::Sealed;
use super::version::Version;
use super::Storage;

/// In-memory key/value storage. Versioned for full CAS support.
#[derive(Debug, Default)]
pub struct MemoryStorage {
    inner: Mutex<BTreeMap<StorageKey, (Bytes, Version)>>,
    /// Monotonic version counter shared across all keys. Each successful
    /// put assigns the next value as that key's new version.
    next_version: AtomicU64,
}

impl MemoryStorage {
    pub fn new() -> Self {
        Self::default()
    }

    fn mint_version(&self) -> Version {
        let n = self.next_version.fetch_add(1, Ordering::Relaxed);
        Version::from_bytes(n.to_be_bytes().to_vec())
    }
}

impl Sealed for MemoryStorage {}

#[async_trait]
impl Storage for MemoryStorage {
    async fn get(&self, key: &StorageKey) -> Result<Option<Bytes>, StorageError> {
        let guard = self.inner.lock().expect("MemoryStorage mutex poisoned");
        Ok(guard.get(key).map(|(b, _)| b.clone()))
    }

    async fn put(&self, key: &StorageKey, bytes: Bytes) -> Result<(), StorageError> {
        let version = self.mint_version();
        let mut guard = self.inner.lock().expect("MemoryStorage mutex poisoned");
        guard.insert(key.clone(), (bytes, version));
        Ok(())
    }

    async fn delete(&self, key: &StorageKey) -> Result<(), StorageError> {
        let mut guard = self.inner.lock().expect("MemoryStorage mutex poisoned");
        guard.remove(key);
        Ok(())
    }

    async fn list(&self, prefix: &StorageKey) -> Result<Vec<StorageKey>, StorageError> {
        let guard = self.inner.lock().expect("MemoryStorage mutex poisoned");
        let prefix_str = prefix.as_str();
        let out: Vec<StorageKey> = guard
            .range::<StorageKey, _>(prefix.clone()..)
            .take_while(|(k, _)| k.as_str().starts_with(prefix_str))
            .map(|(k, _)| k.clone())
            .collect();
        Ok(out)
    }

    async fn put_if_version(
        &self,
        key: &StorageKey,
        bytes: Bytes,
        expected_version: Option<&Version>,
    ) -> Result<bool, StorageError> {
        let new_version = self.mint_version();
        let mut guard = self.inner.lock().expect("MemoryStorage mutex poisoned");
        match (guard.get(key), expected_version) {
            (None, None) => {
                guard.insert(key.clone(), (bytes, new_version));
                Ok(true)
            }
            (Some((_, current)), Some(expected)) if current == expected => {
                guard.insert(key.clone(), (bytes, new_version));
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn get_with_version(
        &self,
        key: &StorageKey,
    ) -> Result<Option<(Bytes, Version)>, StorageError> {
        let guard = self.inner.lock().expect("MemoryStorage mutex poisoned");
        Ok(guard.get(key).cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::Context;

    fn key(id: &str) -> StorageKey {
        StorageKey::lesson(&Context::single_user_local(), "active", id)
    }

    #[tokio::test]
    async fn round_trip() {
        let s = MemoryStorage::new();
        let k = key("a");
        s.put(&k, Bytes::from_static(b"hello")).await.unwrap();
        assert_eq!(s.get(&k).await.unwrap().unwrap().as_ref(), b"hello");
    }

    #[tokio::test]
    async fn delete_then_get_returns_none() {
        let s = MemoryStorage::new();
        let k = key("a");
        s.put(&k, Bytes::from_static(b"x")).await.unwrap();
        s.delete(&k).await.unwrap();
        assert!(s.get(&k).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_returns_keys_under_prefix() {
        let s = MemoryStorage::new();
        let ctx = Context::single_user_local();
        s.put(
            &StorageKey::lesson(&ctx, "active", "a"),
            Bytes::from_static(b"1"),
        )
        .await
        .unwrap();
        s.put(
            &StorageKey::lesson(&ctx, "active", "b"),
            Bytes::from_static(b"2"),
        )
        .await
        .unwrap();
        s.put(
            &StorageKey::lesson(&ctx, "archived", "c"),
            Bytes::from_static(b"3"),
        )
        .await
        .unwrap();

        let prefix = StorageKey::from_raw("lessons/active".into());
        let mut keys: Vec<String> = s
            .list(&prefix)
            .await
            .unwrap()
            .into_iter()
            .map(|k| k.as_str().to_string())
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["lessons/active/a.md", "lessons/active/b.md"]
        );
    }

    #[tokio::test]
    async fn put_if_version_create_only_succeeds_on_absent() {
        let s = MemoryStorage::new();
        let k = key("a");
        let ok = s
            .put_if_version(&k, Bytes::from_static(b"first"), None)
            .await
            .unwrap();
        assert!(ok);
        // Second create-only on the now-present key must fail.
        let ok = s
            .put_if_version(&k, Bytes::from_static(b"second"), None)
            .await
            .unwrap();
        assert!(!ok);
        // Original value still there.
        assert_eq!(s.get(&k).await.unwrap().unwrap().as_ref(), b"first");
    }

    #[tokio::test]
    async fn put_if_version_rmw_round_trip() {
        let s = MemoryStorage::new();
        let k = key("a");
        s.put(&k, Bytes::from_static(b"v1")).await.unwrap();
        let (_bytes, v1) = s.get_with_version(&k).await.unwrap().unwrap();

        // Correct CAS: succeeds.
        let ok = s
            .put_if_version(&k, Bytes::from_static(b"v2"), Some(&v1))
            .await
            .unwrap();
        assert!(ok);
        assert_eq!(s.get(&k).await.unwrap().unwrap().as_ref(), b"v2");

        // Stale CAS: the v1 token is now wrong.
        let ok = s
            .put_if_version(&k, Bytes::from_static(b"v3"), Some(&v1))
            .await
            .unwrap();
        assert!(!ok);
        // Value unchanged from the stale CAS.
        assert_eq!(s.get(&k).await.unwrap().unwrap().as_ref(), b"v2");
    }
}

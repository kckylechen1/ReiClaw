//! Snapshot containers and identities used by configuration load/save tracking.

use std::collections::HashMap;

/// Bound identity recorded when a dotted leaf property path begins tracking in
/// `dirty_paths`, `explicit_prop_dirty_paths`, `env_overridden_paths`, or
/// `pre_override_snapshots`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedPropIdentity {
    pub encoded_key: String,
    pub had_sibling_entry: bool,
}

/// Thread-safe map of snapshot metadata captured on load and synchronized whenever
/// a full or incremental save writes updated values or references cleanly.
#[derive(Default)]
pub struct SharedStringMap(std::sync::RwLock<HashMap<String, String>>);

/// Alias for on-disk external secret reference (`op://`, `vault:`) snapshots.
pub type ExternalRefSnapshots = SharedStringMap;

/// Alias for pre-override plaintext snapshots captured when `REI_*` env vars apply.
pub type PreOverrideSnapshots = SharedStringMap;

impl SharedStringMap {
    #[must_use]
    pub fn new() -> Self {
        Self(std::sync::RwLock::new(HashMap::new()))
    }

    #[must_use]
    pub fn from_map(map: HashMap<String, String>) -> Self {
        Self(std::sync::RwLock::new(map))
    }

    #[must_use]
    pub fn to_map(&self) -> HashMap<String, String> {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }

    #[must_use]
    pub fn contains_key(&self, key: &str) -> bool {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(key)
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<String> {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(key)
            .cloned()
    }

    #[must_use]
    pub fn keys(&self) -> std::vec::IntoIter<String> {
        let keys: Vec<String> = self
            .0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect();
        keys.into_iter()
    }

    #[must_use]
    pub fn iter(&self) -> std::vec::IntoIter<(String, String)> {
        let entries: Vec<(String, String)> = self
            .0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        entries.into_iter()
    }

    pub fn insert(&self, key: String, value: String) -> Option<String> {
        self.0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, value)
    }

    pub fn remove(&self, key: &str) -> Option<String> {
        self.0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(key)
    }

    pub fn retain<F>(&self, f: F)
    where
        F: FnMut(&String, &mut String) -> bool,
    {
        self.0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(f);
    }

    pub fn extend<I>(&self, iter: I)
    where
        I: IntoIterator<Item = (String, String)>,
    {
        self.0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(iter);
    }
}

impl From<HashMap<String, String>> for SharedStringMap {
    fn from(map: HashMap<String, String>) -> Self {
        Self::from_map(map)
    }
}

impl Clone for SharedStringMap {
    fn clone(&self) -> Self {
        Self::from_map(self.to_map())
    }
}

impl std::fmt::Debug for SharedStringMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let guard = self
            .0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::fmt::Debug::fmt(&*guard, f)
    }
}

impl PartialEq for SharedStringMap {
    fn eq(&self, other: &Self) -> bool {
        if std::ptr::eq(self, other) {
            return true;
        }
        self.to_map() == other.to_map()
    }
}

impl Eq for SharedStringMap {}

/// Thread-safe set of env-overridden property paths captured on load and
/// synchronized whenever a full or incremental save persists edits.
#[derive(Default)]
pub struct SharedStringSet(std::sync::RwLock<std::collections::HashSet<String>>);

/// Alias for tracked `REI_*` env-overridden property paths.
pub type EnvOverriddenPaths = SharedStringSet;

impl SharedStringSet {
    #[must_use]
    pub fn new() -> Self {
        Self(std::sync::RwLock::new(std::collections::HashSet::new()))
    }

    #[must_use]
    pub fn from_set(set: std::collections::HashSet<String>) -> Self {
        Self(std::sync::RwLock::new(set))
    }

    #[must_use]
    pub fn to_set(&self) -> std::collections::HashSet<String> {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }

    #[must_use]
    pub fn contains(&self, value: &str) -> bool {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(value)
    }

    #[must_use]
    pub fn iter(&self) -> std::vec::IntoIter<String> {
        let entries: Vec<String> = self
            .0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned()
            .collect();
        entries.into_iter()
    }

    pub fn insert(&self, value: String) -> bool {
        self.0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(value)
    }

    pub fn remove(&self, value: &str) -> bool {
        self.0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(value)
    }

    pub fn retain<F>(&self, f: F)
    where
        F: FnMut(&String) -> bool,
    {
        self.0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(f);
    }

    pub fn extend<I>(&self, iter: I)
    where
        I: IntoIterator<Item = String>,
    {
        self.0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(iter);
    }
}

impl From<std::collections::HashSet<String>> for SharedStringSet {
    fn from(set: std::collections::HashSet<String>) -> Self {
        Self::from_set(set)
    }
}

impl Clone for SharedStringSet {
    fn clone(&self) -> Self {
        Self::from_set(self.to_set())
    }
}

impl std::fmt::Debug for SharedStringSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let guard = self
            .0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::fmt::Debug::fmt(&*guard, f)
    }
}

impl PartialEq for SharedStringSet {
    fn eq(&self, other: &Self) -> bool {
        if std::ptr::eq(self, other) {
            return true;
        }
        self.to_set() == other.to_set()
    }
}

impl Eq for SharedStringSet {}

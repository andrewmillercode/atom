//! Serde helpers for JSON payloads that use explicit nulls where Go's
//! zero values would be omitted or tolerated (models.dev, provider APIs).

use serde::{Deserialize, Deserializer};
use std::collections::HashMap;
use std::hash::Hash;

/// Deserialize a field that may be present-but-null into Default::default().
pub fn null_as_default<'de, T, D>(d: D) -> Result<T, D::Error>
where
    T: Default + Deserialize<'de>,
    D: Deserializer<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// Deserialize a sequence (or null) whose elements may themselves be
/// null; null elements become Default::default() like Go's encoding/json.
pub fn null_elements_as_default<'de, T, D>(d: D) -> Result<Vec<T>, D::Error>
where
    T: Default + Deserialize<'de>,
    D: Deserializer<'de>,
{
    Ok(Option::<Vec<Option<T>>>::deserialize(d)?
        .unwrap_or_default()
        .into_iter()
        .map(|e| e.unwrap_or_default())
        .collect())
}

/// Deserialize a map (or null) whose values may themselves be null;
/// null values become Default::default() like Go's encoding/json.
pub fn null_map_values_as_default<'de, K, T, D>(d: D) -> Result<HashMap<K, T>, D::Error>
where
    K: Eq + Hash + Deserialize<'de>,
    T: Default + Deserialize<'de>,
    D: Deserializer<'de>,
{
    Ok(Option::<HashMap<K, Option<T>>>::deserialize(d)?
        .unwrap_or_default()
        .into_iter()
        .map(|(k, v)| (k, v.unwrap_or_default()))
        .collect())
}

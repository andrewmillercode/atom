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

/// Deserialize a tool-call `arguments` field delivered as a JSON string
/// (OpenAI), as an actual JSON object (some OpenCode Zen free-tier
/// routers stream it that way), or as null. Objects and other JSON
/// values are re-serialized to their compact string form; null and
/// absence yield "". Without this the object form fails the whole
/// StreamChunk parse, the chunk is silently skipped, and the tool call
/// survives with permanently empty arguments — the model then retries
/// the identical call forever.
pub fn string_or_object<'de, D>(d: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let v = Option::<serde_json::Value>::deserialize(d)?;
    Ok(match v {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(s)) => s,
        Some(other) => serde_json::to_string(&other).unwrap_or_default(),
    })
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

// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Typed attribute values attached to an observation.
//!
//! [`Attributes`] is a small ordered map of `AttrKey ->
//! AttrValue`. The value type is closed (a typed union of
//! string / integer / unsigned / float / boolean / list)
//! so the wire shape is stable and consumers can match on
//! a known set without coercing from JSON-untyped values.
//!
//! The implementation uses a `Vec` of pairs rather than a
//! `HashMap` because observations carry few keys (typically
//! 2–8) and reading order matters on the wire — an ordered
//! map renders deterministically and diff-able.

use serde::{Deserialize, Serialize};
use std::borrow::Cow;

/// An attribute key.
///
/// Backed by `Cow<'static, str>` so emission seams that key
/// with `&'static str` constants — the common case in the
/// substrate — incur zero allocation, while wire-side
/// deserialisation can land owned `String` keys without
/// further conversion.
pub type AttrKey = Cow<'static, str>;

/// A typed attribute value.
///
/// On the wire, integers serialise as plain JSON numbers.
/// Deserialisation is custom so that non-negative integers
/// re-enter as `UInt` and negative integers as `Int`,
/// preserving the producer's distinction across round-trip
/// — JSON itself does not carry an unsigned tag.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum AttrValue {
    /// Boolean.
    Bool(bool),
    /// Unsigned integer. Distinct from `Int` so byte sizes
    /// / counters round-trip as unsigned.
    UInt(u64),
    /// Signed integer.
    Int(i64),
    /// Floating point — latency seconds, ratios.
    Float(f64),
    /// String value. The most common shape — paths, ids,
    /// names, fingerprints.
    Str(String),
    /// List of strings. Sufficient for "list of granted
    /// capabilities", "list of hostnames on a leaf", etc.
    StrList(Vec<String>),
}

impl<'de> serde::Deserialize<'de> for AttrValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{self, Visitor};
        use std::fmt;

        struct AttrValueVisitor;
        impl<'de> Visitor<'de> for AttrValueVisitor {
            type Value = AttrValue;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str(
                    "a bool, integer, float, string, or array of strings",
                )
            }

            fn visit_bool<E: de::Error>(self, v: bool) -> Result<AttrValue, E> {
                Ok(AttrValue::Bool(v))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<AttrValue, E> {
                Ok(AttrValue::UInt(v))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<AttrValue, E> {
                if v >= 0 {
                    Ok(AttrValue::UInt(v as u64))
                } else {
                    Ok(AttrValue::Int(v))
                }
            }
            fn visit_i128<E: de::Error>(self, v: i128) -> Result<AttrValue, E> {
                if v >= 0 && v <= i64::MAX as i128 {
                    Ok(AttrValue::UInt(v as u64))
                } else if v >= i64::MIN as i128 && v < 0 {
                    Ok(AttrValue::Int(v as i64))
                } else {
                    Err(E::custom(format!(
                        "integer {v} out of range for AttrValue"
                    )))
                }
            }
            fn visit_u128<E: de::Error>(self, v: u128) -> Result<AttrValue, E> {
                if v <= u64::MAX as u128 {
                    Ok(AttrValue::UInt(v as u64))
                } else {
                    Err(E::custom(format!(
                        "unsigned {v} out of range for AttrValue"
                    )))
                }
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<AttrValue, E> {
                Ok(AttrValue::Float(v))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<AttrValue, E> {
                Ok(AttrValue::Str(v.to_string()))
            }
            fn visit_string<E: de::Error>(
                self,
                v: String,
            ) -> Result<AttrValue, E> {
                Ok(AttrValue::Str(v))
            }
            fn visit_seq<A: de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<AttrValue, A::Error> {
                let mut out = Vec::new();
                while let Some(item) = seq.next_element::<String>()? {
                    out.push(item);
                }
                Ok(AttrValue::StrList(out))
            }
        }

        deserializer.deserialize_any(AttrValueVisitor)
    }
}

impl AttrValue {
    /// Shortcut: wrap a string.
    pub fn str<S: Into<String>>(s: S) -> Self {
        Self::Str(s.into())
    }
}

impl From<String> for AttrValue {
    fn from(s: String) -> Self {
        AttrValue::Str(s)
    }
}

impl From<&str> for AttrValue {
    fn from(s: &str) -> Self {
        AttrValue::Str(s.to_string())
    }
}

impl From<i64> for AttrValue {
    fn from(v: i64) -> Self {
        AttrValue::Int(v)
    }
}

impl From<i32> for AttrValue {
    fn from(v: i32) -> Self {
        AttrValue::Int(v as i64)
    }
}

impl From<u64> for AttrValue {
    fn from(v: u64) -> Self {
        AttrValue::UInt(v)
    }
}

impl From<u32> for AttrValue {
    fn from(v: u32) -> Self {
        AttrValue::UInt(v as u64)
    }
}

impl From<usize> for AttrValue {
    fn from(v: usize) -> Self {
        AttrValue::UInt(v as u64)
    }
}

impl From<f64> for AttrValue {
    fn from(v: f64) -> Self {
        AttrValue::Float(v)
    }
}

impl From<bool> for AttrValue {
    fn from(v: bool) -> Self {
        AttrValue::Bool(v)
    }
}

impl From<Vec<String>> for AttrValue {
    fn from(v: Vec<String>) -> Self {
        AttrValue::StrList(v)
    }
}

/// Ordered attribute map carried on every observation.
///
/// Backed by a `Vec` of `(key, value)` pairs to preserve
/// insertion order (so JSON renders deterministically) and
/// because observations carry few attributes — typically
/// 2–8 — making the linear scan in `get` strictly faster
/// than a `HashMap` lookup.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Attributes(Vec<(AttrKey, AttrValue)>);

impl Attributes {
    /// Empty attribute map.
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Construct from an iterator of pairs.
    pub fn from_pairs<I, K, V>(iter: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<AttrKey>,
        V: Into<AttrValue>,
    {
        Self(
            iter.into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        )
    }

    /// Number of attributes.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the map carries no attributes.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Insert or replace a key. Builder-style: returns
    /// `self`.
    pub fn with<K, V>(mut self, key: K, value: V) -> Self
    where
        K: Into<AttrKey>,
        V: Into<AttrValue>,
    {
        self.set(key, value);
        self
    }

    /// Insert or replace a key in place.
    pub fn set<K, V>(&mut self, key: K, value: V)
    where
        K: Into<AttrKey>,
        V: Into<AttrValue>,
    {
        let key = key.into();
        let value = value.into();
        if let Some(slot) = self.0.iter_mut().find(|(k, _)| k == &key) {
            slot.1 = value;
            return;
        }
        self.0.push((key, value));
    }

    /// Look up by key.
    pub fn get(&self, key: &str) -> Option<&AttrValue> {
        self.0
            .iter()
            .find(|(k, _)| k.as_ref() == key)
            .map(|(_, v)| v)
    }

    /// Iterate over the entries in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&AttrKey, &AttrValue)> {
        self.0.iter().map(|(k, v)| (k, v))
    }
}

impl<K, V> Extend<(K, V)> for Attributes
where
    K: Into<AttrKey>,
    V: Into<AttrValue>,
{
    fn extend<T: IntoIterator<Item = (K, V)>>(&mut self, iter: T) {
        for (k, v) in iter {
            self.set(k, v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attributes_preserve_insertion_order() {
        let a = Attributes::new()
            .with("first", "a")
            .with("second", 2i64)
            .with("third", true);
        let keys: Vec<&str> = a.iter().map(|(k, _)| k.as_ref()).collect();
        assert_eq!(keys, vec!["first", "second", "third"]);
    }

    #[test]
    fn attributes_replace_on_re_set() {
        let mut a = Attributes::new().with("key", "old");
        a.set("key", "new");
        assert_eq!(a.len(), 1);
        assert_eq!(a.get("key"), Some(&AttrValue::Str("new".into())));
    }

    #[test]
    fn attributes_serialise_as_ordered_pairs() {
        let a = Attributes::new()
            .with("op", "describe_capabilities")
            .with("latency_us", 1234u64);
        let json = serde_json::to_string(&a).unwrap();
        // The default serde derive on a tuple-struct around
        // a Vec<(K,V)> serialises as an array of two-element
        // arrays. Either shape is acceptable so long as it
        // round-trips and preserves order; assert both
        // properties.
        let back: Attributes = serde_json::from_str(&json).unwrap();
        assert_eq!(back, a);
        assert_eq!(
            back.iter().map(|(k, _)| k.as_ref()).collect::<Vec<_>>(),
            vec!["op", "latency_us"]
        );
    }

    #[test]
    fn attr_value_from_helpers_cover_each_variant() {
        let s: AttrValue = "x".into();
        assert!(matches!(s, AttrValue::Str(_)));
        let i: AttrValue = 1i64.into();
        assert!(matches!(i, AttrValue::Int(1)));
        let u: AttrValue = 2u64.into();
        assert!(matches!(u, AttrValue::UInt(2)));
        let f: AttrValue = 0.5f64.into();
        assert!(matches!(f, AttrValue::Float(_)));
        let b: AttrValue = true.into();
        assert!(matches!(b, AttrValue::Bool(true)));
        let l: AttrValue = vec!["a".to_string()].into();
        assert!(matches!(l, AttrValue::StrList(_)));
    }

    #[test]
    fn get_returns_none_for_unknown_key() {
        let a = Attributes::new().with("present", 1u64);
        assert!(a.get("absent").is_none());
    }

    #[test]
    fn extend_inserts_and_replaces_in_order() {
        let mut a = Attributes::new().with("a", 1u64);
        a.extend([("b", AttrValue::UInt(2)), ("a", AttrValue::UInt(3))]);
        // "a" gets re-set in place; "b" is appended; order
        // therefore is ["a", "b"].
        assert_eq!(a.len(), 2);
        let keys: Vec<&str> = a.iter().map(|(k, _)| k.as_ref()).collect();
        assert_eq!(keys, vec!["a", "b"]);
        assert_eq!(a.get("a"), Some(&AttrValue::UInt(3)));
    }
}

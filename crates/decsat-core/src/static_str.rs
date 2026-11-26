// made by CasualArclamp
//! Serde for `&'static str` fields: the labels decoders put in their views
//! ("strong", "°C") come from a fixed set in the code, but a view read off
//! the wire (smart mode) has only owned strings. Each distinct string is
//! leaked once and kept in a table, so a status arriving five times a second
//! does not leak five times a second.
//!
//! Use it on a field with `#[serde(with = "decsat_core::static_str")]`.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Deserializer, Serializer};

/// `&'static str` under another name. serde's derive takes any field
/// spelled `&str` as borrowed from the input, even with `with` set, which
/// would tie the view to the message it came from; it cannot see through an
/// alias. Write label fields as `Str` with this module's `with`.
pub type Str = &'static str;

/// The one `&'static str` for `s`, made the first time it is seen.
pub fn intern(s: &str) -> &'static str {
    static TABLE: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let mut t = TABLE.get_or_init(Default::default).lock().unwrap();
    if let Some(&k) = t.get(s) {
        return k;
    }
    // Rust note: `Box::leak` hands back a reference that lives for the rest
    // of the program; the table stops the same text being leaked twice.
    let k: &'static str = Box::leak(s.to_owned().into_boxed_str());
    t.insert(k);
    k
}

pub fn serialize<S: Serializer>(v: &&'static str, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(v)
}

pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<&'static str, D::Error> {
    let s = String::deserialize(d)?;
    Ok(intern(&s))
}

/// The same for `Option<&'static str>`.
pub mod option {
    use super::*;

    pub fn serialize<S: Serializer>(v: &Option<&'static str>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(v) => s.serialize_some(v),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<&'static str>, D::Error> {
        Ok(Option::<String>::deserialize(d)?.map(|s| intern(&s)))
    }
}

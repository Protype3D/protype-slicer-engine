//! Version 1 stored opaque values. Version 2 packs children before hashing the
//! parent. A singleton marker supplied by a user is itself put in the table:
//! only its children, never the root of a table entry, are interpreted as links.
use super::tree::json_bytes;
use anyhow::{anyhow, ensure, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

use serde::de::{Error, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;

const BLOB: &str = "$protype_blob";

#[derive(Default)]
pub(super) struct Table<'a> {
    entries: BTreeMap<String, &'a RawValue>,
}

impl<'de: 'a, 'a> Deserialize<'de> for Table<'a> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Blobs;
        impl<'de> Visitor<'de> for Blobs {
            type Value = Table<'de>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("table of content-addressed JSON values")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut table = Table::default();
                while let Some((key, raw)) = map.next_entry::<String, &'de RawValue>()? {
                    // Daemon writers use compact JSON. Hash the mapped bytes
                    // directly; no allocation even for an oversized paint step.
                    if key != format!("{:x}", Sha256::digest(raw.get().as_bytes())) {
                        // Old writers also allow the file to be reformatted.
                        // Keep only its borrowed span once the hash is verified.
                        let value: Value =
                            serde_json::from_str(raw.get()).map_err(A::Error::custom)?;
                        if key != hash_value(&value) {
                            return Err(A::Error::custom("invalid blob hash"));
                        }
                    }
                    if table.entries.insert(key, raw).is_some() {
                        return Err(A::Error::custom("duplicate history blob"));
                    }
                }
                Ok(table)
            }
        }
        d.deserialize_map(Blobs)
    }
}

pub(super) fn hash_value(value: &Value) -> String {
    let mut hash = Sha256::new();
    serde_json::to_writer(&mut hash, value).expect("serializable JSON value");
    format!("{:x}", hash.finalize())
}

fn children(value: &mut Value, f: impl FnMut(&mut Value) -> Result<()>) -> Result<()> {
    match value {
        Value::Object(map) => map.values_mut().try_for_each(f),
        Value::Array(items) => items.iter_mut().try_for_each(f),
        _ => Ok(()),
    }
}

fn marker(value: &Value) -> Option<&Value> {
    value.as_object().filter(|map| map.len() == 1)?.get(BLOB)
}

pub(super) fn pack(
    value: &mut Value,
    emit: &mut impl FnMut(&str, &Value) -> Result<()>,
) -> Result<()> {
    if matches!(value, Value::String(_) | Value::Array(_) | Value::Object(_)) {
        children(value, |child| pack(child, emit))?;
        if marker(value).is_some() || json_bytes(value) > 4096 {
            let key = hash_value(value);
            emit(&key, value)?;
            *value = json!({BLOB:key});
        }
    }
    Ok(())
}

pub(super) struct Reader<'a, 'data> {
    table: &'a Table<'data>,
    recursive: bool,
    // None marks a lookup in progress. Repeated references cost one lookup.
    sizes: BTreeMap<String, Option<usize>>,
}

impl<'a, 'data> Reader<'a, 'data> {
    pub(super) fn new(table: &'a Table<'data>, recursive: bool) -> Self {
        Self {
            table,
            recursive,
            sizes: BTreeMap::new(),
        }
    }

    fn entry(&self, key: &str) -> Result<&'a RawValue> {
        self.table
            .entries
            .get(key)
            .copied()
            .ok_or_else(|| anyhow!("missing history blob"))
    }

    pub(super) fn at_ms(&self, raw: &RawValue) -> Result<i64> {
        #[derive(Deserialize)]
        struct Timing {
            at_ms: Option<i64>,
            #[serde(rename = "$protype_blob")]
            blob: Option<String>,
        }
        let mut timing: Timing = serde_json::from_str(raw.get())?;
        if let Some(key) = timing.blob {
            timing = serde_json::from_str(self.entry(&key)?.get())?;
        }
        timing
            .at_ms
            .ok_or_else(|| anyhow!("invalid history timestamp"))
    }

    pub(super) fn size(&mut self, raw: &RawValue) -> Result<usize> {
        self.raw_size(raw, true, 0)
    }

    fn raw_size(&mut self, raw: &RawValue, links: bool, depth: usize) -> Result<usize> {
        ensure!(depth < 128, "history blob nesting too deep");
        let text = raw.get();
        // Physical lengths are exact for compact writer output. Whitespace in
        // a reformatted inline payload can only make this estimate larger.
        if !text.starts_with(['{', '[']) || (!text.contains(BLOB) && !text.contains("\\u")) {
            return Ok(text.len());
        }
        let mut deserializer = serde_json::Deserializer::from_str(text);
        Ok(deserializer.deserialize_any(SizeVisitor {
            reader: self,
            links,
            depth,
        })?)
    }

    fn blob_size(&mut self, key: &str, depth: usize) -> Result<usize> {
        let raw = self.entry(key)?;
        if !self.recursive {
            return Ok(raw.get().len());
        }
        if let Some(size) = self.sizes.get(key) {
            return size.ok_or_else(|| anyhow!("cyclic history blob"));
        }
        self.sizes.insert(key.to_owned(), None);
        // A literal marker in the table is data at this level. Its children
        // can themselves be packed.
        let size = self.raw_size(raw, false, depth)?;
        self.sizes.insert(key.to_owned(), Some(size));
        Ok(size)
    }

    pub(super) fn expand(&self, value: &mut Value) -> Result<()> {
        self.expand_at(value, 0)
    }

    fn expand_at(&self, value: &mut Value, depth: usize) -> Result<()> {
        ensure!(depth < 128, "history blob nesting too deep");
        if let Some(key) = marker(value) {
            *value = serde_json::from_str(
                self.entry(
                    key.as_str()
                        .ok_or_else(|| anyhow!("invalid blob reference"))?,
                )?
                .get(),
            )?;
            if !self.recursive {
                return Ok(());
            }
        }
        children(value, |child| self.expand_at(child, depth + 1))
    }
}

// Walk packed containers a field at a time. Even an array with one link and
// millions of numeric entries can be measured without constructing a Value.
struct SizeVisitor<'r, 'a, 'data> {
    reader: &'r mut Reader<'a, 'data>,
    links: bool,
    depth: usize,
}

impl<'de> Visitor<'de> for SizeVisitor<'_, '_, '_> {
    type Value = usize;
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a packed JSON container")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<usize, A::Error> {
        let mut size = 2usize;
        let mut comma = 0;
        while let Some(child) = seq.next_element::<&RawValue>()? {
            let child = self
                .reader
                .raw_size(child, true, self.depth + 1)
                .map_err(A::Error::custom)?;
            size = size
                .checked_add(comma)
                .and_then(|n| n.checked_add(child))
                .ok_or_else(|| A::Error::custom("history size overflow"))?;
            comma = 1;
        }
        Ok(size)
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<usize, A::Error> {
        let mut field = map.next_entry::<String, &RawValue>()?;
        let mut size = 2usize;
        let mut comma = 0;
        while let Some((key, child)) = field {
            field = map.next_entry()?;
            if self.links && comma == 0 && field.is_none() && key == BLOB {
                let blob: String = serde_json::from_str(child.get()).map_err(A::Error::custom)?;
                return self
                    .reader
                    .blob_size(&blob, self.depth)
                    .map_err(A::Error::custom);
            }
            let child = self
                .reader
                .raw_size(child, true, self.depth + 1)
                .map_err(A::Error::custom)?;
            size = size
                .checked_add(json_bytes(&key) + 1 + comma)
                .and_then(|n| n.checked_add(child))
                .ok_or_else(|| A::Error::custom("history size overflow"))?;
            comma = 1;
        }
        Ok(size)
    }
}

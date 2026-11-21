//! Seekable, immutable JSON input. A ZIP member or orphan checkpoint can
//! contain much more packed data than the retained history. Borrow raw JSON
//! slices from a private snapshot; large inputs need no file-sized heap buffer.
use anyhow::{ensure, Result};
use serde::de::{Error, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;
use std::collections::BTreeMap;
use std::{fmt, io};

pub(crate) enum Input {
    Small(Vec<u8>),
    File(memmap2::Mmap),
}

impl Input {
    pub(crate) fn read(mut reader: impl io::Read) -> io::Result<Self> {
        let mut scratch = tempfile::spooled_tempfile(64 * 1024);
        io::copy(&mut reader, &mut scratch)?;
        match scratch.into_inner() {
            tempfile::SpooledData::InMemory(cursor) => Ok(Self::Small(cursor.into_inner())),
            tempfile::SpooledData::OnDisk(file) => {
                // SAFETY: this is our private temporary copy. No writer or
                // mutable mapping survives this call, including on Windows.
                Ok(Self::File(unsafe { memmap2::Mmap::map(&file)? }))
            }
        }
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        match self {
            Self::Small(bytes) => bytes,
            Self::File(map) => map,
        }
    }
}

#[derive(Default)]
pub(super) struct Rows<'a> {
    pub legacy: bool,
    pub index: BTreeMap<u64, &'a RawValue>,
}

impl<'a> Rows<'a> {
    fn push(&mut self, key: Option<u64>, raw: &'a RawValue) -> Result<()> {
        #[derive(Deserialize)]
        struct Id {
            id: u64,
        }
        let Id { id } = serde_json::from_str(raw.get())?;
        ensure!(key.is_none_or(|k| k == id), "invalid checkpoint node id");
        ensure!(
            self.index.insert(id, raw).is_none(),
            "duplicate history node"
        );
        Ok(())
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for Rows<'a> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // Array in archives and v2 checkpoints, map in legacy checkpoints.
        struct Nodes;
        impl<'de> Visitor<'de> for Nodes {
            type Value = Rows<'de>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("history node array or legacy checkpoint node map")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut rows = Rows::default();
                while let Some(raw) = seq.next_element()? {
                    rows.push(None, raw).map_err(A::Error::custom)?;
                }
                Ok(rows)
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut rows = Rows {
                    legacy: true,
                    ..Default::default()
                };
                while let Some((id, raw)) = map.next_entry()? {
                    rows.push(Some(id), raw).map_err(A::Error::custom)?;
                }
                Ok(rows)
            }
        }
        d.deserialize_any(Nodes)
    }
}

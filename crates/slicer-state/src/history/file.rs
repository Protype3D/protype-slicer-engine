//! Versioned, optional history sidecar for a project archive.
use super::blobs::{self, Reader, Table};
use super::storage::{Input, Rows};
use super::tree::{HistoryNode, HistoryTree, MAX_HISTORY_BYTES, MAX_NODES};
use anyhow::{anyhow, ensure, Result};
use serde::ser::{Error as _, SerializeMap, SerializeSeq};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::value::RawValue;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::{
    collections::BTreeSet,
    io::{Read, Seek, Write},
};
use zip::{write::SimpleFileOptions, ZipArchive, ZipWriter};

const NAME: &str = "Protype/history.json";
const COMPRESSED_LIMIT: u64 = 64 * 1024 * 1024;

#[derive(Debug)]
pub enum HistoryLoad {
    Loaded(HistoryTree),
    None,
    Mismatch,
    Corrupt(String),
}
impl HistoryLoad {
    pub fn status(&self) -> &'static str {
        match self {
            Self::Loaded(_) => "loaded",
            Self::None => "none",
            Self::Mismatch => "mismatch",
            Self::Corrupt(_) => "corrupt",
        }
    }
    pub fn into_tree(self) -> Option<HistoryTree> {
        if let Self::Loaded(tree) = self {
            Some(tree)
        } else {
            None
        }
    }
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Serialize)]
struct PackedHistory<'a> {
    format: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    fingerprint: Option<String>,
    root: u64,
    head: u64,
    next_id: u64,
    pruned_until_ms: Option<i64>,
    nodes: PackedNodes<'a>,
    blobs: PackedBlobs<'a>,
}

impl<'a> PackedHistory<'a> {
    fn new(tree: &'a HistoryTree, project: Option<&[u8]>) -> Self {
        Self {
            format: 2,
            fingerprint: project.map(hash),
            root: tree.root,
            head: tree.head,
            next_id: tree.next_id,
            pruned_until_ms: tree.pruned_until_ms,
            nodes: PackedNodes(tree),
            blobs: PackedBlobs(tree),
        }
    }
}

impl Serialize for HistoryTree {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        PackedHistory::new(self, None).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for HistoryTree {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        let saved: SavedHistory =
            serde_json::from_str(raw.get()).map_err(serde::de::Error::custom)?;
        saved
            .restore(MAX_NODES, MAX_HISTORY_BYTES)
            .map_err(serde::de::Error::custom)
    }
}

// Two traversals of the same immutable snapshot. Only one node and the set
// of hashes survive between writes to the JSON serializer.
struct PackedNodes<'a>(&'a HistoryTree);
struct PackedBlobs<'a>(&'a HistoryTree);

fn pack_node(
    node: &HistoryNode,
    redo: Option<&u64>,
    mut emit: impl FnMut(&str, &Value) -> Result<()>,
) -> Result<Value> {
    let mut row = serde_json::to_value(node)?;
    for child in row.as_object_mut().unwrap().values_mut() {
        blobs::pack(child, &mut emit)?;
    }
    row["redo_child"] = serde_json::to_value(redo)?;
    Ok(row)
}

impl Serialize for PackedNodes<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(self.0.nodes.len()))?;
        for (&id, node) in &self.0.nodes {
            let row =
                pack_node(node, self.0.redo.get(&id), |_, _| Ok(())).map_err(S::Error::custom)?;
            seq.serialize_element(&row)?;
        }
        seq.end()
    }
}

impl Serialize for PackedBlobs<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut seen = BTreeSet::new();
        let mut map = s.serialize_map(None)?;
        for (&id, node) in &self.0.nodes {
            pack_node(node, self.0.redo.get(&id), |key, entry| {
                if seen.insert(key.to_owned()) {
                    map.serialize_entry(key, entry)
                        .map_err(|e| anyhow!(e.to_string()))?;
                }
                Ok(())
            })
            .map_err(S::Error::custom)?;
        }
        map.end()
    }
}

pub fn encode(tree: &HistoryTree, project_json: &[u8]) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&PackedHistory::new(
        tree,
        Some(project_json),
    ))?)
}

#[derive(Deserialize)]
pub(crate) struct SavedHistory<'a> {
    #[serde(default)]
    format: u32,
    fingerprint: Option<String>,
    root: u64,
    head: u64,
    next_id: u64,
    pruned_until_ms: Option<i64>,
    #[serde(borrow)]
    nodes: Rows<'a>,
    #[serde(default)]
    blobs: Table<'a>,
}

#[derive(Deserialize)]
struct SavedNode<'a> {
    parent: Option<u64>,
    redo_child: Option<u64>,
    #[serde(borrow)]
    meta: &'a RawValue,
    #[serde(borrow)]
    forward: &'a RawValue,
    inverse: &'a RawValue,
}

pub fn decode(bytes: &[u8], project_json: &[u8]) -> HistoryLoad {
    decode_with_limits(bytes, project_json, MAX_NODES, MAX_HISTORY_BYTES)
}

pub(super) fn decode_with_limits(
    bytes: &[u8],
    project_json: &[u8],
    max_nodes: usize,
    max_bytes: usize,
) -> HistoryLoad {
    decode_sidecar(
        serde_json::from_slice(bytes),
        project_json,
        max_nodes,
        max_bytes,
    )
}

fn decode_sidecar(
    parsed: serde_json::Result<SavedHistory>,
    project: &[u8],
    max_nodes: usize,
    max_bytes: usize,
) -> HistoryLoad {
    let parse = || -> Result<HistoryLoad> {
        let saved = parsed?;
        ensure!(matches!(saved.format, 1 | 2), "unsupported history format");
        if saved.fingerprint != Some(hash(project)) {
            return Ok(HistoryLoad::Mismatch);
        }
        Ok(HistoryLoad::Loaded(saved.restore(max_nodes, max_bytes)?))
    };
    parse().unwrap_or_else(|e| HistoryLoad::Corrupt(e.to_string()))
}

#[cfg(test)]
thread_local! { pub(super) static EXPANDED_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

impl SavedHistory<'_> {
    fn restore(self, max_nodes: usize, max_bytes: usize) -> Result<HistoryTree> {
        let mut tree = self.skeleton(max_nodes, max_bytes)?;
        tree.pruned = tree.enforce_limits(false);
        self.hydrate(&mut tree)?;
        Ok(tree)
    }

    pub(crate) fn skeleton(&self, max_nodes: usize, max_bytes: usize) -> Result<HistoryTree> {
        let mut reader = Reader::new(&self.blobs, self.format == 2);
        let mut tree = HistoryTree::with_limits(max_nodes, max_bytes);
        tree.nodes.clear();
        tree.root = self.root;
        tree.head = self.head;
        tree.next_id = self.next_id;
        tree.pruned_until_ms = self.pruned_until_ms;
        let legacy = self.nodes.legacy;
        ensure!(
            legacy == (self.format == 0) && self.format <= 2,
            "unsupported history format"
        );
        for (&id, raw) in &self.nodes.index {
            let row: SavedNode = serde_json::from_str(raw.get())?;
            let at_ms = reader.at_ms(row.meta)?;
            let bytes = if id == tree.root {
                ensure!(
                    serde_json::from_str::<[(); 0]>(row.forward.get()).is_ok()
                        && serde_json::from_str::<[(); 0]>(row.inverse.get()).is_ok(),
                    "invalid history root"
                );
                0
            } else if legacy {
                row.forward.get().len() + row.inverse.get().len()
            } else {
                reader
                    .size(row.forward)?
                    .checked_add(reader.size(row.inverse)?)
                    .ok_or_else(|| anyhow!("history size overflow"))?
            };
            let node = HistoryNode {
                id,
                parent: row.parent,
                forward: vec![],
                inverse: vec![],
                meta: slicer_types::NodeMeta {
                    at_ms,
                    ..Default::default()
                },
                bytes,
            };
            if let Some(child) = row.redo_child {
                tree.redo.insert(id, child);
            }
            tree.nodes.insert(id, Arc::new(node));
            tree.bytes = tree
                .bytes
                .checked_add(bytes)
                .ok_or_else(|| anyhow!("history size overflow"))?;
        }
        tree.validate()?;
        Ok(tree)
    }

    pub(crate) fn hydrate(self, tree: &mut HistoryTree) -> Result<()> {
        let mut reader = Reader::new(&self.blobs, self.format == 2);
        #[cfg(test)]
        EXPANDED_BYTES.set(0);
        for (&id, node) in &mut tree.nodes {
            // A promoted root no longer owns its previous payload. Journal
            // nodes in the recovered tree have no row in the checkpoint.
            if id == tree.root && id != self.root {
                continue;
            }
            let Some(raw) = self.nodes.index.get(&id) else {
                continue;
            };
            let row: SavedNode = serde_json::from_str(raw.get())?;
            let node = Arc::get_mut(node).expect("unshared loading tree");
            if self.nodes.legacy {
                node.meta = serde_json::from_str(row.meta.get())?;
                node.forward = serde_json::from_str(row.forward.get())?;
                node.inverse = serde_json::from_str(row.inverse.get())?;
            } else {
                ensure!(
                    reader.size(row.meta)? <= MAX_HISTORY_BYTES,
                    "history metadata exceeds size limit"
                );
                let mut meta = serde_json::from_str(row.meta.get())?;
                reader.expand(&mut meta)?;
                node.meta = serde_json::from_value(meta)?;
                let events = |raw: &RawValue| -> Result<_> {
                    let mut value = serde_json::from_str(raw.get())?;
                    reader.expand(&mut value)?;
                    Ok(serde_json::from_value(value)?)
                };
                node.forward = events(row.forward)?;
                node.inverse = events(row.inverse)?;
            }
            ensure!(
                node.forward
                    .iter()
                    .chain(&node.inverse)
                    .all(super::is_replayable),
                "invalid history event"
            );
            #[cfg(test)]
            if id != tree.root {
                EXPANDED_BYTES.set(
                    EXPANDED_BYTES.get()
                        + super::tree::json_bytes(&node.forward)
                        + super::tree::json_bytes(&node.inverse),
                );
            }
        }
        Ok(())
    }
}

impl HistoryTree {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.nodes.contains_key(&self.root) && self.nodes.contains_key(&self.head),
            "missing root or head"
        );
        let root = &self.nodes[&self.root];
        ensure!(
            root.parent.is_none() && root.forward.is_empty() && root.inverse.is_empty(),
            "invalid history root"
        );
        let parents: BTreeSet<_> = self.nodes.values().filter_map(|n| n.parent).collect();
        for (&id, n) in &self.nodes {
            ensure!(
                id == n.id && id < self.next_id && self.next_id < u64::MAX,
                "invalid node id"
            );
            ensure!(
                id == self.root
                    || n.parent
                        .is_some_and(|p| p < id && self.nodes.contains_key(&p)),
                "invalid parent"
            );
            ensure!(
                self.redo.contains_key(&id) == parents.contains(&id),
                "missing preferred branch"
            );
            if let Some(c) = self.redo.get(&id) {
                ensure!(
                    self.nodes
                        .get(c)
                        .is_some_and(|child| child.parent == Some(id)),
                    "invalid preferred branch"
                );
            }
            ensure!(
                n.forward.iter().chain(&n.inverse).all(super::is_replayable),
                "invalid history event"
            );
        }
        ensure!(
            self.current_path().contains(&self.head),
            "head outside selected branch"
        );
        Ok(())
    }
}

pub(crate) fn write<W: Write + Seek>(
    zip: &mut ZipWriter<W>,
    tree: HistoryTree,
    project: &[u8],
) -> Result<()> {
    write_bounded(zip, tree, project, COMPRESSED_LIMIT)
}

fn write_bounded<W: Write + Seek>(
    zip: &mut ZipWriter<W>,
    mut tree: HistoryTree,
    project: &[u8],
    limit: u64,
) -> Result<()> {
    loop {
        let mut staging = ZipWriter::new(tempfile::spooled_tempfile(1 << 20));
        staging.start_file(
            NAME,
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated),
        )?;
        serde_json::to_writer(&mut staging, &PackedHistory::new(&tree, Some(project)))?;
        let mut compressed = ZipArchive::new(staging.finish()?)?;
        let entry = compressed.by_name(NAME)?;
        if entry.compressed_size() <= limit {
            zip.raw_copy_file(entry)?;
            return Ok(());
        }
        ensure!(tree.nodes.len() > 1, "history root exceeds size limit");
        tree.prune_to(tree.nodes.len() * 9 / 10, usize::MAX, false);
    }
}

pub(crate) fn read<R: Read + Seek>(zip: &mut ZipArchive<R>, project: &[u8]) -> HistoryLoad {
    let entry = match zip.by_name(NAME) {
        Ok(e) => e,
        Err(zip::result::ZipError::FileNotFound) => return HistoryLoad::None,
        Err(e) => return HistoryLoad::Corrupt(e.to_string()),
    };
    if entry.compressed_size() > COMPRESSED_LIMIT {
        return HistoryLoad::Corrupt("history exceeds size limit".into());
    }
    match Input::read(entry) {
        Ok(input) => decode(input.bytes(), project),
        Err(error) => HistoryLoad::Corrupt(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slicer_types::NodeMeta;
    use std::io::Cursor;

    #[test]
    fn deflated_size_limit_prunes_and_preserves_the_head_and_next_id() {
        let mut tree = HistoryTree::new(0);
        for i in 0..30 {
            let label = (0..128)
                .map(|v| hash(format!("node {i} value {v}").as_bytes()))
                .collect();
            tree.record(
                vec![],
                vec![],
                NodeMeta {
                    label: Some(label),
                    ..Default::default()
                },
            );
        }
        let mut archive = ZipWriter::new(Cursor::new(Vec::new()));
        write_bounded(&mut archive, tree, b"project", 12_000).unwrap();
        let mut zip = ZipArchive::new(archive.finish().unwrap()).unwrap();
        let entry = zip.by_name(NAME).unwrap();
        assert_eq!(entry.compression(), zip::CompressionMethod::Deflated);
        assert!(entry.compressed_size() <= 12_000);
        drop(entry);
        let loaded = read(&mut zip, b"project").into_tree().unwrap();
        assert!(loaded.root() > 0);
        assert_eq!((loaded.head(), loaded.next_id), (30, 31));
    }
}

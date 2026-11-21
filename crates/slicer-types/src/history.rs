use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Author {
    #[default]
    Person,
    Assistant,
    Mcp,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StepTag {
    pub author: Option<Author>,
    pub turn: Option<String>,
    pub group: Option<String>,
    pub label: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ObjectRef {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NodeMeta {
    pub at_ms: i64,
    pub author: Author,
    pub turn: Option<String>,
    pub group: Option<String>,
    pub label: Option<String>,
    pub kind: String,
    pub kinds: Vec<String>,
    pub objects: Vec<ObjectRef>,
    pub keys: Vec<String>,
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ForkView {
    pub child: u64,
    pub tip: u64,
    pub steps: u32,
    pub tip_kind: String,
    pub tip_label: Option<String>,
    pub tip_objects: Vec<ObjectRef>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum HistoryCause {
    Record,
    Goto,
    Reset,
    Load,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HistoryChanged {
    pub pruned: bool,
    pub event_id: u64,
    pub head: u64,
    pub previous_head: u64,
    pub can_undo: bool,
    pub can_redo: bool,
    pub by: Author,
    pub cause: HistoryCause,
    pub added: Option<HistoryNodeView>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HistoryNodeView {
    pub id: u64,
    pub parent: Option<u64>,
    #[serde(flatten)]
    pub meta: NodeMeta,
    pub children: u32,
    pub redo_child: Option<u64>,
    pub ahead: bool,
    pub has_recipe: bool,
    pub forks: Vec<ForkView>,
}

#[derive(Clone, Debug, Serialize)]
pub struct HistoryListView {
    pub head: u64,
    pub root: u64,
    pub can_undo: bool,
    pub can_redo: bool,
    pub pruned_until_ms: Option<i64>,
    pub nodes: Vec<HistoryNodeView>,
}

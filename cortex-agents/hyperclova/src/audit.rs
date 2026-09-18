//! Every tool call the model made, with what the tree answered.
//!
//! A compliance team does not ask what the model *said*; it asks what it *touched*. This is
//! that record: actor, tool, path, allowed or denied, and when. It is printed at the end of a
//! run and written beside the report, so the artefact the agent produced travels with the
//! trail of how it was produced.

use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use chrono::{DateTime, Local};
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct Entry {
    pub at: DateTime<Local>,
    pub actor: String,
    pub tool: String,
    pub path: String,
    pub allowed: bool,
    pub detail: String,
}

#[derive(Clone, Default)]
pub struct Audit {
    entries: Arc<Mutex<Vec<Entry>>>,
}

impl Audit {
    pub fn record(
        &self,
        actor: &str,
        tool: &str,
        path: &Path,
        allowed: bool,
        detail: impl Into<String>,
    ) {
        self.entries.lock().unwrap().push(Entry {
            at: Local::now(),
            actor: actor.to_string(),
            tool: tool.to_string(),
            path: path.display().to_string(),
            allowed,
            detail: detail.into(),
        });
    }

    pub fn entries(&self) -> Vec<Entry> {
        self.entries.lock().unwrap().clone()
    }

    /// One JSON object per line — the shape `mem --json` and `index` already speak.
    pub fn to_jsonl(&self) -> String {
        self.entries()
            .iter()
            .map(|e| serde_json::to_string(e).expect("an audit entry serializes"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

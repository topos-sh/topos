//! WHICH agents' hooks have actually RUN a sweep here — the machine-local evidence document
//! behind `status`'s "last auto-update …" lines.
//!
//! A quiet sweep that knows its caller (`--hook <slug>`, or the evidence-only `--from <slug>`)
//! records the slug with a timestamp — BEFORE the throttle, because a throttled exit is still a
//! hook that fired, and firing is exactly the evidence. Unmarked runs record nothing, so absence
//! never proves a trigger dead; it only means nothing here watched one fire.
//!
//! Best-effort on both sides: a write that fails costs one status line its freshness and must
//! never fail a sweep; an unreadable document reads as empty.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::fs_seam::FsOps;
use crate::sidecar::Layout;

const SCHEMA_VERSION: u32 = 1;

/// The document: per harness slug, when its hook last invoked a sweep (epoch ms).
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct HookEvidence {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub agents: BTreeMap<String, i64>,
}

fn path(layout: &Layout) -> std::path::PathBuf {
    layout.state_dir().join("hook_evidence.json")
}

/// Read the document; absent or unreadable is EMPTY (evidence is never load-bearing).
pub(crate) fn read(fs: &dyn FsOps, layout: &Layout) -> HookEvidence {
    match crate::doc::read_doc::<HookEvidence>(fs, &path(layout)) {
        Ok(Some(doc)) => doc,
        _ => HookEvidence::default(),
    }
}

/// Record that `slug`'s hook invoked a sweep at `now_ms`. Merge-write over the last document;
/// every failure is swallowed (a sweep must never fail on its own bookkeeping).
pub(crate) fn record(fs: &dyn FsOps, layout: &Layout, slug: &str, now_ms: i64) {
    let mut doc = read(fs, layout);
    doc.schema_version = SCHEMA_VERSION;
    doc.agents.insert(slug.to_owned(), now_ms);
    if fs.create_dir_all(&layout.state_dir()).is_err() {
        return;
    }
    let bytes = match serde_json::to_vec_pretty(&doc) {
        Ok(b) => b,
        Err(_) => return,
    };
    let _ = crate::atomic::atomic_write(fs, &path(layout), &bytes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_seam::RealFs;

    #[test]
    fn record_then_read_round_trips_and_merges() {
        let dir = std::env::temp_dir().join(format!("topos-hookev-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let layout = Layout::new(&dir);
        record(&RealFs, &layout, "cursor", 1_000);
        record(&RealFs, &layout, "codex", 2_000);
        record(&RealFs, &layout, "cursor", 3_000);
        let doc = read(&RealFs, &layout);
        assert_eq!(doc.agents.get("cursor"), Some(&3_000));
        assert_eq!(doc.agents.get("codex"), Some(&2_000));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

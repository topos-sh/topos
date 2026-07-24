//! The residual verb-surface guards that survived the manifest fold (the retired
//! follow/channel/agent-scope suites left with their verbs; the governance verbs' coverage moves
//! to the composed `tests/` member against the real server).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use topos_harness::{DiscoveredPlacement, HarnessAdapter, PlacementTarget};
use topos_types::{CurrencyKind, HarnessId, TriggerReport, TriggerState};

use crate::ctx::Ctx;
use crate::fs_seam::RealFs;
use crate::ids::test_sources::{FixedClock, SeqIds};
use crate::ops;
use crate::plane::{FollowSource, InertFollow, InertPlane, PlaneSource};
use crate::sidecar::Layout;

struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("topos-verbsb-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct NullHarness;
impl HarnessAdapter for NullHarness {
    fn id(&self) -> HarnessId {
        HarnessId::ClaudeCode
    }
    fn discover(&self) -> Vec<DiscoveredPlacement> {
        Vec::new()
    }
    fn placement_for(
        &self,
        skill_id: &str,
        _naming: topos_harness::PlacementNaming<'_>,
        _d: Option<&DiscoveredPlacement>,
    ) -> PlacementTarget {
        PlacementTarget {
            dir: std::env::temp_dir().join(skill_id),
        }
    }
    fn currency_kind(&self) -> CurrencyKind {
        CurrencyKind::ExplicitPullOnly
    }
    fn install_currency_trigger(&self) -> TriggerReport {
        TriggerReport {
            harness: HarnessId::ClaudeCode,
            currency_kind: CurrencyKind::ExplicitPullOnly,
            touched_path: None,
            marker_id: "test".into(),
            state: TriggerState::Inactive,
        }
    }
    fn remove_currency_trigger(&self) -> TriggerReport {
        self.install_currency_trigger()
    }
    fn uninstall_footprint(&self) -> Vec<PathBuf> {
        Vec::new()
    }
}

struct Rig {
    home: Scratch,
    fs: RealFs,
    ids: SeqIds,
    clock: FixedClock,
    harness: NullHarness,
}
impl Rig {
    fn new(tag: &str) -> Self {
        Self {
            home: Scratch::new(tag),
            fs: RealFs,
            ids: SeqIds::new("s"),
            clock: FixedClock(1_700_000_000_000),
            harness: NullHarness,
        }
    }
    fn layout(&self) -> Layout {
        Layout::new(&self.home.0)
    }
    fn ctx<'a>(&'a self, plane: &'a dyn PlaneSource, follow: &'a dyn FollowSource) -> Ctx<'a> {
        Ctx {
            fs: &self.fs,
            ids: &self.ids,
            clock: &self.clock,
            device_id: "d_test".into(),
            layout: self.layout(),
            harness: &self.harness,
            plane,
            follow,
            roots: None,
        }
    }
}

#[test]
fn reset_without_a_named_skill_is_refused() {
    // `update --reset` throws away edits — it must never blanket-reset every followed skill.
    let rig = Rig::new("reset-bare");
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    let err = ops::reset(&ctx, &[], false).unwrap_err();
    assert_eq!(err.code(), "INVALID_ARGUMENT");
    assert!(err.to_string().contains("needs a skill name"), "{err}");
}

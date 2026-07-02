//! The storage-maintenance scheduler — the composing server's half of the store's reclamation contract.
//!
//! `plane-store` exposes reclamation as three public authority ops (the recovery sweep, the quarantine
//! janitor, the per-workspace GC pass) and holds NO scheduler: the composing server must run recovery +
//! janitor on startup and all three periodically, or storage abandoned by rejected/stale proposals and
//! crashed migrates grows without bound (and a stranded `deleting` row blocks every re-migrate of that
//! content). This module is that scheduling half, placed in the LIBRARY so every composition owns it the
//! same way: the OSS bin calls [`spawn_maintenance`] once before serving, and a downstream plane makes the
//! same call after [`PlaneState::open`] — or drives [`run_maintenance_pass`] from its own scheduler.
//! [`crate::router`] deliberately does NOT start it: building a router is a pure composition step (tests
//! build many), while spawning a background task is a runtime decision the composition root makes once.
//!
//! A pass NEVER crashes the loop or the server: every authority error is `tracing::error!`-logged with its
//! full source chain (the same server-side diagnostics discipline as the wire error mapper) and the pass
//! moves on to the next step / workspace. `now` for every op is the SAME wall clock the wire layer stamps
//! onto writes — epoch **milliseconds** (`wire::now_utc`), re-read per step so a long pass never back-dates
//! a late one.

use std::time::Duration;

use crate::state::PlaneState;
use crate::wire;
use crate::wire::error::error_chain;

/// One maintenance pass's tallies — what [`run_maintenance_pass`] did (errors are logged + counted, never
/// raised). The spawned scheduler logs a nonzero pass at `info`; a composition driving the pass itself can
/// do the same.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MaintenancePass {
    /// Stale `deleting` rows a crashed GC left behind, finalized by the recovery sweep (cross-workspace).
    pub recovered: usize,
    /// Expired/abandoned upload quarantines swept by the janitor (cross-workspace).
    pub quarantines_swept: usize,
    /// Unrooted objects reclaimed by the per-workspace GC passes.
    pub objects_reclaimed: usize,
    /// Authority errors logged (and skipped over) this pass — nonzero means the log has the chains.
    pub faults: usize,
}

impl MaintenancePass {
    /// Nothing recovered, swept, reclaimed, or failed — the steady state of a healthy, garbage-free store.
    fn is_noop(&self) -> bool {
        *self == Self::default()
    }
}

/// Run ONE maintenance pass — the body a scheduled tick executes: the recovery sweep, then the quarantine
/// janitor, then a GC pass over every workspace currently holding objects ([`Authority::workspaces`]
/// enumerates them; recovery + janitor sweep cross-workspace internally). Every step's failure is
/// `tracing::error!`-logged with its full source chain and tallied — never propagated — so one faulting
/// step (or workspace) never starves the rest.
///
/// [`Authority::workspaces`]: plane_store::Authority::workspaces
pub async fn run_maintenance_pass(state: &PlaneState) -> MaintenancePass {
    let mut pass = MaintenancePass::default();
    let authority = state.authority();

    let (_, now) = wire::now_utc();
    match authority.run_recovery(now).await {
        Ok(recovered) => pass.recovered = recovered,
        Err(error) => {
            pass.faults += 1;
            tracing::error!(step = "recovery", error = %error_chain(&error), "maintenance step failed");
        }
    }

    let (_, now) = wire::now_utc();
    match authority.run_janitor(now).await {
        Ok(swept) => pass.quarantines_swept = swept,
        Err(error) => {
            pass.faults += 1;
            tracing::error!(step = "janitor", error = %error_chain(&error), "maintenance step failed");
        }
    }

    match authority.workspaces().await {
        Ok(workspaces) => {
            for ws in workspaces {
                let (_, now) = wire::now_utc();
                match authority.run_gc(&ws, now).await {
                    Ok(reclaimed) => pass.objects_reclaimed += reclaimed,
                    Err(error) => {
                        pass.faults += 1;
                        tracing::error!(
                            step = "gc",
                            workspace = %ws,
                            error = %error_chain(&error),
                            "maintenance step failed"
                        );
                    }
                }
            }
        }
        Err(error) => {
            pass.faults += 1;
            tracing::error!(step = "workspaces", error = %error_chain(&error), "maintenance step failed");
        }
    }

    pass
}

/// Spawn the periodic maintenance task onto the ambient tokio runtime: one [`run_maintenance_pass`]
/// immediately (the first tick completes at once — that IS the mandated startup recovery + janitor run),
/// then one per `every`. A slow pass delays the next tick rather than bursting to catch up, and `every` is
/// clamped to ≥ 1 s (a zero interval would busy-spin; "disabled" is the caller's decision — the OSS bin
/// treats `TOPOS_PLANE_GC_INTERVAL_SECS=0` as "do not spawn"). Returns the task handle: a composition may
/// `.abort()` it on shutdown; dropping it detaches the task for the process lifetime (what the bin does).
pub fn spawn_maintenance(state: PlaneState, every: Duration) -> tokio::task::JoinHandle<()> {
    let every = every.max(Duration::from_secs(1));
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(every);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let pass = run_maintenance_pass(&state).await;
            if pass.is_noop() {
                tracing::debug!("maintenance pass: nothing to do");
            } else {
                tracing::info!(
                    recovered = pass.recovered,
                    quarantines_swept = pass.quarantines_swept,
                    objects_reclaimed = pass.objects_reclaimed,
                    faults = pass.faults,
                    "maintenance pass"
                );
            }
        }
    })
}

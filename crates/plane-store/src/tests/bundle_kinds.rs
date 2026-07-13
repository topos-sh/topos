//! The second-kind smoke — the door "beyond skills" opens without touching the vault.
//!
//! Custody is generic over bundles: the directory's catalog carries the ONE `kind` tag (`'skill'`
//! for everything that exists today), and nothing below the catalog reads it. This module proves
//! that property behaviorally: it re-tags a live catalog row to a dummy kind — standing in for a
//! future kind's registration surface, which is directory/surface work by design — and then drives
//! the full custody machinery for that bundle: the pointer-move transaction (CAS, availability,
//! lineage, receipt), the delivery predicate, and the device catalog read. Everything answers
//! identically, with the new kind riding every read as display metadata. The repo's grep gate pins the
//! structural half of the same claim (no custody ORCHESTRATION module — the gated trees — names a
//! kind or a skill; the SQL twins keep only the frozen table spellings); this test is the
//! behavioral half.

use super::*;

use crate::delivery::{DeliveredSkill, Delivery};

const ALICE: &str = "alice@acme.com";
const BOB: &str = "bob@acme.com";

/// Seat a person's device (holding its `(ws, dkid)` credential) + their confirmed workspace seat.
async fn seat(fx: &Fixture, w: &WorkspaceId, dkid: &str, seed: u8, principal: &str) {
    let p = prin(principal);
    fx.authority
        .db()
        .seed_device(w, dkid, &dev_key(seed), &p, false, &cred(w, dkid))
        .await
        .unwrap();
    fx.authority
        .db()
        .seed_workspace_member(w, &p, "member", "confirmed")
        .await
        .unwrap();
}

/// Genesis-publish `s` as `dkid`'s device presents it (review off ⇒ lands, catalog row registered).
async fn gpub(
    fx: &Fixture,
    w: &WorkspaceId,
    s: &BundleId,
    dkid: &str,
    op_id: &str,
    files: Vec<UploadedFile>,
) -> crate::SetCurrentReceipt {
    let auth = DeviceOpAuth {
        credential: cred(w, dkid),
        op: DeviceOp::PublishDirect,
        expected: gn(0, 0),
    };
    fx.authority
        .publish(
            w,
            s,
            &op(op_id),
            genesis(files),
            auth,
            None,
            None,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap()
}

/// Re-tag a catalog row's `kind` — the stand-in for a future kind's registration surface (raw
/// `sqlx`, off the committed `.sqlx` drift surface, exactly because no production path writes a
/// non-skill kind yet).
async fn retag_kind(pool: &PgPool, w: &str, skill_id: &str, kind: &str) {
    sqlx::query("UPDATE catalog SET kind = $1 WHERE workspace_id = $2 AND skill_id = $3")
        .bind(kind)
        .bind(w)
        .bind(skill_id)
        .execute(pool)
        .await
        .unwrap();
}

fn find<'a>(d: &'a Delivery, skill_id: &str) -> Option<&'a DeliveredSkill> {
    d.skills.iter().find(|s| s.skill_id == skill_id)
}

/// A bundle re-tagged to a dummy kind rides the WHOLE loop — delivery, the device catalog read, and
/// a fresh pointer-move — with zero custody involvement in the tag: the kind is display metadata
/// the directory serves, and the vault's machinery answers byte-identically to a skill's.
#[sqlx::test]
async fn a_second_kind_rides_the_whole_loop_with_zero_custody_changes(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "kind-smoke").await;
    let (w, s) = (ws("w_kinds"), skill("s_runbook"));
    seat(&fx, &w, "dk_alice", 21, ALICE).await;
    seat(&fx, &w, "dk_bob", 22, BOB).await;

    // Genesis: registered as a skill (the one kind the production surface writes today).
    let r = gpub(
        &fx,
        &w,
        &s,
        "dk_alice",
        "aaaaaaaa-0000-4000-8000-0000000000a1",
        vec![file("SKILL.md", b"v0")],
    )
    .await;
    assert!(r.is_ok());
    let d = fx
        .authority
        .delivery(&w, &cred(&w, "dk_bob"))
        .await
        .unwrap();
    assert_eq!(
        find(&d, "s_runbook").unwrap().kind,
        "skill",
        "a genesis publish registers the first kind"
    );

    // The future kind arrives: a `kind` value + surface work — HERE, in the directory, never below.
    retag_kind(&pool, "w_kinds", "s_runbook", "runbook").await;

    // Delivery rides the new kind, every custody fact unchanged.
    let d = fx
        .authority
        .delivery(&w, &cred(&w, "dk_bob"))
        .await
        .unwrap();
    let entry = find(&d, "s_runbook").expect("the re-tagged bundle still delivers");
    assert_eq!(entry.kind, "runbook", "the kind is served, not interpreted");
    assert_eq!(
        entry.generation,
        gn(1, 1),
        "the pointer facts are untouched"
    );
    let v0 = r.version_id.unwrap();
    assert_eq!(entry.version_id, v0.0);
    assert_eq!(entry.bundle_digest, r.bundle_digest.unwrap());

    // The device catalog read rides it too.
    let index = fx
        .authority
        .list_skills_device(&w, &cred(&w, "dk_bob"), NOW)
        .await
        .unwrap();
    let row = index.iter().find(|r| r.skill_id == "s_runbook").unwrap();
    assert_eq!(row.kind, "runbook");

    // And the pointer-move transaction is kind-blind: a child publish on the re-tagged bundle runs
    // the identical CAS/availability/lineage body and lands.
    let expected = fx
        .authority
        .db()
        .read_current_generation(&w, &s)
        .await
        .unwrap()
        .unwrap();
    let auth = DeviceOpAuth {
        credential: cred(&w, "dk_alice"),
        op: DeviceOp::PublishDirect,
        expected,
    };
    let r2 = fx
        .authority
        .publish(
            &w,
            &s,
            &op("aaaaaaaa-0000-4000-8000-0000000000a2"),
            child(v0, vec![file("SKILL.md", b"v1")]),
            auth,
            None,
            None,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert!(r2.is_ok(), "the pointer-move never reads the kind");
    let d = fx
        .authority
        .delivery(&w, &cred(&w, "dk_bob"))
        .await
        .unwrap();
    assert_eq!(find(&d, "s_runbook").unwrap().generation, gn(1, 2));
}

/// A pointer WITHOUT a catalog row (the pre-catalog seeded shape) folds to the first kind on the
/// index read — the same fallback arm `name`/`status` take, so an unregistered pointer can never
/// serve an empty kind.
#[sqlx::test]
async fn an_unregistered_pointer_reads_as_a_skill(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "kind-fold").await;
    let (w, s) = (ws("w_kinds2"), skill("s_bare"));
    seat(&fx, &w, "dk_alice", 23, ALICE).await;
    let r = gpub(
        &fx,
        &w,
        &s,
        "dk_alice",
        "aaaaaaaa-0000-4000-8000-0000000000a3",
        vec![file("SKILL.md", b"v0")],
    )
    .await;
    assert!(r.is_ok());
    // Strip the registration — what remains is the bare pointer a pre-catalog seed produced (the
    // genesis's `everyone` placement and the author self-follow reference the row, so they go first).
    for table in ["channel_skills", "skill_follows"] {
        sqlx::query(&format!(
            "DELETE FROM {table} WHERE workspace_id = $1 AND skill_id = $2"
        ))
        .bind("w_kinds2")
        .bind("s_bare")
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query("DELETE FROM catalog WHERE workspace_id = $1 AND skill_id = $2")
        .bind("w_kinds2")
        .bind("s_bare")
        .execute(&pool)
        .await
        .unwrap();
    let index = fx
        .authority
        .list_skills_device(&w, &cred(&w, "dk_alice"), NOW)
        .await
        .unwrap();
    let row = index.iter().find(|r| r.skill_id == "s_bare").unwrap();
    assert_eq!(row.kind, "skill", "the fold arm serves the first kind");
    assert_eq!(
        row.name, "s_bare",
        "the same arm names the bare pointer by id"
    );
}

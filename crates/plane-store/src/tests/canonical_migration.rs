//! The migration-0010 dedupe/fold logic, probed against MIRROR tables.
//!
//! An honest caveat up front: `#[sqlx::test]` databases arrive with `0010_canonical_principal`
//! already applied — including its `lower(… COLLATE "C")` CHECK constraints — so the hostile
//! pre-migration states (mixed-case and case-variant-DUPLICATE principals) can no longer be
//! seeded into the real `roster` / `workspace_member` tables. Each test therefore creates two
//! PROBE tables whose DDL is hand-copied from migrations `0001` (`roster`) and `0006`
//! (`workspace_member`) — the pre-0010 shapes, no canonical CHECK — seeds the hostile rows
//! there, and executes the statements of `migrations/0010_canonical_principal.sql` ITSELF
//! (`include_str!`, table names textually rewritten to the probe names, split on `;`), the two
//! `ALTER TABLE … ADD CONSTRAINT` statements rewritten too and asserted to succeed post-fold.
//! If the migration file's dedupe/fold SQL ever drifts, these tests re-run the new text verbatim.

use sqlx::PgPool;

/// The migration under probe, verbatim from the file the migrator ran.
const MIGRATION_0010: &str = include_str!("../../migrations/0010_canonical_principal.sql");

/// The 0010 statements with `workspace_member` / `roster` rewritten to the probe tables:
/// comment lines stripped, split on `;`, empties dropped. The fold-in-place UPDATEs on the other
/// real tables (`device_registry`, `admin_claim`, `genesis_requests`, `proposals`) survive untouched
/// and run against the real — empty — tables, so the WHOLE script is proven to execute in order. The
/// one exception is 0010's `UPDATE read_token …`: a LATER migration (0014) DROPPED `read_token`, so on
/// a fully-migrated probe DB that table is gone and its fold statement is skipped here.
fn probe_statements() -> Vec<String> {
    let rewritten = MIGRATION_0010
        .lines()
        .filter(|l| !l.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n")
        .replace("workspace_member", "probe_workspace_member")
        .replace("roster", "probe_roster");
    rewritten
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        // `read_token` was dropped by migration 0014, so its 0010 fold statement can no longer run
        // against a fully-migrated database — skip it (the other real-table folds still run).
        .filter(|s| !s.contains("read_token"))
        .map(str::to_owned)
        .collect()
}

/// Create the two probe tables in the PRE-0010 shape (hand-copied from 0001/0006 — the absent
/// canonical CHECK is exactly what lets the hostile rows in).
async fn create_probe_tables(pool: &PgPool) {
    sqlx::query(
        "CREATE TABLE probe_roster (
            workspace_id TEXT NOT NULL,
            skill_id     TEXT NOT NULL,
            principal    TEXT NOT NULL,
            PRIMARY KEY (workspace_id, skill_id, principal)
        )",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE probe_workspace_member (
            workspace_id TEXT NOT NULL,
            principal    TEXT NOT NULL,
            role         TEXT NOT NULL CHECK (role IN ('owner', 'reviewer', 'member')),
            status       TEXT NOT NULL CHECK (status IN ('invited', 'confirmed')),
            invited_by   TEXT,
            added_at     TEXT NOT NULL,
            PRIMARY KEY (workspace_id, principal)
        )",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// Execute every rewritten 0010 statement in file order, panicking on the first failure — which
/// also asserts the two rewritten ADD CONSTRAINTs go through against the post-fold rows.
async fn run_probe_migration(pool: &PgPool) {
    for stmt in probe_statements() {
        sqlx::query(&stmt)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("0010 probe statement failed: {e}\n{stmt}"));
    }
}

async fn seed_roster_row(pool: &PgPool, ws: &str, skill: &str, principal: &str) {
    sqlx::query("INSERT INTO probe_roster (workspace_id, skill_id, principal) VALUES ($1, $2, $3)")
        .bind(ws)
        .bind(skill)
        .bind(principal)
        .execute(pool)
        .await
        .unwrap();
}

async fn seed_member_row(
    pool: &PgPool,
    ws: &str,
    principal: &str,
    role: &str,
    status: &str,
    added_at: &str,
) {
    sqlx::query(
        "INSERT INTO probe_workspace_member (workspace_id, principal, role, status, invited_by, added_at) \
         VALUES ($1, $2, $3, $4, NULL, $5)",
    )
    .bind(ws)
    .bind(principal)
    .bind(role)
    .bind(status)
    .bind(added_at)
    .execute(pool)
    .await
    .unwrap();
}

/// Like [`seed_member_row`] but with an `invited_by` witness — the one column the member dedupe's
/// final tiebreak leaves observable (the fold erases the principal casing that decided it, and the
/// migration never rewrites `invited_by`).
async fn seed_member_row_by(
    pool: &PgPool,
    ws: &str,
    principal: &str,
    role: &str,
    status: &str,
    added_at: &str,
    invited_by: &str,
) {
    sqlx::query(
        "INSERT INTO probe_workspace_member (workspace_id, principal, role, status, invited_by, added_at) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(ws)
    .bind(principal)
    .bind(role)
    .bind(status)
    .bind(invited_by)
    .bind(added_at)
    .execute(pool)
    .await
    .unwrap();
}

/// Every probe roster row, deterministically ordered.
async fn roster_rows(pool: &PgPool) -> Vec<(String, String, String)> {
    sqlx::query_as::<_, (String, String, String)>(
        "SELECT workspace_id, skill_id, principal FROM probe_roster \
         ORDER BY workspace_id, skill_id, principal",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

/// A workspace's probe member rows as `(principal, role, status, added_at)`, ordered.
async fn member_rows(pool: &PgPool, ws: &str) -> Vec<(String, String, String, String)> {
    sqlx::query_as::<_, (String, String, String, String)>(
        "SELECT principal, role, status, added_at FROM probe_workspace_member \
         WHERE workspace_id = $1 ORDER BY principal",
    )
    .bind(ws)
    .fetch_all(pool)
    .await
    .unwrap()
}

#[sqlx::test]
async fn roster_probe_collapses_three_case_variants_to_one_folded_row(pool: PgPool) {
    create_probe_tables(&pool).await;
    // Three casings of ONE (ws, skill, mailbox) — the attacker-constructible duplicate state.
    seed_roster_row(&pool, "w1", "s_deploy", "alice@x.io").await;
    seed_roster_row(&pool, "w1", "s_deploy", "Alice@X.io").await;
    seed_roster_row(&pool, "w1", "s_deploy", "ALICE@X.IO").await;
    // Scope isolation: the same mailbox in ANOTHER workspace (a lone mixed-case row that must
    // fold in place, never merge across workspaces) and a distinct mailbox both survive.
    seed_roster_row(&pool, "w2", "s_deploy", "Alice@X.io").await;
    seed_roster_row(&pool, "w1", "s_deploy", "bob@x.io").await;

    run_probe_migration(&pool).await;

    assert_eq!(
        roster_rows(&pool).await,
        vec![
            (
                "w1".to_owned(),
                "s_deploy".to_owned(),
                "alice@x.io".to_owned()
            ),
            (
                "w1".to_owned(),
                "s_deploy".to_owned(),
                "bob@x.io".to_owned()
            ),
            (
                "w2".to_owned(),
                "s_deploy".to_owned(),
                "alice@x.io".to_owned()
            ),
        ]
    );

    // The rewritten ADD CONSTRAINT really landed: a mixed-case insert is now a loud violation,
    // never a silent second identity.
    assert!(
        sqlx::query(
            "INSERT INTO probe_roster (workspace_id, skill_id, principal) \
             VALUES ('w1', 's_deploy', 'Zed@X.io')",
        )
        .execute(&pool)
        .await
        .is_err()
    );
}

#[sqlx::test]
async fn member_probe_keeps_the_confirmed_owner_over_the_invited_case_variant(pool: PgPool) {
    create_probe_tables(&pool).await;
    // The invited variant is OLDER (earlier added_at sorts first), yet status is the FIRST sort
    // key: the confirmed owner survives — no genesis workspace is ever orphaned by the dedupe.
    seed_member_row(
        &pool,
        "w1",
        "alice@x.io",
        "member",
        "invited",
        "2026-01-01T00:00:00Z",
    )
    .await;
    seed_member_row(
        &pool,
        "w1",
        "Alice@X.io",
        "owner",
        "confirmed",
        "2026-06-01T00:00:00Z",
    )
    .await;

    run_probe_migration(&pool).await;

    assert_eq!(
        member_rows(&pool, "w1").await,
        vec![(
            "alice@x.io".to_owned(),
            "owner".to_owned(),
            "confirmed".to_owned(),
            "2026-06-01T00:00:00Z".to_owned(),
        )]
    );
}

#[sqlx::test]
async fn member_probe_keeps_the_reviewer_between_two_invited_case_variants(pool: PgPool) {
    create_probe_tables(&pool).await;
    // Equal status (both invited) ⇒ the ROLE rank decides next: the reviewer survives even
    // though the member row is older (added_at would have favored it had role tied).
    seed_member_row(
        &pool,
        "w1",
        "bob@x.io",
        "member",
        "invited",
        "2026-01-01T00:00:00Z",
    )
    .await;
    seed_member_row(
        &pool,
        "w1",
        "Bob@x.io",
        "reviewer",
        "invited",
        "2026-06-01T00:00:00Z",
    )
    .await;

    run_probe_migration(&pool).await;

    assert_eq!(
        member_rows(&pool, "w1").await,
        vec![(
            "bob@x.io".to_owned(),
            "reviewer".to_owned(),
            "invited".to_owned(),
            "2026-06-01T00:00:00Z".to_owned(),
        )]
    );
}

#[sqlx::test]
async fn member_probe_breaks_full_ties_on_added_at_then_principal_order(pool: PgPool) {
    create_probe_tables(&pool).await;
    // w1 — status and role EQUAL (two invited members): `added_at` decides, and the EARLIER seat
    // survives (its added_at is what the folded row carries forward).
    seed_member_row(
        &pool,
        "w1",
        "erin@x.io",
        "member",
        "invited",
        "2026-01-01T00:00:00Z",
    )
    .await;
    seed_member_row(
        &pool,
        "w1",
        "Erin@X.io",
        "member",
        "invited",
        "2026-06-01T00:00:00Z",
    )
    .await;
    // w2 — the FULL tie (status, role, AND added_at all equal): the principal itself is the final,
    // pure-determinism tiebreak, compared under COLLATE "C" like every other principal comparison
    // in the migration — so the BYTE-least variant survives on every database locale
    // ("Frank@X.io" beats "frank@x.io": 0x46 < 0x66). The `invited_by` witness is the only
    // post-fold observable of WHICH variant's seat survived.
    seed_member_row_by(
        &pool,
        "w2",
        "frank@x.io",
        "member",
        "invited",
        "2026-03-01T00:00:00Z",
        "seat-lower",
    )
    .await;
    seed_member_row_by(
        &pool,
        "w2",
        "Frank@X.io",
        "member",
        "invited",
        "2026-03-01T00:00:00Z",
        "seat-upper",
    )
    .await;
    run_probe_migration(&pool).await;

    assert_eq!(
        member_rows(&pool, "w1").await,
        vec![(
            "erin@x.io".to_owned(),
            "member".to_owned(),
            "invited".to_owned(),
            "2026-01-01T00:00:00Z".to_owned(),
        )],
        "equal status + role: the EARLIER added_at survives the dedupe"
    );
    let survivors: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT principal, invited_by FROM probe_workspace_member WHERE workspace_id = 'w2'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        survivors,
        vec![("frank@x.io".to_owned(), Some("seat-upper".to_owned()))],
        "the full tie resolves to exactly ONE folded row — the byte-least principal's seat \
         (COLLATE \"C\": \"Frank@X.io\" < \"frank@x.io\"), locale-independent"
    );
}

#[sqlx::test]
async fn a_no_collision_mixed_case_row_simply_folds_in_place(pool: PgPool) {
    create_probe_tables(&pool).await;
    // No case-variant sibling anywhere: the DELETE dedupe matches nothing and the UPDATE folds
    // the principal bytes in place, every other column untouched.
    seed_member_row(
        &pool,
        "w1",
        "Carol@X.io",
        "member",
        "confirmed",
        "2026-03-01T00:00:00Z",
    )
    .await;
    seed_member_row(
        &pool,
        "w1",
        "dave@x.io",
        "member",
        "invited",
        "2026-03-02T00:00:00Z",
    )
    .await;
    seed_roster_row(&pool, "w1", "s_deploy", "Carol@X.io").await;

    run_probe_migration(&pool).await;

    assert_eq!(
        member_rows(&pool, "w1").await,
        vec![
            (
                "carol@x.io".to_owned(),
                "member".to_owned(),
                "confirmed".to_owned(),
                "2026-03-01T00:00:00Z".to_owned(),
            ),
            (
                "dave@x.io".to_owned(),
                "member".to_owned(),
                "invited".to_owned(),
                "2026-03-02T00:00:00Z".to_owned(),
            ),
        ]
    );
    assert_eq!(
        roster_rows(&pool).await,
        vec![(
            "w1".to_owned(),
            "s_deploy".to_owned(),
            "carol@x.io".to_owned()
        )]
    );
}

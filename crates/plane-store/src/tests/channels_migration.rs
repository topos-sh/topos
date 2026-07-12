//! Migration 0015's BACKFILL + LIFT logic, probed against a SEEDED pre-channels database.
//!
//! The honest caveat, as in `canonical_migration` / `receipts_migration`: `#[sqlx::test]` databases
//! arrive fully migrated, so the pre-channels state (a `current` row still carrying `display_name`,
//! a live per-skill `roster`) cannot be seeded into the real tables — 0015 dropped the column and
//! the table. Each test therefore creates PROBE tables in the PRE-0015 shape (DDL hand-copied from
//! `0001` + `0011`), seeds a realistic and adversarial pre-channels workspace into them, and runs
//! **the backfill statements of `0015_channels.sql` itself** (`include_str!`, table names textually
//! rewritten to the probe names) — so if that SQL ever drifts, these tests re-run the new text.
//!
//! This is the one arm of the increment that executes exactly once against real rows, and can never
//! be re-run after `DROP TABLE roster`. What it must get right: every `current` row becomes a
//! catalog entry whose minted name satisfies the charset + length CHECK (long display names capped,
//! collisions deduped), every workspace is born with the structural `everyone` delivering every
//! published skill, and the interim per-skill `roster` rows become person-scoped direct follows —
//! losslessly for the skills that can be delivered, and dropped for the ones that never published.

use sqlx::PgPool;

/// The migration under probe, verbatim from the file the migrator ran.
const MIGRATION_0015: &str = include_str!("../../migrations/0015_channels.sql");

/// The BACKFILL + LIFT half of 0015 (everything from its banner to the end), with every table name
/// rewritten to its probe twin: comment lines stripped, split on `;`, empties dropped. The DDL and
/// the guarded functions above the banner are NOT re-run — the migrator already created them, and
/// this probe is about the DATA logic.
fn backfill_statements() -> Vec<String> {
    let (_, banner_tail) = MIGRATION_0015
        .split_once("-- BACKFILL + LIFT")
        .expect("0015 carries its backfill banner");
    // Drop the remainder of the banner's own line before the comment filter runs (it is prose, not
    // a `--`-prefixed line of its own).
    let (_, backfill) = banner_tail.split_once('\n').expect("the banner line ends");
    let stripped = backfill
        .lines()
        .filter(|l| !l.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");
    // Longest names first: `channel_skills` before `channels`, `workspace_member` before `workspace`.
    let rewritten = stripped
        .replace("channel_skills", "probe_channel_skills")
        .replace("workspace_member", "probe_workspace_member")
        .replace("skill_follows", "probe_skill_follows")
        .replace("FROM channels", "FROM probe_channels")
        .replace("INTO channels", "INTO probe_channels")
        .replace("INTO catalog", "INTO probe_catalog")
        .replace("FROM catalog", "FROM probe_catalog")
        .replace("JOIN catalog", "JOIN probe_catalog")
        .replace("FROM current", "FROM probe_current")
        .replace("TABLE current", "TABLE probe_current")
        .replace("FROM workspace\n", "FROM probe_workspace\n")
        .replace("FROM roster", "FROM probe_roster")
        .replace("TABLE roster", "TABLE probe_roster");
    rewritten
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The PRE-0015 shapes (hand-copied from `0001` + `0011`), plus the probe twins of the tables 0015
/// creates. The absent name CHECK on `probe_catalog` is deliberate: it lets a BAD mint land so the
/// assertions can catch it, rather than the INSERT failing and hiding which rule was violated.
async fn create_probe_tables(pool: &PgPool) {
    for ddl in [
        // Pre-0015: the pointer row still carries the advisory display name (0011).
        "CREATE TABLE probe_current (
            workspace_id TEXT NOT NULL, skill_id TEXT NOT NULL, commit_id BYTEA NOT NULL,
            epoch BIGINT NOT NULL, seq BIGINT NOT NULL, record BYTEA, updated_at BIGINT NOT NULL,
            display_name TEXT, PRIMARY KEY (workspace_id, skill_id))",
        // Pre-0015: the interim per-skill roster (0001) — membership = a row exists.
        "CREATE TABLE probe_roster (
            workspace_id TEXT NOT NULL, skill_id TEXT NOT NULL, principal TEXT NOT NULL,
            PRIMARY KEY (workspace_id, skill_id, principal))",
        "CREATE TABLE probe_workspace (workspace_id TEXT NOT NULL PRIMARY KEY)",
        "CREATE TABLE probe_workspace_member (
            workspace_id TEXT NOT NULL, principal TEXT NOT NULL, PRIMARY KEY (workspace_id, principal))",
        // The 0015 targets (no name CHECK — see above).
        "CREATE TABLE probe_catalog (
            workspace_id TEXT NOT NULL, skill_id TEXT NOT NULL, name TEXT NOT NULL,
            display_name TEXT, status TEXT NOT NULL, protection TEXT, base_name TEXT,
            archived_at BIGINT, deleted_at BIGINT, created_at TEXT NOT NULL,
            PRIMARY KEY (workspace_id, skill_id))",
        "CREATE UNIQUE INDEX probe_catalog_by_name ON probe_catalog (workspace_id, name)",
        "CREATE TABLE probe_channels (
            workspace_id TEXT NOT NULL, channel_id TEXT NOT NULL, name TEXT NOT NULL,
            mode TEXT NOT NULL, builtin BIGINT NOT NULL, created_by TEXT, created_at TEXT NOT NULL,
            PRIMARY KEY (workspace_id, channel_id))",
        "CREATE TABLE probe_channel_skills (
            workspace_id TEXT NOT NULL, channel_id TEXT NOT NULL, skill_id TEXT NOT NULL,
            added_by TEXT NOT NULL, added_at TEXT NOT NULL,
            PRIMARY KEY (workspace_id, channel_id, skill_id))",
        "CREATE TABLE probe_skill_follows (
            workspace_id TEXT NOT NULL, principal TEXT NOT NULL, skill_id TEXT NOT NULL,
            created_at TEXT NOT NULL, PRIMARY KEY (workspace_id, principal, skill_id))",
    ] {
        sqlx::query(ddl)
            .execute(pool)
            .await
            .expect("probe DDL applies");
    }
}

/// Seed a published skill (a `current` row, pre-0015 shape) with an optional advisory display name.
async fn seed_current(pool: &PgPool, ws: &str, skill: &str, display_name: Option<&str>) {
    sqlx::query(
        "INSERT INTO probe_current \
           (workspace_id, skill_id, commit_id, epoch, seq, updated_at, display_name) \
         VALUES ($1, $2, decode(md5($2), 'hex'), 1, 1, 1000, $3)",
    )
    .bind(ws)
    .bind(skill)
    .bind(display_name)
    .execute(pool)
    .await
    .expect("seed current");
}

async fn seed_roster(pool: &PgPool, ws: &str, skill: &str, principal: &str) {
    sqlx::query("INSERT INTO probe_roster (workspace_id, skill_id, principal) VALUES ($1, $2, $3)")
        .bind(ws)
        .bind(skill)
        .bind(principal)
        .execute(pool)
        .await
        .expect("seed roster");
}

async fn run_backfill(pool: &PgPool) {
    for stmt in backfill_statements() {
        sqlx::query(&stmt)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("backfill statement failed: {e}\n---\n{stmt}"));
    }
}

/// One row of the probed catalog.
async fn catalog_name(pool: &PgPool, ws: &str, skill: &str) -> String {
    sqlx::query_scalar::<_, String>(
        "SELECT name FROM probe_catalog WHERE workspace_id = $1 AND skill_id = $2",
    )
    .bind(ws)
    .bind(skill)
    .fetch_one(pool)
    .await
    .expect("a catalog row per published skill")
}

/// EVERY published skill gets a catalog entry whose minted name satisfies the real table's CHECK —
/// including the two shapes that would otherwise break the upgrade: an over-long display name (the
/// real `catalog.name` CHECK caps at 200; the mint caps at the 64-char birth limit) and two skills
/// whose names fold to the SAME candidate (the unique index would reject the duplicate).
#[sqlx::test]
async fn the_backfill_mints_a_legal_name_for_every_published_skill(pool: PgPool) {
    create_probe_tables(&pool).await;
    seed_current(&pool, "w_acme", "topos_deploy", Some("Deploy Guide")).await;
    // Two DIFFERENT skills whose display names fold to the same candidate — the collision the
    // dedupe suffix exists for.
    seed_current(&pool, "w_acme", "topos_aaa", Some("PR Review")).await;
    seed_current(&pool, "w_acme", "topos_bbb", Some("pr!!!review")).await;
    // Pathological: 400 characters of display name, and one with nothing usable in it at all.
    seed_current(
        &pool,
        "w_acme",
        "topos_long",
        Some(&"Very Long Name ".repeat(40)),
    )
    .await;
    seed_current(&pool, "w_acme", "topos_junk", Some("!!!")).await;
    // No display name at all (a publish that carried none) — falls back to the skill id fold.
    seed_current(&pool, "w_acme", "topos_bare", None).await;

    run_backfill(&pool).await;

    assert_eq!(
        catalog_name(&pool, "w_acme", "topos_deploy").await,
        "deploy-guide"
    );
    // The collision: first by skill_id order keeps the bare name, the second is suffixed.
    let a = catalog_name(&pool, "w_acme", "topos_aaa").await;
    let b = catalog_name(&pool, "w_acme", "topos_bbb").await;
    assert_eq!(a, "pr-review");
    assert_eq!(
        b, "pr-review-2",
        "a folded-name collision dedupes deterministically"
    );
    // The over-long name is capped inside the birth limit and carries no trailing hyphen (which the
    // real charset CHECK would reject).
    let long = catalog_name(&pool, "w_acme", "topos_long").await;
    assert!(
        long.len() <= 64,
        "a long display name is capped: {} chars",
        long.len()
    );
    assert!(
        !long.ends_with('-'),
        "the cap never leaves a trailing hyphen: {long:?}"
    );
    // Junk folds away entirely → the skill-id fallback.
    assert_eq!(
        catalog_name(&pool, "w_acme", "topos_junk").await,
        "topos-junk"
    );
    assert_eq!(
        catalog_name(&pool, "w_acme", "topos_bare").await,
        "topos-bare"
    );

    // EVERY minted name satisfies the REAL table's constraints (charset + length) — the property
    // that decides whether migration 0015 applies or aborts on a seeded database.
    let bad: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM probe_catalog \
         WHERE name !~ '^[a-z0-9][a-z0-9-]*$' OR length(name) > 200",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        bad, 0,
        "every backfilled name satisfies catalog.name's CHECK"
    );
}

/// Every workspace is born (retroactively) with the structural `everyone`, delivering every skill
/// that had published — so an existing follower keeps receiving exactly what the per-skill roster
/// used to deliver, now through the union.
#[sqlx::test]
async fn the_backfill_seats_everyone_and_places_every_published_skill(pool: PgPool) {
    create_probe_tables(&pool).await;
    // Three workspaces, each known through a DIFFERENT table — the backfill's union of sources.
    sqlx::query("INSERT INTO probe_workspace (workspace_id) VALUES ('w_standalone')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO probe_workspace_member (workspace_id, principal) VALUES ('w_seated', 'alice@x.io')")
        .execute(&pool)
        .await
        .unwrap();
    seed_current(&pool, "w_published", "topos_deploy", Some("Deploy")).await;
    seed_roster(&pool, "w_rostered", "topos_ghost", "bob@x.io").await;

    run_backfill(&pool).await;

    for ws in ["w_standalone", "w_seated", "w_published", "w_rostered"] {
        let builtin: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM probe_channels \
             WHERE workspace_id = $1 AND channel_id = 'everyone' AND builtin = 1",
        )
        .bind(ws)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(builtin, 1, "{ws} is born with the structural everyone");
    }
    // The one published skill is placed in its workspace's everyone.
    let placed: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM probe_channel_skills \
         WHERE workspace_id = 'w_published' AND channel_id = 'everyone' AND skill_id = 'topos_deploy'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(placed, 1, "every published skill lands in everyone");
}

/// THE INC-2 → INC-3 HANDOFF: the interim per-skill `roster` rows become person-scoped DIRECT
/// follows — losslessly for every skill that can actually be delivered (a catalog entry exists), and
/// dropped for a roster row naming a skill that never published (no `current`, so nothing to
/// deliver; the row carried no deliverable state). Then the table is dropped, and this data can
/// never be re-derived.
#[sqlx::test]
async fn the_roster_rows_lift_into_person_scoped_follows_and_the_table_is_dropped(pool: PgPool) {
    create_probe_tables(&pool).await;
    seed_current(&pool, "w_acme", "topos_deploy", Some("Deploy")).await;
    seed_current(&pool, "w_acme", "topos_review", Some("Review")).await;
    // Two people on two published skills…
    seed_roster(&pool, "w_acme", "topos_deploy", "alice@x.io").await;
    seed_roster(&pool, "w_acme", "topos_deploy", "bob@x.io").await;
    seed_roster(&pool, "w_acme", "topos_review", "alice@x.io").await;
    // …and one roster row for a skill that NEVER published (no `current` ⇒ no catalog row).
    seed_roster(&pool, "w_acme", "topos_never", "carol@x.io").await;

    run_backfill(&pool).await;

    let mut follows: Vec<(String, String)> = sqlx::query_as(
        "SELECT principal, skill_id FROM probe_skill_follows WHERE workspace_id = 'w_acme' \
         ORDER BY principal, skill_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    follows.sort();
    assert_eq!(
        follows,
        vec![
            ("alice@x.io".to_owned(), "topos_deploy".to_owned()),
            ("alice@x.io".to_owned(), "topos_review".to_owned()),
            ("bob@x.io".to_owned(), "topos_deploy".to_owned()),
        ],
        "every deliverable roster row lifts to a direct follow; the current-less one does not"
    );

    // The lift is the LAST read of the table: 0015 drops it in the same migration.
    assert!(
        MIGRATION_0015.contains("DROP TABLE roster"),
        "0015 drops the per-skill roster after lifting it"
    );
    // …and the advisory display name moves off the pointer row in the same breath.
    assert!(
        MIGRATION_0015.contains("ALTER TABLE current DROP COLUMN display_name"),
        "0015 retires current.display_name once the catalog absorbs it"
    );
}

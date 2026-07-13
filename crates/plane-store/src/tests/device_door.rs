//! The device door's database half: the `topos_web` GRANT SHAPE (migration 0019 carries the
//! grants next to the schema they bound) and the `topos_device_actor` front-door resolver.
//!
//! The grant assertions are the row/byte rule's teeth, asserted at COLUMN grain — not just "which
//! tables", but exactly which columns the web role can UPDATE. A table-wide UPDATE on
//! `device_registry` would let a compromised web tier rewrite a device's credential hash and then
//! drive the device lane as that device; the positive list below is small on purpose and every
//! probe beyond it must refuse 42501. Production provisioning inherits this shape by running the
//! same migrations with the role pre-created — this test IS the in-repo record of the shape.

use sqlx::PgPool;

use crate::MIGRATOR;

/// Create the web role cluster-wide (idempotent, race-safe — roles outlive the per-test databases)
/// BEFORE the migrator runs, so 0019's role-guarded grant block executes rather than skips. LOGIN +
/// password match the e2e bootstrap's shape so a shared dev cluster stays usable by both suites.
async fn ensure_web_role(pool: &PgPool) {
    sqlx::raw_sql(
        r#"DO $$
        BEGIN
            IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'topos_web') THEN
                CREATE ROLE topos_web LOGIN PASSWORD 'web';
            END IF;
        EXCEPTION WHEN duplicate_object THEN
            NULL; -- a parallel test won the race; the role exists either way
        END $$"#,
    )
    .execute(pool)
    .await
    .expect("ensure topos_web role");
}

async fn migrate_with_role(pool: &PgPool) {
    ensure_web_role(pool).await;
    MIGRATOR.run(pool).await.expect("run migrations");
}

/// The exact column-grain UPDATE map — every `(table, column)` the web role may write, and
/// NOTHING else. Additions here must land with the guarded function that writes the column.
const EXPECTED_UPDATE_COLUMNS: &[(&str, &str)] = &[
    ("catalog", "protection"),
    ("channels", "mode"),
    ("channels", "name"),
    ("device_registry", "last_report_at"),
    ("device_registry", "revoked"),
    ("device_skill_state", "applied_commit"),
    ("device_skill_state", "detached"),
    ("device_skill_state", "detached_at"),
    ("device_skill_state", "reported_at"),
    ("notices", "acked_at"),
    ("workspace_member", "invited_by"),
    ("workspace_member", "role"),
    ("workspace_policy", "invite_policy"),
    ("workspace_policy", "review_required"),
    ("workspace_policy", "staleness_window_ms"),
];

const EXPECTED_INSERT_TABLES: &[&str] = &[
    "channel_events",
    "channel_members",
    "channel_skills",
    "channels",
    "device_exclusions",
    "device_skill_state",
    "skill_detachments",
    "skill_follows",
    "skill_unfollows",
    "workspace_member",
    "workspace_policy",
];

const EXPECTED_DELETE_TABLES: &[&str] = &[
    "channel_members",
    "channel_skills",
    "channels",
    "device_exclusions",
    "device_skill_state",
    "skill_detachments",
    "skill_follows",
    "skill_unfollows",
    "workspace_member",
];

/// The grant shape, asserted exactly: the three DML families equal their expected sets, and no
/// table-wide UPDATE exists at all (a table-privilege UPDATE row would mean every column).
#[sqlx::test(migrations = false)]
async fn web_role_grants_are_exactly_the_column_grain_shape(pool: PgPool) {
    migrate_with_role(&pool).await;

    let update_columns: Vec<(String, String)> = sqlx::query_as(
        "SELECT table_name::text, column_name::text
         FROM information_schema.column_privileges
         WHERE grantee = 'topos_web' AND privilege_type = 'UPDATE'
           AND table_schema = current_schema()
         ORDER BY table_name, column_name",
    )
    .fetch_all(&pool)
    .await
    .expect("read column privileges");
    let expected: Vec<(String, String)> = EXPECTED_UPDATE_COLUMNS
        .iter()
        .map(|(t, c)| ((*t).to_owned(), (*c).to_owned()))
        .collect();
    assert_eq!(
        update_columns, expected,
        "the topos_web UPDATE grant must be exactly the column-grain map"
    );

    let table_updates: Vec<String> = sqlx::query_scalar(
        "SELECT table_name::text FROM information_schema.table_privileges
         WHERE grantee = 'topos_web' AND privilege_type = 'UPDATE'
           AND table_schema = current_schema()",
    )
    .fetch_all(&pool)
    .await
    .expect("read table privileges");
    assert!(
        table_updates.is_empty(),
        "no TABLE-wide UPDATE may exist for topos_web (found {table_updates:?})"
    );

    for (privilege, expected_tables) in [
        ("INSERT", EXPECTED_INSERT_TABLES),
        ("DELETE", EXPECTED_DELETE_TABLES),
    ] {
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT table_name::text FROM information_schema.table_privileges
             WHERE grantee = 'topos_web' AND privilege_type = $1
               AND table_schema = current_schema()
             ORDER BY table_name",
        )
        .bind(privilege)
        .fetch_all(&pool)
        .await
        .expect("read table privileges");
        let expected: Vec<String> = expected_tables.iter().map(|t| (*t).to_owned()).collect();
        assert_eq!(tables, expected, "{privilege} table set for topos_web");
    }
}

/// The blanket `GRANT EXECUTE ON ALL FUNCTIONS` (and the matching default-privileges edge) is safe
/// ONLY because every guarded `topos_*` function runs SECURITY INVOKER — a function body then
/// executes with the CALLER's privileges, so the column-grain grants above are the whole
/// enforcement and no function can amplify what `topos_web` may write. A future SECURITY DEFINER
/// function would run as its (privileged) OWNER, handing `topos_web` that owner's reach the instant
/// it becomes executable — bypassing the grant discipline wholesale. This asserts the invariant the
/// blanket grant rests on: NO SECURITY DEFINER function is executable by `topos_web`.
#[sqlx::test(migrations = false)]
async fn web_role_can_execute_no_security_definer_function(pool: PgPool) {
    migrate_with_role(&pool).await;
    let leaks: Vec<String> = sqlx::query_scalar(
        "SELECT p.proname::text
         FROM pg_proc p
         JOIN pg_namespace n ON n.oid = p.pronamespace
         WHERE n.nspname = current_schema()
           AND p.prosecdef
           AND has_function_privilege('topos_web', p.oid, 'EXECUTE')
         ORDER BY p.proname",
    )
    .fetch_all(&pool)
    .await
    .expect("read security-definer functions executable by topos_web");
    assert!(
        leaks.is_empty(),
        "topos_web may execute NO security-definer function — the blanket EXECUTE grant rests on \
         every guarded function being SECURITY INVOKER; these break it: {leaks:?}"
    );
}

/// The teeth, probed live: SET ROLE topos_web and try to cross the line. Every probe beyond the
/// granted shape must refuse 42501 (insufficient_privilege); the granted shape must work.
#[sqlx::test(migrations = false)]
async fn web_role_cannot_cross_the_row_byte_line(pool: PgPool) {
    migrate_with_role(&pool).await;
    // Seed a REVOKED device as the superuser (before the role switch) — the un-revoke probe below
    // needs an already-revoked row to try to flip.
    sqlx::raw_sql(
        "INSERT INTO device_registry (workspace_id, device_key_id, public_key, principal, credential_sha256, revoked)
         VALUES ('wr', 'dk_dead', decode(repeat('11', 32), 'hex'), 'a@x.com', sha256(convert_to('c', 'UTF8')), 1)",
    )
    .execute(&pool)
    .await
    .expect("seed a revoked device");

    let mut conn = pool.acquire().await.expect("acquire");
    sqlx::raw_sql("SET ROLE topos_web")
        .execute(&mut *conn)
        .await
        .expect("set role");

    // The escalation the column grain exists to prevent: rewriting a device's credential hash
    // would let this tier mint itself INTO the device lane.
    for denied in [
        "UPDATE device_registry SET credential_sha256 = NULL",
        "UPDATE device_registry SET principal = 'x@y.z'",
        "UPDATE device_registry SET public_key = '\\x00'::bytea",
        // The pointer move is the vault's alone — no UPDATE on `current` at all.
        "UPDATE current SET epoch = 99",
        // Receipts are the vault's durable trust artifacts — not writable from this tier.
        "INSERT INTO op_receipts (workspace_id, actor, op_id) VALUES ('w','d','o')",
        // Catalog identity is written by the vault's registering publish / the rename ceremony's
        // guarded fn — the web role's own reach on catalog is the protection column only.
        "UPDATE catalog SET name = 'stolen'",
        "DELETE FROM catalog",
        "INSERT INTO skill_commit (workspace_id, commit_id, skill_id) VALUES ('w', '\\x00'::bytea, 's')",
    ] {
        let err = sqlx::raw_sql(denied)
            .execute(&mut *conn)
            .await
            .expect_err(&format!("must refuse: {denied}"));
        let code = err
            .as_database_error()
            .and_then(|d| d.code().map(|c| c.to_string()));
        assert_eq!(
            code.as_deref(),
            Some("42501"),
            "expected insufficient_privilege for: {denied}"
        );
    }

    // Revocation is ONE-WAY below the grant: the web role HOLDS UPDATE(revoked) (for the revoke
    // ceremony), but the monotonic trigger refuses a 1→0 flip, so a compromised web tier cannot
    // resurrect the revoked device seeded above.
    let unrevoke = sqlx::raw_sql(
        "UPDATE device_registry SET revoked = 0 WHERE workspace_id = 'wr' AND device_key_id = 'dk_dead'",
    )
    .execute(&mut *conn)
    .await
    .expect_err("un-revoke must be refused by the monotonic trigger");
    // A trigger RAISE is P0001 (raise_exception), NOT 42501 — the grant allows the write, the
    // trigger blocks the value transition.
    assert_eq!(
        unrevoke
            .as_database_error()
            .and_then(|d| d.code().map(|c| c.to_string()))
            .as_deref(),
        Some("P0001"),
        "un-revoke must raise the monotonic trigger's exception"
    );

    // The granted shape works: broad SELECT, the column-grain UPDATE, EXECUTE on the guarded fns.
    sqlx::raw_sql("SELECT * FROM op_receipts LIMIT 0")
        .execute(&mut *conn)
        .await
        .expect("broad SELECT");
    sqlx::raw_sql("UPDATE device_registry SET last_report_at = 1 WHERE workspace_id = 'none'")
        .execute(&mut *conn)
        .await
        .expect("granted column UPDATE");
    let window: i64 = sqlx::query_scalar("SELECT topos_staleness_window('w-none')")
        .fetch_one(&mut *conn)
        .await
        .expect("EXECUTE on a guarded function");
    assert_eq!(window, 604_800_000, "the default staleness window");
}

/// The front-door resolver: one row for a live credential on a confirmed seat; the same empty
/// answer for a wrong credential, a revoked device, an unconfirmed seat, and a foreign workspace.
#[sqlx::test(migrations = false)]
async fn device_actor_resolves_only_a_live_confirmed_credential(pool: PgPool) {
    migrate_with_role(&pool).await;
    sqlx::raw_sql(
        r#"
        INSERT INTO workspace_member (workspace_id, principal, role, status, added_at)
        VALUES ('w1', 'a@x.com', 'member', 'confirmed', '2026-07-12T00:00:00Z'),
               ('w1', 'b@x.com', 'reviewer', 'invited', '2026-07-12T00:00:00Z');
        INSERT INTO device_registry (workspace_id, device_key_id, public_key, principal, credential_sha256, revoked)
        VALUES ('w1', 'dk_a', decode(repeat('ab', 32), 'hex'), 'a@x.com', sha256(convert_to('cred-a', 'UTF8')), 0),
               ('w1', 'dk_b', decode(repeat('cd', 32), 'hex'), 'b@x.com', sha256(convert_to('cred-b', 'UTF8')), 0),
               ('w1', 'dk_r', decode(repeat('ef', 32), 'hex'), 'a@x.com', sha256(convert_to('cred-r', 'UTF8')), 1);
        "#,
    )
    .execute(&pool)
    .await
    .expect("seed");

    let hit: Option<(String, String, String)> = sqlx::query_as(
        "SELECT person, device_key_id, role FROM topos_device_actor('w1', 'cred-a')",
    )
    .fetch_optional(&pool)
    .await
    .expect("probe");
    assert_eq!(
        hit,
        Some(("a@x.com".into(), "dk_a".into(), "member".into())),
        "a live credential on a confirmed seat resolves"
    );

    for (ws, cred, why) in [
        ("w1", "cred-wrong", "an unknown credential"),
        ("w1", "cred-r", "a revoked device"),
        ("w1", "cred-b", "an unconfirmed (invited) seat"),
        ("w2", "cred-a", "a foreign workspace"),
        ("w1", "", "a blank credential"),
    ] {
        let miss: Option<(String, String, String)> =
            sqlx::query_as("SELECT person, device_key_id, role FROM topos_device_actor($1, $2)")
                .bind(ws)
                .bind(cred)
                .fetch_optional(&pool)
                .await
                .expect("probe");
        assert!(miss.is_none(), "{why} must resolve to the empty set");
    }
}

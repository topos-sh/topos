//! The op_receipts migration chain (0012 widening + 0013 de-crypto), probed against MIRROR tables.
//!
//! An honest caveat up front: `#[sqlx::test]` databases arrive with the WHOLE migration set applied —
//! 0012's rename/new-columns/CHECKs AND 0013's de-crypto (signed_record → record, DROP key_id, method
//! 'device_signed' → 'device') are already live on the real `op_receipts` — so the pre-migration state
//! (rows keyed `device_key_id`, no `method`) can no longer be seeded there. Each test therefore creates
//! PROBE tables whose DDL is hand-copied from migrations `0003` (`op_receipts`) and `0004` (`proposals`)
//! — the pre-0012 shapes — seeds device-era receipt rows, then executes the statements of
//! `migrations/0012_web_review.sql` AND the op_receipts statements of `migrations/0013_decrypto.sql`
//! ITSELF (`include_str!`, table names textually rewritten to the probe names, split on `;`), so the probe
//! reaches the FINAL post-all-migrations receipt shape. If the migration files' SQL ever drifts, these
//! tests re-run the new text verbatim (the `canonical_migration` pattern).

use sqlx::PgPool;

/// The 0012 migration under probe, verbatim from the file the migrator ran.
const MIGRATION_0012: &str = include_str!("../../migrations/0012_web_review.sql");

/// The 0013 de-crypto migration under probe, verbatim from the file the migrator ran.
const MIGRATION_0013: &str = include_str!("../../migrations/0013_decrypto.sql");

/// One migrated probe row, as the backfill assertion reads it:
/// `(workspace_id, actor, op_id, method, request_sha256, step_up_attestation, outcome)`.
type MigratedReceiptRow = (
    String,
    String,
    String,
    String,
    Option<Vec<u8>>,
    Option<String>,
    String,
);

/// The 0012 statements with `op_receipts` / `proposals` rewritten to the probe tables: comment
/// lines stripped, split on `;`, empties dropped. The rewrite also renames the index
/// (`op_receipts_by_ws_op` → `probe_op_receipts_by_ws_op` — index names are schema-global, and the
/// real one already exists), so the WHOLE script is proven to execute in order.
fn probe_statements() -> Vec<String> {
    let rewritten = MIGRATION_0012
        .lines()
        .filter(|l| !l.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n")
        .replace("op_receipts", "probe_op_receipts")
        .replace("proposals", "probe_proposals");
    rewritten
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Create the probe tables in the PRE-0012 shape. `probe_op_receipts` is hand-copied from `0003`
/// (the `device_key_id`-keyed original — exactly what lets the device-era rows in); `probe_proposals`
/// is hand-copied from `0004` (no `resolved_reason` / `resolved_at`), its `skill_commit` foreign key
/// dropped (orthogonal to 0012's ADD COLUMNs, and the probe seeds no provenance).
async fn create_probe_tables(pool: &PgPool) {
    sqlx::query(
        "CREATE TABLE probe_op_receipts (
            workspace_id   TEXT   NOT NULL,
            device_key_id  TEXT   NOT NULL,
            op_id          TEXT   NOT NULL,
            command        TEXT   NOT NULL,
            skill_id       TEXT   NOT NULL,
            commit_id      BYTEA           CHECK (commit_id IS NULL OR octet_length(commit_id) = 32),
            bundle_digest  BYTEA           CHECK (bundle_digest IS NULL OR octet_length(bundle_digest) = 32),
            expected_epoch BIGINT NOT NULL CHECK (expected_epoch >= 0 AND expected_epoch <= 9007199254740991),
            expected_seq   BIGINT NOT NULL CHECK (expected_seq   >= 0 AND expected_seq   <= 9007199254740991),
            outcome        TEXT   NOT NULL CHECK (outcome IN (
                               'OK', 'APPROVAL_REQUIRED', 'NEEDS_REVIEW', 'CONFLICT', 'DIVERGED', 'DENIED',
                               'UNAVAILABLE', 'AMBIGUOUS_NAME', 'KEY_REPIN_REQUIRED', 'RETRYABLE_FAILURE',
                               'PERMANENT_FAILURE')),
            current_epoch  BIGINT          CHECK (current_epoch IS NULL OR (current_epoch >= 0 AND current_epoch <= 9007199254740991)),
            current_seq    BIGINT          CHECK (current_seq   IS NULL OR (current_seq   >= 0 AND current_seq   <= 9007199254740991)),
            signed_record  BYTEA,
            key_id         TEXT,
            created_at     TEXT   NOT NULL,
            details        TEXT,
            PRIMARY KEY (workspace_id, device_key_id, op_id)
        )",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE probe_proposals (
            workspace_id   TEXT   NOT NULL,
            id             TEXT   NOT NULL,
            skill_id       TEXT   NOT NULL,
            commit_id      BYTEA  NOT NULL CHECK (octet_length(commit_id) = 32),
            base_commit_id BYTEA  NOT NULL CHECK (octet_length(base_commit_id) = 32),
            base_epoch     BIGINT NOT NULL CHECK (base_epoch >= 0 AND base_epoch <= 9007199254740991),
            base_seq       BIGINT NOT NULL CHECK (base_seq   >= 0 AND base_seq   <= 9007199254740991),
            status         TEXT   NOT NULL CHECK (status IN ('open', 'accepted', 'rejected')),
            proposer       TEXT   NOT NULL,
            resolved_by    TEXT,
            created_at     TEXT   NOT NULL,
            PRIMARY KEY (workspace_id, id)
        )",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// Execute every rewritten 0012 statement in file order, panicking on the first failure.
async fn run_probe_migration(pool: &PgPool) {
    for stmt in probe_statements() {
        sqlx::query(&stmt)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("0012 probe statement failed: {e}\n{stmt}"));
    }
}

/// The 0013 statements that touch `op_receipts`, rewritten to `probe_op_receipts`. 0013 also rewrites
/// `current` + `workspace_events` (tables the probe does not create), so every statement NOT naming
/// `op_receipts` is dropped — leaving exactly the receipt de-crypto (signed_record → record, strip the
/// legacy signed-envelope json field, DROP key_id, and rename method 'device_signed' → 'device' behind a
/// re-added CHECK).
fn probe_statements_0013() -> Vec<String> {
    let rewritten = MIGRATION_0013
        .lines()
        .filter(|l| !l.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n")
        .replace("op_receipts", "probe_op_receipts");
    rewritten
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter(|s| s.contains("probe_op_receipts"))
        .map(str::to_owned)
        .collect()
}

/// Execute the rewritten 0013 op_receipts statements in file order (after `run_probe_migration`), so the
/// probe carries the final receipt shape.
async fn run_probe_migration_0013(pool: &PgPool) {
    for stmt in probe_statements_0013() {
        sqlx::query(&stmt)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("0013 probe statement failed: {e}\n{stmt}"));
    }
}

/// Seed a device-era receipt row in the pre-0012 shape (the minimal NOT NULL set + the outcome).
async fn seed_device_receipt(
    pool: &PgPool,
    ws: &str,
    device_key_id: &str,
    op_id: &str,
    command: &str,
    outcome: &str,
    created_at: &str,
) {
    sqlx::query(
        "INSERT INTO probe_op_receipts \
           (workspace_id, device_key_id, op_id, command, skill_id, commit_id, bundle_digest, \
            expected_epoch, expected_seq, outcome, created_at) \
         VALUES ($1, $2, $3, $4, 's_deploy', $5, $6, 1, 1, $7, $8)",
    )
    .bind(ws)
    .bind(device_key_id)
    .bind(op_id)
    .bind(command)
    .bind(vec![0xC1u8; 32])
    .bind(vec![0xD1u8; 32])
    .bind(outcome)
    .bind(created_at)
    .execute(pool)
    .await
    .unwrap();
}

#[sqlx::test]
async fn the_probe_renames_the_actor_column_and_backfills_every_device_row(pool: PgPool) {
    create_probe_tables(&pool).await;
    // Three device-era rows: two devices in one workspace, plus the SAME (device, op id) tuple in
    // ANOTHER workspace — the rename must carry the whole PK tuple across untouched.
    seed_device_receipt(
        &pool,
        "w1",
        "dk_alpha",
        "op-1",
        "publish-direct",
        "OK",
        "t1",
    )
    .await;
    seed_device_receipt(
        &pool,
        "w1",
        "dk_beta",
        "op-2",
        "review-reject",
        "DENIED",
        "t2",
    )
    .await;
    seed_device_receipt(
        &pool,
        "w2",
        "dk_alpha",
        "op-1",
        "publish-propose",
        "NEEDS_REVIEW",
        "t3",
    )
    .await;

    run_probe_migration(&pool).await;
    run_probe_migration_0013(&pool).await;

    // The rename preserved every row byte-for-byte under the new `actor` name; 0012's DEFAULT backfilled
    // 'device_signed', then 0013 renamed it to 'device'; the new nullable columns arrived NULL.
    let rows: Vec<MigratedReceiptRow> = sqlx::query_as(
        "SELECT workspace_id, actor, op_id, method, request_sha256, step_up_attestation, outcome \
             FROM probe_op_receipts ORDER BY workspace_id, actor, op_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 3);
    let facts: Vec<(&str, &str, &str, &str)> = rows
        .iter()
        .map(|(ws, actor, op, method, sha, step_up, outcome)| {
            assert_eq!(
                method, "device",
                "0013 renamed the backfilled 'device_signed' to 'device'"
            );
            assert!(sha.is_none(), "device-era rows carry no request identity");
            assert!(step_up.is_none(), "the step-up slot is schema-only");
            (ws.as_str(), actor.as_str(), op.as_str(), outcome.as_str())
        })
        .collect();
    assert_eq!(
        facts,
        vec![
            ("w1", "dk_alpha", "op-1", "OK"),
            ("w1", "dk_beta", "op-2", "DENIED"),
            ("w2", "dk_alpha", "op-1", "NEEDS_REVIEW"),
        ]
    );

    // 0013 renamed signed_record → record and dropped key_id (the final receipt columns).
    let cols: Vec<String> = sqlx::query_scalar(
        "SELECT column_name FROM information_schema.columns WHERE table_name = 'probe_op_receipts'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(
        cols.iter().any(|c| c == "record"),
        "signed_record renamed to record"
    );
    assert!(
        !cols.iter().any(|c| c == "signed_record"),
        "signed_record is gone"
    );
    assert!(!cols.iter().any(|c| c == "key_id"), "key_id is dropped");

    // The (workspace, op id) replay-probe index landed on the probe — and the REAL one exists on the
    // real table (the applied migration, probed through pg_indexes).
    for (table, index) in [
        ("probe_op_receipts", "probe_op_receipts_by_ws_op"),
        ("op_receipts", "op_receipts_by_ws_op"),
    ] {
        let n = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM pg_indexes WHERE tablename = $1 AND indexname = $2",
        )
        .bind(table)
        .bind(index)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(n, 1, "missing index {index} on {table}");
    }

    // The proposals side of the script ran too: the resolution columns exist on the probe.
    let cols = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM information_schema.columns \
         WHERE table_name = 'probe_proposals' AND column_name IN ('resolved_reason', 'resolved_at')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(cols, 2);
}

#[sqlx::test]
async fn the_widened_receipt_columns_enforce_their_checks(pool: PgPool) {
    create_probe_tables(&pool).await;
    run_probe_migration(&pool).await;
    run_probe_migration_0013(&pool).await;

    let insert = |method: Option<&str>, sha: Option<Vec<u8>>, op_id: &str| {
        let pool = pool.clone();
        let method = method.map(str::to_owned);
        let op_id = op_id.to_owned();
        async move {
            // `method` binds only when given — the omitting variant exercises the kept DEFAULT (the
            // raw-SQL writers that predate the lane column rely on it).
            let q = match &method {
                Some(m) => sqlx::query(
                    "INSERT INTO probe_op_receipts \
                       (workspace_id, actor, op_id, method, request_sha256, command, skill_id, \
                        expected_epoch, expected_seq, outcome, created_at) \
                     VALUES ('w1', 'a1', $1, $2, $3, 'review-approve', 's_deploy', 1, 1, 'OK', 't1')",
                )
                .bind(op_id)
                .bind(m.clone())
                .bind(sha),
                None => sqlx::query(
                    "INSERT INTO probe_op_receipts \
                       (workspace_id, actor, op_id, request_sha256, command, skill_id, \
                        expected_epoch, expected_seq, outcome, created_at) \
                     VALUES ('w1', 'a1', $1, $2, 'review-approve', 's_deploy', 1, 1, 'OK', 't1')",
                )
                .bind(op_id)
                .bind(sha),
            };
            q.execute(&pool).await
        }
    };

    // A method outside the two-lane vocabulary is a loud violation, never a third silent lane.
    assert!(
        insert(Some("carrier_pigeon"), None, "op-bad-method")
            .await
            .is_err()
    );
    // 0013 tightened the vocabulary: the pre-0013 'device_signed' spelling is no longer accepted.
    assert!(
        insert(Some("device_signed"), None, "op-old-method")
            .await
            .is_err()
    );
    // A request identity that is not exactly 32 bytes violates its width CHECK.
    assert!(
        insert(Some("web_session"), Some(vec![0u8; 33]), "op-bad-sha")
            .await
            .is_err()
    );
    // The positive controls: a well-formed session row and an explicit device row both land, and an
    // insert that OMITS `method` still lands as a device row (the kept DEFAULT, now 'device').
    assert!(
        insert(Some("web_session"), Some(vec![0u8; 32]), "op-good")
            .await
            .is_ok()
    );
    assert!(insert(Some("device"), None, "op-device").await.is_ok());
    assert!(insert(None, None, "op-default").await.is_ok());
    let defaulted = sqlx::query_scalar::<_, String>(
        "SELECT method FROM probe_op_receipts WHERE workspace_id = 'w1' AND op_id = 'op-default'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(defaulted, "device");
}

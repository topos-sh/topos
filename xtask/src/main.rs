//! `xtask` — the one codegen + conformance entrypoint.
//!
//! `cargo xtask gen-schema`          → (re)generate `contracts/schemas/*.schema.json` from topos-types.
//! `cargo xtask gen-schema --check`  → the CI drift gate (fails if a committed schema is stale).
//! `cargo xtask conformance`         → the store matrices.
//!
//! The plane OpenAPI generation (utoipa) is wired here once `topos-plane` exposes its
//! annotated routes. No `model` subcommand — formal verification is out of v0.

use anyhow::{Context, Result, bail};
use std::{
    env, fs,
    path::{Path, PathBuf},
};

/// The committed JSON-Schema artifacts (the per-loop L1 oracle). One entry per top-level wire type.
fn schemas() -> Vec<(&'static str, String)> {
    vec![
        ("json-envelope", emit(schemars::schema_for!(topos_types::JsonEnvelope))),
        ("receipt", emit(schemars::schema_for!(topos_types::Receipt))),
        ("wire-error", emit(schemars::schema_for!(topos_types::WireError))),
        (
            "signed-current-record",
            emit(schemars::schema_for!(topos_types::SignedCurrentRecord)),
        ),
        ("next-action", emit(schemars::schema_for!(topos_types::NextAction))),
        ("trigger-report", emit(schemars::schema_for!(topos_types::TriggerReport))),
    ]
}

fn emit(schema: schemars::schema::RootSchema) -> String {
    let mut s = serde_json::to_string_pretty(&schema).expect("a schema always serializes");
    s.push('\n');
    s
}

fn schemas_dir() -> PathBuf {
    // xtask lives at <workspace-root>/xtask, so its manifest dir's parent is the root.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask has a parent dir")
        .join("contracts/schemas")
}

fn gen_schema(check: bool) -> Result<()> {
    let dir = schemas_dir();
    if !check {
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut drift = Vec::new();
    for (name, content) in schemas() {
        let path = dir.join(format!("{name}.schema.json"));
        if check {
            let existing = fs::read_to_string(&path).unwrap_or_default();
            if existing != content {
                drift.push(name);
            }
        } else {
            fs::write(&path, &content).with_context(|| format!("writing {}", path.display()))?;
            println!("wrote {}", path.display());
        }
    }
    if check {
        if drift.is_empty() {
            println!("schemas up to date");
        } else {
            bail!(
                "schema drift in: {} — run `cargo xtask gen-schema` and commit",
                drift.join(", ")
            );
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let check = args.iter().any(|a| a == "--check");
    match cmd {
        "gen-schema" => gen_schema(check)?,
        "conformance" => println!("conformance: not yet implemented"),
        _ => {
            eprintln!("usage: cargo xtask <gen-schema [--check] | conformance>");
            std::process::exit(2);
        }
    }
    Ok(())
}

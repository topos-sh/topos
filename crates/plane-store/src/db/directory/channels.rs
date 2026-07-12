//! The channel / subscription / protection SQL — the raw-`sqlx` half of the directory's channel-era
//! device-lane ops. A child of `mod db`; no `sqlx` type crosses the boundary.
//!
//! Every policy decision lives in the guarded `topos_*` SQL functions migration 0015 created (role
//! gates, the structural-`everyone` refusals, the lapse-detach/re-attach reconciles); these methods
//! only resolve the user-facing NAMES to internal ids, run ONE function call per op inside a
//! `SERIALIZABLE` transaction, and map the outcome codes to the orchestration's typed vocabulary.
//! Curation/membership audit rows are TRIGGER-emitted on the underlying table writes — no code here
//! (or anywhere) can skip them.

use sqlx::{Postgres, Transaction};

use crate::channels::{
    ChannelIndexEntry, ChannelMembershipOutcome, ChannelSkillRef, CurationOutcome, ProtectKind,
    ProtectLevel, ProtectOutcome, SubscriptionOutcome,
};
use crate::db::Db;
use crate::error::{AuthorityError, Result};
use crate::id::{Principal, WorkspaceId};

impl Db {
    /// `channel add` / `publish --to` outside a pointer move: place a skill reference (creating the
    /// channel on first use).
    pub(crate) async fn channel_place_txn(
        &self,
        ws: &WorkspaceId,
        channel: &str,
        skill_name: &str,
        actor: &Principal,
        created_at: &str,
    ) -> Result<CurationOutcome> {
        run_serializable!(self, tx, {
            let skill_id = resolve_skill_name(&mut tx, ws, skill_name).await?;
            let code = call_policy(
                &mut tx,
                PolicyCall::Place {
                    ws,
                    channel,
                    skill_id: &skill_id,
                    actor,
                    created_at,
                },
            )
            .await?;
            map_curation(&code, "topos_channel_place")
        })
    }

    /// `channel remove`: take a skill reference out (symmetric gate with place).
    pub(crate) async fn channel_unplace_txn(
        &self,
        ws: &WorkspaceId,
        channel: &str,
        skill_name: &str,
        actor: &Principal,
        created_at: &str,
    ) -> Result<CurationOutcome> {
        run_serializable!(self, tx, {
            let skill_id = resolve_skill_name(&mut tx, ws, skill_name).await?;
            let code = call_policy(
                &mut tx,
                PolicyCall::Unplace {
                    ws,
                    channel,
                    skill_id: &skill_id,
                    actor,
                    created_at,
                },
            )
            .await?;
            map_curation(&code, "topos_channel_unplace")
        })
    }

    /// Join a channel (self-serve; `everyone` refuses — membership there IS the roster).
    pub(crate) async fn channel_join_txn(
        &self,
        ws: &WorkspaceId,
        channel: &str,
        principal: &Principal,
        created_at: &str,
    ) -> Result<ChannelMembershipOutcome> {
        run_serializable!(self, tx, {
            let code = call_policy(
                &mut tx,
                PolicyCall::Join {
                    ws,
                    channel,
                    principal,
                    created_at,
                },
            )
            .await?;
            Ok(match code.as_str() {
                "joined" => ChannelMembershipOutcome::Joined,
                "builtin" => ChannelMembershipOutcome::Builtin,
                "unknown_channel" | "member_required" => return Err(AuthorityError::NotFound),
                other => return Err(unexpected("topos_channel_join", other)),
            })
        })
    }

    /// Leave a channel (self-serve; runs the lapse-detach reconcile inside the function).
    pub(crate) async fn channel_leave_txn(
        &self,
        ws: &WorkspaceId,
        channel: &str,
        principal: &Principal,
        now: i64,
        created_at: &str,
    ) -> Result<ChannelMembershipOutcome> {
        run_serializable!(self, tx, {
            let code = call_policy(
                &mut tx,
                PolicyCall::Leave {
                    ws,
                    channel,
                    principal,
                    now,
                    created_at,
                },
            )
            .await?;
            Ok(match code.as_str() {
                "left" => ChannelMembershipOutcome::Left,
                "not_member" => ChannelMembershipOutcome::NotMember,
                "builtin" => ChannelMembershipOutcome::Builtin,
                "unknown_channel" => return Err(AuthorityError::NotFound),
                other => return Err(unexpected("topos_channel_leave", other)),
            })
        })
    }

    /// Direct-follow a skill (clears the unfollow mask + this device's exclusion; re-attaches).
    pub(crate) async fn follow_skill_txn(
        &self,
        ws: &WorkspaceId,
        skill_name: &str,
        principal: &Principal,
        device_key_id: &str,
        created_at: &str,
    ) -> Result<SubscriptionOutcome> {
        run_serializable!(self, tx, {
            let skill_id = resolve_skill_name(&mut tx, ws, skill_name).await?;
            let code = call_policy(
                &mut tx,
                PolicyCall::Follow {
                    ws,
                    skill_id: &skill_id,
                    principal,
                    device_key_id,
                    created_at,
                },
            )
            .await?;
            Ok(match code.as_str() {
                "followed" => SubscriptionOutcome::Followed,
                "skill_not_active" => SubscriptionOutcome::SkillNotActive,
                "unknown_skill" | "member_required" => return Err(AuthorityError::NotFound),
                other => return Err(unexpected("topos_follow_skill", other)),
            })
        })
    }

    /// Unfollow a skill (the standing mask + the final per-device detach records).
    pub(crate) async fn unfollow_skill_txn(
        &self,
        ws: &WorkspaceId,
        skill_name: &str,
        principal: &Principal,
        now: i64,
        created_at: &str,
    ) -> Result<SubscriptionOutcome> {
        run_serializable!(self, tx, {
            let skill_id = resolve_skill_name(&mut tx, ws, skill_name).await?;
            let code = call_policy(
                &mut tx,
                PolicyCall::Unfollow {
                    ws,
                    skill_id: &skill_id,
                    principal,
                    now,
                    created_at,
                },
            )
            .await?;
            Ok(match code.as_str() {
                "unfollowed" => SubscriptionOutcome::Unfollowed,
                "unknown_skill" | "member_required" => return Err(AuthorityError::NotFound),
                other => return Err(unexpected("topos_unfollow_skill", other)),
            })
        })
    }

    /// Exclude a followed skill from THIS device ("not on this device"; `follow` lifts it).
    pub(crate) async fn exclude_device_txn(
        &self,
        ws: &WorkspaceId,
        skill_name: &str,
        device_key_id: &str,
        created_at: &str,
    ) -> Result<SubscriptionOutcome> {
        run_serializable!(self, tx, {
            let skill_id = resolve_skill_name(&mut tx, ws, skill_name).await?;
            let code = call_policy(
                &mut tx,
                PolicyCall::Exclude {
                    ws,
                    skill_id: &skill_id,
                    device_key_id,
                    created_at,
                },
            )
            .await?;
            Ok(match code.as_str() {
                "excluded" => SubscriptionOutcome::Excluded,
                "unknown_skill" | "member_required" => return Err(AuthorityError::NotFound),
                other => return Err(unexpected("topos_exclude_device", other)),
            })
        })
    }

    /// The `protect` setter for either kind (skill protection / channel mode). Tightening takes
    /// reviewer+; loosening back to open takes an owner — the gates live in the SQL functions.
    pub(crate) async fn protect_txn(
        &self,
        ws: &WorkspaceId,
        kind: ProtectKind,
        target_name: &str,
        level: ProtectLevel,
        actor: &Principal,
        created_at: &str,
    ) -> Result<ProtectOutcome> {
        run_serializable!(self, tx, {
            let code = match kind {
                ProtectKind::Skill => {
                    let skill_id = resolve_skill_name(&mut tx, ws, target_name).await?;
                    call_policy(
                        &mut tx,
                        PolicyCall::ProtectSkill {
                            ws,
                            skill_id: &skill_id,
                            level: level.skill_str(),
                            actor,
                        },
                    )
                    .await?
                }
                ProtectKind::Channel => {
                    call_policy(
                        &mut tx,
                        PolicyCall::ProtectChannel {
                            ws,
                            channel: target_name,
                            mode: level.channel_str(),
                            actor,
                            created_at,
                        },
                    )
                    .await?
                }
            };
            Ok(match code.as_str() {
                "set" => ProtectOutcome::Set,
                "reviewer_role_required" => ProtectOutcome::ReviewerRoleRequired,
                "owner_role_required" => ProtectOutcome::OwnerRoleRequired,
                "unknown_skill" | "unknown_channel" | "member_required" => {
                    return Err(AuthorityError::NotFound);
                }
                other => return Err(unexpected("topos_protect", other)),
            })
        })
    }

    /// The workspace's channels index: every channel — `everyone` included, name-sorted — with the
    /// caller's membership, its member count (roster-derived for the builtin, else the
    /// `channel_members` count), and its name-sorted skill references. Two pool reads assembled in
    /// Rust: the skill references grouped by channel, then the channels with `member`/`member_count`
    /// computed per row. The caller's membership gate has already run (a pure read; no transaction).
    pub(crate) async fn channels_index(
        &self,
        ws: &WorkspaceId,
        principal: &Principal,
    ) -> Result<Vec<ChannelIndexEntry>> {
        let (ws_s, prin) = (ws.as_str(), principal.as_str());
        // Skill references first, name-sorted through the catalog, then grouped by channel.
        let skill_rows = sqlx::query!(
            r#"SELECT cs.channel_id AS "channel_id!", cs.skill_id AS "skill_id!", cat.name AS "name!"
               FROM channel_skills cs
               JOIN catalog cat ON cat.workspace_id = cs.workspace_id AND cat.skill_id = cs.skill_id
               WHERE cs.workspace_id = $1
               ORDER BY cat.name"#,
            ws_s,
        )
        .fetch_all(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        let mut by_channel: std::collections::HashMap<String, Vec<ChannelSkillRef>> =
            std::collections::HashMap::new();
        for r in skill_rows {
            by_channel
                .entry(r.channel_id)
                .or_default()
                .push(ChannelSkillRef {
                    skill_id: r.skill_id,
                    name: r.name,
                });
        }
        // The channels, with membership + count computed per row (builtin ⇒ roster-derived, so the
        // structural `everyone` needs no `channel_members` rows).
        let rows = sqlx::query!(
            r#"SELECT ch.channel_id AS "channel_id!", ch.name AS "name!", ch.mode AS "mode!",
                      ch.builtin AS "builtin!: i64",
                      (CASE WHEN ch.builtin = 1 THEN 1
                            WHEN EXISTS (SELECT 1 FROM channel_members cm
                                         WHERE cm.workspace_id = ch.workspace_id
                                           AND cm.channel_id = ch.channel_id AND cm.principal = $2)
                            THEN 1 ELSE 0 END)::int8 AS "member!: i64",
                      (CASE WHEN ch.builtin = 1
                            THEN (SELECT COUNT(*) FROM workspace_member m
                                  WHERE m.workspace_id = ch.workspace_id AND m.status = 'confirmed')
                            ELSE (SELECT COUNT(*) FROM channel_members cm
                                  WHERE cm.workspace_id = ch.workspace_id AND cm.channel_id = ch.channel_id)
                            END) AS "member_count!: i64"
               FROM channels ch
               WHERE ch.workspace_id = $1
               ORDER BY ch.name"#,
            ws_s,
            prin,
        )
        .fetch_all(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        rows.into_iter()
            .map(|r| {
                Ok(ChannelIndexEntry {
                    skills: by_channel.remove(&r.channel_id).unwrap_or_default(),
                    name: r.name,
                    mode: r.mode,
                    builtin: r.builtin != 0,
                    member: r.member != 0,
                    member_count: u64::try_from(r.member_count)
                        .map_err(AuthorityError::integrity)?,
                })
            })
            .collect()
    }
}

/// Resolve a user-facing skill NAME to its immutable skill id through the catalog — the one
/// name→id resolution every channel-era op runs (id-keyed references are what make rename-on-archive
/// safe). An unknown name is the uniform miss.
pub(super) async fn resolve_skill_name(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    name: &str,
) -> Result<String> {
    let ws_s = ws.as_str();
    let row = sqlx::query!(
        r#"SELECT skill_id AS "skill_id!" FROM catalog WHERE workspace_id = $1 AND name = $2"#,
        ws_s,
        name,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    row.map(|r| r.skill_id).ok_or(AuthorityError::NotFound)
}

/// The one guarded-function dispatcher — each arm is a single `SELECT topos_*(…)`, so the calling
/// convention (and the "policy lives in the database" rule) is visible in one place.
enum PolicyCall<'a> {
    Place {
        ws: &'a WorkspaceId,
        channel: &'a str,
        skill_id: &'a str,
        actor: &'a Principal,
        created_at: &'a str,
    },
    Unplace {
        ws: &'a WorkspaceId,
        channel: &'a str,
        skill_id: &'a str,
        actor: &'a Principal,
        created_at: &'a str,
    },
    Join {
        ws: &'a WorkspaceId,
        channel: &'a str,
        principal: &'a Principal,
        created_at: &'a str,
    },
    Leave {
        ws: &'a WorkspaceId,
        channel: &'a str,
        principal: &'a Principal,
        now: i64,
        created_at: &'a str,
    },
    Follow {
        ws: &'a WorkspaceId,
        skill_id: &'a str,
        principal: &'a Principal,
        device_key_id: &'a str,
        created_at: &'a str,
    },
    Unfollow {
        ws: &'a WorkspaceId,
        skill_id: &'a str,
        principal: &'a Principal,
        now: i64,
        created_at: &'a str,
    },
    Exclude {
        ws: &'a WorkspaceId,
        skill_id: &'a str,
        device_key_id: &'a str,
        created_at: &'a str,
    },
    ProtectSkill {
        ws: &'a WorkspaceId,
        skill_id: &'a str,
        level: &'a str,
        actor: &'a Principal,
    },
    ProtectChannel {
        ws: &'a WorkspaceId,
        channel: &'a str,
        mode: &'a str,
        actor: &'a Principal,
        created_at: &'a str,
    },
}

async fn call_policy(tx: &mut Transaction<'_, Postgres>, call: PolicyCall<'_>) -> Result<String> {
    let outcome = match call {
        PolicyCall::Place {
            ws,
            channel,
            skill_id,
            actor,
            created_at,
        } => {
            sqlx::query_scalar!(
                r#"SELECT topos_channel_place($1, $2, $3, $4, $5) AS "outcome!""#,
                ws.as_str(),
                channel,
                skill_id,
                actor.as_str(),
                created_at,
            )
            .fetch_one(&mut **tx)
            .await
        }
        PolicyCall::Unplace {
            ws,
            channel,
            skill_id,
            actor,
            created_at,
        } => {
            sqlx::query_scalar!(
                r#"SELECT topos_channel_unplace($1, $2, $3, $4, $5) AS "outcome!""#,
                ws.as_str(),
                channel,
                skill_id,
                actor.as_str(),
                created_at,
            )
            .fetch_one(&mut **tx)
            .await
        }
        PolicyCall::Join {
            ws,
            channel,
            principal,
            created_at,
        } => {
            sqlx::query_scalar!(
                r#"SELECT topos_channel_join($1, $2, $3, $4) AS "outcome!""#,
                ws.as_str(),
                channel,
                principal.as_str(),
                created_at,
            )
            .fetch_one(&mut **tx)
            .await
        }
        PolicyCall::Leave {
            ws,
            channel,
            principal,
            now,
            created_at,
        } => {
            sqlx::query_scalar!(
                r#"SELECT topos_channel_leave($1, $2, $3, $4, $5) AS "outcome!""#,
                ws.as_str(),
                channel,
                principal.as_str(),
                now,
                created_at,
            )
            .fetch_one(&mut **tx)
            .await
        }
        PolicyCall::Follow {
            ws,
            skill_id,
            principal,
            device_key_id,
            created_at,
        } => {
            sqlx::query_scalar!(
                r#"SELECT topos_follow_skill($1, $2, $3, $4, $5) AS "outcome!""#,
                ws.as_str(),
                principal.as_str(),
                skill_id,
                device_key_id,
                created_at,
            )
            .fetch_one(&mut **tx)
            .await
        }
        PolicyCall::Unfollow {
            ws,
            skill_id,
            principal,
            now,
            created_at,
        } => {
            sqlx::query_scalar!(
                r#"SELECT topos_unfollow_skill($1, $2, $3, $4, $5) AS "outcome!""#,
                ws.as_str(),
                principal.as_str(),
                skill_id,
                now,
                created_at,
            )
            .fetch_one(&mut **tx)
            .await
        }
        PolicyCall::Exclude {
            ws,
            skill_id,
            device_key_id,
            created_at,
        } => {
            sqlx::query_scalar!(
                r#"SELECT topos_exclude_device($1, $2, $3, $4) AS "outcome!""#,
                ws.as_str(),
                device_key_id,
                skill_id,
                created_at,
            )
            .fetch_one(&mut **tx)
            .await
        }
        PolicyCall::ProtectSkill {
            ws,
            skill_id,
            level,
            actor,
        } => {
            sqlx::query_scalar!(
                r#"SELECT topos_protect_skill($1, $2, $3, $4) AS "outcome!""#,
                ws.as_str(),
                skill_id,
                level,
                actor.as_str(),
            )
            .fetch_one(&mut **tx)
            .await
        }
        PolicyCall::ProtectChannel {
            ws,
            channel,
            mode,
            actor,
            created_at,
        } => {
            sqlx::query_scalar!(
                r#"SELECT topos_protect_channel($1, $2, $3, $4, $5) AS "outcome!""#,
                ws.as_str(),
                channel,
                mode,
                actor.as_str(),
                created_at,
            )
            .fetch_one(&mut **tx)
            .await
        }
    };
    outcome.map_err(AuthorityError::internal)
}

fn map_curation(code: &str, function: &'static str) -> Result<CurationOutcome> {
    Ok(match code {
        "placed" => CurationOutcome::Placed,
        "created" => CurationOutcome::Created,
        "removed" => CurationOutcome::Removed,
        "not_placed" => CurationOutcome::NotPlaced,
        "curated_role_required" => CurationOutcome::CuratedRoleRequired,
        "bad_name" => CurationOutcome::BadName,
        "skill_not_active" => CurationOutcome::SkillNotActive,
        "unknown_skill" | "unknown_channel" | "member_required" => {
            return Err(AuthorityError::NotFound);
        }
        other => return Err(unexpected(function, other)),
    })
}

/// `pub(super)` so the sibling `describe` db twin maps its own guarded-function outcomes through the
/// SAME internal-fault helper (one out-of-contract-outcome vocabulary for every `topos_*` call).
pub(super) fn unexpected(function: &'static str, outcome: &str) -> AuthorityError {
    AuthorityError::internal(UnexpectedPolicyOutcome {
        function,
        outcome: outcome.to_owned(),
    })
}

#[derive(Debug, thiserror::Error)]
#[error("guarded policy function {function} answered {outcome:?}, outside its contract")]
struct UnexpectedPolicyOutcome {
    function: &'static str,
    outcome: String,
}

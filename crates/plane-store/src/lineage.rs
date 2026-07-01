//! The cross-skill lineage predicate — read-only this increment, enforced transactionally later.
//!
//! Two layers: a tiny database gather (which committed ids already have provenance, and under which
//! skill) lives in `mod db`; the real logic is the **pure** decision function here, over the
//! gathered facts. The candidate's parents are a projection of the server rehash (the id is derived
//! from exactly those parents), never a free-standing client `(id, parents)` pair — that binding is the
//! confused-deputy guard extended to lineage.

use std::collections::{HashMap, HashSet};

use crate::authority::Authority;
use crate::error::Result;
use crate::id::{CommitId, SkillId, WorkspaceId};

/// A candidate commit + its parents — a projection of the server rehash. Construct it from the
/// recomputed commit (whose id is derived from these exact parents), never from a client-supplied
/// `(id, parents)` pair.
#[derive(Debug, Clone)]
pub struct CandidateCommit {
    /// The candidate's commit id (= `version_id`), derived from its parents + tree + author + message.
    pub id: CommitId,
    /// The candidate's parent commit ids, exactly as hashed into [`Self::id`].
    pub parents: Vec<CommitId>,
}

/// The lineage decision over a candidate set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineageDecision {
    /// The candidate set's lineage is valid for this skill (a normal publish, a forward revert, or an
    /// author merge).
    Pass,
    /// A candidate adopts a commit already owned by another skill, or a new commit's parent lives only
    /// in another skill's history (or nowhere) — a cross-skill graft.
    Deny,
}

pub(crate) async fn check_lineage(
    authority: &Authority,
    ws: &WorkspaceId,
    skill: &SkillId,
    candidates: &[CandidateCommit],
) -> Result<LineageDecision> {
    // An empty candidate set (e.g. an approve that uploads nothing) trivially passes.
    if candidates.is_empty() {
        return Ok(LineageDecision::Pass);
    }
    // Gather the owning skill of every id in the candidate-and-parents closure that already has
    // provenance in this workspace (absent ids — no provenance in any skill — are not returned).
    let mut ids: Vec<CommitId> = Vec::new();
    for c in candidates {
        ids.push(c.id);
        ids.extend(c.parents.iter().copied());
    }
    let owners = authority.db().commit_owners(ws, &ids).await?;
    Ok(decide(skill, candidates, &owners))
}

/// The pure decision — no I/O, no SQL — over the gathered ownership facts.
fn decide(
    skill: &SkillId,
    candidates: &[CandidateCommit],
    owners: &[(CommitId, SkillId)],
) -> LineageDecision {
    let owner_of: HashMap<[u8; 32], &SkillId> = owners.iter().map(|(c, s)| (c.0, s)).collect();

    // Rule 1: no candidate commit may already belong to a DIFFERENT skill (content-addressing makes a
    // re-upload of another skill's commit the same id). Checked over the FULL candidate set, including
    // any new-ancestor commits — not just the head.
    for c in candidates {
        if let Some(&owner) = owner_of.get(&c.id.0)
            && owner != skill
        {
            return LineageDecision::Deny;
        }
    }

    // NEW = candidates with no provenance in ANY skill (genuinely new to the workspace).
    let new_ids: HashSet<[u8; 32]> = candidates
        .iter()
        .filter(|c| !owner_of.contains_key(&c.id.0))
        .map(|c| c.id.0)
        .collect();

    // Rule 2: every parent of every NEW commit must itself be NEW or already in THIS skill's
    // provenance. A parent only in another skill's history, or nowhere, is denied. (A non-NEW candidate
    // already has valid provenance, so its parents are not re-checked here.)
    for c in candidates {
        if !new_ids.contains(&c.id.0) {
            continue;
        }
        for p in &c.parents {
            let in_new = new_ids.contains(&p.0);
            let in_this_skill = owner_of.get(&p.0).copied() == Some(skill);
            if !in_new && !in_this_skill {
                return LineageDecision::Deny;
            }
        }
    }

    LineageDecision::Pass
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cid(b: u8) -> CommitId {
        CommitId([b; 32])
    }
    fn skill(s: &str) -> SkillId {
        SkillId::parse(s).expect("skill id")
    }
    fn owner(c: CommitId, s: &str) -> (CommitId, SkillId) {
        (c, skill(s))
    }

    #[test]
    fn normal_one_parent_publish_passes() {
        // child(parent=tip); tip already belongs to skill s.
        let s = skill("s_x");
        let candidates = [CandidateCommit {
            id: cid(2),
            parents: vec![cid(1)],
        }];
        let owners = [owner(cid(1), "s_x")];
        assert_eq!(decide(&s, &candidates, &owners), LineageDecision::Pass);
    }

    #[test]
    fn forward_revert_passes() {
        // a revert is a new commit (new id) whose parent is the current tip of this skill.
        let s = skill("s_x");
        let candidates = [CandidateCommit {
            id: cid(9),
            parents: vec![cid(3)],
        }];
        let owners = [owner(cid(3), "s_x")];
        assert_eq!(decide(&s, &candidates, &owners), LineageDecision::Pass);
    }

    #[test]
    fn two_parent_author_merge_passes() {
        // merge(parents=[tip(in skill), losing(NEW, also a candidate)]); losing(parent=base in skill).
        let s = skill("s_x");
        let candidates = [
            CandidateCommit {
                id: cid(10),
                parents: vec![cid(1), cid(11)],
            },
            CandidateCommit {
                id: cid(11),
                parents: vec![cid(1)],
            },
        ];
        let owners = [owner(cid(1), "s_x")];
        assert_eq!(decide(&s, &candidates, &owners), LineageDecision::Pass);
    }

    #[test]
    fn genesis_zero_parent_passes() {
        let s = skill("s_x");
        let candidates = [CandidateCommit {
            id: cid(5),
            parents: vec![],
        }];
        assert_eq!(decide(&s, &candidates, &[]), LineageDecision::Pass);
    }

    #[test]
    fn resubmit_of_own_commit_passes() {
        // a commit that already has provenance under this skill (e.g. GC'd then re-uploaded).
        let s = skill("s_x");
        let candidates = [CandidateCommit {
            id: cid(7),
            parents: vec![cid(1)],
        }];
        let owners = [owner(cid(7), "s_x"), owner(cid(1), "s_x")];
        assert_eq!(decide(&s, &candidates, &owners), LineageDecision::Pass);
    }

    #[test]
    fn exact_cross_skill_adoption_denied() {
        // candidate id already owned by another skill (rule 1).
        let s = skill("s_x");
        let candidates = [CandidateCommit {
            id: cid(4),
            parents: vec![cid(1)],
        }];
        let owners = [owner(cid(4), "s_y"), owner(cid(1), "s_x")];
        assert_eq!(decide(&s, &candidates, &owners), LineageDecision::Deny);
    }

    #[test]
    fn cross_skill_graft_parent_in_other_skill_denied() {
        // a NEW commit whose parent lives only in another skill's history (rule 2).
        let s = skill("s_x");
        let candidates = [CandidateCommit {
            id: cid(8),
            parents: vec![cid(20)],
        }];
        let owners = [owner(cid(20), "s_y")];
        assert_eq!(decide(&s, &candidates, &owners), LineageDecision::Deny);
    }

    #[test]
    fn new_commit_parent_nowhere_denied() {
        // a NEW commit whose parent has no provenance anywhere and is not itself a candidate.
        let s = skill("s_x");
        let candidates = [CandidateCommit {
            id: cid(8),
            parents: vec![cid(30)],
        }];
        assert_eq!(decide(&s, &candidates, &[]), LineageDecision::Deny);
    }
}

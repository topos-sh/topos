//! **The pick rule** — which agents to use when nothing is picked at the scope a verb acts on and
//! no `-a` named one.
//!
//! topos never prompts. One agent installed: use it, and the receipt says so. Inside an agent that
//! announces itself (Claude Code sets `CLAUDECODE`) that agent is the pick, silently. Several
//! agents installed and none picked: nothing is guessed and nothing is asked — the installed list
//! rides a [`ClientError::PickRequired`] answer (exit 2) that names them and the command that
//! records the choice, so the next invocation says which. A person and an agent driving the CLI
//! get the same two lines.
//!
//! `--frozen` never reaches this rule with several agents installed: a CI posture that stopped
//! over a file no clone can carry (the pick is personal and git-ignored) is a pipeline that fails
//! on a question no runner can answer. A frozen run picks nobody, places nothing, and says so.
//!
//! NO agent installed is not a question either. There is nothing to choose from, so nothing is
//! chosen and nothing is recorded: the verb proceeds, places nothing, and says so. A build box
//! with no agent at all still has to run `topos install --frozen` and edit its manifest.
//!
//! Detection feeds this module and nothing else that writes ([`crate::agents_pick`] explains
//! why). The caller persists whatever is chosen BEFORE it reconciles, then reads it back.

use crate::error::ClientError;

/// The two facts about the invocation the pick rule decides on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AskInputs {
    /// Whether the invocation runs inside Claude Code (`CLAUDECODE` is set).
    pub in_claude_code: bool,
    /// Whether this run is `--frozen`. A frozen run never derives a pick from an ambiguous
    /// machine: the same invocation must answer the same way on every runner.
    pub frozen: bool,
}

/// Choose the pick for a scope with none, from the agents installed on this machine. An empty
/// answer means there was nothing to choose from — see the module docs. `global` spells the way
/// out for the machine scope.
///
/// # Errors
/// [`ClientError::PickRequired`] when several agents are installed and none is picked here.
pub(crate) fn choose(
    installed: &[String],
    inputs: &AskInputs,
    global: bool,
) -> Result<Vec<String>, ClientError> {
    // Nothing installed: no question, and no pick either. A file naming nobody would be a
    // decision nobody made, so the caller records none and says what happened.
    if installed.is_empty() {
        return Ok(Vec::new());
    }
    if let [one] = installed {
        return Ok(vec![one.clone()]);
    }
    if inputs.in_claude_code {
        return Ok(vec!["claude-code".to_owned()]);
    }
    Err(ClientError::PickRequired {
        installed: installed.to_vec(),
        global,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slugs(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    const PLAIN: AskInputs = AskInputs {
        in_claude_code: false,
        frozen: false,
    };

    #[test]
    fn one_installed_agent_is_the_pick() {
        assert_eq!(
            choose(&slugs(&["cursor"]), &PLAIN, false).unwrap(),
            ["cursor"]
        );
        assert_eq!(
            choose(&slugs(&["cursor"]), &PLAIN, true).unwrap(),
            ["cursor"],
            "the machine scope answers the same way"
        );
    }

    /// **The refusal names the installed agents and the command.** Both lines, byte for byte, at
    /// both scopes — nothing is prompted, whatever stdin is.
    #[test]
    fn the_refusal_names_the_installed_agents_and_the_command() {
        let installed = slugs(&["claude-code", "codex", "cursor"]);
        let err = choose(&installed, &PLAIN, false).unwrap_err();
        assert_eq!(err.code(), "PICK_REQUIRED");
        let ClientError::PickRequired {
            installed: listed,
            global,
        } = &err
        else {
            panic!("{err:?}");
        };
        assert_eq!(listed, &installed);
        assert!(!global);
        assert_eq!(
            crate::error::pick_required_lines(listed, *global),
            "Several agents are installed here: claude-code, codex, cursor.\n\
             Pick with: topos init -a claude-code [-a codex]"
        );
        let err = choose(&installed, &PLAIN, true).unwrap_err();
        assert_eq!(
            crate::error::pick_required_lines(&installed, true),
            "Several agents are installed here: claude-code, codex, cursor.\n\
             Pick with: topos init -g -a claude-code [-a codex]"
        );
        assert_eq!(err.code(), "PICK_REQUIRED");
    }

    /// **Nothing installed is not a question.** There is no list to choose from and nothing to
    /// place, so the answer is the empty pick — never a refusal a build box cannot answer.
    #[test]
    fn nothing_installed_is_not_a_question() {
        for global in [false, true] {
            assert!(choose(&[], &PLAIN, global).unwrap().is_empty());
        }
    }

    #[test]
    fn inside_claude_code_the_rule_picks_claude_code_silently() {
        let inside = AskInputs {
            in_claude_code: true,
            ..PLAIN
        };
        assert_eq!(
            choose(&slugs(&["claude-code", "cursor"]), &inside, false).unwrap(),
            ["claude-code"]
        );
    }
}

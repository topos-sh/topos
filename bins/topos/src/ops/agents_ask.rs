//! **The ask** — which agents to pick when nothing is picked yet and no `-a` named one.
//!
//! One agent installed: use it, and the receipt says so. Several: the answer has to come from
//! somewhere, and the ask is agent-friendly about where. Inside an agent that announces itself
//! (Claude Code sets `CLAUDECODE`) that agent is the pick, silently. Otherwise, stdin not a
//! terminal (an agent, a script, a pipe) or `--json`: nothing is asked — the installed list rides
//! a [`ClientError::PickRequired`] answer (exit 2) whose way out is `topos init -a <agent>`, so
//! the caller picks by name. A terminal gets one numbered prompt, read from stdin, no new
//! dependency. Nothing installed at all is the same answer as "cannot ask": pick by name.
//!
//! Detection feeds this module and nothing else that writes ([`crate::agents_pick`] explains
//! why). The caller persists whatever is chosen BEFORE it reconciles, then reads it back.

use std::io::Write as _;

use crate::error::ClientError;

/// The three facts about the invocation the ask decides on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AskInputs {
    /// Whether stdin is a terminal a prompt can be read from.
    pub stdin_tty: bool,
    /// Whether the invocation runs inside Claude Code (`CLAUDECODE` is set).
    pub in_claude_code: bool,
    /// Whether `--json` was asked: the one document on stdout admits no prompt.
    pub json: bool,
    /// Whether the way out is spelled for the machine scope (`-g`).
    pub global: bool,
}

/// Choose the pick for a scope with none, from the agents installed on this machine.
///
/// # Errors
/// [`ClientError::PickRequired`] when several (or no) agents are installed and no prompt can be
/// put: stdin is not a terminal, or `--json` was asked, or the prompt got no usable answer.
pub(crate) fn choose(installed: &[String], inputs: &AskInputs) -> Result<Vec<String>, ClientError> {
    choose_with(installed, inputs, prompt_on_terminal)
}

/// [`choose`] over an explicit prompt seam — what the terminal prompt does with the list, and
/// the raw answer it gets back (`None` = no answer at all).
pub(crate) fn choose_with(
    installed: &[String],
    inputs: &AskInputs,
    prompt: impl FnOnce(&[String]) -> Option<String>,
) -> Result<Vec<String>, ClientError> {
    if let [one] = installed {
        return Ok(vec![one.clone()]);
    }
    let refused = || ClientError::PickRequired {
        installed: installed.to_vec(),
        global: inputs.global,
    };
    if installed.is_empty() {
        return Err(refused());
    }
    if inputs.in_claude_code {
        return Ok(vec!["claude-code".to_owned()]);
    }
    if !inputs.stdin_tty || inputs.json {
        return Err(refused());
    }
    let answer = prompt(installed).ok_or_else(refused)?;
    parse_answer(&answer, installed).ok_or_else(refused)
}

/// The prompt a terminal gets: the numbered list on stderr (stdout stays the receipt), one line
/// read from stdin. Space- or comma-separated numbers pick several.
fn prompt_on_terminal(installed: &[String]) -> Option<String> {
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "Which agents should topos use here?");
    for (i, slug) in installed.iter().enumerate() {
        let _ = writeln!(err, "  {}. {slug}", i + 1);
    }
    let _ = write!(err, "Numbers, separated by spaces: ");
    let _ = err.flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).ok()?;
    Some(line)
}

/// The numbers a person typed, resolved against the list: 1-based, any separator that is not a
/// digit, duplicates collapsed, list order kept. `None` when nothing resolves (an empty line, a
/// number off the list, a word).
pub(crate) fn parse_answer(answer: &str, installed: &[String]) -> Option<Vec<String>> {
    let mut indices: Vec<usize> = Vec::new();
    for token in answer.split(|c: char| !c.is_ascii_digit()) {
        if token.is_empty() {
            continue;
        }
        let n: usize = token.parse().ok()?;
        let i = n.checked_sub(1)?;
        if i >= installed.len() {
            return None;
        }
        if !indices.contains(&i) {
            indices.push(i);
        }
    }
    indices.sort_unstable();
    let picked: Vec<String> = indices.into_iter().map(|i| installed[i].clone()).collect();
    (!picked.is_empty()).then_some(picked)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slugs(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    const PIPED: AskInputs = AskInputs {
        stdin_tty: false,
        in_claude_code: false,
        json: false,
        global: false,
    };

    #[test]
    fn one_installed_agent_is_used_and_said() {
        let no_prompt = |_: &[String]| panic!("one agent never prompts");
        assert_eq!(
            choose_with(&slugs(&["cursor"]), &PIPED, no_prompt).unwrap(),
            ["cursor"]
        );
    }

    #[test]
    fn the_non_tty_ask_exits_2_with_the_list_and_json_shape() {
        let installed = slugs(&["claude-code", "cursor"]);
        let err = choose_with(&installed, &PIPED, |_| panic!("piped: no prompt")).unwrap_err();
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
            err.to_string(),
            "no agents picked yet in this project. Installed on this machine: claude-code, \
             cursor. Pick with: topos init -a <agent>"
        );
        // `--json` on a terminal is the same answer: the one document admits no prompt.
        let json = AskInputs {
            stdin_tty: true,
            json: true,
            ..PIPED
        };
        assert!(choose_with(&installed, &json, |_| panic!("json: no prompt")).is_err());
        // Nothing installed cannot be chosen from either; the `-g` spelling rides through.
        let global = AskInputs {
            global: true,
            ..PIPED
        };
        let err = choose_with(&[], &global, |_| panic!("nothing to prompt")).unwrap_err();
        assert_eq!(
            err.to_string(),
            "no agents picked yet on this machine. No agent topos knows is installed on this \
             machine. Pick with: topos init -g -a <agent>"
        );
    }

    #[test]
    fn inside_claude_code_the_ask_picks_claude_code_silently() {
        let inside = AskInputs {
            in_claude_code: true,
            ..PIPED
        };
        assert_eq!(
            choose_with(&slugs(&["claude-code", "cursor"]), &inside, |_| panic!(
                "silent"
            ))
            .unwrap(),
            ["claude-code"]
        );
    }

    #[test]
    fn a_terminal_gets_one_numbered_prompt_and_its_numbers_pick() {
        let tty = AskInputs {
            stdin_tty: true,
            ..PIPED
        };
        let installed = slugs(&["claude-code", "codex", "cursor"]);
        let shown = std::cell::RefCell::new(Vec::new());
        let picked = choose_with(&installed, &tty, |list| {
            shown.borrow_mut().extend(list.iter().cloned());
            Some("3, 1\n".to_owned())
        })
        .unwrap();
        assert_eq!(*shown.borrow(), installed, "the whole list is offered");
        assert_eq!(
            picked,
            ["claude-code", "cursor"],
            "list order, whatever was typed"
        );
        // An empty or off-list answer is the same as no answer: pick by name.
        for bad in ["", "\n", "4", "codex", "0"] {
            let err = choose_with(&installed, &tty, |_| Some(bad.to_owned())).unwrap_err();
            assert_eq!(err.code(), "PICK_REQUIRED", "{bad:?}");
        }
        assert!(choose_with(&installed, &tty, |_| None).is_err());
        assert_eq!(parse_answer("2 2", &installed).unwrap(), ["codex"]);
    }
}

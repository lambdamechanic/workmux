//! The multiplexer identity of one worktree's window or session.
//!
//! `workmux list` answers "which worktrees exist"; this answers "what exactly
//! is the live multiplexer object behind this handle, right now". Supervisors
//! that record an agent's identity before starting it need the durable ids —
//! the session, the stable window id, the pane ids, and the server incarnation
//! they belong to — because a name is not an identity: window names are
//! renamed by terminal integrations, and window and pane ids are reissued from
//! zero when the server restarts, so an id without its incarnation beside it
//! means nothing.
//!
//! Nothing here is a status read. It does not reconcile, delete, or write
//! state; two calls in a row describe the same world.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};

use crate::config::{Config, MuxMode};
use crate::git;
use crate::multiplexer::types::LivePaneInfo;
use crate::multiplexer::util::prefixed;
use crate::multiplexer::{Multiplexer, WindowTarget};

/// Everything durable about the live multiplexer object behind one handle.
#[derive(Clone, Debug, PartialEq)]
pub struct MuxIdentity {
    /// The worktree handle (the worktree directory's name).
    pub handle: String,
    pub branch: String,
    pub path: PathBuf,
    pub mode: MuxMode,
    /// The multiplexer backend that answered, e.g. `tmux`.
    pub backend: String,
    /// The server incarnation every id below belongs to. `None` when the
    /// backend does not expose one.
    pub server_start_time: Option<String>,
    /// Whether a live window or session was found for this handle.
    pub is_open: bool,
    pub session: Option<String>,
    /// The stable window id (e.g. `@42`). `None` in session mode, and on
    /// backends without stable window ids.
    pub window_id: Option<String>,
    pub window_name: Option<String>,
    /// Every pane of that window, or of that session in session mode.
    pub pane_ids: Vec<String>,
}

/// Resolve the live identity behind a handle or branch name.
///
/// The name is resolved the way every other command resolves it: handle first,
/// then branch. A worktree that exists but has no window is not an error — it
/// reports `is_open: false`, which is a different fact from "no such worktree"
/// and callers need to tell the two apart.
pub fn identity(name: &str, config: &Config, mux: &dyn Multiplexer) -> Result<MuxIdentity> {
    let (path, branch) = git::find_worktree(name).map_err(|_| {
        anyhow!(
            "Worktree '{}' not found. Use 'workmux list' to see available worktrees.",
            name
        )
    })?;
    let handle = path
        .file_name()
        .ok_or_else(|| anyhow!("Invalid worktree path: no directory name"))?
        .to_string_lossy()
        .to_string();
    let mode = git::get_worktree_mode(&handle);
    let prefix = config.window_prefix();

    let mut identity = MuxIdentity {
        handle: handle.clone(),
        branch,
        path: path.clone(),
        mode,
        backend: mux.name().to_string(),
        server_start_time: None,
        is_open: false,
        session: None,
        window_id: None,
        window_name: None,
        pane_ids: Vec::new(),
    };

    // A server that is not running has no ids to report, and saying so is the
    // whole answer: every field below would otherwise be an invention.
    if !mux.is_running().unwrap_or(false) {
        return Ok(identity);
    }
    identity.server_start_time = mux.server_boot_id().unwrap_or(None);
    let panes = mux.get_all_live_pane_info().unwrap_or_default();

    match mode {
        MuxMode::Session => {
            let session = prefixed(
                prefix,
                &git::get_worktree_target_session(&handle).unwrap_or_else(|| handle.clone()),
            );
            identity.pane_ids = panes_in_session(&panes, &session);
            identity.is_open = mux.session_exists(&session).unwrap_or(false);
            if identity.is_open {
                identity.session = Some(session);
            }
        }
        MuxMode::Window => {
            let window_name = prefixed(
                prefix,
                &git::get_worktree_target_window(&handle).unwrap_or_else(|| handle.clone()),
            );
            let parent_session = git::get_worktree_window_session(&handle);
            let target =
                window_target(mux, &handle, &window_name, parent_session.as_deref(), &path)?;
            identity.is_open = mux.window_target_exists(&target).unwrap_or(false);
            if !identity.is_open {
                return Ok(identity);
            }
            identity.window_name = Some(target.full_name.clone());
            identity.window_id = target.window_id.clone();
            identity.pane_ids = panes_in_window(
                &panes,
                target.window_id.as_deref(),
                &target.full_name,
                target.parent_session(),
            );
            // Taken from the live panes rather than from the recorded parent
            // session: a window that was moved between sessions is still the
            // same window, and the caller wants where it is, not where it was.
            identity.session = identity
                .pane_ids
                .first()
                .and_then(|pane| panes.get(pane))
                .and_then(|pane| pane.session.clone())
                .or(parent_session);
            // A window id the backend can supply but this path did not resolve
            // — an untokened window, say — is still recoverable from the panes.
            if identity.window_id.is_none() {
                identity.window_id = identity
                    .pane_ids
                    .first()
                    .and_then(|pane| panes.get(pane))
                    .and_then(|pane| pane.window_id.clone());
            }
        }
    }

    Ok(identity)
}

/// The window this handle owns, preferring the ownership-token resolution that
/// survives a rename over the window's current name.
fn window_target(
    mux: &dyn Multiplexer,
    handle: &str,
    window_name: &str,
    parent_session: Option<&str>,
    worktree_path: &Path,
) -> Result<WindowTarget> {
    let token = mux
        .supports_window_ownership()
        .then(|| git::get_worktree_window_token(handle))
        .flatten();
    let owned = match token.as_deref() {
        Some(token) => {
            mux.resolve_owned_window_targets(token, window_name, parent_session, worktree_path)?
        }
        None => Vec::new(),
    };
    // The primary window, not merely the first: `workmux open --new` gives one
    // handle several windows, and only one of them is the worktree's own.
    if let Some(primary) = owned.iter().find(|owned| owned.is_primary) {
        return Ok(primary.target.clone());
    }
    if owned.len() == 1 {
        return Ok(owned[0].target.clone());
    }
    if owned.len() > 1 {
        return Err(anyhow!(
            "'{}' owns {} windows and none of them is the primary window; \
             close the duplicates before asking for its identity",
            handle,
            owned.len()
        ));
    }
    Ok(WindowTarget::new(
        window_name.to_string(),
        parent_session.map(str::to_string),
    ))
}

/// Every pane of one window, in a stable order.
///
/// Matched by stable window id when there is one. The name is the fallback,
/// and it is a weaker one: names are not unique across sessions, so a parent
/// session narrows it when the caller knows one.
fn panes_in_window(
    panes: &HashMap<String, LivePaneInfo>,
    window_id: Option<&str>,
    window_name: &str,
    parent_session: Option<&str>,
) -> Vec<String> {
    let matched = panes.iter().filter(|(_, pane)| match window_id {
        Some(window_id) => pane.window_id.as_deref() == Some(window_id),
        None => {
            pane.window.as_deref() == Some(window_name)
                && parent_session.is_none_or(|session| pane.session.as_deref() == Some(session))
        }
    });
    sorted_pane_ids(matched.map(|(pane_id, _)| pane_id.clone()))
}

/// Every pane of one session, in a stable order.
fn panes_in_session(panes: &HashMap<String, LivePaneInfo>, session: &str) -> Vec<String> {
    sorted_pane_ids(
        panes
            .iter()
            .filter(|(_, pane)| pane.session.as_deref() == Some(session))
            .map(|(pane_id, _)| pane_id.clone()),
    )
}

/// Pane ids in the order the multiplexer issued them.
///
/// `%9` before `%10`: a caller that records the first pane as the one it
/// started something in must get the same answer on every call, and a
/// lexicographic sort silently reorders itself as a server passes ten panes.
fn sorted_pane_ids(pane_ids: impl Iterator<Item = String>) -> Vec<String> {
    let mut sorted: Vec<String> = pane_ids.collect();
    sorted.sort_by(|left, right| {
        // A backend that spells pane ids some other way sorts after the
        // numeric ones rather than ahead of them, and lexicographically among
        // themselves: the order still has to be total and stable.
        pane_ordinal(left)
            .unwrap_or(u64::MAX)
            .cmp(&pane_ordinal(right).unwrap_or(u64::MAX))
            .then_with(|| left.cmp(right))
    });
    sorted
}

fn pane_ordinal(pane_id: &str) -> Option<u64> {
    pane_id.strip_prefix('%')?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(session: &str, window: &str, window_id: &str) -> LivePaneInfo {
        LivePaneInfo {
            pid: Some(1),
            current_command: Some("zsh".to_string()),
            working_dir: PathBuf::from("/repo"),
            title: None,
            session: Some(session.to_string()),
            window: Some(window.to_string()),
            session_id: Some("$0".to_string()),
            window_id: Some(window_id.to_string()),
        }
    }

    fn snapshot() -> HashMap<String, LivePaneInfo> {
        HashMap::from([
            ("%10".to_string(), pane("main", "wm-work", "@2")),
            ("%9".to_string(), pane("main", "wm-work", "@2")),
            ("%3".to_string(), pane("main", "wm-other", "@3")),
            ("%4".to_string(), pane("side", "wm-work", "@7")),
        ])
    }

    #[test]
    fn a_window_id_selects_its_panes_in_issue_order() {
        assert_eq!(
            panes_in_window(&snapshot(), Some("@2"), "wm-work", None),
            vec!["%9".to_string(), "%10".to_string()]
        );
    }

    #[test]
    fn without_a_window_id_the_parent_session_disambiguates_the_name() {
        // Two sessions carry a window called wm-work, so the name alone would
        // hand back panes from a window this handle does not own.
        assert_eq!(
            panes_in_window(&snapshot(), None, "wm-work", None),
            vec!["%4".to_string(), "%9".to_string(), "%10".to_string()]
        );
        assert_eq!(
            panes_in_window(&snapshot(), None, "wm-work", Some("side")),
            vec!["%4".to_string()]
        );
    }

    #[test]
    fn a_session_reports_every_pane_it_holds() {
        assert_eq!(
            panes_in_session(&snapshot(), "main"),
            vec!["%3".to_string(), "%9".to_string(), "%10".to_string()]
        );
        assert!(panes_in_session(&snapshot(), "absent").is_empty());
    }

    #[test]
    fn pane_ids_that_are_not_numeric_still_sort_deterministically() {
        // WezTerm numbers panes without a sigil, and a backend could invent a
        // third spelling; none of them may make the order depend on the hash
        // map's iteration.
        let sorted = sorted_pane_ids(["pane-b", "pane-a", "%2"].into_iter().map(str::to_string));
        assert_eq!(sorted, vec!["%2", "pane-a", "pane-b"]);
    }
}

use std::borrow::Cow;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use serde::Serialize;
use tabled::{
    Table, Tabled,
    settings::{Padding, Style, object::Columns},
};

use crate::git;
use crate::multiplexer::{
    AgentPane, AgentStatus, Multiplexer, create_backend, detect_backend_strict,
};
use crate::state::{PaneKey, StateStore};
use crate::util;
use crate::workflow;

#[derive(Serialize)]
struct StatusEntry {
    worktree: String,
    branch: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    observation: Option<CodexObservation>,
    elapsed_secs: Option<u64>,
    title: Option<String>,
    pane_id: String,
    workdir: PathBuf,
    agent_kind: Option<String>,
    session: Option<String>,
    window_name: Option<String>,
    updated_ts: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git: Option<GitInfo>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum CodexObservation {
    /// Current terminal quota error; quota health and resume intent are unknown.
    QuotaInterrupted,
    /// A capture could not be attributed to a stable live pane.
    SensorUnavailable,
}

fn observe_codex(entry: &mut StatusEntry, capture: impl FnOnce() -> Option<String>) {
    // A user-input wait and successful completion take precedence over terminal
    // history. Never reinterpret a missing hook state as an authorized task.
    if entry.agent_kind.as_deref() != Some("codex") || entry.status != "working" {
        return;
    }
    match capture() {
        Some(text) if super::codex_quota::is_quota_interruption(&text) => {
            entry.status = "quota_wait".into();
            entry.observation = Some(CodexObservation::QuotaInterrupted);
        }
        None => {
            entry.status = "unknown".into();
            entry.observation = Some(CodexObservation::SensorUnavailable);
        }
        Some(_) => return,
    }
    // The hook timestamp dates the old working state, not this observation.
    entry.elapsed_secs = None;
}

fn capture_stable_pane(
    mux: &dyn Multiplexer,
    store: &StateStore,
    key: &PaneKey,
    expected_updated_ts: Option<u64>,
) -> Option<String> {
    let state = store.get_agent(key).ok()??;
    if state.status != Some(AgentStatus::Working)
        || state.agent_kind.as_deref() != Some("codex")
        || Some(state.updated_ts) != expected_updated_ts
    {
        return None;
    }
    let pane_id = &key.pane_id;
    let boot = mux.server_boot_id().ok()?;
    let before = mux.get_live_pane_info(pane_id).ok()??;
    if before.pid != Some(state.pane_pid)
        || before.current_command.as_deref() != Some(&state.command)
        || state.boot_id != boot
    {
        return None;
    }
    let text = mux.capture_pane(pane_id, 80)?;
    let after = mux.get_live_pane_info(pane_id).ok()??;
    let current_state = store.get_agent(key).ok()??;
    // A recycled pane must not lend its output to the previous agent. Backends
    // lacking a process identity cannot provide this stronger observation.
    (before.pid.is_some()
        && before.pid == after.pid
        && before.current_command == after.current_command
        && before.working_dir == after.working_dir
        && before.session_id == after.session_id
        && before.window_id == after.window_id
        && serde_json::to_value(state).ok()? == serde_json::to_value(current_state).ok()?
        && boot == mux.server_boot_id().ok()?)
    .then_some(text)
}

#[derive(Serialize)]
struct StatusContext {
    backend: String,
    instance: String,
}

#[derive(Serialize)]
struct StatusScope {
    repository: Option<PathBuf>,
    targets: Vec<String>,
}

#[derive(Serialize)]
struct StatusTargetError {
    target: String,
    code: &'static str,
    message: String,
}

#[derive(Serialize)]
struct StatusOutput {
    context: StatusContext,
    scope: StatusScope,
    state_files_total: usize,
    state_files_invalid: usize,
    state_files_invalid_unattributed: usize,
    state_files_invalid_matching_context: usize,
    state_files_matching_context: usize,
    reconciled_agent_count: usize,
    agents: Vec<StatusEntry>,
    target_errors: Vec<StatusTargetError>,
}

#[derive(Serialize, Clone)]
struct GitInfo {
    has_staged: bool,
    has_unstaged: bool,
    has_unmerged_commits: bool,
}

struct StatusRow {
    worktree: String,
    status: String,
    elapsed: String,
    git: String,
    title: String,
}

impl Tabled for StatusRow {
    const LENGTH: usize = 5;

    fn fields(&self) -> Vec<Cow<'_, str>> {
        vec![
            Cow::Borrowed(&self.worktree),
            Cow::Borrowed(&self.status),
            Cow::Borrowed(&self.elapsed),
            Cow::Borrowed(&self.git),
            Cow::Borrowed(&self.title),
        ]
    }

    fn headers() -> Vec<Cow<'static, str>> {
        vec![
            Cow::Borrowed("WORKTREE"),
            Cow::Borrowed("STATUS"),
            Cow::Borrowed("ELAPSED"),
            Cow::Borrowed("GIT"),
            Cow::Borrowed("TITLE"),
        ]
    }
}

fn git_label(git: &Option<GitInfo>) -> String {
    let Some(g) = git else {
        return "-".to_string();
    };
    let mut parts = Vec::new();
    if g.has_staged {
        parts.push("staged");
    }
    if g.has_unstaged {
        parts.push("unstaged");
    }
    if g.has_unmerged_commits {
        parts.push("unmerged");
    }
    if parts.is_empty() {
        "clean".to_string()
    } else {
        parts.join(",")
    }
}

fn status_label(status: Option<AgentStatus>) -> String {
    match status {
        Some(AgentStatus::Working) => "working".to_string(),
        Some(AgentStatus::Waiting) => "waiting".to_string(),
        Some(AgentStatus::Done) => "done".to_string(),
        None => "-".to_string(),
    }
}

fn optional_name(name: &str) -> Option<String> {
    (!name.is_empty()).then(|| name.to_string())
}

fn normalized_branch(branch: String) -> String {
    if branch.is_empty() {
        "(detached)".to_string()
    } else {
        branch
    }
}

fn status_entry(
    agent: &AgentPane,
    worktree: String,
    branch: String,
    now: u64,
    git: Option<GitInfo>,
) -> StatusEntry {
    StatusEntry {
        worktree,
        branch,
        status: status_label(agent.status),
        observation: None,
        elapsed_secs: agent.status_ts.map(|ts| now.saturating_sub(ts)),
        title: agent.pane_title.clone(),
        pane_id: agent.pane_id.clone(),
        workdir: agent.path.clone(),
        agent_kind: agent.agent_kind.clone(),
        session: optional_name(&agent.session),
        window_name: optional_name(&agent.window_name),
        updated_ts: agent.updated_ts,
        git,
    }
}

/// Compute git info for a worktree path.
///
/// Runs git commands with the worktree's directory as the working dir,
/// so it works correctly for cross-project agents.
fn compute_git_info(wt_path: &std::path::Path, branch: &str) -> Result<GitInfo> {
    let has_staged = git::has_staged_changes(wt_path)?;
    let has_unstaged = git::has_unstaged_changes(wt_path)?;
    let main = git::get_default_branch_in(Some(wt_path))?;
    let base = git::get_merge_base_in(Some(wt_path), &main)?;
    let unmerged = git::get_unmerged_branches_in(Some(wt_path), &base)?;

    Ok(GitInfo {
        has_staged,
        has_unstaged,
        has_unmerged_commits: unmerged.contains(branch),
    })
}

pub fn run(worktrees: &[String], json: bool, show_git: bool) -> Result<()> {
    let mux = create_backend(detect_backend_strict()?);
    let store = StateStore::open_read_only()?;
    let mut report = store.load_reconciled_agent_report(mux.as_ref())?;
    let agent_panes = std::mem::take(&mut report.agents);
    let invalid_state_failures =
        report.state_files_invalid_matching_context + report.state_files_invalid_unattributed;
    if invalid_state_failures > 0 {
        return Err(anyhow!(
            "{} invalid agent state file(s) for {} instance {} or with unattributable filenames; inspect the workmux agents state directory",
            invalid_state_failures,
            report.backend,
            report.instance
        ));
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let reconciled_agent_count = agent_panes.len();
    let mut entries: Vec<StatusEntry> = Vec::new();
    let mut target_errors = Vec::new();
    let repository;

    if worktrees.is_empty() {
        if git::get_repo_root_if_present()?.is_some() {
            let all_worktrees = git::list_worktrees()?;
            repository = Some(git::get_main_worktree_root()?);
            let has_scoped_agents = all_worktrees.iter().any(|(wt_path, _)| {
                !workflow::match_agents_to_worktree(&agent_panes, wt_path).is_empty()
            });
            let unmerged_branches = if show_git && has_scoped_agents {
                let main = git::get_default_branch()?;
                let base = git::get_merge_base(&main)?;
                git::get_unmerged_branches(&base)?
            } else {
                std::collections::HashSet::new()
            };

            for (wt_path, branch) in &all_worktrees {
                let matching = workflow::match_agents_to_worktree(&agent_panes, wt_path);
                if matching.is_empty() {
                    continue;
                }
                let worktree_name = wt_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("unknown")
                    .to_string();
                let git_info = if show_git {
                    Some(GitInfo {
                        has_staged: git::has_staged_changes(wt_path)?,
                        has_unstaged: git::has_unstaged_changes(wt_path)?,
                        has_unmerged_commits: unmerged_branches.contains(branch),
                    })
                } else {
                    None
                };

                entries.extend(matching.into_iter().map(|agent| {
                    status_entry(
                        agent,
                        worktree_name.clone(),
                        branch.clone(),
                        now,
                        git_info.clone(),
                    )
                }));
            }
        } else {
            repository = None;
            for agent in &agent_panes {
                let worktree_path =
                    workflow::find_worktree_root(&agent.path).unwrap_or_else(|| agent.path.clone());
                let worktree_name = worktree_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("unknown")
                    .to_string();
                let branch = match git::get_branch_for_worktree(&worktree_path) {
                    Ok(branch) => normalized_branch(branch),
                    Err(error) if show_git => return Err(error),
                    Err(_) => worktree_name.clone(),
                };
                let git_info = if show_git {
                    Some(compute_git_info(&worktree_path, &branch)?)
                } else {
                    None
                };
                entries.push(status_entry(agent, worktree_name, branch, now, git_info));
            }
        }
    } else {
        repository = if git::get_repo_root_if_present()?.is_some() {
            Some(git::get_main_worktree_root()?)
        } else {
            None
        };
        for name in worktrees {
            let (wt_path, matching) =
                match workflow::resolve_worktree_agents_from_snapshot(name, &agent_panes) {
                    Ok(resolved) => resolved,
                    Err(error) => {
                        target_errors.push(StatusTargetError {
                            target: name.clone(),
                            code: "target_resolution_failed",
                            message: error.to_string(),
                        });
                        continue;
                    }
                };
            if matching.is_empty() {
                continue;
            }
            let worktree_name = wt_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("unknown")
                .to_string();
            let branch = match git::get_branch_for_worktree(&wt_path) {
                Ok(branch) => branch,
                Err(error) if show_git => return Err(error),
                Err(_) => worktree_name.clone(),
            };
            let git_info = if show_git {
                Some(compute_git_info(&wt_path, &branch)?)
            } else {
                None
            };

            entries.extend(matching.iter().map(|agent| {
                status_entry(
                    agent,
                    worktree_name.clone(),
                    branch.clone(),
                    now,
                    git_info.clone(),
                )
            }));
        }
    }

    for entry in &mut entries {
        let key = PaneKey {
            backend: report.backend.clone(),
            instance: report.instance.clone(),
            pane_id: entry.pane_id.clone(),
        };
        let expected_updated_ts = entry.updated_ts;
        observe_codex(entry, || {
            capture_stable_pane(mux.as_ref(), &store, &key, expected_updated_ts)
        });
    }

    let target_failure_count = target_errors.len();
    if json {
        let output = StatusOutput {
            context: StatusContext {
                backend: report.backend,
                instance: report.instance,
            },
            scope: StatusScope {
                repository,
                targets: worktrees.to_vec(),
            },
            state_files_total: report.state_files_total,
            state_files_invalid: report.state_files_invalid,
            state_files_invalid_unattributed: report.state_files_invalid_unattributed,
            state_files_invalid_matching_context: report.state_files_invalid_matching_context,
            state_files_matching_context: report.state_files_matching_context,
            reconciled_agent_count,
            agents: entries,
            target_errors,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        for error in &target_errors {
            eprintln!("{}: {}", error.target, error.message);
        }

        if entries.is_empty() && target_errors.is_empty() {
            if report.state_files_total > 0 && report.state_files_matching_context == 0 {
                eprintln!(
                    "{} agent state file(s) exist, but none match {} instance {}",
                    report.state_files_total, report.backend, report.instance
                );
            } else if reconciled_agent_count > 0 {
                eprintln!(
                    "{reconciled_agent_count} tracked agent(s) exist outside the requested scope"
                );
            }
            println!("No active agents");
        } else if !entries.is_empty() {
            let rows: Vec<StatusRow> = entries
                .iter()
                .map(|e| {
                    let worktree = if e.branch != e.worktree {
                        format!("{} ({})", e.worktree, e.branch)
                    } else {
                        e.worktree.clone()
                    };
                    StatusRow {
                        worktree,
                        status: e.status.clone(),
                        elapsed: e
                            .elapsed_secs
                            .map(util::format_elapsed_secs)
                            .unwrap_or("-".to_string()),
                        git: git_label(&e.git),
                        title: e.title.clone().unwrap_or("-".to_string()),
                    }
                })
                .collect();

            let mut table = Table::new(rows);
            table
                .with(Style::blank())
                .modify(Columns::new(..), Padding::new(0, 1, 0, 0));
            if !show_git {
                table.with(tabled::settings::Remove::column(
                    tabled::settings::location::ByColumnName::new("GIT"),
                ));
            }
            println!("{table}");
        }
    }

    if target_failure_count > 0 {
        return Err(anyhow!(
            "failed to resolve {target_failure_count} status target(s)"
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(status: &str, kind: &str) -> StatusEntry {
        StatusEntry {
            worktree: "task".into(),
            branch: "task".into(),
            status: status.into(),
            observation: None,
            elapsed_secs: Some(3600),
            title: None,
            pane_id: "%1".into(),
            workdir: PathBuf::from("/repo/task"),
            agent_kind: Some(kind.into()),
            session: Some("test".into()),
            window_name: Some("wm-task".into()),
            updated_ts: Some(10),
            git: None,
        }
    }

    #[test]
    fn terminal_observation_preserves_user_waits_done_and_other_agents() {
        for (status, kind) in [
            ("waiting", "codex"),
            ("done", "codex"),
            ("-", "codex"),
            ("working", "claude"),
        ] {
            let mut value = entry(status, kind);
            observe_codex(&mut value, || panic!("must not capture this agent"));
            assert_eq!(value.status, status);
            assert_eq!(value.elapsed_secs, Some(3600));
        }
    }

    #[test]
    fn failed_sensor_is_unknown_not_working_or_quota_wait() {
        let mut value = entry("working", "codex");
        observe_codex(&mut value, || None);
        assert_eq!(value.status, "unknown");
        assert_eq!(value.observation, Some(CodexObservation::SensorUnavailable));
        assert_eq!(value.elapsed_secs, None);
        assert_eq!(value.updated_ts, Some(10));
    }

    #[test]
    fn live_long_tool_keeps_hook_status() {
        let mut value = entry("working", "codex");
        observe_codex(&mut value, || {
            Some("• Working (3600s • esc to interrupt)".into())
        });
        assert_eq!(value.status, "working");
        assert_eq!(value.observation, None);
        assert_eq!(value.elapsed_secs, Some(3600));
    }

    #[test]
    fn git_info_fails_for_non_repository_path() {
        let dir = tempfile::tempdir().unwrap();

        assert!(compute_git_info(dir.path(), "feature").is_err());
    }
}

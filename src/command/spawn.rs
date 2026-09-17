//! `workmux spawn` — run a command as the foreground process of a handle's pane.
//!
//! `workmux run` splits a fresh pane and streams the result back, which is the
//! right shape for a human asking a question. A supervisor needs the opposite:
//! the process has to become the foreground command of a pane whose id it
//! already recorded, so that the pane's exit *is* the process's exit and a
//! caller watching that id learns when the agent is gone. `respawn` keeps the
//! pane id across the replacement, which is what makes the identity recorded
//! before the spawn still describe the thing afterwards.

use anyhow::{Result, anyhow, bail};
use serde::Serialize;

use crate::command::identity::JsonIdentity;
use crate::multiplexer::agent::shell_quote;
use crate::multiplexer::{create_backend, detect_backend};
use crate::workflow::identity::MuxIdentity;
use crate::{config, workflow};

#[derive(Debug, Serialize)]
struct JsonSpawn {
    #[serde(flatten)]
    identity: JsonIdentity,
    /// The pane the command now runs in.
    pane_id: String,
}

/// Which pane to respawn, given what the caller asked for.
///
/// A pane the caller names must belong to this handle's window. Respawning
/// kills whatever is in the pane, so accepting an id from another window would
/// let one handle's spawn take down another's agent — and the caller would be
/// told it had succeeded.
fn target_pane<'a>(identity: &'a MuxIdentity, requested: Option<&'a str>) -> Result<&'a str> {
    match requested {
        Some(pane_id) => {
            if !identity.pane_ids.iter().any(|pane| pane == pane_id) {
                bail!(
                    "Pane '{}' is not part of '{}' (its panes are: {})",
                    pane_id,
                    identity.handle,
                    if identity.pane_ids.is_empty() {
                        "none".to_string()
                    } else {
                        identity.pane_ids.join(", ")
                    }
                );
            }
            Ok(pane_id)
        }
        None => identity
            .pane_ids
            .first()
            .map(String::as_str)
            .ok_or_else(|| anyhow!("'{}' has no panes to spawn into", identity.handle)),
    }
}

pub fn run(name: &str, pane: Option<&str>, command_parts: &[String], json: bool) -> Result<()> {
    if command_parts.is_empty() {
        bail!("No command provided");
    }
    let config = config::Config::load(None)?;
    let mux = create_backend(detect_backend());
    let identity = workflow::identity::identity(name, &config, mux.as_ref())?;
    if !identity.is_open {
        bail!(
            "'{}' has no open {}. Open one first: workmux open --background --no-pane-cmds {}",
            identity.handle,
            match identity.mode {
                crate::config::MuxMode::Session => "session",
                crate::config::MuxMode::Window => "window",
            },
            identity.handle
        );
    }
    let pane_id = target_pane(&identity, pane)?.to_string();

    // Argument boundaries survive the trip through the multiplexer's shell.
    let command = command_parts
        .iter()
        .map(|part| shell_quote(part))
        .collect::<Vec<_>>()
        .join(" ");
    mux.respawn_pane(&pane_id, &identity.path, Some(&command))?;

    if json {
        println!(
            "{}",
            serde_json::to_string(&JsonSpawn {
                identity: JsonIdentity::from(&identity),
                pane_id,
            })?
        );
    } else {
        println!("✓ Spawned in pane {} of '{}'", pane_id, identity.handle);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MuxMode;
    use std::path::PathBuf;

    fn identity(pane_ids: &[&str]) -> MuxIdentity {
        MuxIdentity {
            handle: "work".to_string(),
            branch: "feature".to_string(),
            path: PathBuf::from("/repo/work"),
            mode: MuxMode::Window,
            backend: "tmux".to_string(),
            server_start_time: Some("1755000000".to_string()),
            is_open: true,
            session: Some("main".to_string()),
            window_id: Some("@2".to_string()),
            window_name: Some("wm-work".to_string()),
            pane_ids: pane_ids.iter().map(|pane| pane.to_string()).collect(),
        }
    }

    #[test]
    fn the_first_pane_is_the_default_target() {
        let identity = identity(&["%9", "%10"]);
        assert_eq!(target_pane(&identity, None).unwrap(), "%9");
        assert_eq!(target_pane(&identity, Some("%10")).unwrap(), "%10");
    }

    #[test]
    fn a_pane_from_another_window_is_refused_rather_than_respawned() {
        // Respawning kills what is in the pane, so a wrong target is not a
        // no-op: it would take down somebody else's agent and report success.
        let error = target_pane(&identity(&["%9"]), Some("%42")).unwrap_err();
        assert!(error.to_string().contains("is not part of 'work'"));
    }

    #[test]
    fn a_window_with_no_panes_has_nothing_to_spawn_into() {
        let error = target_pane(&identity(&[]), None).unwrap_err();
        assert!(error.to_string().contains("no panes"));
    }

    #[test]
    fn the_json_shape_carries_the_identity_and_the_pane_it_spawned_into() {
        let spawned = JsonSpawn {
            identity: JsonIdentity::from(&identity(&["%9"])),
            pane_id: "%9".to_string(),
        };
        let json = serde_json::to_value(&spawned).unwrap();
        assert_eq!(json["pane_id"], "%9");
        assert_eq!(json["window_id"], "@2");
        assert_eq!(json["server_start_time"], "1755000000");
    }
}

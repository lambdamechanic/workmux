//! `workmux identity` — the durable multiplexer identity of one handle.

use anyhow::Result;
use serde::Serialize;

use crate::config::MuxMode;
use crate::multiplexer::{create_backend, detect_backend};
use crate::workflow::identity::MuxIdentity;
use crate::{config, workflow};

/// The identity as machine-readable output.
///
/// A flat object on purpose: every field is one of the facts a caller records,
/// and nesting them would only make the ids harder to read back.
#[derive(Debug, Serialize)]
pub struct JsonIdentity {
    pub handle: String,
    pub branch: String,
    pub path: String,
    pub mode: String,
    pub backend: String,
    pub server_start_time: Option<String>,
    pub is_open: bool,
    pub session: Option<String>,
    pub window_id: Option<String>,
    pub window_name: Option<String>,
    pub pane_ids: Vec<String>,
}

impl From<&MuxIdentity> for JsonIdentity {
    fn from(identity: &MuxIdentity) -> Self {
        Self {
            handle: identity.handle.clone(),
            branch: identity.branch.clone(),
            path: identity.path.to_string_lossy().to_string(),
            mode: match identity.mode {
                MuxMode::Window => "window".to_string(),
                MuxMode::Session => "session".to_string(),
            },
            backend: identity.backend.clone(),
            server_start_time: identity.server_start_time.clone(),
            is_open: identity.is_open,
            session: identity.session.clone(),
            window_id: identity.window_id.clone(),
            window_name: identity.window_name.clone(),
            pane_ids: identity.pane_ids.clone(),
        }
    }
}

pub fn print_json(identity: &MuxIdentity) -> Result<()> {
    println!("{}", serde_json::to_string(&JsonIdentity::from(identity))?);
    Ok(())
}

pub fn print_human(identity: &MuxIdentity) {
    let json = JsonIdentity::from(identity);
    let dash = "-".to_string();
    println!("handle            {}", json.handle);
    println!("branch            {}", json.branch);
    println!("path              {}", json.path);
    println!("mode              {}", json.mode);
    println!("backend           {}", json.backend);
    println!(
        "server start time {}",
        json.server_start_time.as_ref().unwrap_or(&dash)
    );
    println!("open              {}", json.is_open);
    println!(
        "session           {}",
        json.session.as_ref().unwrap_or(&dash)
    );
    println!(
        "window id         {}",
        json.window_id.as_ref().unwrap_or(&dash)
    );
    println!(
        "window name       {}",
        json.window_name.as_ref().unwrap_or(&dash)
    );
    println!(
        "panes             {}",
        if json.pane_ids.is_empty() {
            dash
        } else {
            json.pane_ids.join(" ")
        }
    );
}

pub fn run(name: &str, json: bool) -> Result<()> {
    let config = config::Config::load(None)?;
    let mux = create_backend(detect_backend());
    let identity = workflow::identity::identity(name, &config, mux.as_ref())?;
    if json {
        print_json(&identity)
    } else {
        print_human(&identity);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn identity() -> MuxIdentity {
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
            pane_ids: vec!["%9".to_string(), "%10".to_string()],
        }
    }

    #[test]
    fn json_carries_every_id_a_caller_records() {
        let json = serde_json::to_value(JsonIdentity::from(&identity())).unwrap();
        assert_eq!(json["handle"], "work");
        assert_eq!(json["mode"], "window");
        assert_eq!(json["backend"], "tmux");
        assert_eq!(json["server_start_time"], "1755000000");
        assert_eq!(json["session"], "main");
        assert_eq!(json["window_id"], "@2");
        assert_eq!(json["window_name"], "wm-work");
        assert_eq!(json["pane_ids"], serde_json::json!(["%9", "%10"]));
        assert_eq!(json["is_open"], true);
    }

    #[test]
    fn a_closed_handle_reports_nulls_rather_than_omitting_the_fields() {
        // A caller distinguishes "no window" from "field missing because this
        // build is older", so the keys are always present.
        let mut closed = identity();
        closed.is_open = false;
        closed.session = None;
        closed.window_id = None;
        closed.window_name = None;
        closed.pane_ids.clear();
        let json = serde_json::to_value(JsonIdentity::from(&closed)).unwrap();
        assert_eq!(json["is_open"], false);
        assert!(json["window_id"].is_null());
        assert!(json["session"].is_null());
        assert!(json["window_name"].is_null());
        assert_eq!(json["pane_ids"], serde_json::json!([]));
    }
}

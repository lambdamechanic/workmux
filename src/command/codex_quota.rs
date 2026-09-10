//! Conservative, read-only recognition of a Codex terminal quota interruption.
//!
//! This is display evidence, never authorization to send input. In particular,
//! an empty-looking composer cannot prove that the user wants a turn resumed.

use console::strip_ansi_codes;

/// Recognize the current error block, rather than a quota quote in scrollback.
pub(super) fn is_quota_interruption(capture: &str) -> bool {
    let plain = strip_ansi_codes(capture);
    let lines: Vec<_> = plain.lines().map(str::trim).collect();
    let Some(prompt) = lines.iter().rposition(|line| line.starts_with('›')) else {
        return false;
    };
    // Only the observed empty composer forms are understood. Typed text and
    // unfamiliar UI layouts must not be mistaken for a recoverable prompt.
    if !matches!(lines[prompt], "›" | "› Ask Codex to do anything") {
        return false;
    }
    let footer: Vec<_> = lines[prompt + 1..]
        .iter()
        .filter(|line| !line.is_empty())
        .collect();
    if footer.len() != 1 || !footer[0].starts_with("gpt-") || !footer[0].contains(" · ") {
        return false;
    }
    let Some(end) = lines[..prompt].iter().rposition(|line| !line.is_empty()) else {
        return false;
    };
    let Some(start) = lines[..=end].iter().rposition(|line| line.starts_with('■')) else {
        return false;
    };
    // Joining wrapped lines accommodates terminal width without accepting a
    // later commentary/tool/user block after the error.
    let error = lines[start..=end].join(" ");
    error.starts_with("■ You've hit your usage limit.")
        && error.contains("https://chatgpt.com/codex/settings/usage")
        && error.contains("try again at ")
        && !lines[start + 1..=end]
            .iter()
            .any(|line| line.is_empty() || line.starts_with(['■', '•', '›']))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ERROR: &str = "■ You've hit your usage limit. Visit https://chatgpt.com/codex/settings/usage to purchase more credits or try again at Sep 15th, 2026 1:22 AM.";

    fn screen(body: &str, composer: &str) -> String {
        format!("{body}\n\n{composer}\n\n  gpt-6-astra high · ~/project")
    }

    #[test]
    fn recognizes_quota_with_missing_stop_hook_and_wrapping() {
        assert!(is_quota_interruption(&screen(ERROR, "›")));
        assert!(is_quota_interruption(&screen(
            &format!("Prior output\n{ERROR}"),
            "›"
        )));
        let wrapped = ERROR.replace("to purchase", "\n  to purchase");
        assert!(is_quota_interruption(&screen(
            &format!("• Review is underway.\n\n\x1b[31m{wrapped}\x1b[0m"),
            "› Ask Codex to do anything"
        )));
    }

    #[test]
    fn quota_history_does_not_override_progress_or_user_input() {
        for next in [
            "• Working (14s • esc to interrupt)",
            "• Implemented the fix.",
            "■ Conversation interrupted - tell the model what to do differently.",
            "• Explored\n  └ Read AGENTS.md",
        ] {
            for separator in ["\n", "\n\n"] {
                assert!(!is_quota_interruption(&screen(
                    &format!("{ERROR}{separator}{next}"),
                    "›"
                )));
            }
        }
        for composer in [
            "› Do not continue",
            "› continue",
            "› Ask Codex to do anything else",
        ] {
            assert!(!is_quota_interruption(&screen(ERROR, composer)));
        }
    }

    #[test]
    fn incomplete_or_quoted_error_is_not_a_quota_wait() {
        for body in [
            "",
            "You've hit your usage limit.",
            "■ You've hit your usage limit.",
        ] {
            assert!(!is_quota_interruption(&screen(body, "›")));
        }
        assert!(!is_quota_interruption(&screen(&format!("> {ERROR}"), "›")));
        assert!(!is_quota_interruption(ERROR));
        assert!(!is_quota_interruption(&format!(
            "{}\n• Working (1s)",
            screen(ERROR, "›")
        )));
    }
}

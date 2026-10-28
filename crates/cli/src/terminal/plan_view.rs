//! The plan review, as in the reference CLI: "Ready to code?", the plan
//! between dashed rules, three choices, and the plan file in the footer.
use super::change_view::{self, Line};

/// What the engine reads after "Esc" on a plan: rejected, wait for the user.
const STOPPED: &str = "The user rejected the plan. STOP what you are doing and wait for the user to tell you how to proceed.";

pub struct Review {
    pub lines: Vec<Line>,
    pub question: String,
    pub options: Vec<String>,
    pub footer: String,
}

pub fn review(plan: &str, path: &str, auto: bool, width: usize) -> Review {
    let rule = change_view::muted(&"\u{254c}".repeat(width.max(20)));
    let mut lines = vec![
        change_view::heading("Ready to code?"),
        change_view::body("Here is DeepSeek Code's plan:"),
        rule.clone(),
    ];
    lines.extend(plan.lines().map(change_view::body));
    lines.push(rule);
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .ok()
        .and_then(|editor| editor.split_whitespace().next().map(str::to_owned))
        .unwrap_or_else(|| "vi".into());
    Review {
        lines,
        question: "DeepSeek Code has written up a plan and is ready to execute. Would you like to proceed?".into(),
        options: vec![
            if auto { "Yes, and use auto mode" } else { "Yes, auto-accept edits" }.into(),
            "Yes, manually approve edits".into(),
            "Tell DeepSeek Code what to change".into(),
        ],
        footer: format!("ctrl+g to edit in {editor} \u{b7} {}", super::home_path(std::path::Path::new(path))),
    }
}

/// What a menu answer means for the engine's plan approval.
#[derive(Debug, PartialEq)]
pub struct Decision {
    /// The engine's plan answer (`accept_edits_keep`, `manual_keep`, `feedback: ...`).
    pub engine: String,
    /// Switch to this mode once the plan is approved.
    pub mode: Option<&'static str>,
    /// Sent to the model as a user message after the approval result.
    pub note: Option<String>,
    /// The turn stops, as after Esc in the reference CLI.
    pub stop: bool,
    /// Shown in the transcript: the plan was turned down.
    pub rejected: bool,
}

/// `answer` is what the menu sent: `1`, `2`, `2\nnote`, `3\nchanges`,
/// `esc` or `edit`.
pub fn decide(answer: &str, auto: bool) -> Decision {
    let (choice, text) = answer.split_once('\n').unwrap_or((answer, ""));
    let text = text.trim().to_owned();
    let mut decision = Decision { engine: String::new(), mode: None, note: None, stop: false, rejected: false };
    match choice.trim() {
        "1" => {
            decision.engine = "accept_edits_keep".into();
            decision.mode = auto.then_some("auto");
        }
        "2" => {
            decision.engine = "manual_keep".into();
            decision.note = Some(text).filter(|text| !text.is_empty());
        }
        "3" => {
            decision.engine = format!("feedback: {text}");
            decision.rejected = true;
        }
        "edit" => {
            decision.engine = "feedback: The user edited the plan file. Read it again and request approval for the updated plan.".into();
        }
        _ => {
            decision.engine = format!("feedback: {STOPPED}");
            decision.stop = true;
            decision.rejected = true;
        }
    }
    decision
}

/// `⎿ User rejected DeepSeek Code's plan:` above the plan in a rounded box.
pub fn rejected(plan: &str, width: usize) -> Vec<Line> {
    // Five cells of indent, then `│ ` and ` │` around the text.
    let inner = width.saturating_sub(9).max(20);
    let mut lines = vec![change_view::failure("  \u{23bf}  User rejected DeepSeek Code's plan:")];
    lines.push(change_view::muted(&format!("     \u{256d}{}\u{256e}", "\u{2500}".repeat(inner + 2))));
    for line in plan.lines() {
        let line = super::input::editor::clipped(line, inner);
        let pad = inner.saturating_sub(unicode_width::UnicodeWidthStr::width(line.as_str()));
        lines.push(change_view::muted(&format!("     \u{2502} {line}{} \u{2502}", " ".repeat(pad))));
    }
    lines.push(change_view::muted(&format!("     \u{2570}{}\u{256f}", "\u{2500}".repeat(inner + 2))));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_choices_map_to_the_engine_answers() {
        assert_eq!(decide("1", true).mode, Some("auto"));
        assert_eq!(decide("1", false).engine, "accept_edits_keep");
        assert_eq!(decide("2\nadd a test too", false).note.as_deref(), Some("add a test too"));
        let revise = decide("3\nuse TypeError", false);
        assert_eq!(revise.engine, "feedback: use TypeError");
        assert!(revise.rejected && !revise.stop);
        let esc = decide("esc", false);
        assert!(esc.stop && esc.rejected && esc.engine.contains("STOP"));
        assert!(decide("edit", false).engine.contains("edited the plan file"));
    }

    #[test]
    fn review_offers_the_reference_choices_and_the_rejected_box_frames_the_plan() {
        let view = review("Plan: fix divide\nRaise ValueError", "/w/plan.md", false, 40);
        assert_eq!(view.options, ["Yes, auto-accept edits", "Yes, manually approve edits", "Tell DeepSeek Code what to change"]);
        assert!(view.footer.contains("/w/plan.md"));
        let boxed: Vec<String> = rejected("Plan: fix divide", 40).iter().map(|line| change_view::render(line, usize::MAX, false)).collect();
        assert!(boxed[0].contains("User rejected"));
        assert!(boxed[2].starts_with("     \u{2502} Plan: fix divide"));
    }
}

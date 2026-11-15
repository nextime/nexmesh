// nexmesh -- multi-agent orchestration for agent CLIs across machines.
// Copyright (C) 2026 Stefy Lanza <stefy@nexlab.net>
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free Software
// Foundation, either version 3 of the License, or (at your option) any later
// version. This program is distributed WITHOUT ANY WARRANTY; see the LICENSE
// file, or <https://www.gnu.org/licenses/>, for the full terms.

//! Doing something about what is on the screen.
//!
//! The console could show the fleet and attach to it, and everything else --
//! a seat in the wrong session, a seat that should not exist, a seat that
//! should -- meant leaving it for a shell and a `nexmesh` subcommand. So the
//! same four endpoints the subcommands use are here behind one small dialog:
//! a question, a list or a line to type, Enter to do it and Esc to not.
//!
//! Nothing here invents an operation. `move`, `name`, `spawn`, `DELETE` and
//! `POST .../slots` are the hub's, exactly as `nexmesh move-agent` and the
//! rest call them, so an action taken from the console and the same action
//! taken from a script are the same action.

use super::Composer;
use serde_json::{json, Value};

/// One row of a pick list: what it means, and what it says.
#[derive(Debug, Clone)]
pub struct Choice {
    pub value: String,
    pub label: String,
}

/// What a dialog is asking for.
pub enum Kind {
    /// One of these.
    Pick(Vec<Choice>),
    /// A line of text.
    Text(Composer),
    /// Yes or no, and no is the default.
    Confirm,
}

/// Where the flow is. A creation takes three answers -- which team, which
/// CLI, what to call it -- so each step carries what the ones before it
/// settled rather than a half-built request living in `App`.
#[derive(Debug, Clone)]
pub enum Step {
    Move { slot: String, name: String },
    Rename { slot: String, name: String },
    Remove { slot: String, name: String, live: bool },
    Start { slot: String, name: String },
    NewSession,
    NewCli { session: String },
    NewName { session: String, cli: String },
    NewStart { session: String, cli: String, name: String },
}

pub struct Dialog {
    pub title: String,
    pub hint: String,
    pub kind: Kind,
    pub at: usize,
    pub step: Step,
}

impl Dialog {
    pub fn pick(title: impl Into<String>, hint: impl Into<String>, step: Step, options: Vec<Choice>) -> Self {
        Dialog {
            title: title.into(),
            hint: hint.into(),
            kind: Kind::Pick(options),
            at: 0,
            step,
        }
    }

    pub fn text(title: impl Into<String>, hint: impl Into<String>, step: Step, prefill: &str) -> Self {
        let mut c = Composer::default();
        c.set(prefill.to_string());
        Dialog { title: title.into(), hint: hint.into(), kind: Kind::Text(c), at: 0, step }
    }

    pub fn confirm(title: impl Into<String>, hint: impl Into<String>, step: Step) -> Self {
        Dialog { title: title.into(), hint: hint.into(), kind: Kind::Confirm, at: 0, step }
    }

    pub fn rows(&self) -> usize {
        match &self.kind {
            Kind::Pick(v) => v.len(),
            _ => 0,
        }
    }

    pub fn chosen(&self) -> Option<&Choice> {
        match &self.kind {
            Kind::Pick(v) => v.get(self.at),
            _ => None,
        }
    }

    pub fn typed(&self) -> String {
        match &self.kind {
            Kind::Text(c) => c.text.trim().to_string(),
            _ => String::new(),
        }
    }

    pub fn composer_mut(&mut self) -> Option<&mut Composer> {
        match &mut self.kind {
            Kind::Text(c) => Some(c),
            _ => None,
        }
    }

    pub fn down(&mut self) {
        let n = self.rows();
        if n > 0 {
            self.at = (self.at + 1).min(n - 1);
        }
    }

    pub fn up(&mut self) {
        self.at = self.at.saturating_sub(1);
    }
}

/// The mode a seat is started in, offered rather than assumed: a seat you
/// mean to type at and a seat something else drives are both ordinary, and
/// guessing wrong means killing it and starting again.
pub fn modes() -> Vec<Choice> {
    vec![
        Choice { value: "interactive".into(), label: "interactive \u{2014} a terminal you can attach to and type in".into() },
        Choice { value: "headless".into(), label: "headless \u{2014} driven by the fleet, watched but not typed at".into() },
        Choice { value: "".into(), label: "do not start it yet \u{2014} just make the seat".into() },
    ]
}

/// What came back, said in one line for the status row.
pub type Said = String;

/// Carry out what the dialog settled. The only place in the console that
/// changes anything, so every write the console can do is readable in one
/// screenful.
pub async fn perform(
    base: &str,
    client: &reqwest::Client,
    step: &Step,
    answer: &str,
) -> anyhow::Result<Said> {
    match step {
        Step::Move { slot, name } => {
            let v = crate::post(
                client,
                format!("{base}/v1/slots/{slot}/move"),
                json!({ "session": answer }),
            )
            .await?;
            said(&v)?;
            let left = v["messages_left_unread"].as_i64().unwrap_or(0);
            let mut out = format!(
                "{name} is now in {}",
                v["session_name"].as_str().unwrap_or(answer)
            );
            if left > 0 {
                out.push_str(&format!("; {left} unread message(s) stayed behind"));
            }
            Ok(out)
        }
        Step::Rename { slot, name } => {
            if answer.is_empty() {
                anyhow::bail!("a seat needs a name");
            }
            let v = crate::post(
                client,
                format!("{base}/v1/slots/{slot}/name"),
                json!({ "name": answer }),
            )
            .await?;
            said(&v)?;
            Ok(format!("{name} is now called {answer}"))
        }
        Step::Remove { slot, name, live } => {
            let url = format!("{base}/v1/slots/{slot}{}", if *live { "?force=1" } else { "" });
            let r = crate::authed(client.delete(&url)).send().await?;
            let v = crate::body_or_error(r, "removing the seat").await?;
            Ok(format!(
                "{name} is gone ({} agent row(s) with it)",
                v["agent_rows"].as_i64().unwrap_or(0)
            ))
        }
        Step::Start { slot, name } => {
            let v = crate::post(
                client,
                format!("{base}/v1/slots/{slot}/spawn"),
                json!({ "mode": answer }),
            )
            .await?;
            said(&v)?;
            Ok(format!(
                "{name} is starting on {} ({})",
                v["node"].as_str().unwrap_or("?"),
                v["mode"].as_str().unwrap_or("?")
            ))
        }
        Step::NewStart { session, cli, name } => {
            let v = crate::post(
                client,
                format!("{base}/v1/sessions/{session}/slots"),
                json!({
                    "cli_id": cli,
                    "display_name": name,
                    // A seat made from a keyboard is one you mean to talk to
                    // unless it is told otherwise, and `any` lets the hub put
                    // it on a machine that actually has the CLI installed.
                    "mode": if answer.is_empty() { "interactive" } else { answer },
                    "placement": { "any": true },
                }),
            )
            .await?;
            said(&v)?;
            let slot = v["slot_id"].as_str().unwrap_or("").to_string();
            if answer.is_empty() {
                return Ok(format!("{name} has a seat ({slot}); S starts it"));
            }
            let started = crate::post(
                client,
                format!("{base}/v1/slots/{slot}/spawn"),
                json!({ "mode": answer }),
            )
            .await?;
            said(&started)?;
            Ok(format!(
                "{name} ({slot}) is starting on {} ({answer})",
                started["node"].as_str().unwrap_or("?")
            ))
        }
        // The steps in the middle of a flow never reach here: the key
        // handler turns each answer into the next question.
        Step::NewSession | Step::NewCli { .. } | Step::NewName { .. } => {
            anyhow::bail!("nothing to do yet")
        }
    }
}

/// The hub reports a refusal in the body, with a 200. Surfacing it as an
/// error is the difference between "it did not work, and why" and a status
/// line that says nothing changed.
fn said(v: &Value) -> anyhow::Result<()> {
    if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
        anyhow::bail!("{e}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pick_list_cannot_be_walked_off_either_end() {
        let mut d = Dialog::pick(
            "where",
            "",
            Step::NewSession,
            vec![
                Choice { value: "a".into(), label: "a".into() },
                Choice { value: "b".into(), label: "b".into() },
            ],
        );
        d.up();
        assert_eq!(d.at, 0);
        d.down();
        d.down();
        d.down();
        assert_eq!(d.at, 1);
        assert_eq!(d.chosen().map(|c| c.value.as_str()), Some("b"));
    }

    #[test]
    fn a_text_dialog_starts_with_the_old_value_and_the_caret_at_its_end() {
        // Renaming a seat is usually a correction, not a fresh name.
        let d = Dialog::text("name", "", Step::Rename { slot: "sl-1".into(), name: "Dev".into() }, "Dev");
        assert_eq!(d.typed(), "Dev");
        match &d.kind {
            Kind::Text(c) => assert_eq!(c.caret, 3),
            _ => panic!("not a text dialog"),
        }
    }

    #[test]
    fn a_confirm_has_nothing_to_pick_and_nothing_typed() {
        let d = Dialog::confirm(
            "remove",
            "",
            Step::Remove { slot: "sl-1".into(), name: "Dev".into(), live: false },
        );
        assert_eq!(d.rows(), 0);
        assert!(d.chosen().is_none());
        assert_eq!(d.typed(), "");
    }

    #[test]
    fn the_hub_refusing_in_the_body_is_an_error_not_a_success() {
        assert!(said(&json!({ "error": "slot still has a live agent" })).is_err());
        assert!(said(&json!({ "removed": "sl-1" })).is_ok());
    }

    #[test]
    fn starting_a_seat_offers_both_modes_and_neither() {
        let m = modes();
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].value, "interactive");
        assert_eq!(m[1].value, "headless");
        assert!(m[2].value.is_empty(), "the third choice must mean 'do not start it'");
    }
}

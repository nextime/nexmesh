//! What a seat is allowed to do TO THE FLEET.
//!
//! This exists because authority used to be a property of an INSTALL. The
//! orchestrator MCP server holds an operator token, so whoever's MCP config
//! named that server got fleet-wide power -- and a user-scoped install
//! therefore handed it to every CLI that user ever started on that machine,
//! all of them acting as the operator and none of them distinguishable from
//! each other or from the person running the fleet.
//!
//! Authority has to belong to an identity the hub already knows. The hub
//! knows seats: every tool call arrives with the calling slot. So it lives on
//! the slot, it is granted deliberately, and it is checked here.
//!
//! It is NOT `slots.role`, which is the seat's job -- "Dev", "Reviewer" --
//! and confers nothing.

use serde::{Deserialize, Serialize};

/// Four levels: the two reaches `operator_tokens.session_id` already
/// described, the ordinary case that has neither, and one BELOW it.
///
/// Ordered from least to most, so a comparison reads the way it looks.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Authority {
    /// WATCHES AND CANNOT ACT. Below a plain agent, and the only level that
    /// takes tools away rather than adding them.
    ///
    /// For a seat whose job is to be read: somebody outside the team -- a
    /// browser session, a dashboard of their own, a person with a phone --
    /// who wants to see what the team is doing without being able to steer
    /// it. It can read the state and the team's memory, answer whoever asks
    /// it, and ask a seat for its progress; it cannot send a message of its
    /// own writing, write to the team's memory, take a lock, or touch a
    /// seat.
    ///
    /// ENFORCED, not asked for. A brief saying "do not give instructions"
    /// is a request, and a model that believes it is helping will route
    /// around it. These restrictions are checked on every tool call, in the
    /// hub, from the calling seat.
    Observer,
    /// No fleet tools at all. The default, and what almost every seat is.
    ///
    /// `#[default]` is load-bearing: every path that cannot read an authority
    /// -- a missing slot, an unreadable column, a level a newer hub knows and
    /// this build does not -- lands here. Authority has to fail CLOSED.
    #[default]
    Agent,
    /// Runs ONE team: its own session. Can elect other orchestrators there.
    Orchestrator,
    /// Runs the fleet. The only level that can mint another of itself.
    Super,
}

impl Authority {
    /// Read a stored or typed level. Unknown text is `Agent`, never an error:
    /// a column this code has not heard of must fail CLOSED.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "observer" | "watcher" | "readonly" | "read-only" => Self::Observer,
            "orchestrator" | "orch" => Self::Orchestrator,
            // `superorchestrator` is what the operator calls it out loud.
            "super" | "superorchestrator" | "super-orchestrator" | "superorch" => Self::Super,
            _ => Self::Agent,
        }
    }

    /// Exactly as stored. `parse` round-trips this.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Observer => "observer",
            Self::Agent => "agent",
            Self::Orchestrator => "orchestrator",
            Self::Super => "super",
        }
    }

    /// What to call it to a person.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Observer => "observer",
            Self::Agent => "agent",
            Self::Orchestrator => "orchestrator",
            Self::Super => "superorchestrator",
        }
    }

    /// Does this level get the fleet-management tools at all?
    pub fn is_orchestrating(&self) -> bool {
        matches!(self, Self::Orchestrator | Self::Super)
    }

    /// Is this a seat that may only READ?
    ///
    /// Its own predicate rather than `== Observer` at a dozen call sites, so
    /// a level added below it later is covered by every check at once.
    pub fn is_read_only(&self) -> bool {
        matches!(self, Self::Observer)
    }

    /// May a caller at this level act on a seat in `target`?
    ///
    /// An orchestrator reaches its own session and nothing else -- it cannot
    /// see, touch or discover another team, which is what a session-scoped
    /// operator token already meant.
    pub fn reaches(&self, caller_session: &str, target_session: &str) -> bool {
        match self {
            Self::Super => true,
            Self::Orchestrator => caller_session == target_session,
            Self::Agent | Self::Observer => false,
        }
    }

    /// May a caller at this level change a seat from `current` to `want`?
    ///
    /// `Err` carries the sentence to give the caller, because "denied" on its
    /// own sends an agent round the loop again trying synonyms.
    ///
    /// The rule that matters: an orchestrator can elect other orchestrators,
    /// but only inside its own team and never a `super`. A privilege able to
    /// widen itself is not a boundary -- an orchestrator that could mint a
    /// super could mint one and be promoted by it a moment later.
    pub fn may_elect(
        &self,
        want: Authority,
        current: Authority,
        same_session: bool,
    ) -> Result<(), String> {
        match self {
            Self::Super => Ok(()),
            Self::Agent => Err("Only an orchestrator can change what a seat is allowed \
                                to do. Ask the operator, or the orchestrator of this \
                                session."
                .to_string()),
            Self::Observer => Err("This seat observes; it cannot change what anybody is \
                                   allowed to do, including itself. Ask the operator."
                .to_string()),
            Self::Orchestrator => {
                if !same_session {
                    return Err("An orchestrator reaches its own session only. That seat is \
                                on another team: a superorchestrator or the operator has to \
                                do it."
                        .to_string());
                }
                if want == Authority::Super {
                    return Err("An orchestrator cannot make a superorchestrator -- that \
                                would be granting more authority than it holds. Only a \
                                superorchestrator or the operator can."
                        .to_string());
                }
                if current == Authority::Super {
                    return Err("That seat is a superorchestrator, which an orchestrator \
                                cannot demote. Only another superorchestrator or the \
                                operator can."
                        .to_string());
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Authority::*;
    use super::*;

    #[test]
    fn unknown_text_is_the_least_authority_there_is() {
        // A level this build has not heard of -- a newer hub's column, a
        // typo, an empty string -- must never read as power.
        for s in ["", "root", "admin", "ORCHESTRATOR_", "nonsense"] {
            assert_eq!(Authority::parse(s), Agent, "{s:?}");
        }
    }

    #[test]
    fn the_stored_spelling_round_trips() {
        for a in [Agent, Orchestrator, Super] {
            assert_eq!(Authority::parse(a.as_str()), a);
        }
    }

    #[test]
    fn the_operators_own_word_for_it_is_understood() {
        assert_eq!(Authority::parse("superorchestrator"), Super);
        assert_eq!(Authority::parse("Orch"), Orchestrator);
    }

    #[test]
    fn an_orchestrator_reaches_its_own_team_and_no_other() {
        assert!(Orchestrator.reaches("s-1", "s-1"));
        assert!(!Orchestrator.reaches("s-1", "s-2"));
        assert!(Super.reaches("s-1", "s-2"));
        assert!(!Agent.reaches("s-1", "s-1"));
    }

    #[test]
    fn an_orchestrator_elects_orchestrators_in_its_own_session() {
        // The thing that was actually asked for.
        assert!(Orchestrator.may_elect(Orchestrator, Agent, true).is_ok());
        assert!(Orchestrator.may_elect(Agent, Orchestrator, true).is_ok());
    }

    #[test]
    fn an_orchestrator_cannot_widen_itself() {
        // It could otherwise mint a super and be promoted by it.
        let e = Orchestrator.may_elect(Super, Agent, true).unwrap_err();
        assert!(e.contains("cannot make a superorchestrator"), "{e}");
    }

    #[test]
    fn an_orchestrator_cannot_demote_a_super() {
        let e = Orchestrator.may_elect(Agent, Super, true).unwrap_err();
        assert!(e.contains("cannot demote"), "{e}");
    }

    #[test]
    fn an_orchestrator_cannot_elect_across_sessions() {
        let e = Orchestrator.may_elect(Orchestrator, Agent, false).unwrap_err();
        assert!(e.contains("own session only"), "{e}");
    }

    #[test]
    fn a_plain_agent_elects_nobody() {
        for want in [Agent, Orchestrator, Super] {
            assert!(Agent.may_elect(want, Agent, true).is_err());
        }
    }

    #[test]
    fn a_super_may_do_all_of_it() {
        for want in [Agent, Orchestrator, Super] {
            for cur in [Agent, Orchestrator, Super] {
                for same in [true, false] {
                    assert!(Super.may_elect(want, cur, same).is_ok());
                }
            }
        }
    }

    #[test]
    fn only_the_two_upper_levels_orchestrate() {
        assert!(!Agent.is_orchestrating());
        assert!(Orchestrator.is_orchestrating());
        assert!(Super.is_orchestrating());
    }
}

//! The fleet-management tools, as called BY A SEAT.
//!
//! These are the same operations the operator has had all along through the
//! dashboard and `nexmesh`. What is new is that a SEAT can call them, and
//! that whether it may is decided here, per call, from `slots.authority`.
//!
//! They used to live in a separate MCP server holding an operator token.
//! That made authority a property of an MCP config rather than of an
//! identity: a user-scoped install handed fleet-wide power to every CLI that
//! user ever started, all of them acting as "operator" in the audit log and
//! none of them distinguishable from the person running the fleet. The hub
//! already knows which seat is calling -- every tool call carries its slot --
//! so the check belongs here.
//!
//! Reach is never implied by the arguments. Every one of these resolves the
//! seat it was given THROUGH the caller's reach, so an orchestrator naming a
//! seat on another team is told no such seat exists within its reach rather
//! than being allowed to act on it.

use anyhow::{bail, Result};
use nexmesh_proto::authority::Authority;
use nexmesh_proto::now_ms;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::json;

/// What a seat is allowed to do, as the hub has it.
///
/// Absent, unreadable or unknown text is `Agent`. Reading authority must fail
/// CLOSED: a column a build does not understand is not a grant.
pub fn authority_of(db: &Connection, slot_id: &str) -> Authority {
    db.query_row(
        "SELECT authority FROM slots WHERE id = ?",
        [slot_id],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
    .map(|s| Authority::parse(&s))
    .unwrap_or_default()
}

/// The same, for a caller the MCP door knows only by agent id.
pub fn authority_of_agent(db: &Connection, agent_id: &str) -> Authority {
    db.query_row(
        "SELECT COALESCE(s.authority, 'agent') FROM slots s
           JOIN agents a ON a.slot_id = s.id WHERE a.id = ?",
        [agent_id],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
    .map(|s| Authority::parse(&s))
    .unwrap_or_default()
}

#[derive(Debug)]
pub struct Seat {
    pub slot: String,
    pub session: String,
    pub name: String,
    pub authority: Authority,
}

/// Find a seat the caller can actually reach, by slot id or by name.
///
/// A display name is only unique WITHIN a session, so a superorchestrator
/// naming one can legitimately match several. The caller's own session wins,
/// and anything still ambiguous is refused with the candidates named --
/// picking one at random would eventually rename or remove the wrong seat on
/// another team.
fn reachable(
    db: &Connection,
    needle: &str,
    caller: Authority,
    caller_session: &str,
) -> Result<Seat> {
    // A seat with no authority reaches nothing, so every lookup below would
    // report "no such seat" -- true, but it reads as a typo and sends the
    // caller round again with different spellings. Say what is actually
    // wrong. (The dispatch gate refuses these before they arrive; this is
    // the same answer given independently, so the module is safe called
    // directly.)
    if !caller.is_orchestrating() {
        bail!(
            "this seat is a plain agent and holds no authority over the fleet. \
             Authority lives on the seat in the hub: the operator grants it from the \
             dashboard or with /authority, and an orchestrator of this session can \
             with `elect`."
        );
    }
    let needle = needle.trim();
    if needle.is_empty() {
        bail!("name a seat: its name, or its slot id");
    }
    let mut stmt = db.prepare(
        "SELECT id, session_id, COALESCE(display_name, ''), COALESCE(authority, 'agent')
           FROM slots WHERE id = ?1 OR display_name = ?1",
    )?;
    let all: Vec<Seat> = stmt
        .query_map([needle], |r| {
            Ok(Seat {
                slot: r.get(0)?,
                session: r.get(1)?,
                name: r.get(2)?,
                authority: Authority::parse(&r.get::<_, String>(3)?),
            })
        })?
        .filter_map(|x| x.ok())
        .collect();
    // Reach is applied BEFORE anything is reported, so a seat out of reach is
    // indistinguishable from a seat that does not exist. An orchestrator
    // must not be able to enumerate another team by guessing names.
    let mut mine: Vec<Seat> = all
        .into_iter()
        .filter(|s| caller.reaches(caller_session, &s.session))
        .collect();
    if mine.is_empty() {
        bail!("no seat '{needle}' that you can reach");
    }
    if mine.len() > 1 {
        if let Some(i) = mine.iter().position(|s| s.session == caller_session) {
            return Ok(mine.swap_remove(i));
        }
        let where_ = mine
            .iter()
            .map(|s| format!("{} in {}", s.slot, s.session))
            .collect::<Vec<_>>()
            .join(", ");
        bail!("'{needle}' names more than one seat ({where_}); use the slot id");
    }
    Ok(mine.remove(0))
}

fn audit(db: &Connection, actor: &str, action: &str, detail: serde_json::Value) {
    let _ = db.execute(
        "INSERT INTO audit (at_ms, actor, node_id, action, detail) VALUES (?,?,NULL,?,?)",
        params![now_ms(), actor, action, detail.to_string()],
    );
}

/// Who the audit log should name. A seat, by its slot -- never "operator",
/// which is what the token-holding orchestrator server logged as and the
/// reason a fleet change could not be traced to whoever made it.
fn actor_of(db: &Connection, slot: &str) -> String {
    let name: Option<String> = db
        .query_row("SELECT display_name FROM slots WHERE id = ?", [slot], |r| r.get(0))
        .optional()
        .ok()
        .flatten();
    match name {
        Some(n) if !n.is_empty() => format!("seat {n} ({slot})"),
        _ => format!("seat {slot}"),
    }
}

// ---------------------------------------------------------------- guest ----

/// Have a seat answer in another team as well as its own.
///
/// Needs reach over BOTH teams, which makes this a superorchestrator's tool:
/// an orchestrator reaches exactly one session, and inviting a seat from
/// that session into that session is not an invitation. Said plainly in the
/// refusal rather than left for the caller to work out.
pub fn invite_agent(
    db: &Connection,
    caller_slot: &str,
    caller_session: &str,
    needle: &str,
    dest: &str,
    leave: bool,
) -> Result<String> {
    let caller = authority_of(db, caller_slot);
    let seat = reachable(db, needle, caller, caller_session)?;
    let dest_id: String = db
        .query_row(
            "SELECT id FROM sessions WHERE id = ?1 OR name = ?1",
            [dest.trim()],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| anyhow::anyhow!("no session '{dest}'"))?;
    if !caller.reaches(caller_session, &dest_id) {
        bail!(
            "having a seat answer in another team needs reach over that team as well as \
             over the seat. An orchestrator reaches only its own session, so this is a \
             superorchestrator's or the operator's call."
        );
    }
    let by = actor_of(db, caller_slot);
    if leave {
        crate::guests::leave(db, &seat.slot, &dest_id, &by)
    } else {
        crate::guests::join(db, &seat.slot, &dest_id, &by)
    }
}

// ----------------------------------------------------------------- elect ----

/// Grant or take away authority.
///
/// The rules are `Authority::may_elect`: a superorchestrator may do all of
/// it; an orchestrator may elect other orchestrators inside its own session
/// and may never mint a super, because a privilege that can widen itself is
/// not a boundary.
pub fn elect(
    db: &Connection,
    caller_slot: &str,
    caller_session: &str,
    needle: &str,
    want_raw: &str,
) -> Result<String> {
    let caller = authority_of(db, caller_slot);
    // Refused rather than parsed loosely: `Authority::parse` falls back to
    // `agent`, which is right when reading a column and very wrong when a
    // caller is trying to grant something and misspelled it.
    let want = match want_raw.trim().to_lowercase().as_str() {
        "observer" | "watcher" | "readonly" | "read-only" => Authority::Observer,
        "agent" | "none" => Authority::Agent,
        "orchestrator" | "orch" => Authority::Orchestrator,
        "super" | "superorchestrator" | "super-orchestrator" | "superorch" => Authority::Super,
        other => bail!(
            "'{other}' is not an authority. Use 'observer', 'agent', 'orchestrator' \
             or 'super'."
        ),
    };
    let seat = reachable(db, needle, caller, caller_session)?;
    caller
        .may_elect(want, seat.authority, seat.session == caller_session)
        .map_err(anyhow::Error::msg)?;
    if seat.authority == want {
        return Ok(format!(
            "{} is already {}.",
            seat.name, want.label()
        ));
    }
    db.execute(
        "UPDATE slots SET authority = ? WHERE id = ?",
        params![want.as_str(), seat.slot],
    )?;
    audit(
        db,
        &actor_of(db, caller_slot),
        "authority_changed",
        json!({ "slot": seat.slot, "from": seat.authority.as_str(), "to": want.as_str() }),
    );
    tracing::info!(
        by = %caller_slot, slot = %seat.slot,
        from = %seat.authority.as_str(), to = %want.as_str(),
        "authority changed by a seat"
    );
    // Enforcement is immediate -- every call re-reads the column -- but a
    // CLI lists the tools once when it starts, so what it has been OFFERED
    // can lag behind what it is allowed. Say which is which, or a newly
    // elected orchestrator reports that nothing happened.
    //
    // This used to end "the tools work when called either way", which was
    // true of the hub and false of the seat: a model cannot emit a call for
    // a tool that is not in the list its client gave it, so an unadvertised
    // fleet tool is unreachable no matter what the hub would allow. The
    // restart is not a nicety, it is the whole difference.
    Ok(format!(
        "{} is now {} (was {}). The hub enforces this from the next call -- it reads \
         the seat's authority every time. Its CLI will not OFFER the fleet tools \
         until the seat restarts, though: an MCP client asks for the tool list once, \
         at startup, and a model cannot call a tool it was never shown. Restart the \
         seat for {} to be able to use them.",
        seat.name,
        want.label(),
        seat.authority.label(),
        seat.name
    ))
}

// ------------------------------------------------------------ list_seats ----

pub fn list_seats(db: &Connection, caller_slot: &str, caller_session: &str) -> Result<String> {
    let caller = authority_of(db, caller_slot);
    let mut stmt = db.prepare(
        "SELECT s.id, s.session_id, COALESCE(se.name, s.session_id),
                COALESCE(s.display_name, ''), COALESCE(s.role, ''),
                COALESCE(s.authority, 'agent'), s.status, COALESCE(s.cli_id, '')
           FROM slots s LEFT JOIN sessions se ON se.id = s.session_id
          ORDER BY se.name, s.display_name",
    )?;
    let rows: Vec<serde_json::Value> = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, String>(7)?,
            ))
        })?
        .filter_map(|x| x.ok())
        .filter(|(_, sid, ..)| caller.reaches(caller_session, sid))
        .map(|(slot, _, session, name, role, authority, status, cli)| {
            json!({ "slot": slot, "session": session, "name": name, "role": role,
                    "authority": Authority::parse(&authority).label(),
                    "status": status, "cli": cli })
        })
        .collect();
    Ok(json!({
        "your_authority": caller.label(),
        "reach": if caller == Authority::Super { "the fleet" } else { "your own session" },
        "seats": rows
    })
    .to_string())
}

// ------------------------------------------------------- add / remove -------

pub fn add_agent(
    db: &Connection,
    caller_slot: &str,
    caller_session: &str,
    args: &serde_json::Value,
) -> Result<String> {
    let caller = authority_of(db, caller_slot);
    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("add_agent needs 'name'"))?;
    if name.chars().count() > 64 {
        bail!("a name must be 64 characters or fewer");
    }
    // Default to the caller's own team, which is the only one an
    // orchestrator can add to.
    let session = match args.get("session").and_then(|v| v.as_str()).map(str::trim) {
        Some(s) if !s.is_empty() => db
            .query_row(
                "SELECT id FROM sessions WHERE id = ?1 OR name = ?1",
                [s],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| anyhow::anyhow!("no session '{s}'"))?,
        _ => caller_session.to_string(),
    };
    if !caller.reaches(caller_session, &session) {
        bail!(
            "that team is outside your reach -- an orchestrator adds seats to its own \
             session only. A superorchestrator or the operator can."
        );
    }
    // A name is how everything addresses a seat, so a duplicate inside one
    // session is how three seats called "Hermes" came to sit in one team.
    let taken: bool = db
        .query_row(
            "SELECT 1 FROM slots WHERE session_id = ? AND display_name = ?",
            params![session, name],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if taken {
        bail!("'{name}' is already a seat in that team; pick another name");
    }
    let slot = format!("sl-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    let cli = args.get("cli").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty());
    let role = args.get("role").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty());
    let model = args.get("model").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty());
    db.execute(
        "INSERT INTO slots (id, session_id, cli_id, participant_kind, mode, placement,
                            display_name, role, model, capabilities, authority, created_at_ms)
         VALUES (?,?,?,'cli','headless','{}',?,?,?,'[]','agent',?)",
        params![slot, session, cli, name, role, model, now_ms()],
    )?;
    audit(
        db,
        &actor_of(db, caller_slot),
        "slot_added",
        json!({ "slot": slot, "session": session, "name": name, "cli": cli }),
    );
    Ok(format!(
        "Added '{name}' ({slot}) to that team, as a plain agent. It is empty and \
         disconnected: it has to be spawned before anything fills it."
    ))
}

pub fn remove_agent(
    db: &Connection,
    caller_slot: &str,
    caller_session: &str,
    needle: &str,
) -> Result<String> {
    let caller = authority_of(db, caller_slot);
    let seat = reachable(db, needle, caller, caller_session)?;
    if seat.slot == caller_slot {
        bail!("that is your own seat. Removing it would stop you mid-call.");
    }
    // The same rule the operator's own door applies: a seat may not be
    // removed from under a live agent. There is no `force` here on purpose
    // -- that decision is the operator's.
    let cutoff = now_ms() - nexmesh_proto::NODE_LEASE_MS as i64;
    let live: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM agents
              WHERE slot_id = ? AND exited_at_ms IS NULL AND status <> 'disconnected'
                AND COALESCE(last_seen_ms, 0) > ?",
            params![seat.slot, cutoff],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if live > 0 {
        bail!(
            "'{}' still has a live agent in it. Stop it first -- or ask the operator, \
             who can remove a seat out from under one.",
            seat.name
        );
    }
    let agents = db.execute("DELETE FROM agents WHERE slot_id = ?", [&seat.slot])?;
    db.execute("DELETE FROM slots WHERE id = ?", [&seat.slot])?;
    audit(
        db,
        &actor_of(db, caller_slot),
        "slot_removed",
        json!({ "slot": seat.slot, "name": seat.name, "agents": agents }),
    );
    Ok(format!(
        "Removed '{}' ({}). Its messages stay in the session log.",
        seat.name, seat.slot
    ))
}

// ------------------------------------------------------ rename / move -------

pub fn rename_agent(
    db: &Connection,
    caller_slot: &str,
    caller_session: &str,
    needle: &str,
    new_name: &str,
) -> Result<String> {
    let caller = authority_of(db, caller_slot);
    let new_name = new_name.trim();
    if new_name.is_empty() {
        bail!("rename_agent needs a new name");
    }
    if new_name.chars().count() > 64 {
        bail!("a name must be 64 characters or fewer");
    }
    let seat = reachable(db, needle, caller, caller_session)?;
    let taken: bool = db
        .query_row(
            "SELECT 1 FROM slots WHERE session_id = ? AND display_name = ? AND id <> ?",
            params![seat.session, new_name, seat.slot],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if taken {
        bail!("'{new_name}' is already a seat in that team; pick another name");
    }
    db.execute(
        "UPDATE slots SET display_name = ? WHERE id = ?",
        params![new_name, seat.slot],
    )?;
    audit(
        db,
        &actor_of(db, caller_slot),
        "slot_renamed",
        json!({ "slot": seat.slot, "from": seat.name, "to": new_name }),
    );
    Ok(format!(
        "'{}' is now '{new_name}'. Same seat, same agent, same history, same inbox.",
        seat.name
    ))
}

/// Move a seat to another team.
///
/// Needs reach over BOTH sides, which makes this a superorchestrator's tool
/// in practice: an orchestrator reaches exactly one session, and moving a
/// seat from that session to that session is not a move. Stated plainly in
/// the refusal rather than left for the caller to work out.
pub fn move_agent(
    db: &Connection,
    caller_slot: &str,
    caller_session: &str,
    needle: &str,
    dest: &str,
) -> Result<String> {
    let caller = authority_of(db, caller_slot);
    let seat = reachable(db, needle, caller, caller_session)?;
    let dest_id: String = db
        .query_row(
            "SELECT id FROM sessions WHERE id = ?1 OR name = ?1",
            [dest.trim()],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| anyhow::anyhow!("no session '{dest}'"))?;
    if !caller.reaches(caller_session, &dest_id) {
        bail!(
            "moving a seat needs reach over the team it is going to as well as the one \
             it is in. An orchestrator reaches only its own session, so this is a \
             superorchestrator's or the operator's move."
        );
    }
    if dest_id == seat.session {
        bail!("'{}' is already in that team.", seat.name);
    }
    let taken: bool = db
        .query_row(
            "SELECT 1 FROM slots WHERE session_id = ? AND display_name = ?",
            params![dest_id, seat.name],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if taken {
        bail!(
            "that team already has a seat called '{}'; rename one of them first",
            seat.name
        );
    }
    // The workspace belongs to the session it was in, so it cannot come
    // along. Said out loud: a seat that silently lost its workspace would
    // look like a broken agent rather than a moved one.
    db.execute(
        "UPDATE slots SET session_id = ?, workspace_id = NULL WHERE id = ?",
        params![dest_id, seat.slot],
    )?;
    audit(
        db,
        &actor_of(db, caller_slot),
        "slot_moved",
        json!({ "slot": seat.slot, "from": seat.session, "to": dest_id }),
    );
    Ok(format!(
        "Moved '{}' to that team. It has no workspace there yet -- a workspace belongs \
         to the session it was created in, so this does not follow the seat.",
        seat.name
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;

    /// Two teams. `boss` runs team one, `sub` runs team two, and the rest are
    /// plain agents -- which is what every seat is until someone says
    /// otherwise.
    fn fleet() -> AppState {
        let st = AppState::in_memory().unwrap();
        let now = now_ms();
        {
            let db = st.db.lock().unwrap();
            for (id, name) in [("s1", "one"), ("s2", "two")] {
                db.execute(
                    "INSERT INTO sessions (id, name, created_at_ms, last_active_ms)
                     VALUES (?,?,?,?)",
                    params![id, name, now, now],
                )
                .unwrap();
            }
            for (id, sess, name, auth) in [
                ("sl-boss", "s1", "Boss", "orchestrator"),
                ("sl-max", "s1", "Max", "agent"),
                ("sl-iris", "s1", "Iris", "agent"),
                ("sl-sub", "s2", "Sub", "orchestrator"),
                ("sl-far", "s2", "Far", "agent"),
                ("sl-god", "s1", "God", "super"),
            ] {
                db.execute(
                    "INSERT INTO slots (id, session_id, display_name, authority, created_at_ms)
                     VALUES (?,?,?,?,?)",
                    params![id, sess, name, auth, now],
                )
                .unwrap();
            }
        }
        st
    }

    fn auth(st: &AppState, slot: &str) -> Authority {
        authority_of(&st.db.lock().unwrap(), slot)
    }

    #[test]
    fn a_seat_with_no_authority_set_is_a_plain_agent() {
        // The default has to be the powerless one: a seat that appears
        // without anyone deciding must not arrive able to command the fleet.
        let st = fleet();
        {
            let db = st.db.lock().unwrap();
            db.execute(
                "INSERT INTO slots (id, session_id, display_name, created_at_ms)
                 VALUES ('sl-new','s1','New',1)",
                [],
            )
            .unwrap();
        }
        assert_eq!(auth(&st, "sl-new"), Authority::Agent);
        // And so is a slot that does not exist at all.
        assert_eq!(auth(&st, "sl-nothing"), Authority::Agent);
    }

    #[test]
    fn an_orchestrator_elects_an_orchestrator_in_its_own_team() {
        // The thing that was asked for.
        let st = fleet();
        let db = st.db.lock().unwrap();
        let out = elect(&db, "sl-boss", "s1", "Max", "orchestrator").unwrap();
        assert!(out.contains("Max is now orchestrator"), "{out}");
        assert_eq!(authority_of(&db, "sl-max"), Authority::Orchestrator);
    }

    #[test]
    fn an_orchestrator_cannot_elect_into_another_team() {
        // And is told the seat is out of reach rather than that it exists:
        // an orchestrator must not be able to enumerate another team by
        // guessing at names.
        let st = fleet();
        let db = st.db.lock().unwrap();
        let e = elect(&db, "sl-boss", "s1", "Far", "orchestrator").unwrap_err().to_string();
        assert!(e.contains("no seat 'Far' that you can reach"), "{e}");
        assert_eq!(authority_of(&db, "sl-far"), Authority::Agent);
    }

    #[test]
    fn an_orchestrator_cannot_mint_a_super() {
        // It could otherwise mint one and be promoted by it a moment later,
        // which would make the whole boundary decorative.
        let st = fleet();
        let db = st.db.lock().unwrap();
        let e = elect(&db, "sl-boss", "s1", "Max", "super").unwrap_err().to_string();
        assert!(e.contains("cannot make a superorchestrator"), "{e}");
        assert_eq!(authority_of(&db, "sl-max"), Authority::Agent);
    }

    #[test]
    fn an_orchestrator_cannot_demote_a_super() {
        let st = fleet();
        let db = st.db.lock().unwrap();
        let e = elect(&db, "sl-boss", "s1", "God", "agent").unwrap_err().to_string();
        assert!(e.contains("cannot demote"), "{e}");
        assert_eq!(authority_of(&db, "sl-god"), Authority::Super);
    }

    #[test]
    fn a_plain_agent_elects_nobody_including_itself() {
        // The escalation that matters most: the seat has the tool name, and
        // the only thing standing in the way is this check.
        let st = fleet();
        let db = st.db.lock().unwrap();
        for target in ["Max", "Iris"] {
            let e = elect(&db, "sl-max", "s1", target, "orchestrator").unwrap_err().to_string();
            assert!(e.contains("holds no authority over the fleet"), "{e}");
        }
        assert_eq!(authority_of(&db, "sl-max"), Authority::Agent);
    }

    #[test]
    fn a_super_elects_across_teams_and_can_mint_another_super() {
        let st = fleet();
        let db = st.db.lock().unwrap();
        elect(&db, "sl-god", "s1", "Far", "super").unwrap();
        assert_eq!(authority_of(&db, "sl-far"), Authority::Super);
    }

    #[test]
    fn a_misspelled_level_is_refused_rather_than_read_as_agent() {
        // `Authority::parse` falls back to `agent`, which is right when
        // reading a column and would silently DEMOTE a seat here.
        let st = fleet();
        let db = st.db.lock().unwrap();
        let e = elect(&db, "sl-god", "s1", "Boss", "orchestrater").unwrap_err().to_string();
        assert!(e.contains("is not an authority"), "{e}");
        assert_eq!(authority_of(&db, "sl-boss"), Authority::Orchestrator);
    }

    #[test]
    fn electing_is_audited_against_the_seat_that_did_it() {
        // Never "operator": not being able to tell which caller changed the
        // fleet is what the token-holding orchestrator server cost us.
        let st = fleet();
        let db = st.db.lock().unwrap();
        elect(&db, "sl-boss", "s1", "Max", "orchestrator").unwrap();
        let (actor, detail): (String, String) = db
            .query_row(
                "SELECT actor, detail FROM audit WHERE action = 'authority_changed'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(actor.contains("sl-boss"), "{actor}");
        assert!(actor.contains("Boss"), "{actor}");
        assert!(detail.contains("\"to\":\"orchestrator\""), "{detail}");
    }

    #[test]
    fn list_seats_shows_one_team_to_an_orchestrator_and_the_fleet_to_a_super() {
        let st = fleet();
        let db = st.db.lock().unwrap();
        let mine: serde_json::Value =
            serde_json::from_str(&list_seats(&db, "sl-boss", "s1").unwrap()).unwrap();
        let names: Vec<&str> =
            mine["seats"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"Max"), "{names:?}");
        assert!(!names.contains(&"Far"), "an orchestrator must not see another team: {names:?}");
        assert_eq!(mine["reach"], "your own session");

        let all: serde_json::Value =
            serde_json::from_str(&list_seats(&db, "sl-god", "s1").unwrap()).unwrap();
        assert_eq!(all["seats"].as_array().unwrap().len(), 6);
        assert_eq!(all["your_authority"], "superorchestrator");
    }

    #[test]
    fn renaming_stays_inside_the_callers_reach() {
        let st = fleet();
        let db = st.db.lock().unwrap();
        rename_agent(&db, "sl-boss", "s1", "Max", "Maxine").unwrap();
        let e = rename_agent(&db, "sl-boss", "s1", "Far", "Nope").unwrap_err().to_string();
        assert!(e.contains("that you can reach"), "{e}");
    }

    #[test]
    fn a_duplicate_name_is_refused_in_either_direction() {
        // Three seats called "Hermes" in one team is how this was found the
        // first time.
        let st = fleet();
        let db = st.db.lock().unwrap();
        let e = rename_agent(&db, "sl-boss", "s1", "Max", "Iris").unwrap_err().to_string();
        assert!(e.contains("already a seat"), "{e}");
        let e = add_agent(
            &db, "sl-boss", "s1",
            &json!({ "name": "Iris", "cli": "claude" }),
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("already a seat"), "{e}");
    }

    #[test]
    fn an_orchestrator_adds_to_its_own_team_and_the_seat_starts_powerless() {
        let st = fleet();
        let db = st.db.lock().unwrap();
        let out = add_agent(
            &db, "sl-boss", "s1",
            &json!({ "name": "Nina", "cli": "claude", "role": "Dev" }),
        )
        .unwrap();
        assert!(out.contains("as a plain agent"), "{out}");
        let (sess, auth): (String, String) = db
            .query_row(
                "SELECT session_id, authority FROM slots WHERE display_name = 'Nina'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(sess, "s1");
        // A seat added BY an orchestrator does not inherit its authority.
        assert_eq!(auth, "agent");
    }

    #[test]
    fn an_orchestrator_cannot_add_to_another_team() {
        let st = fleet();
        let db = st.db.lock().unwrap();
        let e = add_agent(
            &db, "sl-boss", "s1",
            &json!({ "name": "Nina", "session": "two" }),
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("outside your reach"), "{e}");
    }

    #[test]
    fn a_seat_cannot_remove_itself_out_from_under_its_own_call() {
        let st = fleet();
        let db = st.db.lock().unwrap();
        let e = remove_agent(&db, "sl-boss", "s1", "Boss").unwrap_err().to_string();
        assert!(e.contains("your own seat"), "{e}");
    }

    #[test]
    fn removing_an_empty_seat_works_and_is_audited() {
        let st = fleet();
        let db = st.db.lock().unwrap();
        remove_agent(&db, "sl-boss", "s1", "Iris").unwrap();
        let left: i64 = db
            .query_row("SELECT COUNT(*) FROM slots WHERE id = 'sl-iris'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 0);
        let actor: String = db
            .query_row("SELECT actor FROM audit WHERE action = 'slot_removed'", [], |r| r.get(0))
            .unwrap();
        assert!(actor.contains("sl-boss"), "{actor}");
    }

    #[test]
    fn moving_needs_reach_over_both_ends() {
        // Which makes it a super's tool in practice, and the refusal says so
        // rather than leaving an orchestrator to work it out.
        let st = fleet();
        let db = st.db.lock().unwrap();
        let e = move_agent(&db, "sl-boss", "s1", "Max", "two").unwrap_err().to_string();
        assert!(e.contains("superorchestrator's or the operator's move"), "{e}");
        move_agent(&db, "sl-god", "s1", "Max", "two").unwrap();
        let sess: String = db
            .query_row("SELECT session_id FROM slots WHERE id = 'sl-max'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sess, "s2");
    }

    #[test]
    fn a_name_in_two_teams_resolves_to_the_callers_own() {
        // A display name is only unique within a session, and a super
        // naming one legitimately matches several. Picking at random would
        // eventually rename the wrong team's seat.
        let st = fleet();
        let db = st.db.lock().unwrap();
        db.execute(
            "INSERT INTO slots (id, session_id, display_name, authority, created_at_ms)
             VALUES ('sl-dup','s2','Max','agent',1)",
            [],
        )
        .unwrap();
        let seat = reachable(&db, "Max", Authority::Super, "s1").unwrap();
        assert_eq!(seat.slot, "sl-max");
        // From neither session it is ambiguous, and says so with both.
        let e = reachable(&db, "Max", Authority::Super, "s-other").unwrap_err().to_string();
        assert!(e.contains("more than one seat"), "{e}");
        assert!(e.contains("use the slot id"), "{e}");
    }
}

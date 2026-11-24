//! Role templates: what a seat in a given role is FOR.
//!
//! A seat already carries a system prompt and an instruction, and an operator
//! can write both by hand. The trouble with by hand is that it is written
//! once, for one seat, at the moment the seat is made -- so the fourth
//! reviewer on the fleet gets a worse brief than the first, and nothing says
//! what a "reviewer" is supposed to do here at all.
//!
//! So the common roles have templates. `--template reviewer` gives that seat
//! the same brief as every other reviewer, and improving the brief improves
//! all of them.
//!
//! WHY THEY ARE COMPILED IN. A template that lives in a file beside the hub
//! is a template that has to be deployed, and a fleet where one machine has
//! an older copy produces agents that disagree about their own job for
//! reasons nobody can see. These are in the binary, like the schema and the
//! skill. An operator who wants their own puts a file in `--prompts-dir`,
//! which wins over the built-in of the same name -- so local changes survive
//! an upgrade instead of being overwritten by it.
//!
//! WHAT A TEMPLATE DELIBERATELY DOES NOT SAY. How messages work, who owns
//! what, how to reach the operator: that is the team protocol, it is in
//! every first prompt already, and repeating it per role is how the two
//! drift apart. A template says what the JOB is.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// One role's brief.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleTemplate {
    /// How it is named on a command line: `reviewer`, `sysadmin`.
    pub id: String,
    /// How it is shown to a person: `Reviewer`.
    pub title: String,
    /// One line, for an operator choosing between them.
    pub summary: String,
    /// Skills this role's brief carries, by name. Each is a reusable
    /// fragment -- how to review, how to work on a live machine -- that
    /// several roles want and none should restate.
    #[serde(default)]
    pub skills: Vec<String>,
    /// The brief itself.
    pub prompt: String,
}

/// Every built-in template, as the TOML it is written in.
///
/// An explicit list rather than a directory walk, because `include_str!`
/// needs one -- and because a list is reviewable: adding a role is a line
/// here, and a role nobody added cannot appear by accident.
const BUILTIN_ROLES: &[(&str, &str)] = &[
    ("orchestrator", include_str!("../roles/orchestrator.toml")),
    ("supervisor", include_str!("../roles/supervisor.toml")),
    ("coder", include_str!("../roles/coder.toml")),
    ("engineer", include_str!("../roles/engineer.toml")),
    ("hacker", include_str!("../roles/hacker.toml")),
    ("reviewer", include_str!("../roles/reviewer.toml")),
    ("planner", include_str!("../roles/planner.toml")),
    ("coordinator", include_str!("../roles/coordinator.toml")),
    ("designer", include_str!("../roles/designer.toml")),
    ("sysadmin", include_str!("../roles/sysadmin.toml")),
    ("reverser", include_str!("../roles/reverser.toml")),
    ("webmaster", include_str!("../roles/webmaster.toml")),
    ("assistant", include_str!("../roles/assistant.toml")),
    ("observer", include_str!("../roles/observer.toml")),
];

/// Every built-in skill fragment.
const BUILTIN_SKILLS: &[(&str, &str)] = &[
    ("small-steps", include_str!("../skills/small-steps.md")),
    ("handover", include_str!("../skills/handover.md")),
    ("review-craft", include_str!("../skills/review-craft.md")),
    ("plan-shape", include_str!("../skills/plan-shape.md")),
    ("live-systems", include_str!("../skills/live-systems.md")),
    ("authorised-scope", include_str!("../skills/authorised-scope.md")),
    ("secrets-hygiene", include_str!("../skills/secrets-hygiene.md")),
    ("independent-evidence", include_str!("../skills/independent-evidence.md")),
    ("interface-states", include_str!("../skills/interface-states.md")),
];

/// The names of the built-in roles, in the order they are declared.
pub fn builtin_ids() -> Vec<&'static str> {
    BUILTIN_ROLES.iter().map(|(id, _)| *id).collect()
}

/// The names of the built-in skills.
pub fn builtin_skill_names() -> Vec<&'static str> {
    BUILTIN_SKILLS.iter().map(|(n, _)| *n).collect()
}

/// Every skill this hub can offer: the built-in ones, plus any a fleet has
/// put in its own directory, sorted and without duplicates.
///
/// What a surface draws a checkbox from. The built-in list alone would be
/// wrong on a fleet with skills of its own -- a `house-style` fragment that
/// a template names but no checkbox offers is a skill nobody can turn back
/// on once it is off.
pub fn all_skill_names(dir: Option<&Path>) -> Vec<String> {
    let mut out: Vec<String> =
        BUILTIN_SKILLS.iter().map(|(n, _)| (*n).to_string()).collect();
    if let Some(dir) = dir {
        if let Ok(entries) = std::fs::read_dir(dir.join("skills")) {
            for e in entries.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) != Some("md") {
                    continue;
                }
                if let Some(stem) = p.file_stem().and_then(|x| x.to_str()) {
                    let name = stem.to_lowercase();
                    if !out.contains(&name) {
                        out.push(name);
                    }
                }
            }
        }
    }
    out.sort();
    out
}

/// Every template, with any in `dir` replacing the built-in of the same name
/// and any new ones added.
///
/// A file that will not parse is SKIPPED and reported, not fatal: one bad
/// local template must not take away the other thirteen, and a hub that
/// refuses to start over a typo in a prompt is worse than one that starts
/// without it.
pub fn all(dir: Option<&Path>) -> Vec<RoleTemplate> {
    let mut out: Vec<RoleTemplate> = Vec::new();
    for (id, text) in BUILTIN_ROLES {
        // A built-in that does not parse is a bug in this crate, caught by
        // the test below rather than in production.
        let _ = id;
        if let Ok(t) = toml::from_str::<RoleTemplate>(text) {
            out.push(t);
        }
    }
    if let Some(dir) = dir {
        for t in local(&dir.join("roles")) {
            match out.iter().position(|o| o.id == t.id) {
                Some(i) => out[i] = t,
                None => out.push(t),
            }
        }
    }
    out
}

fn local(dir: &Path) -> Vec<RoleTemplate> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("toml") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&p) else { continue };
        if let Ok(t) = toml::from_str::<RoleTemplate>(&text) {
            out.push(t);
        }
    }
    out
}

/// What is wrong with the templates in `dir`, in sentences.
///
/// This crate has no logger -- it is the shared protocol and is kept free of
/// one on purpose -- and a local template that will not parse must not be
/// silently absent either: an operator who wrote one and sees the built-in
/// behaviour has nothing to go on. So the problems are RETURNED, and the hub
/// says them at startup where they will be read.
pub fn complaints(dir: Option<&Path>) -> Vec<String> {
    let Some(dir) = dir else { return Vec::new() };
    let mut out = Vec::new();
    let roles = dir.join("roles");
    if let Ok(entries) = std::fs::read_dir(&roles) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("toml") {
                continue;
            }
            match std::fs::read_to_string(&p) {
                Err(err) => out.push(format!("{}: cannot be read ({err})", p.display())),
                Ok(text) => {
                    if let Err(err) = toml::from_str::<RoleTemplate>(&text) {
                        out.push(format!("{}: {err}", p.display()));
                    }
                }
            }
        }
    }
    out
}

/// One template by name, local copies winning.
pub fn find(dir: Option<&Path>, id: &str) -> Option<RoleTemplate> {
    let id = id.trim().to_lowercase();
    all(dir).into_iter().find(|t| t.id == id)
}

/// One skill fragment's text, local copies winning.
pub fn skill(dir: Option<&Path>, name: &str) -> Option<String> {
    let name = name.trim().to_lowercase();
    if let Some(dir) = dir {
        let p = dir.join("skills").join(format!("{name}.md"));
        if let Ok(text) = std::fs::read_to_string(&p) {
            return Some(text);
        }
    }
    BUILTIN_SKILLS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, text)| (*text).to_string())
}

/// A skill fragment with its YAML front matter taken off.
///
/// The front matter is there so a skill-aware CLI can load the same file
/// from its own skills directory. Injected into a prompt it is noise, and
/// `name:` / `description:` lines read to a model as something to act on.
///
/// It also carries a `sources:` list -- where the claims in that fragment
/// came from, for the operator reading the file and deciding whether to keep
/// them. That is exactly the kind of thing that should not reach a model's
/// context, and stripping the front matter is what keeps it out.
fn body_of(text: &str) -> &str {
    let t = text.trim_start();
    let Some(rest) = t.strip_prefix("---") else { return t };
    match rest.find("\n---") {
        Some(end) => rest[end + 4..].trim_start_matches('\n'),
        None => t,
    }
}

/// The text a seat is given: the role's brief, then the skills it carries.
///
/// `skills` overrides the template's own list when it is `Some`, which is
/// what lets an operator take one of them off or add one the template does
/// not name. `None` means "whatever the template says".
///
/// A skill that cannot be found is named in the output rather than silently
/// dropped: a brief missing a section it was told to carry should say so,
/// where the operator who asked for it will see it.
pub fn render(t: &RoleTemplate, skills: Option<&[String]>, dir: Option<&Path>) -> String {
    let wanted: Vec<String> = match skills {
        Some(s) => s.iter().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect(),
        None => t.skills.clone(),
    };
    let mut out = String::new();
    out.push_str(&format!("YOUR ROLE: {}\n\n", t.title.to_uppercase()));
    out.push_str(t.prompt.trim());
    out.push('\n');
    for name in &wanted {
        out.push_str("\n\n");
        match skill(dir, name) {
            Some(text) => {
                out.push_str(&format!("--- HOW TO DO THIS WELL: {name} ---\n\n"));
                out.push_str(body_of(&text).trim());
                out.push('\n');
            }
            None => {
                out.push_str(&format!(
                    "--- NOTE: this role is meant to carry a '{name}' section, and this \
                     hub has no such skill. Ask the operator for it. ---\n"
                ));
            }
        }
    }
    out
}

// ---------------------------------------------------------------- editing ----
//
// Built-ins are compiled in and never touched. Everything here writes or
// removes a LOCAL file in `dir`, which `all`/`skill`/`find` already prefer
// over the built-in of the same name -- so "edit a built-in" is "write a local
// copy", "reset" is "delete the local copy", and the built-in cannot be lost.
// An operator with no `--prompts-dir` cannot edit, which is why the hub now
// provisions one.

/// A name that is safe to use as a file stem and to address on a command line.
///
/// Lowercased, `[a-z0-9-]`, 1..=40 chars. This is the whole defence against a
/// write escaping the skills/roles directory: no separators, no dots, no `..`,
/// so `dir.join(format!("{name}.md"))` cannot land anywhere but in `dir`.
pub fn valid_name(name: &str) -> Result<String, String> {
    let n = name.trim().to_lowercase();
    if n.is_empty() || n.len() > 40 {
        return Err("a name is 1 to 40 characters".into());
    }
    if !n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
        return Err("a name is lowercase letters, digits and hyphens only".into());
    }
    Ok(n)
}

/// Where a skill or template comes from, as a surface badges it:
///   * `builtin`  -- the compiled-in one, OR a local file byte-identical to it
///                   (which is what the installer's seeding leaves behind, and
///                   which is NOT an edit, so it must not read as one);
///   * `override` -- a local file whose contents DIFFER from the built-in;
///   * `local`    -- a local file with no built-in of that name.
///
/// Comparing contents, not merely testing for a file, is the whole point: the
/// store is seeded with every built-in so they are there to edit, and a badge
/// that called all of those "edited" would mean nothing. Seeding writes the
/// built-in's exact bytes, so an untouched seeded file compares equal.
pub fn skill_source(dir: Option<&Path>, name: &str) -> &'static str {
    let n = name.trim().to_lowercase();
    let builtin = BUILTIN_SKILLS.iter().find(|(b, _)| *b == n).map(|(_, t)| *t);
    let local = dir.and_then(|d| std::fs::read_to_string(d.join("skills").join(format!("{n}.md"))).ok());
    source_word(builtin, local.as_deref())
}

pub fn role_source(dir: Option<&Path>, id: &str) -> &'static str {
    let n = id.trim().to_lowercase();
    let builtin = BUILTIN_ROLES.iter().find(|(b, _)| *b == n).map(|(_, t)| *t);
    let local = dir.and_then(|d| std::fs::read_to_string(d.join("roles").join(format!("{n}.toml")).as_path()).ok());
    source_word(builtin, local.as_deref())
}

fn source_word(builtin: Option<&str>, local: Option<&str>) -> &'static str {
    match (builtin, local) {
        // A local copy that matches the built-in is not an edit -- it is the
        // seeded file sitting untouched.
        (Some(b), Some(l)) if b == l => "builtin",
        (Some(_), Some(_)) => "override",
        (None, Some(_)) => "local",
        _ => "builtin",
    }
}

/// The directory a store must have, made if missing. Both subdirectories,
/// because a reader of one should not fail because the other is absent.
fn ensure_store(dir: &Path) -> Result<(), String> {
    for sub in ["skills", "roles"] {
        std::fs::create_dir_all(dir.join(sub))
            .map_err(|e| format!("cannot make {}: {e}", dir.join(sub).display()))?;
    }
    Ok(())
}

/// Write (create or replace) a local skill fragment. `body` is stored as the
/// operator wrote it: a fragment may carry YAML front matter (the built-ins
/// do, so a skill-aware CLI can load the same file) or not -- `render` strips
/// it with `body_of` either way, so neither form reaches a model wrong.
pub fn write_skill(dir: &Path, name: &str, body: &str) -> Result<String, String> {
    let n = valid_name(name)?;
    if body.trim().is_empty() {
        return Err("a skill cannot be empty".into());
    }
    if body.len() > 64 * 1024 {
        return Err("a skill fragment this long is not a fragment".into());
    }
    ensure_store(dir)?;
    let p = dir.join("skills").join(format!("{n}.md"));
    std::fs::write(&p, body).map_err(|e| format!("cannot write {}: {e}", p.display()))?;
    Ok(n)
}

/// Remove a local skill file. Returns whether a built-in of that name remains
/// (so the caller can say "reverted to the built-in" vs "gone"). Removing one
/// that has no local file is not an error: the end state is what was asked.
pub fn delete_skill(dir: &Path, name: &str) -> Result<bool, String> {
    let n = valid_name(name)?;
    let p = dir.join("skills").join(format!("{n}.md"));
    if p.exists() {
        std::fs::remove_file(&p).map_err(|e| format!("cannot remove {}: {e}", p.display()))?;
    }
    Ok(BUILTIN_SKILLS.iter().any(|(b, _)| *b == n))
}

/// Write (create or replace) a local role template, as TOML. Validated by
/// round-tripping through `RoleTemplate` so a surface cannot store a file the
/// loader would later skip as unparseable -- the failure is returned now,
/// where the operator is, not swallowed at the next hub start.
pub fn write_role(dir: &Path, t: &RoleTemplate) -> Result<String, String> {
    let id = valid_name(&t.id)?;
    if t.title.trim().is_empty() || t.summary.trim().is_empty() {
        return Err("a template needs a title and a one-line summary".into());
    }
    if t.prompt.trim().len() < 40 {
        return Err("a template's brief is too thin to be worth having".into());
    }
    for s in &t.skills {
        valid_name(s).map_err(|e| format!("skill '{s}': {e}"))?;
    }
    let canonical = RoleTemplate { id: id.clone(), ..t.clone() };
    let text = toml::to_string_pretty(&canonical).map_err(|e| format!("cannot serialise: {e}"))?;
    // Prove it loads back before it is stored.
    toml::from_str::<RoleTemplate>(&text).map_err(|e| format!("would not parse back: {e}"))?;
    ensure_store(dir)?;
    let p = dir.join("roles").join(format!("{id}.toml"));
    std::fs::write(&p, text).map_err(|e| format!("cannot write {}: {e}", p.display()))?;
    Ok(id)
}

pub fn delete_role(dir: &Path, id: &str) -> Result<bool, String> {
    let n = valid_name(id)?;
    let p = dir.join("roles").join(format!("{n}.toml"));
    if p.exists() {
        std::fs::remove_file(&p).map_err(|e| format!("cannot remove {}: {e}", p.display()))?;
    }
    Ok(BUILTIN_ROLES.iter().any(|(b, _)| *b == n))
}

/// The built-in skill fragments as (name, text), for seeding a fresh store.
pub fn builtin_skills() -> &'static [(&'static str, &'static str)] {
    BUILTIN_SKILLS
}

/// The built-in templates as (id, toml), for seeding a fresh store.
pub fn builtin_roles() -> &'static [(&'static str, &'static str)] {
    BUILTIN_ROLES
}

/// The skills a seat actually carries, resolving enable/disable across scopes.
///
/// `defaults` is the template's own list. `overlays` are per-scope maps of
/// skill-name -> enabled, ordered WIDEST FIRST (global, then session, then the
/// seat): a nearer scope wins, exactly as a nearer memory scope shadows a
/// wider one. A skill the template does not name can be turned ON by an
/// overlay, and one it does name can be turned OFF -- the operator's three
/// dials, resolved to the single set `render` is given.
///
/// Pure on purpose: the hub reads the three scope maps from `skill_prefs` and
/// passes them here, so this crate stays free of the database and the rule is
/// unit-testable on its own.
pub fn resolve_skills(
    defaults: &[String],
    overlays: &[std::collections::BTreeMap<String, bool>],
) -> Vec<String> {
    // Start from the template's defaults (order preserved), then let each
    // overlay add or remove. A name's final state is whatever the nearest
    // overlay that mentions it says; if none does, the default stands.
    let mut enabled: Vec<String> = Vec::new();
    let norm = |s: &str| s.trim().to_lowercase();
    for d in defaults {
        let d = norm(d);
        if !d.is_empty() && !enabled.contains(&d) {
            enabled.push(d);
        }
    }
    for overlay in overlays {
        for (name, on) in overlay {
            let name = norm(name);
            if name.is_empty() {
                continue;
            }
            if *on {
                if !enabled.contains(&name) {
                    enabled.push(name);
                }
            } else {
                enabled.retain(|e| e != &name);
            }
        }
    }
    enabled
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_template_parses_and_is_complete() {
        // A built-in that does not parse is skipped at runtime, which would
        // make a role quietly stop existing. This is the test that stops
        // that reaching a fleet.
        let all = all(None);
        assert_eq!(all.len(), BUILTIN_ROLES.len(), "one or more did not parse");
        for t in &all {
            assert!(!t.title.trim().is_empty(), "{} has no title", t.id);
            assert!(!t.summary.trim().is_empty(), "{} has no summary", t.id);
            assert!(t.prompt.len() > 200, "{}'s brief is too thin to be worth having", t.id);
            assert_eq!(t.id, t.id.to_lowercase(), "{} is not addressable", t.id);
        }
    }

    #[test]
    fn a_templates_file_name_is_the_role_it_declares() {
        // Otherwise `--template coder` finds a file called coder.toml that
        // calls itself something else, and the error makes no sense.
        for (name, text) in BUILTIN_ROLES {
            let t: RoleTemplate = toml::from_str(text).expect(name);
            assert_eq!(&t.id, name);
        }
    }

    #[test]
    fn every_skill_a_template_names_exists() {
        // A template naming a skill nobody shipped renders a note asking the
        // operator for it. Fine as a fallback; not fine for a built-in.
        for t in all(None) {
            for s in &t.skills {
                assert!(skill(None, s).is_some(), "{} names a missing skill '{}'", t.id, s);
            }
        }
    }

    #[test]
    fn the_roles_asked_for_are_all_there() {
        // Named explicitly: these are the roles this fleet works in, and a
        // rename or a deletion should fail here rather than at the moment
        // somebody tries to make that seat.
        for want in [
            "orchestrator", "supervisor", "coder", "engineer", "hacker", "reviewer",
            "planner", "coordinator", "designer", "sysadmin", "reverser", "webmaster",
            "assistant", "observer",
        ] {
            assert!(find(None, want).is_some(), "no '{want}' template");
        }
    }

    #[test]
    fn every_role_that_writes_anything_carries_something() {
        // The observer writes nothing, so it carries nothing. Every other
        // role has at least one thing its agents get wrong often enough to
        // be worth the words.
        for t in all(None) {
            if t.id == "observer" {
                assert!(t.skills.is_empty(), "an observer has nothing to carry");
                continue;
            }
            assert!(!t.skills.is_empty(), "{} carries no skills", t.id);
            assert!(t.skills.len() <= 4, "{} carries too many to be read", t.id);
        }
    }

    #[test]
    fn the_skills_a_role_carries_are_the_ones_it_needs() {
        // Named, because these were chosen from what agents in each role
        // actually get wrong and a later edit should have to argue with it.
        for (role, want) in [
            ("reviewer", "review-craft"),
            ("sysadmin", "live-systems"),
            ("hacker", "authorised-scope"),
            ("designer", "interface-states"),
            ("planner", "plan-shape"),
            ("coder", "secrets-hygiene"),
            ("supervisor", "independent-evidence"),
        ] {
            let t = find(None, role).unwrap();
            assert!(t.skills.iter().any(|s| s == want), "{role} should carry {want}");
        }
    }

    #[test]
    fn rendering_carries_the_roles_own_skills() {
        let t = find(None, "reviewer").unwrap();
        let text = render(&t, None, None);
        assert!(text.contains("YOUR ROLE: REVIEWER"));
        assert!(text.contains("read other people's work".to_uppercase().as_str())
            || text.contains("REVIEW"));
        assert!(text.contains("HOW TO DO THIS WELL: review-craft"),
                "the reviewer's brief carries the reviewing skill");
    }

    #[test]
    fn the_front_matter_is_not_injected() {
        // `name:` and `description:` lines are instructions to a CLI's skill
        // loader, not to a model reading a prompt.
        let text = render(&find(None, "coder").unwrap(), None, None);
        assert!(!text.contains("description:"), "front matter leaked into the brief");
        assert!(!text.contains("---\nname:"));
    }

    #[test]
    fn an_operator_can_choose_the_skills_instead() {
        let t = find(None, "coder").unwrap();
        let text = render(&t, Some(&["live-systems".to_string()]), None);
        assert!(text.contains("HOW TO DO THIS WELL: live-systems"));
        assert!(!text.contains("HOW TO DO THIS WELL: small-steps"),
                "an explicit list replaces the template's own");
    }

    #[test]
    fn asking_for_no_skills_gives_the_brief_alone() {
        let t = find(None, "coder").unwrap();
        let text = render(&t, Some(&[]), None);
        assert!(!text.contains("HOW TO DO THIS WELL"));
        assert!(text.contains("YOUR ROLE: CODER"));
    }

    #[test]
    fn a_skill_that_is_not_there_says_so_in_the_brief() {
        let t = find(None, "coder").unwrap();
        let text = render(&t, Some(&["no-such-skill".to_string()]), None);
        assert!(text.contains("no such skill"), "{text}");
    }

    #[test]
    fn a_local_template_replaces_the_builtin_of_the_same_name() {
        // What makes a local change survive an upgrade.
        let dir = std::env::temp_dir().join(format!("nexmesh-roles-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("roles")).unwrap();
        std::fs::write(
            dir.join("roles/coder.toml"),
            "id = \"coder\"\ntitle = \"Our Coder\"\nsummary = \"ours\"\n\
             skills = []\nprompt = \"Do it our way.\"\n",
        )
        .unwrap();
        let t = find(Some(&dir), "coder").unwrap();
        assert_eq!(t.title, "Our Coder");
        assert!(render(&t, None, Some(&dir)).contains("Do it our way."));
        // And the others are still there.
        assert!(find(Some(&dir), "reviewer").is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_local_template_that_will_not_parse_is_skipped_not_fatal() {
        let dir = std::env::temp_dir().join(format!("nexmesh-roles-bad-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("roles")).unwrap();
        std::fs::write(dir.join("roles/broken.toml"), "this is not toml = = =").unwrap();
        assert_eq!(all(Some(&dir)).len(), BUILTIN_ROLES.len());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_local_skill_replaces_the_builtin_one() {
        let dir = std::env::temp_dir().join(format!("nexmesh-skills-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("skills")).unwrap();
        std::fs::write(dir.join("skills/handover.md"), "---\nname: handover\n---\n\nOurs.\n")
            .unwrap();
        assert_eq!(body_of(&skill(Some(&dir), "handover").unwrap()).trim(), "Ours.");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_observer_is_told_plainly_what_it_cannot_do() {
        // The one template whose content is load-bearing: the seat it
        // describes is restricted in the hub as well, and the brief has to
        // agree with the restriction or the agent spends its turns trying.
        let text = render(&find(None, "observer").unwrap(), None, None);
        for phrase in ["never", "read-only", "You watch"] {
            assert!(
                text.to_lowercase().contains(&phrase.to_lowercase()),
                "the observer's brief should say {phrase:?}"
            );
        }
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir()
            .join(format!("nexmesh-roles-{}-{}-{tag}", std::process::id(), now_nanos()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    fn now_nanos() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    #[test]
    fn a_name_cannot_escape_the_store() {
        // Anything with a separator, a dot, whitespace, or over length is
        // refused outright -- these are the ways a write could land outside
        // the skills/roles directory.
        for bad in ["../evil", "a/b", "", " ", "with.dot", "a b", &"x".repeat(41)] {
            assert!(valid_name(bad).is_err(), "{bad:?} should be rejected");
        }
        for good in ["house-style", "ocr", "x", "a1-b2"] {
            assert_eq!(valid_name(good).unwrap(), good);
        }
        // Uppercase is NORMALISED, not rejected: names are looked up
        // lowercased everywhere (`skill`, `find`), so "Ocr" is "ocr".
        assert_eq!(valid_name("Ocr").unwrap(), "ocr");
    }

    #[test]
    fn a_written_skill_is_read_back_and_a_builtin_is_untouched_by_delete() {
        let dir = scratch("skw");
        // Edit a built-in: a local copy shadows it, and the source says so.
        // Seeding writes the built-in's exact bytes; that is NOT an edit, and
        // must read as the built-in, or every seeded file would look edited.
        let builtin_handover = skill(None, "handover").unwrap();
        write_skill(&dir, "handover", &builtin_handover).unwrap();
        assert_eq!(skill_source(Some(&dir), "handover"), "builtin",
                   "a seeded, unmodified copy is not an override");
        write_skill(&dir, "handover", "---\nname: handover\n---\n\nOur handover.\n").unwrap();
        assert_eq!(skill_source(Some(&dir), "handover"), "override");
        assert_eq!(body_of(&skill(Some(&dir), "handover").unwrap()).trim(), "Our handover.");
        // Reset: the local copy goes, the built-in returns, and the name is
        // still a built-in (so the caller can say "reverted").
        assert!(delete_skill(&dir, "handover").unwrap(), "handover is a built-in");
        assert_eq!(skill_source(Some(&dir), "handover"), "builtin");
        assert_ne!(body_of(&skill(Some(&dir), "handover").unwrap()).trim(), "Our handover.");
        // A brand-new local skill, and its removal.
        write_skill(&dir, "house-style", "Two spaces, never tabs.").unwrap();
        assert_eq!(skill_source(Some(&dir), "house-style"), "local");
        assert!(all_skill_names(Some(&dir)).iter().any(|s| s == "house-style"));
        assert!(!delete_skill(&dir, "house-style").unwrap(), "not a built-in");
        assert_eq!(skill_source(Some(&dir), "house-style"), "builtin",
                   "gone from local, and not a built-in either -> reported as builtin-absent");
        assert!(write_skill(&dir, "x", "   ").is_err(), "empty is refused");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_written_role_round_trips_and_a_broken_one_is_refused_before_it_is_stored() {
        let dir = scratch("rw");
        let t = RoleTemplate {
            id: "housekeeper".into(),
            title: "Housekeeper".into(),
            summary: "Keeps the tree tidy.".into(),
            skills: vec!["handover".into()],
            prompt: "You keep the working tree tidy and the branches pruned, nothing more."
                .into(),
        };
        write_role(&dir, &t).unwrap();
        assert_eq!(role_source(Some(&dir), "housekeeper"), "local");
        let back = find(Some(&dir), "housekeeper").unwrap();
        assert_eq!(back.title, "Housekeeper");
        assert_eq!(back.skills, vec!["handover".to_string()]);
        // A thin brief is refused, and nothing is written.
        let thin = RoleTemplate { prompt: "too short".into(), ..t.clone() };
        assert!(write_role(&dir, &thin).is_err());
        // Overriding a built-in, then reverting it.
        let r = RoleTemplate { id: "reviewer".into(), ..t.clone() };
        write_role(&dir, &r).unwrap();
        assert_eq!(role_source(Some(&dir), "reviewer"), "override");
        assert!(delete_role(&dir, "reviewer").unwrap(), "reviewer is a built-in");
        assert_eq!(role_source(Some(&dir), "reviewer"), "builtin");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn skills_resolve_nearest_scope_wins() {
        use std::collections::BTreeMap;
        let defaults = vec!["review-craft".to_string(), "handover".to_string()];
        let map = |pairs: &[(&str, bool)]| -> BTreeMap<String, bool> {
            pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
        };
        // global turns an extra one on; session turns a default off; seat
        // turns the global-added one back off and adds another. Order is
        // widest-first, nearest wins.
        let global = map(&[("live-systems", true)]);
        let session = map(&[("handover", false)]);
        let seat = map(&[("live-systems", false), ("secrets-hygiene", true)]);
        let got = resolve_skills(&defaults, &[global, session, seat]);
        assert!(got.contains(&"review-craft".to_string()), "default kept: {got:?}");
        assert!(!got.contains(&"handover".to_string()), "session disabled it: {got:?}");
        assert!(!got.contains(&"live-systems".to_string()), "seat overrode global: {got:?}");
        assert!(got.contains(&"secrets-hygiene".to_string()), "seat added it: {got:?}");
        // No overlays at all is just the defaults.
        assert_eq!(resolve_skills(&defaults, &[]), defaults);
    }
}

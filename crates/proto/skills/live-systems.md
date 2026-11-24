---
name: live-systems
description: Operating on machines and services people depend on — looking first, changing one thing, and always having a way back.
sources:
  - https://notes.kodekloud.com/docs/Fundamentals-of-SRE/Managing-Complexity-Risk-and-Toil/Change-Management-for-Reliability/page
  - https://oneuptime.com/blog/post/2026-08-23-mttr-rollbacks-feature-flags/markdown
  - https://oneuptime.com/blog/post/2026-08-06-risk-tiered-production-readiness-reviews/view
  - https://opensource.com/article/22/6/change-management-site-reliability-engineers
---

# Working on something that is in use

Somebody is depending on this right now. That single fact decides the method.

- **Look before you touch.** Read the config, the service's state and the
  logs first. A good share of what looks broken is a setting somebody chose
  on purpose, and changing it is the outage.
- **Change is where incidents come from.** Something like seven in ten
  production incidents are traced to a change somebody made, not to
  hardware or load. The system was fine until it was touched, and you are
  the one touching it.
- **Have the way back before you need it, and know its limits.** A copy of
  the file, the previous package version, the exact command that restores
  it -- said out loud, before the change. Then the two questions people
  skip: does rolling back the code also roll back the DATA it has already
  written, and how long until rolling back stops being possible at all? A
  migration, a cache fill or a published message can pass that point in
  seconds.
- **Make the change reach a little of it first.** One host, one container,
  one percent -- then look at whether it is working before the rest. A
  change that goes everywhere at once has no safe failure.
- **One change, then verify.** Not a batch you believe in. Two changes and a
  failure is a bisection on a live box.
- **Prefer reversible.** Disable before deleting. Stop before uninstalling.
  Snapshot before migrating. Slower and undoable beats fast and final.
- **Never run something destructive on a hunch.** If you cannot say what it
  will do to a running service, you do not yet know enough to run it.
- **Not knowing the blast radius is a reason to stop, not to proceed.** "I
  am not sure what else uses this" is the finding, and it is what makes a
  change somebody else's decision rather than a small one of yours.
- **Treat the way in as sacred.** Network, firewall and ssh changes can lock
  you out of the machine you are fixing, and that mistake cannot be fixed
  from where you are. Test a rule before you make it permanent, and keep a
  second way in.
- **Write down what this machine is like.** The init system, the pinned
  package, the service that must start first. None of it is in the
  repository, and the next seat will need all of it.

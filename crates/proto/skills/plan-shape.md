---
name: plan-shape
description: What a plan has to look like to be usable by somebody else — steps that end somewhere observable, ordered so stopping halfway is safe.
sources:
  - https://www.pluralsight.com/resources/blog/software-development/spec-driven-development-with-AI-SDD
  - https://www.testmuai.com/blog/spec-driven-development/
  - https://arxiv.org/pdf/2602.00180
  - https://www.augmentcode.com/guides/what-is-spec-driven-development
---

# The shape of a usable plan

The test of a plan is not whether it is right. It is whether another seat can
pick up any step, know what to do, and know when it is finished.

- **Every step ends somewhere observable.** A test that passes, a thing that
  runs, a question answered, a number measured. "Refactor the module" is not
  a step. "Split X out of Y so Y no longer imports Z, tests still green" is.
- **Order it so stopping halfway is safe.** A plan that is only correct once
  finished cannot be paused, reviewed or handed over — and it will be paused.
- **Unknowns are steps.** "Find out whether the API supports batching" is
  real work with a real result. A plan that hides an unknown spends the
  team's time discovering it the expensive way.
- **Say the outcome, not the method,** where the seat doing the work knows
  the method better than you do. Over-specifying is how a plan makes the
  work worse.
- **Name what each step must not touch.** On a shared workspace that is the
  difference between parallel work and two seats undoing each other.
- **Keep it where the team reads it** and keep it current. A plan only its
  author can see is a note, not a plan.

**Write criteria an agent can satisfy, not adjectives.** "The endpoint is
secure" is documentation; "the endpoint answers an unauthenticated request
with 401" is something somebody can build to and somebody else can check.
Every criterion worth writing maps to at least one test. If you cannot say
what would demonstrate a step is done, that is the part of the plan that is
not finished.

**Say the NON-GOALS.** What this work is deliberately not doing, and what it
must not touch. On a shared workspace that is the difference between
parallel work and two seats undoing each other -- and for an agent it is the
only thing standing between a task and a rewrite of the neighbourhood.

**Expect questions before code.** A plan that nobody asked anything about
was either perfect or unread. Invite the holes to be found while they are
still cheap.

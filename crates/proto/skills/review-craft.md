---
name: review-craft
description: How to review somebody else's work so the review finds defects that matter and is usable by the person who has to fix them.
sources:
  - https://docs.github.com/en/copilot/tutorials/review-ai-generated-code
  - https://dev.to/pyor/reviewing-ai-generated-code-a-practical-checklist-4017
  - https://www.augmentcode.com/guides/reviewing-ai-generated-code
  - https://spectrum.ieee.org/ai-code-review-software-engineers
---

# Reviewing well

A review is worth exactly the defects it finds that would otherwise have
shipped. Everything else in it competes with those for attention.

**Review against the goal.** Find out what the change was for first. Work
reviewed against the wrong goal gets rejected for being right.

**Look where things actually break:**

- the error path nobody wrote, and what the caller sees when it fires
- an assumption that holds in the test and not in production (ordering,
  empties, concurrency, clocks, encodings, sizes)
- state left behind on failure: a half-written file, a held lock, a
  transaction nobody rolled back
- a migration that works forwards and not backwards
- a change that is correct and makes a neighbouring caller wrong

**And at the three that are specific to code a model wrote**, because they
are the ones a human author rarely produces and a reviewer rarely looks for:

- **Does that API exist, at the version this project pins?** Plausible
  method names, plausible arguments, plausible imports -- for a library
  release that is not the one in the lockfile, or for a package that does
  not exist at all. Check the call against the real documentation for the
  pinned version, and check a new dependency is real and maintained before
  it arrives.
- **Authenticated is not authorised.** A very common shape is code that
  confirms there IS a user and never asks whether THIS user may do THIS
  thing. Anything touching identity, access or ownership is high-risk by
  default.
- **The tests came from the same assumptions as the code.** They were
  written beside it, so they pass for the reason it is wrong. Read them as
  part of the change under review, not as evidence about it.

**Be concrete or be honest about being unsure.** "This panics when the list
is empty" is a finding. "Consider edge cases" is noise. If you cannot name
the input that breaks it, say explicitly that it is a suspicion.

**Rank them.** Three real defects first. Style notes last, or not at all.

**Do not rewrite it.** The work belongs to whoever wrote it; send them the
defect. A reviewer who edits has reviewed their own code, and nobody has
reviewed theirs.

**If you cannot explain the change in your own words, the review is not
finished.** Not "it looks right" -- what it does, and why that is the right
thing. That sentence is also the one thing an approval is worth.

**Say what you checked when you approve.** An approval with nothing behind it
stops anybody else looking.

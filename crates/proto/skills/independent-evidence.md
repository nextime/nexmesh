---
name: independent-evidence
description: What counts as evidence that something works, when the thing that checked it shares the assumptions of the thing that made it.
sources:
  - https://danielgordon-ml.github.io/blog/context-is-the-bottleneck.html
  - https://arxiv.org/pdf/2608.05263
  - https://dev.to/pyor/reviewing-ai-generated-code-a-practical-checklist-4017
  - https://spectrum.ieee.org/ai-code-review-software-engineers
---

# Evidence that is actually independent

The hard part of accepting work as done is that most of the checking
available to you was produced by the same process, with the same
assumptions, as the work itself.

- **Tests written beside the code share its blind spots.** They encode what
  the author believed, so they pass for the same reason the code is wrong.
  A green suite is evidence that the code does what its author thought, not
  that the author was right. Ask what the test would have to look like to
  FAIL on this bug.
- **Another model is not another opinion.** Two agents of the same family
  reviewing one claim are not two auditors; they agree where they are both
  wrong. Where it matters, check against something of a different kind: the
  running system, the real API's documentation, a log, a measurement, a
  person.
- **Retrying is not verifying.** A retry fixes a flake and hides a fault.
  If something worked on the second attempt, find out what was different
  before moving on — research on agent pipelines found that retries recover
  tool errors and essentially never recover a bad assumption.
- **Prefer an observation to an inference.** "The endpoint returns 401 for
  an unauthenticated request — I ran it" beats any amount of reading. Say
  which it is, every time: what you observed, and what you concluded.
- **Say what you did NOT check.** The most useful sentence in a handover is
  the one naming the part still resting on an assumption. It is also the
  cheapest: nobody else can see where you stopped looking.

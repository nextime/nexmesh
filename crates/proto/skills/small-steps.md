---
name: small-steps
description: Changing code in steps that each work, and verifying each one with something that would fail if it were broken.
sources:
  - https://martinfowler.com/articles/continuousIntegration.html
  - https://dora.dev/capabilities/working-in-small-batches/
  - https://git-scm.com/docs/git-bisect
---

# Small steps, each verified

The failure this prevents: a large change made confidently, then an hour
spent working out which part of it broke the thing that used to work.

- **One change, then check.** The smallest edit that could move you forward,
  then run the thing. Two changes and a failure is a bisection you now have
  to do by hand.
- **Verify with something that would FAIL.** A test, a run, a measurement, a
  request that returns the wrong answer before and the right one after.
  Re-reading your own diff is not verification: it is the same reasoning that
  produced the bug, applied twice.
- **Keep it green.** If a step leaves the build or the tests broken, that is
  the step to finish before starting another, not a state to work on top of.
- **Say what you verified it with.** "Done" is a claim; "done, `cargo test -p
  x` green, 41 tests" is the claim with its evidence, and it is what lets
  somebody else trust it without redoing it.
- **When something unexpected works, find out why** before moving on. A fix
  you cannot explain is a fix that will come back.

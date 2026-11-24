---
name: interface-states
description: The states an interface has to be designed for, and what each one owes a person who cannot see it — loading, empty, error, too much data.
sources:
  - https://design-system.agriculture.gov.au/patterns/loading-error-empty-states
  - https://cloudscape.design/patterns/general/empty-states/
  - https://barista.dynatrace.com/patterns/empty-states
  - https://www.design-system.health.qld.gov.au/components/loading-spinner/tabs/accessibility
  - https://dir.texas.gov/txds/accessibility/conformance/
---

# The states, and who they are for

A screen has at least four states and the happy one is the easiest. Design
the others or they get invented during the build, differently each time.

- **Loading** says what is happening, not "please wait". A spinner with no
  words is a question mark. It is also a *status message*: it has to be
  announced to a screen reader, not only drawn.
- **Empty is not an error.** They read the same on screen and mean opposite
  things: one is "there is nothing here yet, here is how to start", the
  other is "something went wrong, here is what to do". Design systems that
  mix them produce interfaces where a first-time user is told they have a
  problem.
- **Error says what happened and what to do next.** Identified as *text* —
  never colour alone, and never an icon alone. A message tied to the field
  it is about, so somebody using a screen reader hears it with the input
  rather than somewhere else on the page.
- **Too much data** is a state. What happens at ten thousand rows, a
  hundred-character name, a right-to-left language, a 4-inch screen.
- **Say where focus goes.** After a retry, after a dismissal, after a
  dialog closes. Unspecified, focus returns to the top of the document and
  a keyboard user starts the page again.
- **Write the words with the design.** The empty state's sentence, the
  error's sentence, the button's verb. Left to implementation they become
  "An error occurred".

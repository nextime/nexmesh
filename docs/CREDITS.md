# Credits

nexmesh exists because two other projects worked things out first. This file is
the long version, kept whole. The README has a short section pointing here, so
that a reader who arrives wanting to know what this does is not made to read
two pages of attribution first, and so that the attribution gets more room than
a paragraph.

## The team model comes from multiagents

**nexmesh is a reimplementation of
[multiagents](https://github.com/zetbrush/multiagents) by Arman Andreasyan
([@zetbrush](https://github.com/zetbrush)).** That project worked out what a
team of agent CLIs needs in order to function as a team, and nexmesh would not
exist without it. If you want this on a single machine, use the original. It is
the more mature tool, and it is where these ideas come from.

The design nexmesh inherits from it, essentially unchanged:

- **The slot/peer split.** A *slot* is a seat on the team with a stable
  identity; an *agent* is whatever process is currently filling it. This is the
  idea that makes crash recovery and respawning coherent, and everything else
  here is built on it.
- **The task state machine.** `idle → working → done_pending_review →
  addressing_feedback → approved → released`, with reviewer roles
  self-approving, and agents unable to disconnect until the loop completes.
- **Delivery with acknowledgement.** A message counts as delivered only once it
  has actually been surfaced to the model, so a dropped hand-off is retried
  instead of lost.
- **Piggybacking delivery on MCP tool results.** Unread messages are prepended
  to an agent's next tool result under a `[PENDING MESSAGES]` header. This is
  what makes an arbitrary MCP-speaking CLI usable with no bespoke code, and it
  is the floor the whole plugin model here stands on.
- **The shared knowledge store.** A persistent key-value store of decisions,
  conventions and discoveries, on the diagnosis that context drift between
  agents is the main way these systems fail.
- **File locks plus ownership zones.** Static zones for cheap separation,
  dynamic locks for genuinely shared files.
- **Driving a headless CLI through its stdin.** Writing teammate messages into
  `claude -p --input-format stream-json` so they are read mid-turn rather than
  at the end of it, and using Codex's `turn/steer` for the same purpose.

Rediscovering any one of those would have taken weeks. The credit for the
system design belongs upstream. What follows is only about running it on more
than one machine.

## The code graph comes from graphify

**The idea behind [Reading a codebase once](docs/GUIDE.md#reading-a-codebase-once-not-once-per-agent)
is [graphify](https://github.com/Graphify-Labs/graphify) by Graphify Labs**
(Apache-2.0, also offered under MIT). It was evaluated as a dependency and
reimplemented instead. What it contributed is the shape of the answer:

- **Parse the codebase deterministically and query the graph**, rather than
  asking a model to read files. graphify's own framing (tree-sitter, no
  embeddings, no vector store, nothing leaving the machine) is the reason the
  code graph here works the same way.
- **Say which edges are facts and which are guesses.** graphify tags edges
  `EXTRACTED` or `INFERRED`. `EdgeKind::is_certain` is that distinction, and
  carrying it all the way to the agent is a direct borrowing: a call resolved
  by name must not read like a syntax fact.
- **Communities as de facto subsystems.** graphify detects them with Leiden, as
  does this, for the same purpose: handing an agent one subsystem instead of a
  repository.
- **Ingest more than code.** Its coverage of docs, PDFs and images is why the
  capability bridge exists at all, though the division is different here. That
  work belongs to an *agent* writing into memory, because nexmesh holds no
  model credentials.

What was **not** taken: any of its code. graphify is Python; this is Rust, and
the Leiden implementation, the language table, the storage and the query
surface were written here. Nothing was copied, adapted or translated, and the
permissive licence was not relied on to vendor anything.

What is deliberately **not** reimplemented: its LLM semantic pass over PDFs,
images and audio, and its HTML report. The first needs model credentials, which
nexmesh refuses to hold. The second is better served by an agent that can
already read.

If you want a code knowledge graph on its own, use graphify. It is the more
capable tool at that one job, and it is where this idea comes from.

## The capability bridge, and the service behind it

`nexmesh-coderai` is the bridge that lets a team ask for things a language
model cannot do by itself: OCR, transcription, speech, embeddings, image and
video work. The service it was written against is
[CoderAI](https://www.nexlab.net), by Nexlab, which is also where nexmesh
itself was written.

The bridge is not tied to it. What it offers a team is read from a manifest,
`capabilities/coderai.toml`, which names the endpoints, the fields they accept
and the probe that decides whether each one is actually serving. Pointing the
bridge at a different service means writing a different manifest, not changing
any Rust. The manifest in this repository is also a worked example of the
awkward parts: a capability document that is out of date against the service's
own routes, request field names that have to be corrected in one line rather
than in a release, and a model whose context is measured in phonemes rather
than characters.

## Licensing, and what that means for the originals

nexmesh is GPLv3-or-later. That covers this work only, and takes nothing away
from either project above.

It is an independent implementation in Rust. It reuses the *design* credited
here and **none of the source code** of
[multiagents](https://github.com/zetbrush/multiagents), which is TypeScript/Bun
and declares MIT in its `package.json` and README (that repository carries no
`LICENSE` file). No upstream file was copied, adapted or translated; the
schema, wire protocol and every line here were written from scratch. The same
goes for [graphify](https://github.com/Graphify-Labs/graphify), which is
Apache-2.0 (also offered under MIT) and whose permissive licence was
deliberately not relied on to vendor anything.

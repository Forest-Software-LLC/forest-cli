# AI use in this project

Transparency matters to us. AI is part of how Forest is developed, and many of the modules in this repository have been modified in some way by AI models working under the direction of the maintainers. We are committed to delivering a quality product, and this document describes what AI does here, what it is not allowed to do, and the rules it works under.

## Facts about Forest

1. **Forest was created and released by human hands; subsequent updates are made in tandem with AI.**
The CLI, backend, and frontend were all designed and built by humans. Everything AI works on sits on top of a foundation and a strict philosophy defined by the maintainers.

2. **Features are ideated and designed by humans.**
Forest was built to solve a problem by people who live with that problem day-to-day.

3. **Everything AI produces is thoroughly reviewed by the maintainers.**
We will not ship code that fails to follow our guidelines or deviates from how the maintainers would have written it just because "it works".

## What AI does, and what humans do

The maintainers own the design: the architecture, the security model, the resolution semantics, the on-disk formats, and every decision about what the tool should do. AI is used to implement, refactor, and review against those decisions.

Nothing ships that a maintainer has not reviewed, understood, and can stand behind. That is non-negotiable. **Responsibility for every line in a release rests with the maintainers**, not the tool that helped write it.

## The rules the AI works under

The AI operates under standing written instructions from the maintainers. The repo-level ones are public in [CLAUDE.md](CLAUDE.md), which documents the architecture and the invariants any change must preserve. The rest are maintainer configuration, summarized here so the policy itself is public.

Style rules:

- Comments are short and plain. They state only what the code cannot show, and they describe the code as it exists now. No narrating changes, no justifying an edit to a reviewer, no references to code that was removed or does not exist yet.
- All committed prose is held to the maintainers' voice: comments, CLI output strings, docs, and READMEs alike. Writing habits that read as machine-generated are banned in committed content. (Reading "load bearing" every 10 lines is exhausting so we scrub it.)
- Public repos never reference private internals. Anything a doc in this repo points to is something a reader can actually open.

Structural rules:

- Single-responsibility modules. A module does what its name says, and when scope grows a new module is extracted instead of letting an existing one become a grab-bag.
- One writer per domain of state.
- Contracts are called directly and fail loudly. No defensive wrappers around logic that is supposed to be correct.
- Simple over clever. Another developer should be able to understand any module at a glance.

Process rules:

- The AI does not commit or push on its own. Those actions happen when a maintainer directs them.
- Changes to guarded areas, such as the install mutation path that a live Rojo watches, cache trust, and release verification, are run against the test harnesses and benchmarks listed in CLAUDE.md before they are considered done.

## Why we're comfortable shipping it

A package manager has to earn trust with more than a process statement, so the guarantees are mechanical rather than promissory:

- Before every release, forest is QA tested in real production environments. We will never blindly ship updates.
- Behavior is pinned by tests. Install layouts, pointer generation, and the shared platform/license contracts are asserted by unit tests, so a change that alters observable behavior fails the build regardless of who or what wrote it.
- Installs verify content. Every archive is checked against the lockfile's SHA-256 before a single file is extracted.
- Releases are offline-signed and builds are attested. It is virtually impossible for an agent to release a build on its own. See the security model in the [README](README.md).

## Why we use AI at all

Forest is built by a small, self-funded team. AI assistance is the difference between this tool existing with the competitive rigor we demand and it not existing at all, not the difference between AI and a larger team we chose not to hire. We think the moral weight of AI use lands on what you ship and whether you answer for it, and this document describes how we answer for it.

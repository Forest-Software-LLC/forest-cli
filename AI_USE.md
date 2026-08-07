# AI use in this project

AI is part of how Forest is developed. Many of the modules in this repository have been modified in some way by AI models working under the direction of the maintainers. Transparency matters to us, and we are committed to delivering a quality product.

## Facts about Forest

1. **Forest was created and released by human hands; subsequent updates are made in tandem with AI.**
The CLI, backend, and frontend were all designed and built by humans. Everything AI works on sits on top of a foundation and a strict philosophy defined by the maintainers.

2. **Features are ideated and designed by humans.**
Forest was built to solve a problem by people who live with that problem day-to-day.

3. **Everything AI produces is thoroughly reviewed by the maintainers.**
We will not ship code that fails to follow our guidelines or deviates from how the maintainers would have written it just because "it works".

## What AI does, and what humans do

The maintainers own the design: the architecture, the security model, the resolution semantics, the on-disk formats, and every decision about what the tool should do. AI is used to implement, refactor, and review against those decisions.

Nothing ships that a maintainer has not reviewed, understood, and can stand behind. That is non-negotiable. Responsibility for every line in a release rests with the maintainers, not the tool that helped write it.

## Why we're comfortable shipping it

A package manager has to earn trust with more than a process statement, so the guarantees are mechanical rather than promissory:

- Behavior is pinned by tests. Install layouts, pointer generation, and the shared platform/license contracts are asserted by unit tests, so a change that alters observable behavior fails the build regardless of who or what wrote it.
- Installs verify content. Every archive is checked against the lockfile's SHA-256 before a single file is extracted.
- Releases are offline-signed and builds are attested. See the security model in the [README](README.md).

## Why we use AI at all. 
Forest is built by a small, self-funded team. AI assistance is the difference between this tool existing with the competitive rigor we demand and it not existing at all, not the difference between AI and a larger team we chose not to hire. We think the moral weight of AI use lands on what you ship and whether you answer for it, and this document describes how we answer for it.
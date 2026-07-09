---
description: >-
  Use the `forest` CLI (forestpm) to manage packages in a Roblox or UEFN game
  project — scaffold forest.json, install/remove dependencies non-interactively,
  and require installed packages from Luau. Use this whenever a user wants to
  add, remove, update, or find a Forest/forestpm package, set up forest in a
  project, or when a repo contains a forest.json / forest-lock.json / packages/
  folder. Also explains which commands require a human (login, publish) and must
  never be run by an agent.
---

# forestpm — package manager for Roblox / UEFN

`forest` is the CLI for **forestpm**, a package manager for Roblox and UEFN
(Luau) projects. Packages are published as `@scope/name` and installed into a
local `packages/` folder that you then `require()` from your game code.

This skill is for **helping a developer build a game** by managing forest
packages. Your job is the consume side: scaffold, install, remove, and wire up
`require()`s. It is **not** to publish packages or log in on the user's behalf.

## Golden rules (read first)

1. **Never run `forest login` or `forest publish` yourself.** Both are
   interactive *and* require the user's password + 2FA code, which you don't
   have. `publish` also asks ~13 questions (name, author, license, visibility,
   version…) and creates a public, hard-to-reverse release under the user's
   identity. If the user needs to log in or publish, tell them to run the
   command themselves in their own terminal.
2. **Always pass `--platform` to `forest init`.** Bare `forest init` opens an
   arrow-key picker that will hang a non-interactive shell.
3. **`install` and `remove` are already non-interactive** — safe to run
   directly.
4. Edits to `forest.json` should generally go through `forest install` /
   `forest remove` so `forest-lock.json` stays in sync. Hand-editing is fine for
   small tweaks, but re-run `forest install` afterward to regenerate the lock.

## Set up forest in a project

Only if there's no `forest.json` yet. Pick the platform from context (a Roblox
project → `roblox`; UEFN → `uefn`):

```bash
forest init --platform roblox     # or: --platform uefn
```

This writes a minimal `forest.json`:

```json
{
  "dependencies": {},
  "platform": "roblox"
}
```

`--platform` accepts `roblox` or `uefn` (case-insensitive); anything else is
rejected without writing a file.

## Install packages

```bash
forest install @scope/name          # aliases: forest i / forest grow
forest install @scope/name -v 1.2.0 # pin a specific version (default: latest)
forest install @scope/name -a myalias   # install under a custom alias
forest install                      # no arg: install everything in forest-lock.json
```

- The identifier is `@scope/name` (the leading `@` is optional). It must have
  exactly two parts separated by `/`.
- Each dependency is recorded in `forest.json` as `"^<version>"` (caret range),
  and the full resolved tree is written to `forest-lock.json`. **Commit both.**
- The **alias** defaults to the name part (`@forest/pasta` → `pasta`) and must be
  unique across your dependencies; use `-a` to disambiguate collisions.

## Remove packages

```bash
forest remove @scope/name    # alias: forest chop @scope/name
```

## How installed packages are laid out & required

`install` extracts each **directly-installed** package to `packages/<alias>/`
with an `init.lua` entrypoint. A package's own (transitive) dependencies are
nested inside it under `packages/<alias>/packages/…` and are **not** directly
requirable from your project — only what you installed yourself is.

```
my-project/
  script.lua
  packages/
    pasta/
      init.lua
      LICENSE
      packages/          <- pasta's own deps, isolated (not yours to require)
        flour/
          init.lua
    sauce/
      init.lua
```

Require a package by its **alias**:

```lua
local Pasta = require("pasta")
local Sauce = require("sauce")
```

If two installed packages share a name, require them by their fuller
`scope_name` identifier to disambiguate.

> How a bare-string `require("alias")` resolves depends on the project's Luau
> toolchain (e.g. Rojo syncing `packages/` into the DataModel, `.luaurc`
> aliases, or a Lune-style runtime). **Match the project's existing `require`
> style** rather than assuming one — look at how current code imports modules
> and follow it. If a transitive dep needs to be used directly, install it at
> the top level yourself (`forest install @scope/dep`).

## Finding packages

There is no `forest search` command. To discover packages:

- Browse **forest.dev** (the registry website), or
- Ask the user for the exact `@scope/name`, or
- Query the public search API (semantic search over READMEs):
  `GET /v1/search/packages?q=<query>&platform=<roblox|uefn>&limit=10`

You need the exact `@scope/name` to install.

## Read-only account commands (safe)

```bash
forest whoami    # print the logged-in user (or that nobody is logged in)
```

Use `whoami` to check auth state before telling a user they need to log in.
Don't run `forest logout` unless the user explicitly asks — it signs them out.

## Quick reference

| Command | Interactive? | Agent may run? |
|---|---|---|
| `forest init --platform <p>` | No (with flag) | ✅ |
| `forest install [@scope/name]` | No | ✅ |
| `forest remove @scope/name` | No | ✅ |
| `forest whoami` | No | ✅ (read-only) |
| `forest logout` | No | ⚠️ only if asked |
| `forest init` (no flag) | **Yes (picker)** | ❌ hangs |
| `forest login` | **Yes + 2FA** | ❌ human only |
| `forest publish` | **Yes + 2FA** | ❌ human only |

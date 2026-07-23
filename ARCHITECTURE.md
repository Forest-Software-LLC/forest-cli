# forest-cli architecture

How the CLI is organized, why, and how to change it safely. The design goal
is that platform support is fully isolated: adding a platform touches one
directory plus one enum, and removing one is a compiler-guided delete.

## Layout

```
shared/              git submodule: forest-shared-resources, pinned at a tag.
                     Contract JSONs (identifier rules, license knowledge) are
                     embedded at compile time; unit tests assert the vectors,
                     so a submodule bump that changes behavior fails the build.

src/
  main.rs            clap dispatch only.
  platform.rs        THE PLATFORM SEAM. enum Platform { Roblox, Uefn } plus a
                     method per platform-divergent capability; every arm is a
                     one-line delegation into that platform's module.
  commands/          Thin orchestration: prompts, spinners, API calls.
                     Contains ZERO platform conditionals; anything divergent
                     goes through platform.rs.

  roblox/            Everything Roblox-specific:
    plan.rs            hoisted-layout planner + pointer-file computation
    install.rs         executor (downloads, prune, pointer regeneration)
    extract.rs         init-rename folder-module extraction
    receipts.rs        recursive Packages/* scan + keep/stale reconcile
    publish.rs         root (entry-point) resolution + naming rules
    init.rs            project scaffold
  uefn/              Everything UEFN/Verse-specific (same shape):
    mod.rs             project discovery, scope mapping, markers, lints
    plan.rs            flat-layout planner + marker computation
    install.rs         executor (verbatim extract, marker regeneration)
    publish.rs         Verse naming rules, compatVersion, pre-pack lint
    init.rs            location-inferred scaffolds

  Core (platform-blind; MUST NOT import roblox/ or uefn/):
    lockfile_gen.rs    lockfile format, resolution entry point, shared
                       download services (CDN base, signed URLs)
    lockfile_solver.rs semver resolution against the registry
    fetch_and_extract.rs  trusted-byte acquisition + verbatim extraction
    receipts.rs        receipt read/write + the flat 3-way tree taxonomy
                       (platform-agnostic; UEFN uses it today, any future
                       flat-layout platform can too)
    contracts.rs       loaders for the shared/ contract JSONs
    http.rs, tokens.rs, cache.rs, message.rs, utils.rs, license_helper.rs,
    release_verify.rs
```

## The dependency rule

Core never imports platform modules. Platform modules import core, never
each other. `commands/` talks to `platform.rs`, not to `roblox/`/`uefn/`
directly (the one exception: `platform.rs` itself is the only file allowed
to name platform modules). If a change requires core to know what Verse or
Luau is, the change is in the wrong place.

## The platform capability surface

`Platform` has one method per divergent behavior. Today that is:

| Method | What it owns |
|---|---|
| `install` | The entire layout/extraction/bookkeeping/post-install pipeline |
| `resolution_roots` | What dependency resolution runs against. Roblox: the invoking manifest. UEFN: the WORKSPACE (project manifest + every authored package's manifest, constraints ANDed) — so installs land at the shared mount and one lockfile (Content/forest-lock.json) governs, no matter where install runs from |
| `publish_preflight` | Entry-point resolution (Roblox) / name + compat checks (UEFN) |
| `validate_package_name` | Naming rules for new packages |
| `name_advisory` | Non-fatal naming advice |
| `prepack_warnings` | Files the registry will reject |
| `init` | The `forest init` scaffold |
| `alias_error` | Whether/why `-a` is rejected |
| `resolved_note` / `added_note` | Post-add UX strings |
| `detects` (via `Platform::detect`) | Project autodetection signals (Rojo/Wally files vs .uefnproject); conclusive only when exactly one platform matches, otherwise the picker prompts |
| `discover_manifest_dir` (free fn) | Where the manifest lives relative to cwd |

A third platform implements exactly this list. If the match arms start
feeling repetitive at three platforms, extract a trait then; the enum makes
that refactor mechanical.

## Ripping a platform out

1. Delete `src/<platform>/`.
2. Delete its variant from `enum Platform` in src/platform.rs.
3. Fix every exhaustiveness error the compiler now reports. That error list
   is the complete, guaranteed-total set of touchpoints.
4. Remove its contract from `shared/` consumption if unused (verse rules for
   UEFN) and its `forest init` picker entry.
5. The remaining platforms' tests must pass unchanged.

## Testing conventions

Inline `#[cfg(test)]` per file. The four contract vector suites
(license inference, SPDX membership, scope mapping, package-name rules) are
the cross-repo drift tripwire: they assert `shared/contracts/*.vectors.json`
against this crate's implementations, mirroring the same assertions in
forest-backend and forest-trust-gateway. Fixture helpers shared between core
and platform extraction tests live in `fetch_and_extract::test_util`.

## Things that look odd but are deliberate

- The download worker pool exists twice (roblox/install.rs and
  uefn/install.rs), cross-referenced by comments. Two copies of ~90 lines
  was chosen over a generic pool parameterized by extraction closure; if you
  fix a bug in one, check the other.
- `license_helper.rs` logic is contract-driven (shared/contracts/
  licenses.json); the fingerprint table's ORDER is semantic (AGPL before
  GPL). Never reorder it locally; change the contract repo and bump the pin.
- Marker `.verse` files and pointer `init.lua` files are deletable ONLY by
  their generated first-line header. Those header strings are load-bearing
  and must never change once shipped.
- `Receipt.root` is `""` on UEFN (verbatim extraction has no entry point);
  it is part of the receipt match key on both platforms.

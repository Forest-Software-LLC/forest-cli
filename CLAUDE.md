# forest-cli

The `forest` command-line package manager for **forestpm** (Roblox packages). Rust, built on `clap` (CLI), `tokio` (async), `reqwest` (HTTP).

> Part of the forestpm ecosystem. Full map + API details in `../forest-backend/CLAUDE.md`. This binary is a client of that API.

## Commands — [src/main.rs](src/main.rs) → [src/commands/](src/commands/)
| Command | Aliases | File |
|---------|---------|------|
| `login` | | [commands/login.rs](src/commands/login.rs) — opens the browser to `FRONTEND_URL/auth/verify/cli`, stores the token |
| `publish` | | [commands/publish.rs](src/commands/publish.rs) — tars the package (respecting `.forestignore`) and uploads |
| `init` | | [commands/initialize.rs](src/commands/initialize.rs) — scaffolds `forest.json` |
| `install [pkg]` | `i`, `grow` | [commands/install.rs](src/commands/install.rs) — `-v/--version`, `-a/--alias`, `-f/--force` (full reinstall) |
| `remove <pkg>` | `chop` | [commands/remove.rs](src/commands/remove.rs) |

## Key modules
- [src/http.rs](src/http.rs) — API client wrapper. Shared `OnceLock` clients (keep-alive + timeouts): async for API calls, blocking for tarball downloads (gzip OFF there — transparent decompression would break integrity hashing).
- [src/tokens.rs](src/tokens.rs) — auth-token storage (home dir via the `dirs` crate).
- [src/lockfile_gen.rs](src/lockfile_gen.rs) + [src/lockfile_solver.rs](src/lockfile_solver.rs) — dependency resolution (`semver` + `version-ranges`) + install execution. Version lists prefetch concurrently; the BFS awaits memoized handles at the original points, so lockfiles stay deterministic. Downloads run on a bounded 8-worker pool.
- [src/install_plan.rs](src/install_plan.rs) — pure path/pointer planning (no IO); layout pinned by unit tests.
- [src/receipts.rs](src/receipts.rs) — incremental installs with **zero files outside `Packages/`**: each installed dir carries a `.forest-receipt` (extension-less ⇒ Rojo-ignored, like LICENSE) written after its extraction succeeds; pointer dirs are recognized by their generated init.lua header. Install = scan tree → reconcile vs plan: matching `(integrity, root)` receipts are kept, stale forest-managed dirs deleted, receipt-less dirs never trusted (covers crashes, branch switches, pre-receipt trees). Nested packages can't be kept when an ancestor reinstalls. `--force` ignores all receipts.
- [src/cache.rs](src/cache.rs) — content-addressed tarball cache at `~/.forest/cache/<sha256>.tgz` (re-verified on every read; corrupt ⇒ delete + redownload). `FOREST_CACHE_DIR` overrides, `FOREST_NO_CACHE=1` disables. Private packages are cached too (hash is the trust anchor; access enforced at first download).
- [src/fetch_and_extract.rs](src/fetch_and_extract.rs) — cache lookup → download → sha256 verify → unpack (`tar` + `flate2`).
- [src/licensce_helper.rs](src/licensce_helper.rs) *(sic — spelling)*, [src/message.rs](src/message.rs), [src/utils.rs](src/utils.rs).

## Package manifest — `forest.json`
```json
{ "name": "orgtest", "author": "org1", "version": "0.1.1",
  "platform": "roblox", "description": "...", "dependencies": {} }
```
`.forestignore` controls what `publish` excludes.

## Config / endpoints
[src/main.rs](src/main.rs) sets env vars at startup via a live `ENV=dev` switch — **prod is the default**:
- prod: `FOREST_API_URL=https://api.forest.dev/`, `FOREST_PACKAGES_URL=https://packages.forest.dev/` (trust gateway), `FRONTEND_URL=https://forest.dev/`; tarball CDN `https://registry.forest.dev` (`FOREST_CDN_BASE` overrides, [src/lockfile_gen.rs](src/lockfile_gen.rs)).
- `ENV=dev`: localhost `3001` (API) / `8081` (gateway) / `3000` (frontend). The backend's `.env` pins port **3001** for local runs, so dev lines up out of the box.

Install-related env knobs: `FOREST_CACHE_DIR`, `FOREST_NO_CACHE=1`, `FOREST_NO_UPDATE_CHECK=1` (used by [scripts/bench.ps1](scripts/bench.ps1)).

## Build / release
```bash
cargo build --release      # target/release/forest(.exe)
```
**Release pipeline lives in this repo:** [.github/workflows/release.yml](.github/workflows/release.yml) (on `v*.*.*` tags) builds four targets — win x64, linux x64, mac arm64 + x64 — then [scripts/release.ts](scripts/release.ts) publishes to R2 under `cli/<tag>/` + `cli/latest/` (binaries named `forest-<tag>-<target>[.exe]`, plus `.sha256` sidecars, `SHA256SUMS`, and a `latest.json` manifest). Public CDN: `https://releases.forest.dev`.

**Distribution channels** (consumers of those artifacts):
- **cli-script-installer** repo (primary): `curl -fsSL https://releases.forest.dev/install.sh | sh` / `irm https://releases.forest.dev/install.ps1 | iex`. Both scripts verify the manifest's offline release signature with the same keys [src/release_verify.rs](src/release_verify.rs) pins — a key rotation must update both repos.
- **cli-installer** repo: Slint GUI wizard
- The old Inno Setup installer (`forest_installer.iss`) was removed in July 2026 — the script + GUI installers replaced it.

## Benchmarks
[scripts/bench.ps1](scripts/bench.ps1) times `install` against prod (fixture of real Wally-mirror packages). `-SetupFixture` once, then `-Exe <binary> -Label <name>`; scenarios: cold / reinstall / warmcache / resolve. Results CSV lands in `-BenchRoot`.

## Gotchas
- `licensce_helper.rs` is misspelled in the filename; match it exactly when importing.
- Install state lives ONLY inside `Packages/` (per-dir `.forest-receipt` files) — nothing is written to the project root besides forest.json/forest-lock.json. Deleting a package dir deletes its receipt with it; `install --force` reinstalls everything.
- `Packages/` entries starting with `_` or `.` are never touched (Wally `_Index` coexistence).

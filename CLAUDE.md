# forest-cli

The `forest` command-line package manager for **forestpm** (Roblox packages). Rust, built on `clap` (CLI), `tokio` (async), `reqwest` (HTTP).

> Part of the forestpm ecosystem. Full map + API details in `../forest-backend/CLAUDE.md`. This binary is a client of that API.

## Commands — [src/main.rs](src/main.rs) → [src/commands/](src/commands/)
| Command | Aliases | File |
|---------|---------|------|
| `login` | | [commands/login.rs](src/commands/login.rs) — opens the browser to `FRONTEND_URL/auth/verify/cli`, stores the token |
| `publish` | | [commands/publish.rs](src/commands/publish.rs) — tars the package (respecting `.forestignore`) and uploads |
| `init` | | [commands/initialize.rs](src/commands/initialize.rs) — scaffolds `forest.json` |
| `install [pkg]` | `i`, `grow` | [commands/install.rs](src/commands/install.rs) — `-v/--version`, `-a/--alias` |
| `remove <pkg>` | `chop` | [commands/remove.rs](src/commands/remove.rs) |

## Key modules
- [src/http.rs](src/http.rs) — API client wrapper.
- [src/tokens.rs](src/tokens.rs) — auth-token storage (home dir via the `dirs` crate).
- [src/lockfile_gen.rs](src/lockfile_gen.rs) + [src/lockfile_solver.rs](src/lockfile_solver.rs) — dependency resolution (`semver` + `version-ranges`).
- [src/fetch_and_extract.rs](src/fetch_and_extract.rs) — downloads + unpacks tarballs (`tar` + `flate2`).
- [src/licensce_helper.rs](src/licensce_helper.rs) *(sic — spelling)*, [src/message.rs](src/message.rs), [src/utils.rs](src/utils.rs).

## Package manifest — `forest.json`
```json
{ "name": "orgtest", "author": "org1", "version": "0.1.1",
  "platform": "roblox", "description": "...", "dependencies": {} }
```
`.forestignore` controls what `publish` excludes.

## Config / endpoints
[src/main.rs](src/main.rs) sets env vars at startup. **Currently hardcoded to dev:**
```rust
FOREST_API_URL = "http://localhost:3001/"
FRONTEND_URL   = "http://localhost:3000/"
```
The commented-out prod values reference `forestpm.dev` — **stale**: the official domain is now `forest.dev` (forestpm.dev still resolves as legacy). Update these when re-enabling the prod switch. The backend's `.env` pins it to port **3001** for local runs (both `npm run dev` and Docker), so dev lines up out of the box — no extra config needed to test the CLI against a local backend.

## Build / release
```bash
cargo build --release      # target/release/forest(.exe)
```
**Release pipeline lives in this repo:** [.github/workflows/release.yml](.github/workflows/release.yml) (on `v*.*.*` tags) builds four targets — win x64, linux x64, mac arm64 + x64 — then [scripts/release.ts](scripts/release.ts) publishes to R2 under `cli/<tag>/` + `cli/latest/` (binaries named `forest-<tag>-<target>[.exe]`, plus `.sha256` sidecars, `SHA256SUMS`, and a `latest.json` manifest). Public CDN: `https://releases.forest.dev`.

**Distribution channels** (consumers of those artifacts):
- **cli-script-installer** repo (primary): `curl -fsSL https://releases.forest.dev/install.sh | sh` / `irm https://releases.forest.dev/install.ps1 | iex`
- **cli-installer** repo: Slint GUI wizard
- The old Inno Setup installer (`forest_installer.iss`) was removed in July 2026 — the script + GUI installers replaced it.

## Gotchas
- The dev/prod URL switch is commented out — don't ship a build with localhost baked in. Confirm the env block in `main.rs` before a release.
- `licensce_helper.rs` is misspelled in the filename; match it exactly when importing.

# forest 🌲

The command-line client for [Forest](https://forest.dev) - a package manager for Roblox.

Forest handles the parts of dependency management that Luau tooling has historically left to chance: real semver resolution with a lockfile, license verification at publish time, and cryptographically verified installs and updates.

## Install

**macOS / Linux:**
```sh
curl -fsSL https://releases.forest.dev/install.sh | sh
```

**Windows (PowerShell):**
```powershell
irm https://releases.forest.dev/install.ps1 | iex
```

Both scripts verify the binary's SHA-256 against the release manifest before installing.

## Quick start

```sh
forest init                    # scaffold forest.json in your project
forest login                   # authenticate via the browser
forest install scope/package   # add a dependency (alias: forest i, forest grow)
forest install                 # install everything from the lockfile
forest install --force         # reinstall from scratch, ignoring installed state
forest install --frozen        # CI: install exactly what the lockfile pins, never update it
forest install x/y --init uefn # no forest.json yet? create one for the platform and continue
forest remove scope/package    # remove a dependency (alias: forest chop)
forest publish                 # publish the current package
forest audit                   # check dependencies for updates and license issues
forest update                  # move dependencies to the newest versions their ranges allow
forest upgrade                 # update the CLI itself
```

Dependencies land in `Packages/` with generated Luau pointer modules, so requiring them from your game code just works. Forest owns that folder: installing removes anything it didn't put there, a Wally `_Index` included. `forest-lock.json` pins every transitive dependency to an exact version and content hash - commit it.

## Mounts

A project can keep several dependency folders, each installed on its own, for example server-only or dev-only packages next to the shared ones. On Roblox:

```sh
forest mount create ServerPackages                  # add a folder (any path inside the project)
forest install scope/package -m ServerPackages
forest mount                                        # list mounts
forest mount rename ServerPackages src/server/Packages
forest mount remove src/server/Packages
```

`-m`/`--mount` takes a folder path or any unique end of one (`server/Packages`), on `install`, `remove`, `update`, `audit`, `tree`, `link`, and `unlink`. Without it, `install` adds to the default folder (forest.json's top-level `dependencies`) and bulk commands cover every mount. Mounts never share packages, so the same package can sit in two at different versions. Only the default mount is published; the others exist for your project alone. Map each folder in your Rojo project as usual.

Every mount belongs to forest the same way `Packages/` does: install removes anything at its top level that isn't a package it installed. A folder forest has never installed into is refused instead when it holds other files, so a mistyped path in forest.json can't wipe source code.

## Coming from Wally

Run `forest init` next to your `wally.toml` and accept the import. `[dependencies]` become the default mount, and `[server-dependencies]` and `[dev-dependencies]` become the `ServerPackages` and `DevPackages` mounts, the same folders Wally used, so your Rojo project keeps working. `wally.lock` is removed. The next `forest install` fills the folders and clears out Wally's `_Index` and link modules; code that requires `Packages.Promise` keeps working.

Installs are incremental: packages already on disk that match the lockfile are skipped (each installed folder carries a tiny `.forest-receipt`, ignored by Rojo like LICENSE files), and downloaded archives are kept in a local content-addressed cache (`~/.forest/cache`, verified by SHA-256 on every read; set `FOREST_NO_CACHE=1` to disable). Forest writes nothing to your project root beyond `forest.json` and `forest-lock.json`.

## CI

Private dependencies need a credential. Create an API token in your forest.dev account or Studio settings, store it as a CI secret, and expose it as `FOREST_TOKEN`. In GitHub Actions:

```yaml
- run: forest install --frozen
  env:
    FOREST_TOKEN: ${{ secrets.FOREST_TOKEN }}
```

`FOREST_TOKEN` wins over a stored login, is never written to disk and is read only. `forest whoami` shows which token is in use. Caching `~/.forest/cache` (or `FOREST_CACHE_DIR`) between runs skips downloads the lockfile already pins.

## Security model

Forest treats its own infrastructure as untrusted:

- **Installs are content-addressed.** The lockfile records each package's SHA-256; the CLI derives download locations from that hash and verifies every archive before extracting a single file. A compromised registry or CDN cannot alter a package your lockfile already pins.
- **Updates are offline-signed.** `forest update` only accepts release manifests carrying a valid SSH signature from one of the release keys pinned in this source (see [src/release_verify.rs](src/release_verify.rs)). Signatures are produced on hardware keys that never touch CI or the release host - a compromise of either cannot push code to existing installs.
- **Builds are attested.** Release binaries carry GitHub build provenance; verify any downloaded binary with `gh attestation verify`.
- **Nothing executes at install time.** Forest packages are pure Luau source. There is no install-script mechanism.

Found a security issue? Please report it privately.

## Building from source

```sh
git submodule update --init    # shared/ = forest-shared-resources contracts (required)
cargo build --release          # target/release/forest(.exe)
cargo test
```

The `shared/` submodule pins [forest-shared-resources](https://github.com/Forest-Software-LLC/forest-shared-resources) at a tagged release; its contract JSONs are embedded at compile time and asserted by unit tests, so the build fails loudly without the submodule.

By default the CLI talks to the production API. Set `ENV=dev` to target a local backend (`localhost:3001`) instead.

## The Forest ecosystem

- [forest.dev](https://forest.dev) - registry and web UI
- [docs](https://docs.forest.dev) - documentation
- `releases.forest.dev` - CLI releases and install scripts

## AI use

Forest is developed with AI assistance, directed and reviewed by the maintainers. See [AI_USE.md](AI_USE.md) for what that means in practice.

## License

See [LICENSE](LICENSE).

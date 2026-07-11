# forest 🌲

The command-line client for [Forest](https://forest.dev) — a package manager for Roblox.

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
forest remove scope/package    # remove a dependency (alias: forest chop)
forest publish                 # publish the current package
forest audit                   # check dependencies for updates and license issues
forest update                  # update the CLI itself
```

Dependencies land in `packages/` with generated Luau pointer modules, so requiring them from your game code just works. `forest-lock.json` pins every transitive dependency to an exact version and content hash — commit it.

## Security model

Forest treats its own infrastructure as untrusted:

- **Installs are content-addressed.** The lockfile records each package's SHA-256; the CLI derives download locations from that hash and verifies every archive before extracting a single file. A compromised registry or CDN cannot alter a package your lockfile already pins.
- **Updates are offline-signed.** `forest update` only accepts release manifests carrying a valid SSH signature from one of the release keys pinned in this source (see [src/release_verify.rs](src/release_verify.rs)). Signatures are produced on hardware keys that never touch CI or the release host — a compromise of either cannot push code to existing installs.
- **Builds are attested.** Release binaries carry GitHub build provenance; verify any downloaded binary with `gh attestation verify`.
- **Nothing executes at install time.** Forest packages are pure Luau source. There is no install-script mechanism.

Found a security issue? Please report it privately.

## Building from source

```sh
cargo build --release          # target/release/forest(.exe)
cargo test
```

By default the CLI talks to the production API. Set `ENV=dev` to target a local backend (`localhost:3001`) instead.

## The Forest ecosystem

- [forest.dev](https://forest.dev) — registry and web UI
- [docs](https://docs.forest.dev) — documentation
- `releases.forest.dev` — CLI releases and install scripts

## License

See [LICENSE](LICENSE).

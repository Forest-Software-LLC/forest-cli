use clap::{Parser, Subcommand};

mod tokens;
mod http;
mod cache;
mod links;
mod contracts;
mod message;
mod lockfile_gen;
mod lockfile_solver;
mod meta_cache;
mod roblox;
mod receipts;
mod fetch_and_extract;
mod commands;
mod license_helper;
mod platform;
mod release_verify;
mod uefn;
mod utils;
use commands::{login_command, logout_command, whoami_command, install_command, init_command, publish_command, remove_command, update_command, upgrade_command, audit_command, tree_command, override_command, exclude_command, link_command, unlink_command, maybe_notify_update};

use std::env;

/// Forest CLI: the Forest package manager
#[derive(Parser)]
#[command(name = "forest", version = env!("CARGO_PKG_VERSION"), about = "Forest CLI: the Forest package manager")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Log in to your Forest account
    Login,

    /// Log out and clear your stored credentials
    Logout,

    /// Show the currently logged-in user
    Whoami,

    /// Publish a package
    Publish,

    /// Start development on a new package
    Init {
        /// Platform for the package (roblox or uefn). Skips the interactive
        /// picker when provided, making `init` scriptable.
        #[arg(short = 'p', long = "platform")]
        platform: Option<String>,

        /// Create a bare project manifest (dependencies + platform) for
        /// consuming packages, instead of the package-authoring scaffold.
        /// Non-interactive-safe.
        #[arg(long = "project")]
        project: bool,

        /// Dependency folder name (Roblox only, default "Packages")
        #[arg(long = "packages-dir", value_name = "NAME")]
        packages_dir: Option<String>,
    },

    /// Install dependencies for the package
    #[command(alias = "i", alias = "grow")]
    Install {
        /// Package name (optional)
        package: Option<String>,

        /// Specify a version to install
        #[arg(short = 'v', long = "version")]
        version: Option<String>,

        /// Specify an alias for the package
        #[arg(short = 'a', long = "alias")]
        alias: Option<String>,

        /// Reinstall everything from scratch, ignoring installed state
        #[arg(short = 'f', long = "force")]
        force: bool,

        /// When no forest.json exists, create one for this platform
        /// (roblox or uefn) and continue. The non-interactive twin of
        /// answering "Yes" to the create prompt. Ignored if a manifest
        /// already exists.
        #[arg(long = "init", value_name = "PLATFORM")]
        init: Option<String>,

        /// How to treat local links (forest link): apply, ignore, or forbid.
        /// Default: ignore under CI, apply otherwise.
        #[arg(long = "links", value_name = "MODE")]
        links: Option<links::LinksMode>,
    },

    /// Remove a package from the project
    #[command(alias = "chop")]
    Remove {
        /// Package name
        package: String,
    },

    /// Update dependencies to the newest versions your declared ranges allow
    Update {
        /// Moved: CLI self-update is now `forest upgrade --check`
        #[arg(long = "check", hide = true)]
        check: bool,
    },

    /// Update forest itself to the latest release
    Upgrade {
        /// Only report whether an update is available; don't install it
        #[arg(long = "check")]
        check: bool,
    },

    /// Check dependencies for available updates and license considerations
    #[command(alias = "outdated")]
    Audit {
        /// Only audit this package (e.g. scope/name)
        package: Option<String>,

        /// Update forest.json to the latest versions and reinstall
        #[arg(short = 'u', long = "update")]
        update: bool,
    },

    /// Show the installed dependency tree
    #[command(alias = "ls", alias = "list")]
    Tree {
        /// Only show this package's subtree (e.g. scope/name, alias, or bare name)
        package: Option<String>,
    },

    /// Force a transitive dependency onto a semver range (lists overrides when no package is given)
    Override {
        /// Package to override (scope/name, or a bare name that is unambiguous)
        package: Option<String>,

        /// The new range, skipping the interactive prompt (fails if it satisfies no versions)
        #[arg(short = 'r', long = "range")]
        range: Option<String>,

        /// Apply without the confirmation prompt
        #[arg(short = 'y', long = "yes")]
        yes: bool,

        /// Remove the override for this package
        #[arg(long = "remove")]
        remove: bool,
    },

    /// Point a dependency at a local directory, machine-locally (lists links when no path is given)
    Link {
        /// Path to a local package directory containing forest.json
        path: Option<String>,

        /// Show active links and their divergence from the lockfile
        #[arg(long = "list")]
        list: bool,
    },

    /// Remove a local link and restore the registry version
    Unlink {
        /// Package (scope/name) or the linked path
        reference: Option<String>,

        /// Remove every active link
        #[arg(long = "all")]
        all: bool,
    },

    /// Ban versions of a package from ever being installed (lists exclusions when no package is given)
    Exclude {
        /// Package to exclude versions of (scope/name, or a bare name that is unambiguous)
        package: Option<String>,

        /// The range of versions to ban (e.g. "=1.6.0"), skipping the interactive prompt
        #[arg(short = 'r', long = "range")]
        range: Option<String>,

        /// Apply without the confirmation prompt
        #[arg(short = 'y', long = "yes")]
        yes: bool,

        /// Remove the exclusion for this package
        #[arg(long = "remove")]
        remove: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load .env based on NODE_ENV or fallback to ".env"
    if env::var("ENV") == Ok("dev".to_string()) {
        env::set_var("FOREST_API_URL", "http://localhost:3001/");
        // Local forest-trust-gateway (its dev server defaults to port 8081)
        env::set_var("FOREST_PACKAGES_URL", "http://localhost:8081/");
        env::set_var("FRONTEND_URL", "http://localhost:3000/");
        // Public tarballs are content-addressed and fetched straight from
        // the CDN, not through the gateway - locally that's the compose
        // stack's MinIO bucket (docker-compose.yml CDN_BASE_URL). Respect an
        // explicit override, unlike the URLs above.
        if env::var("FOREST_CDN_BASE").is_err() {
            env::set_var("FOREST_CDN_BASE", "http://localhost:9000/forest-packages-dev");
        }
    } else {
        env::set_var("FOREST_API_URL", "https://api.forest.dev/");
        // Package upload/download go to the public trust gateway, deployed
        // from the open forest-trust-gateway repo to its own hostname.
        env::set_var("FOREST_PACKAGES_URL", "https://packages.forest.dev/");
        env::set_var("FRONTEND_URL", "https://forest.dev/");
    }

    let cli = Cli::parse();
    let is_upgrade = matches!(cli.command, Commands::Upgrade { .. });

    match cli.command {
        Commands::Login => {
            login_command().await?;
        }
        Commands::Logout => {
            logout_command().await?;
        }
        Commands::Whoami => {
            whoami_command().await?;
        }
        Commands::Publish => {
            publish_command().await?;
        }
        Commands::Init { platform, project, packages_dir } => {
            init_command(platform, project, packages_dir).await?;
        }
        Commands::Install { package, version, alias, force, init, links } => {
            install_command(package, version, alias, force, init, links).await?;
        }
        Commands::Remove { package } => {
            remove_command(package).await?;
        }
        Commands::Update { check } => {
            if check {
                // `forest update --check` was the self-update probe before v1.11.
                crate::message::info("`forest update` now updates dependencies. For the CLI itself, run `forest upgrade --check`.");
            } else {
                update_command().await?;
            }
        }
        Commands::Upgrade { check } => {
            upgrade_command(check).await?;
        }
        Commands::Audit { package, update } => {
            audit_command(package, update).await?;
        }
        Commands::Tree { package } => {
            tree_command(package)?;
        }
        Commands::Override { package, range, yes, remove } => {
            override_command(package, range, yes, remove).await?;
        }
        Commands::Exclude { package, range, yes, remove } => {
            exclude_command(package, range, yes, remove).await?;
        }
        Commands::Link { path, list } => {
            link_command(path, list).await?;
        }
        Commands::Unlink { reference, all } => {
            unlink_command(reference, all).await?;
        }
    }

    // Best-effort, throttled nudge if a newer forest exists (skipped during an
    // explicit upgrade, in CI, and in non-interactive shells).
    if !is_upgrade {
        maybe_notify_update().await;
    }

    Ok(())
}

use clap::{Parser, Subcommand};

mod tokens;
mod api_token;
mod http;
mod cache;
mod links;
mod download_pool;
mod install_report;
mod ci;
mod contracts;
mod message;
mod lockfile;
mod lockfile_gen;
mod lockfile_solver;
mod renames;
mod meta_cache;
mod mounts;
mod roblox;
mod receipts;
mod fetch_and_extract;
mod commands;
mod license_helper;
mod platform;
mod release_verify;
mod uefn;
mod utils;
use commands::{login_command, logout_command, whoami_command, install_command, init_command, publish_command, remove_command, update_command, upgrade_command, audit_command, tree_command, override_command, exclude_command, link_command, unlink_command, mount_list, mount_create, mount_remove, mount_rename, maybe_notify_update};

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
    Publish {
        /// Publish without prompts, reading everything from forest.json
        /// (missing fields are errors). Implied when CI is set.
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },

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

        /// Install exactly what forest-lock.json pins. Fails instead of
        /// updating the lockfile when it is missing or no longer matches
        /// forest.json. For CI.
        #[arg(long = "frozen")]
        frozen: bool,

        /// Mount to add the package to, or the only mount to install
        /// (a mount path, or enough of its end to be unique)
        #[arg(short = 'm', long = "mount", value_name = "MOUNT")]
        mount: Option<String>,
    },

    /// Remove a package from the project
    #[command(alias = "chop")]
    Remove {
        /// Package name
        package: String,

        /// Mount to remove it from, when several declare it
        #[arg(short = 'm', long = "mount", value_name = "MOUNT")]
        mount: Option<String>,
    },

    /// Update dependencies to the newest versions your declared ranges allow
    Update {
        /// Moved: CLI self-update is now `forest upgrade --check`
        #[arg(long = "check", hide = true)]
        check: bool,

        /// Only update this mount
        #[arg(short = 'm', long = "mount", value_name = "MOUNT")]
        mount: Option<String>,
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

        /// Only audit this mount
        #[arg(short = 'm', long = "mount", value_name = "MOUNT")]
        mount: Option<String>,
    },

    /// Show the installed dependency tree
    #[command(alias = "ls", alias = "list")]
    Tree {
        /// Only show this package's subtree (e.g. scope/name, alias, or bare name)
        package: Option<String>,

        /// Only show this mount
        #[arg(short = 'm', long = "mount", value_name = "MOUNT")]
        mount: Option<String>,
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

        /// Mount whose dependency to link, when several declare it
        #[arg(short = 'm', long = "mount", value_name = "MOUNT")]
        mount: Option<String>,
    },

    /// Remove a local link and restore the registry version
    Unlink {
        /// Package (scope/name) or the linked path
        reference: Option<String>,

        /// Remove every active link
        #[arg(long = "all")]
        all: bool,

        /// Only unlink in this mount
        #[arg(short = 'm', long = "mount", value_name = "MOUNT")]
        mount: Option<String>,
    },

    /// Manage mounts: extra dependency folders, each installed on its own (lists mounts when no action is given)
    Mount {
        #[command(subcommand)]
        action: Option<MountAction>,
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

#[derive(Subcommand)]
enum MountAction {
    /// Add a mount: a dependency folder at PATH, relative to forest.json
    Create {
        /// Folder path, e.g. ServerPackages or src/server/Packages
        path: String,
    },

    /// Remove a mount, its dependencies, and its folder
    Remove {
        /// The mount's path, or enough of its end to be unique
        mount: String,

        /// Skip the confirmation prompt
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },

    /// Move or rename a mount's folder
    Rename {
        /// The mount's path, or enough of its end to be unique
        mount: String,

        /// The new folder path, relative to forest.json
        new_path: String,
    },

    /// List the project's mounts
    #[command(alias = "ls")]
    List,
}

#[tokio::main]
async fn main() {
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
    let result = run(cli.command).await;

    // Best-effort, throttled nudge if a newer forest exists (skipped during an
    // explicit upgrade, in CI, and in non-interactive shells).
    if result.is_ok() && !is_upgrade {
        maybe_notify_update().await;
    }

    // Skipped private packages print even when a later step failed, and
    // always fail the run so CI can't pass on a partial tree.
    let skipped = install_report::report();
    if let Err(err) = &result {
        crate::message::error(err);
    }
    if skipped || result.is_err() {
        std::process::exit(1);
    }
}

async fn run(command: Commands) -> anyhow::Result<()> {
    match command {
        Commands::Login => {
            login_command().await?;
        }
        Commands::Logout => {
            logout_command().await?;
        }
        Commands::Whoami => {
            whoami_command().await?;
        }
        Commands::Publish { yes } => {
            publish_command(yes).await?;
        }
        Commands::Init { platform, project, packages_dir } => {
            init_command(platform, project, packages_dir).await?;
        }
        Commands::Install { package, version, alias, force, init, links, frozen, mount } => {
            install_command(package, version, alias, force, init, links, frozen, mount).await?;
        }
        Commands::Remove { package, mount } => {
            remove_command(package, mount).await?;
        }
        Commands::Update { check, mount } => {
            if check {
                // `forest update --check` was the self-update probe before v1.11.
                crate::message::info("`forest update` now updates dependencies. For the CLI itself, run `forest upgrade --check`.");
            } else {
                update_command(mount).await?;
            }
        }
        Commands::Upgrade { check } => {
            upgrade_command(check).await?;
        }
        Commands::Audit { package, update, mount } => {
            audit_command(package, update, mount).await?;
        }
        Commands::Tree { package, mount } => {
            tree_command(package, mount)?;
        }
        Commands::Override { package, range, yes, remove } => {
            override_command(package, range, yes, remove).await?;
        }
        Commands::Exclude { package, range, yes, remove } => {
            exclude_command(package, range, yes, remove).await?;
        }
        Commands::Link { path, list, mount } => {
            link_command(path, list, mount).await?;
        }
        Commands::Unlink { reference, all, mount } => {
            unlink_command(reference, all, mount).await?;
        }
        Commands::Mount { action } => match action {
            None | Some(MountAction::List) => mount_list()?,
            Some(MountAction::Create { path }) => mount_create(path).await?,
            Some(MountAction::Remove { mount, yes }) => mount_remove(mount, yes).await?,
            Some(MountAction::Rename { mount, new_path }) => mount_rename(mount, new_path).await?,
        },
    }

    Ok(())
}

use clap::{Parser, Subcommand};

mod tokens;
mod http;
mod message;
mod lockfile_gen;
mod lockfile_solver;
mod fetch_and_extract;
mod commands;
mod licensce_helper;
mod utils;
use commands::{login_command, logout_command, whoami_command, install_command, init_command, publish_command, remove_command, update_command, maybe_notify_update};

use std::env;

/// Forest CLI - Package manager
#[derive(Parser)]
#[command(name = "forest", version = env!("CARGO_PKG_VERSION"), about = "Forest CLI - Package manager")]
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

    /// Initialize a new package
    Init {
        /// Platform for the package (roblox or uefn). Skips the interactive
        /// picker when provided, making `init` scriptable.
        #[arg(short = 'p', long = "platform")]
        platform: Option<String>,
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
    },

    /// Remove a package from the project
    #[command(alias = "chop")]
    Remove {
        /// Package name
        package: String,
    },

    /// Update forest to the latest release
    #[command(alias = "upgrade")]
    Update {
        /// Only report whether an update is available; don't install it
        #[arg(long = "check")]
        check: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load .env based on NODE_ENV or fallback to ".env"
    //if env::var("ENV") == Ok("dev".to_string()) {
        env::set_var("FOREST_API_URL", "http://localhost:3001/");
        env::set_var("FRONTEND_URL", "http://localhost:3000/");
    //} else {
    //    env::set_var("FOREST_API_URL", "https://api.forestpm.dev/");
   //     env::set_var("FRONTEND_URL", "https://forestpm.dev/");
    //}

    let cli = Cli::parse();
    let is_update = matches!(cli.command, Commands::Update { .. });

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
        Commands::Init { platform } => {
            init_command(platform).await?;
        }
        Commands::Install { package, version, alias } => {
            install_command(package, version, alias).await?;
        }
        Commands::Remove { package } => {
            remove_command(package).await?;
        }
        Commands::Update { check } => {
            update_command(check).await?;
        }
    }

    // Best-effort, throttled nudge if a newer forest exists (skipped during an
    // explicit update, in CI, and in non-interactive shells).
    if !is_update {
        maybe_notify_update().await;
    }

    Ok(())
}

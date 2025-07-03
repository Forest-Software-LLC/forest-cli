use clap::{Parser, Subcommand};
use std::thread;
use std::time::Duration;

mod tokens;
mod http;
mod message;
mod lockfile_gen;
mod lockfile_solver;
mod fetch_and_extract;
mod commands;
use commands::{login_command, install_command, init_command, publish_command};

use std::env;

/// Forest CLI - Package manager
#[derive(Parser)]
#[command(name = "forest", version = "0.1.0", about = "Forest CLI - Package manager")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Log in to your Forest account
    Login,

    /// Publish a package
    Publish,

    /// Initialize a new package
    Init,

    /// Install dependencies for the package
    #[command(alias = "i", alias = "grow")]
    Install {
        /// Package name (optional)
        package: Option<String>,

        /// Specify a version to install
        #[arg(short = 'v', long = "version")]
        version: Option<String>,
    },

    /// Remove a package from the project
    #[command(alias = "chop")]
    Remove,

    /// Update the package to the latest version
    #[command(alias = "water")]
    Update,

    /// Test spinner
    #[command(name = "spin")]
    TestSpinner,

    /// Test lockfile solver
    #[command(name = "test-solver")]
    TestSolver,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load .env based on NODE_ENV or fallback to ".env"
    if env::var("ENV") == Ok("dev".to_string()) {
        env::set_var("FOREST_API_URL", "http://localhost:3001/");
        env::set_var("FRONTEND_URL", "http://localhost:3000/");
    } else {
        env::set_var("FOREST_API_URL", "https://api.forestpm.dev/");
        env::set_var("FRONTEND_URL", "https://forestpm.dev/");
    }

    let cli = Cli::parse();

    match cli.command {
        Commands::Login => {
            login_command().await?;
        }
        Commands::Publish => {
            publish_command().await?;
        }
        Commands::Init => {
            init_command().await?;
        }
        Commands::Install { package, version } => {
            install_command(package, version).await?;
        }
        Commands::Remove => {
            println!("Chopping package... (this feature is not yet implemented)");
        }
        Commands::Update => {
            println!("Updating package... (this feature is not yet implemented)");
        }
        Commands::TestSpinner => {
            let mut msg = message::Message::new("Testing spinner...");
            thread::sleep(Duration::from_secs(10)); // Sleep for 2 seconds
            msg.emit(message::MessageType::Success, "Spinner test completed successfully.");
        }
        Commands::TestSolver => {
            // Placeholder for lockfile solver test
            lockfile_solver::test().await?;
        }
    }

    Ok(())
}

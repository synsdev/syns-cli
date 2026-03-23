mod config;
mod client;
mod output;
mod errors;
mod auth;
mod git;
mod push;
mod commands;

use clap::{Parser, Subcommand};

use config::Config;
use errors::CliError;
use output::Output;

#[derive(Parser)]
#[command(name = "syns", about = "Push, pull, and manage versioned file repositories")]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Override server URL
    #[arg(long, global = true, env = "SYNS_URL")]
    server: Option<String>,

    /// Output as JSON instead of tables
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Push files to a repository
    Push {},
    /// Pull files from a repository
    Pull {},
    /// List files in a repository
    Ls {},
    /// View a file's content
    Cat {},
    /// Show repository status
    Status {},
    /// View version history
    History {},
    /// Show changes between versions
    Diff {},
    /// Revert a file to a previous version
    Revert {},
    /// Repository management
    Repo {},
    /// Manage repository collaborators
    Collaborators {},
    /// Delete a repository
    Delete {},
    /// Browse public repositories
    Explore {},
    /// Fork a repository
    Fork {},
    /// Authenticate with the server
    Login {},
    /// Clear stored credentials
    Logout {},
    /// Show current authenticated user
    Whoami {},
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let output = Output::new(cli.json);

    let _config = match Config::new(cli.server.as_deref()) {
        Ok(config) => config,
        Err(e) => {
            output.error(&e);
            std::process::exit(e.exit_code());
        }
    };

    let result: Result<(), CliError> = match cli.command {
        Commands::Push {} => { output.success("push: not yet implemented"); Ok(()) }
        Commands::Pull {} => { output.success("pull: not yet implemented"); Ok(()) }
        Commands::Ls {} => { output.success("ls: not yet implemented"); Ok(()) }
        Commands::Cat {} => { output.success("cat: not yet implemented"); Ok(()) }
        Commands::Status {} => { output.success("status: not yet implemented"); Ok(()) }
        Commands::History {} => { output.success("history: not yet implemented"); Ok(()) }
        Commands::Diff {} => { output.success("diff: not yet implemented"); Ok(()) }
        Commands::Revert {} => { output.success("revert: not yet implemented"); Ok(()) }
        Commands::Repo {} => { output.success("repo: not yet implemented"); Ok(()) }
        Commands::Collaborators {} => { output.success("collaborators: not yet implemented"); Ok(()) }
        Commands::Delete {} => { output.success("delete: not yet implemented"); Ok(()) }
        Commands::Explore {} => { output.success("explore: not yet implemented"); Ok(()) }
        Commands::Fork {} => { output.success("fork: not yet implemented"); Ok(()) }
        Commands::Login {} => { output.success("login: not yet implemented"); Ok(()) }
        Commands::Logout {} => { output.success("logout: not yet implemented"); Ok(()) }
        Commands::Whoami {} => { output.success("whoami: not yet implemented"); Ok(()) }
    };

    if let Err(e) = result {
        output.error(&e);
        std::process::exit(e.exit_code());
    }
}

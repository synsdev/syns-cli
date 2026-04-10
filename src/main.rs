mod config;
mod client;
mod output;
mod errors;
mod auth;
mod repo;
mod push;
mod commands;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "syns", about = "Push, pull, and manage versioned file repositories")]
struct Cli {
    /// Override server URL
    #[arg(long, global = true, env = "SYNS_URL")]
    server: Option<String>,

    /// Output as JSON instead of tables
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Push files to a repository
    Push {},
    /// Pull files from a repository
    Pull {},
    /// List files in a repository
    Ls {
        /// Subdirectory path to list
        #[arg()]
        path: Option<String>,
    },
    /// View a file's content
    Cat {
        /// File path to display
        #[arg()]
        path: String,
    },
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
    let output = output::Output::new(cli.json);

    let config = match config::Config::new(cli.server.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            output.error(&e);
            std::process::exit(e.exit_code());
        }
    };

    let result = run(cli.command, &config, &output).await;

    if let Err(e) = result {
        output.error(&e);
        std::process::exit(e.exit_code());
    }
}

async fn run(command: Commands, config: &config::Config, output: &output::Output) -> Result<(), errors::CliError> {
    match command {
        Commands::Push {} => output.success("push: not yet implemented"),
        Commands::Pull {} => output.success("pull: not yet implemented"),
        Commands::Ls { path } => commands::ls::cmd_ls(config, output, path).await?,
        Commands::Cat { path } => commands::cat::cmd_cat(config, output, path).await?,
        Commands::Status {} => commands::status::cmd_status(config, output).await?,
        Commands::History {} => output.success("history: not yet implemented"),
        Commands::Diff {} => output.success("diff: not yet implemented"),
        Commands::Revert {} => output.success("revert: not yet implemented"),
        Commands::Repo {} => output.success("repo: not yet implemented"),
        Commands::Collaborators {} => output.success("collaborators: not yet implemented"),
        Commands::Delete {} => output.success("delete: not yet implemented"),
        Commands::Explore {} => output.success("explore: not yet implemented"),
        Commands::Fork {} => output.success("fork: not yet implemented"),
        Commands::Login {} => commands::login::cmd_login(config, output).await?,
        Commands::Logout {} => commands::logout::cmd_logout(config, output).await?,
        Commands::Whoami {} => commands::whoami::cmd_whoami(config, output).await?,
    }
    Ok(())
}

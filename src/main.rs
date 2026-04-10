mod config;
mod client;
mod output;
mod errors;
mod auth;
mod repo;
mod push;
mod commands;

use clap::{Parser, Subcommand};
use crate::commands::collaborators::CollaboratorsAction;
use crate::commands::repo::{CliRepoStatus, CliVisibility};

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
    History {
        /// Filter to a specific file path
        #[arg(long)]
        file: Option<String>,
        /// Maximum number of entries to show
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Show changes between versions
    Diff {
        /// Starting version (number or SHA)
        #[arg(long)]
        from: Option<String>,
        /// Ending version (number or SHA)
        #[arg(long)]
        to: Option<String>,
    },
    /// Revert a file to a previous version
    Revert {
        /// File path to revert
        #[arg()]
        path: String,
        /// Target version (number or SHA) to restore
        #[arg(long)]
        to: String,
        /// Custom commit message
        #[arg(long)]
        message: Option<String>,
    },
    /// Repository management
    Repo {
        /// Update repository description
        #[arg(long)]
        description: Option<String>,
        /// Update repository status
        #[arg(long)]
        status: Option<CliRepoStatus>,
        /// Update repository visibility
        #[arg(long)]
        visibility: Option<CliVisibility>,
        /// Set repository tags (replaces existing)
        #[arg(long)]
        tag: Vec<String>,
    },
    /// Manage repository collaborators
    Collaborators {
        #[command(subcommand)]
        action: Option<CollaboratorsAction>,
    },
    /// Delete a repository
    Delete {
        /// Skip confirmation prompt
        #[arg(long, short)]
        yes: bool,
    },
    /// Browse public repositories
    Explore {
        /// Search repositories by name or description
        #[arg(long, short = 'q')]
        query: Option<String>,
        /// Filter by tag (repeatable)
        #[arg(long)]
        tag: Vec<String>,
        /// Filter by status (active, draft, completed, abandoned)
        #[arg(long)]
        status: Option<String>,
        /// Maximum number of results
        #[arg(long, default_value = "20")]
        limit: u32,
        /// Number of results to skip
        #[arg(long, default_value = "0")]
        offset: u32,
    },
    /// Fork a repository
    Fork {
        /// Source repository in owner/name format
        #[arg(value_name = "REPO")]
        repo: String,
        /// Custom name for the forked repository
        #[arg(long)]
        name: Option<String>,
    },
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
        Commands::History { file, limit } => commands::history::cmd_history(config, output, file, limit).await?,
        Commands::Diff { from, to } => commands::diff::cmd_diff(config, output, from, to).await?,
        Commands::Revert { path, to, message } => commands::revert::cmd_revert(config, output, path, to, message).await?,
        Commands::Repo { description, status, visibility, tag } => commands::repo::cmd_repo(config, output, description, status, visibility, tag).await?,
        Commands::Collaborators { action } => commands::collaborators::cmd_collaborators(config, output, action).await?,
        Commands::Delete { yes } => commands::delete::cmd_delete(config, output, yes).await?,
        Commands::Explore { query, tag, status, limit, offset } => {
            commands::explore::cmd_explore(config, output, query, tag, status, limit, offset).await?
        }
        Commands::Fork { repo, name } => {
            commands::fork::cmd_fork(config, output, repo, name).await?
        }
        Commands::Login {} => commands::login::cmd_login(config, output).await?,
        Commands::Logout {} => commands::logout::cmd_logout(config, output).await?,
        Commands::Whoami {} => commands::whoami::cmd_whoami(config, output).await?,
    }
    Ok(())
}

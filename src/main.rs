use syns_cli::{commands, config, errors, output};

use clap::{CommandFactory, Parser, Subcommand};
use syns_cli::commands::collaborators::CollaboratorsAction;
use syns_cli::commands::push::PushArgs;
use syns_cli::commands::repo::{CliRepoStatus, CliVisibility, RepoAction};
use syns_cli::commands::repos::ReposArgs;
use syns_cli::commands::sync::ResolutionAction;
use syns_cli::commands::teams::TeamsAction;
use syns_cli::commands::upgrade::UpgradeArgs;

#[derive(Parser)]
#[command(
    name = "syns",
    version,
    about = "Push, pull, and manage versioned file repositories"
)]
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
    Push(PushArgs),
    /// Pull files from a repository
    Pull {
        /// Repository in OWNER/NAME format; a lone value not spelt that way is the PATH
        #[arg(value_name = "OWNER/NAME")]
        repo: Option<String>,
        /// Target directory (defaults to current directory)
        #[arg(value_name = "PATH")]
        path: Option<String>,
        /// Pull files at a specific version (number or SHA)
        #[arg(long)]
        version: Option<String>,
        /// Silently skip (exit 0) when no Syns repo identity resolves
        #[arg(long)]
        if_repo: bool,
        /// Rewrite local edits with the repository head (a snapshot of each is kept)
        #[arg(long)]
        overwrite: bool,
    },
    /// Converge this working copy with the repository head, publishing reviewed local work
    Sync {
        /// Silently skip (exit 0) when no Syns repo identity resolves
        #[arg(long)]
        if_repo: bool,
    },
    /// Show, continue or discard a pending resolution
    Resolution {
        #[command(subcommand)]
        action: ResolutionCommand,
    },
    /// List files in a repository
    Ls {
        /// Subdirectory path to list
        #[arg()]
        path: Option<String>,
        /// Silently skip (exit 0) when no Syns repo identity resolves
        #[arg(long)]
        if_repo: bool,
    },
    /// View a file's content
    Cat {
        /// File path to display
        #[arg()]
        path: String,
        /// Silently skip (exit 0) when no Syns repo identity resolves
        #[arg(long)]
        if_repo: bool,
    },
    /// Show repository status
    Status {
        /// Silently skip (exit 0) when no Syns repo identity resolves
        #[arg(long)]
        if_repo: bool,
    },
    /// View version history
    History {
        /// Filter to a specific file path
        #[arg(long)]
        file: Option<String>,
        /// Maximum number of entries to show
        #[arg(long, default_value_t = 50)]
        limit: u32,
        /// Silently skip (exit 0) when no Syns repo identity resolves
        #[arg(long)]
        if_repo: bool,
    },
    /// Show changes between versions
    Diff {
        /// Starting version (number or SHA)
        #[arg(long)]
        from: Option<String>,
        /// Ending version (number or SHA)
        #[arg(long)]
        to: Option<String>,
        /// Silently skip (exit 0) when no Syns repo identity resolves
        #[arg(long)]
        if_repo: bool,
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
        /// Silently skip (exit 0) when no Syns repo identity resolves
        #[arg(long)]
        if_repo: bool,
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
        /// Silently skip (exit 0) when no Syns repo identity resolves
        #[arg(long)]
        if_repo: bool,
        #[command(subcommand)]
        action: Option<RepoAction>,
    },
    /// List the caller's repositories
    Repos(ReposArgs),
    /// Manage repository collaborators
    Collaborators {
        #[command(subcommand)]
        action: Option<CollaboratorsAction>,
        /// Silently skip (exit 0) when no Syns repo identity resolves
        #[arg(long)]
        if_repo: bool,
    },
    /// Delete a repository
    Delete {
        /// Skip confirmation prompt
        #[arg(long, short)]
        yes: bool,
        /// Silently skip (exit 0) when no Syns repo identity resolves
        #[arg(long)]
        if_repo: bool,
    },
    /// Browse public repositories
    Explore {
        /// Search repositories by name or description
        #[arg(long, short = 'q')]
        query: Option<String>,
        /// Filter by tag (repeatable)
        #[arg(long)]
        tag: Vec<String>,
        /// Filter by status
        #[arg(long)]
        status: Option<CliRepoStatus>,
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
    /// Manage teams
    Teams {
        #[command(subcommand)]
        action: Option<TeamsAction>,
    },
    /// Upgrade the syns CLI binary to the latest release
    Upgrade(UpgradeArgs),
    /// Authenticate with the server
    Login {},
    /// Clear stored credentials
    Logout {},
    /// Show current authenticated user
    Whoami {},
}

#[derive(Subcommand)]
enum ResolutionCommand {
    /// Show the pending resolution: both commits, the changed paths, the markers left and the snapshots
    Show {
        /// Silently skip (exit 0) when no Syns repo identity resolves
        #[arg(long)]
        if_repo: bool,
    },
    /// Publish the folder as it stands as the reviewed resolution
    Continue {
        /// Silently skip (exit 0) when no Syns repo identity resolves
        #[arg(long)]
        if_repo: bool,
    },
    /// Put the folder back as it stood before the resolution rewrote it
    Discard {
        /// Silently skip (exit 0) when no Syns repo identity resolves
        #[arg(long)]
        if_repo: bool,
    },
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

async fn run(
    command: Commands,
    config: &config::Config,
    output: &output::Output,
) -> Result<(), errors::CliError> {
    match command {
        Commands::Push(args) => commands::push::cmd_push(config, output, &args).await?,
        Commands::Pull {
            repo,
            path,
            version,
            if_repo,
            overwrite,
        } => {
            // SPEC u262 `run` 1: the positionals bind by spelling before any
            // identity or credential is read; a malformed first of two ends
            // the process through the parser, never as a `CliError`.
            let bound = match commands::pull::bind_pull_positionals(repo, path) {
                Ok(bound) => bound,
                Err(refusal) => refuse_pull_positionals(&refusal),
            };
            commands::pull::cmd_pull(
                config,
                output,
                bound.repository,
                bound.path,
                version,
                if_repo,
                overwrite,
            )
            .await?
        }
        Commands::Sync { if_repo } => commands::sync::cmd_sync(config, output, if_repo).await?,
        Commands::Resolution { action } => {
            let (action, if_repo) = match action {
                ResolutionCommand::Show { if_repo } => (ResolutionAction::Show, if_repo),
                ResolutionCommand::Continue { if_repo } => (ResolutionAction::Continue, if_repo),
                ResolutionCommand::Discard { if_repo } => (ResolutionAction::Discard, if_repo),
            };
            commands::sync::cmd_resolution(config, output, action, if_repo).await?
        }
        Commands::Ls { path, if_repo } => {
            commands::ls::cmd_ls(config, output, path, if_repo).await?
        }
        Commands::Cat { path, if_repo } => {
            commands::cat::cmd_cat(config, output, path, if_repo).await?
        }
        Commands::Status { if_repo } => {
            commands::status::cmd_status(config, output, if_repo).await?
        }
        Commands::History {
            file,
            limit,
            if_repo,
        } => commands::history::cmd_history(config, output, file, limit, if_repo).await?,
        Commands::Diff { from, to, if_repo } => {
            commands::diff::cmd_diff(config, output, from, to, if_repo).await?
        }
        Commands::Revert {
            path,
            to,
            message,
            if_repo,
        } => commands::revert::cmd_revert(config, output, path, to, message, if_repo).await?,
        Commands::Repo {
            description,
            status,
            visibility,
            tag,
            if_repo,
            action,
        } => {
            commands::repo::cmd_repo(
                config,
                output,
                description,
                status,
                visibility,
                tag,
                if_repo,
                action,
            )
            .await?
        }
        Commands::Repos(args) => commands::repos::cmd_repos(config, output, &args).await?,
        Commands::Collaborators {
            action,
            if_repo: parent_if_repo,
        } => {
            let action_if_repo = match &action {
                Some(CollaboratorsAction::Add { if_repo, .. }) => *if_repo,
                Some(CollaboratorsAction::Remove { if_repo, .. }) => *if_repo,
                None => false,
            };
            commands::collaborators::cmd_collaborators(
                config,
                output,
                action,
                parent_if_repo || action_if_repo,
            )
            .await?
        }
        Commands::Delete { yes, if_repo } => {
            commands::delete::cmd_delete(config, output, yes, if_repo).await?
        }
        Commands::Explore {
            query,
            tag,
            status,
            limit,
            offset,
        } => {
            commands::explore::cmd_explore(config, output, query, tag, status, limit, offset)
                .await?
        }
        Commands::Fork { repo, name } => {
            commands::fork::cmd_fork(config, output, repo, name).await?
        }
        Commands::Teams { action } => commands::teams::cmd_teams(config, output, action).await?,
        Commands::Upgrade(args) => commands::upgrade::run(args, output).await?,
        Commands::Login {} => commands::login::cmd_login(config, output).await?,
        Commands::Logout {} => commands::logout::cmd_logout(config, output).await?,
        Commands::Whoami {} => commands::whoami::cmd_whoami(config, output).await?,
    }
    Ok(())
}

/// Ends the process with the parser's refusal of a first `syns pull`
/// positional lacking the repository shape: the message and the `pull`
/// usage block on the diagnostic stream, exit `2`, in both output modes.
fn refuse_pull_positionals(refusal: &commands::pull::PositionalRefusal) -> ! {
    let mut command = Cli::command();
    command.build();
    let pull = command
        .find_subcommand_mut("pull")
        .expect("the pull subcommand is declared");
    pull.error(
        clap::error::ErrorKind::ValueValidation,
        format!(
            "invalid value '{}' for '[OWNER/NAME]': expected OWNER/NAME, or a lone PATH",
            refusal.value
        ),
    )
    .exit()
}

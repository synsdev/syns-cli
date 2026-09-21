use syns_cli::{commands, config, errors, output, read, write};

use clap::{CommandFactory, Parser, Subcommand};
use syns_cli::commands::collaborators::CollaboratorsAction;
use syns_cli::commands::grep::{GrepArgs, GrepOutput};
use syns_cli::commands::history::HistoryAction;
use syns_cli::commands::links::LinksAction;
use syns_cli::commands::push::PushArgs;
use syns_cli::commands::repo::{CliRepoStatus, CliVisibility, RepoAction};
use syns_cli::commands::repos::ReposArgs;
use syns_cli::commands::sync::ResolutionAction;
use syns_cli::commands::teams::TeamsAction;
use syns_cli::commands::upgrade::UpgradeArgs;
use syns_cli::read::RepoScopeArgs;

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

/// The three options every read verb carries, spelt and bound
/// identically on each (SPEC u270 Contract Surface, `ReadOptions`).
#[derive(clap::Args, Clone)]
struct ReadOpts {
    /// Read another repository, as OWNER/NAME
    #[arg(long, value_name = "OWNER/NAME", value_parser = read::parse_repo_id)]
    repo: Option<String>,
    /// Read at a specific version (number or SHA)
    #[arg(long)]
    version: Option<String>,
    /// Silently skip (exit 0) when no Syns repo identity resolves
    #[arg(long)]
    if_repo: bool,
}

impl From<ReadOpts> for read::ReadOptions {
    fn from(opts: ReadOpts) -> read::ReadOptions {
        read::ReadOptions {
            repo: opts.repo,
            version: opts.version,
            if_repo: opts.if_repo,
        }
    }
}

/// The options every write verb carries, spelt and bound identically on
/// each (SPEC u271 Contract Surface, `WriteOptions`). `--parent` is
/// required by the parser on all four and carries no single-letter
/// alias.
#[derive(clap::Args, Clone)]
struct WriteOpts {
    /// Write to another repository, as OWNER/NAME
    #[arg(long, value_name = "OWNER/NAME", value_parser = read::parse_repo_id)]
    repo: Option<String>,
    /// The commit this write is made against (number or SHA)
    #[arg(long, value_name = "REF")]
    parent: String,
    /// Commit message
    #[arg(long, short = 'm')]
    message: Option<String>,
    /// What integration is publishing this commit
    #[arg(long)]
    integration: Option<String>,
    /// The run publishing this commit
    #[arg(long)]
    run: Option<String>,
    /// What triggered this commit
    #[arg(long)]
    trigger: Option<String>,
    /// The task this commit belongs to
    #[arg(long)]
    task_ref: Option<String>,
}

impl From<WriteOpts> for write::WriteOptions {
    fn from(opts: WriteOpts) -> write::WriteOptions {
        write::WriteOptions {
            repo: opts.repo,
            parent: opts.parent,
            message: opts.message,
            provenance: write::ProvenanceOptions {
                integration: opts.integration,
                run: opts.run,
                trigger: opts.trigger,
                task_ref: opts.task_ref,
            },
        }
    }
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
        /// List the whole subtree rather than one directory
        #[arg(long)]
        recursive: bool,
        #[command(flatten)]
        read: ReadOpts,
    },
    /// View a file's content
    Cat {
        /// File path to display
        #[arg()]
        path: String,
        #[command(flatten)]
        read: ReadOpts,
    },
    /// Print a file's numbered lines over a window
    Read {
        /// File path to read
        #[arg()]
        path: String,
        /// First line to print, counting from 1
        #[arg(long, default_value_t = commands::read::DEFAULT_OFFSET)]
        offset: u32,
        /// How many lines to print
        #[arg(long, default_value_t = commands::read::DEFAULT_LIMIT)]
        limit: u32,
        #[command(flatten)]
        read: ReadOpts,
    },
    /// Match a repository's paths against a pattern
    Glob {
        /// Glob pattern, matched against each whole repository-relative path
        #[arg(value_name = "PATTERN")]
        pattern: String,
        /// Narrow the tree read to a subdirectory
        #[arg(long)]
        path: Option<String>,
        #[command(flatten)]
        read: ReadOpts,
    },
    /// Search a repository's file contents
    Grep {
        /// Regular expression to search for
        #[arg(value_name = "PATTERN")]
        pattern: String,
        /// Narrow the tree read to a subdirectory
        #[arg(long)]
        path: Option<String>,
        /// Only search paths matching this glob (repeatable)
        #[arg(long)]
        glob: Vec<String>,
        /// Match case-insensitively
        #[arg(long, short = 'i')]
        ignore_case: bool,
        /// Show line numbers
        #[arg(long, short = 'n')]
        line_number: bool,
        /// Lines to show after each match
        #[arg(long, short = 'A', value_name = "NUM")]
        after_context: Option<u32>,
        /// Lines to show before each match
        #[arg(long, short = 'B', value_name = "NUM")]
        before_context: Option<u32>,
        /// Lines to show either side of each match
        #[arg(long, short = 'C', value_name = "NUM")]
        context: Option<u32>,
        /// What the search answers with
        #[arg(long, value_enum, default_value = "content")]
        output: GrepOutput,
        /// Carry at most this many rows
        #[arg(long, value_name = "NUM")]
        head_limit: Option<u32>,
        #[command(flatten)]
        read: ReadOpts,
    },
    /// Replace text in one file and publish the result as one commit
    Edit {
        /// File path to edit
        #[arg()]
        path: String,
        /// The exact text to replace
        #[arg(long)]
        old: String,
        /// The text to put in its place
        #[arg(long)]
        new: String,
        /// Replace every occurrence rather than refusing more than one
        #[arg(long)]
        replace_all: bool,
        #[command(flatten)]
        write: WriteOpts,
    },
    /// Write one file's whole content, read from standard input
    Write {
        /// File path to write
        #[arg()]
        path: String,
        #[command(flatten)]
        write: WriteOpts,
    },
    /// Remove one path from a repository
    Rm {
        /// File or directory path to remove
        #[arg()]
        path: String,
        #[command(flatten)]
        write: WriteOpts,
    },
    /// Publish a changeset read from standard input as one commit
    Commit {
        #[command(flatten)]
        write: WriteOpts,
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
        #[command(subcommand)]
        action: Option<HistoryAction>,
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
        #[arg(long, short = 'm')]
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
        #[arg(long, short = 't')]
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
        /// Maximum number of collaborators to list
        #[arg(long, default_value_t = commands::collaborators::DEFAULT_COLLABORATOR_LIMIT)]
        limit: u32,
        /// Number of collaborators to skip
        #[arg(long, default_value_t = commands::collaborators::DEFAULT_COLLABORATOR_OFFSET)]
        offset: u32,
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
        #[arg(long, short = 't')]
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
        #[arg(long, short = 'n')]
        name: Option<String>,
    },
    /// List the repositories copied from a repository
    Forks {
        /// Maximum number of forks to show
        #[arg(long, default_value_t = commands::forks::DEFAULT_LIMIT)]
        limit: u32,
        /// Number of forks to skip
        #[arg(long, default_value_t = commands::forks::DEFAULT_OFFSET)]
        offset: u32,
        #[command(flatten)]
        scope: RepoScopeArgs,
    },
    /// Search people by handle or display name
    Users {
        /// What to match against a handle or a display name
        #[arg(value_name = "QUERY")]
        query: String,
        /// Maximum number of matches to show
        #[arg(long, default_value_t = commands::users::DEFAULT_SEARCH_LIMIT)]
        limit: u32,
    },
    /// Show a person's profile, the caller's own where none is named
    User {
        /// The handle to show; the caller's own where it is absent
        #[arg(value_name = "USERNAME")]
        username: Option<String>,
    },
    /// Manage the links on the caller's own profile
    Links {
        #[command(subcommand)]
        action: LinksAction,
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

/// The stack `syns` runs on, reserved by this program rather than by
/// the linker. Windows reserves 1 MiB for a process's main thread — the
/// MSVC and GNU linker defaults — where Linux and macOS reserve 8 MiB,
/// and the whole clap command tree is built on that thread before the
/// first argument is read. An unoptimised build of `Commands` already
/// stands within 2% of 1 MiB, so a subcommand added to it overflows the
/// Windows binary on `syns --help` (CI run 35555203821, job
/// `convergence-windows`). Reserving it here rather than through a
/// linker argument keeps it out of reach of a `RUSTFLAGS` the release
/// build sets, which makes cargo discard every `[target.*]` section of
/// a `.cargo/config.toml`.
const RUN_STACK_BYTES: usize = 8 * 1024 * 1024;

/// The exit code an unwinding `main` takes, which a panic on the run
/// thread must still reach the caller as.
const PANIC_EXIT: i32 = 101;

fn main() {
    let run = std::thread::Builder::new()
        .name("syns".to_string())
        .stack_size(RUN_STACK_BYTES)
        .spawn(run_cli)
        .expect("the run thread spawns");
    // The panic hook has already written whatever a panic on that
    // thread had to say, so the join answer carries nothing left to
    // report and only the exit code is owed.
    if run.join().is_err() {
        std::process::exit(PANIC_EXIT);
    }
}

#[tokio::main]
async fn run_cli() {
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
        Commands::Ls {
            path,
            recursive,
            read,
        } => commands::ls::cmd_ls(config, output, path, recursive, read.into()).await?,
        Commands::Cat { path, read } => {
            commands::cat::cmd_cat(config, output, path, read.into()).await?
        }
        Commands::Read {
            path,
            offset,
            limit,
            read,
        } => commands::read::cmd_read(config, output, path, offset, limit, read.into()).await?,
        Commands::Glob {
            pattern,
            path,
            read,
        } => commands::glob::cmd_glob(config, output, pattern, path, read.into()).await?,
        Commands::Grep {
            pattern,
            path,
            glob,
            ignore_case,
            line_number,
            after_context,
            before_context,
            context,
            output: mode,
            head_limit,
            read,
        } => {
            let args = GrepArgs {
                path,
                glob,
                ignore_case,
                line_number,
                after: after_context,
                before: before_context,
                context,
                output: mode,
                head_limit,
            };
            commands::grep::cmd_grep(config, output, pattern, args, read.into()).await?
        }
        Commands::Edit {
            path,
            old,
            new,
            replace_all,
            write,
        } => {
            commands::edit::cmd_edit(config, output, path, old, new, replace_all, write.into())
                .await?
        }
        Commands::Write { path, write } => {
            commands::write::cmd_write(config, output, path, write.into()).await?
        }
        Commands::Rm { path, write } => {
            commands::rm::cmd_rm(config, output, path, write.into()).await?
        }
        Commands::Commit { write } => {
            commands::commit::cmd_commit(config, output, write.into()).await?
        }
        Commands::Status { if_repo } => {
            commands::status::cmd_status(config, output, if_repo).await?
        }
        Commands::History {
            file,
            limit,
            if_repo,
            action,
        } => match action {
            Some(HistoryAction::Show { reference, scope }) => {
                commands::history::cmd_history_show(config, output, reference, scope).await?
            }
            None => commands::history::cmd_history(config, output, file, limit, if_repo).await?,
        },
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
            limit,
            offset,
            if_repo: parent_if_repo,
        } => {
            let action_if_repo = match &action {
                Some(CollaboratorsAction::Add { if_repo, .. }) => *if_repo,
                Some(CollaboratorsAction::Role { if_repo, .. }) => *if_repo,
                Some(CollaboratorsAction::Remove { if_repo, .. }) => *if_repo,
                None => false,
            };
            let if_repo = parent_if_repo || action_if_repo;
            // The role change binds the repository itself, so it is
            // routed straight rather than through the noun's listing
            // path (SPEC u272 Behaviour, `cmd_collaborators_role` 1).
            match action {
                Some(CollaboratorsAction::Role { user_id, role, .. }) => {
                    commands::collaborators::cmd_collaborators_role(
                        config, output, user_id, role, if_repo,
                    )
                    .await?
                }
                action => {
                    commands::collaborators::cmd_collaborators(
                        config, output, action, if_repo, limit, offset,
                    )
                    .await?
                }
            }
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
        Commands::Forks {
            limit,
            offset,
            scope,
        } => commands::forks::cmd_forks(config, output, limit, offset, scope).await?,
        Commands::Users { query, limit } => {
            commands::users::cmd_users(config, output, query, limit).await?
        }
        Commands::User { username } => commands::users::cmd_user(config, output, username).await?,
        Commands::Links { action } => commands::links::cmd_links(config, output, action).await?,
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

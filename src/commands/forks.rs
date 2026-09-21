//! `syns forks` — the repositories copied from one repository
//! (SPEC u272, `EP-list-forks`).

use crate::client::SynsClient;
use crate::commands::repos::{LIMIT_MAX, LIMIT_MIN, refuse_limit_outside};
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::read::{RepoScopeArgs, resolve_repo_scope};

/// The window `syns forks` sends where the caller names neither half.
pub const DEFAULT_LIMIT: u32 = 20;
pub const DEFAULT_OFFSET: u32 = 0;

/// The count of the rows against the whole, which follows the block on
/// the diagnostic stream (SPEC u272 Contract Surface, the forks render)
/// — and is withheld where the page held the whole.
pub(crate) fn count_line(shown: usize, total: u32) -> Option<String> {
    if total as usize > shown {
        Some(format!("Showing {shown} of {total} forks."))
    } else {
        None
    }
}

pub async fn cmd_forks(
    config: &Config,
    output: &Output,
    limit: u32,
    offset: u32,
    args: RepoScopeArgs,
) -> Result<(), CliError> {
    // 1 — resolve the scope.
    let scope = match resolve_repo_scope(config, output, &args).await? {
        Some(scope) => scope,
        None => return Ok(()),
    };

    // 2 — refuse a window outside the paged-listing bound, before any
    //     request leaves.
    refuse_limit_outside(limit, LIMIT_MIN, LIMIT_MAX)?;

    // 3 — ask the fork listing of that repository.
    let client = SynsClient::new(config.server_url())?;
    let (response, raw) = client
        .list_forks(&scope.repo_id, scope.token.as_deref(), limit, offset)
        .await?;

    // 4 — render the page, or write the served body.
    if output.is_json() {
        output.json(&raw);
        return Ok(());
    }

    let rows: Vec<Vec<String>> = response
        .data
        .iter()
        .map(|fork| {
            vec![
                format!("{}/{}", fork.owner, fork.name),
                fork.description.as_deref().unwrap_or("").to_string(),
                format!("{:?}", fork.visibility).to_lowercase(),
                fork.updated_at.clone(),
            ]
        })
        .collect();
    output.table(
        &["Repository", "Description", "Visibility", "Updated"],
        rows,
    );
    if let Some(line) = count_line(response.data.len(), response.total) {
        eprintln!("{line}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::u272_bodies as B;
    use serial_test::serial;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // SPEC u272 Behaviour, `cmd_forks` 2: a page size outside the bound
    // is refused before any request leaves.
    #[tokio::test]
    #[serial]
    async fn a_page_size_outside_the_bound_is_refused_before_any_request() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        let server = MockServer::start().await;
        let config = Config::new(Some(&server.uri())).unwrap();
        let output = Output::new(false);

        let err = cmd_forks(
            &config,
            &output,
            0,
            0,
            RepoScopeArgs {
                repo: Some("alice/notes".into()),
                if_repo: false,
            },
        )
        .await
        .unwrap_err();
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert_eq!(
            err.to_string(),
            "configuration error: --limit must be between 1 and 100 (got 0)"
        );
        assert_eq!(err.exit_code(), 1);
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    #[serial]
    async fn a_page_size_past_the_ceiling_is_refused_too() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        let server = MockServer::start().await;
        let config = Config::new(Some(&server.uri())).unwrap();
        let output = Output::new(false);

        let err = cmd_forks(
            &config,
            &output,
            101,
            0,
            RepoScopeArgs {
                repo: Some("alice/notes".into()),
                if_repo: false,
            },
        )
        .await
        .unwrap_err();
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert_eq!(
            err.to_string(),
            "configuration error: --limit must be between 1 and 100 (got 101)"
        );
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    // SPEC u272 Behaviour, `cmd_forks` 3 and 4: the served page reaches
    // the render under the window the caller named.
    #[tokio::test]
    #[serial]
    async fn the_served_page_is_rendered_under_the_window_the_caller_named() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/u272alice/u272-parent/forks"))
            .and(query_param("limit", "20"))
            .and(query_param("offset", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_string(B::FORKS_LOCAL))
            .mount(&server)
            .await;
        let config = Config::new(Some(&server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_forks(
            &config,
            &output,
            DEFAULT_LIMIT,
            DEFAULT_OFFSET,
            RepoScopeArgs {
                repo: Some("U272Alice/U272-Parent".into()),
                if_repo: false,
            },
        )
        .await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok(), "got {:?}", result.err());
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    // SPEC u272 Behaviour, `cmd_forks` 1: the skip envelope where no
    // identity resolves and `--if-repo` stands, with no request made.
    #[tokio::test]
    #[serial]
    async fn no_identity_under_if_repo_skips_with_no_request_made() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        let server = MockServer::start().await;
        let config = Config::new(Some(&server.uri())).unwrap();
        let output = Output::new(true);

        let result = cmd_forks(
            &config,
            &output,
            DEFAULT_LIMIT,
            DEFAULT_OFFSET,
            RepoScopeArgs {
                repo: None,
                if_repo: true,
            },
        )
        .await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    // SPEC u272 Contract Surface, the forks render: the count of the
    // rows against the whole stands only where the page held less.
    #[test]
    fn the_count_line_is_withheld_where_the_page_held_the_whole() {
        assert_eq!(count_line(1, 1), None);
        assert_eq!(count_line(0, 0), None);
        assert_eq!(count_line(2, 5).as_deref(), Some("Showing 2 of 5 forks."));
    }
}

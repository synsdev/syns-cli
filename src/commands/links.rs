//! The four profile-link arms (SPEC u272): one noun whose every arm
//! answers the caller's whole ordered link list, so a caller never needs
//! a second read to learn the new order.

use crate::auth::token::TokenStore;
use crate::client::SynsClient;
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;

/// The eight link kinds the identity boundary registers (SPEC u272
/// Contract Surface, `LinkKind`). A value outside them ends the run
/// through the argument parser at exit `2`.
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    Website,
    Github,
    X,
    Mastodon,
    Linkedin,
    Youtube,
    Bluesky,
    Generic,
}

impl LinkKind {
    /// The spelling the entry's own `kind` takes on the wire.
    pub fn as_wire_str(&self) -> &'static str {
        match self {
            LinkKind::Website => "website",
            LinkKind::Github => "github",
            LinkKind::X => "x",
            LinkKind::Mastodon => "mastodon",
            LinkKind::Linkedin => "linkedin",
            LinkKind::Youtube => "youtube",
            LinkKind::Bluesky => "bluesky",
            LinkKind::Generic => "generic",
        }
    }
}

/// Which of the four link entries the run reaches (SPEC u272 Contract
/// Surface, `LinksAction`).
#[derive(clap::Subcommand, Debug, Clone)]
pub enum LinksAction {
    /// Add a link to the caller's profile
    Add {
        /// What kind of link this is
        #[arg(long)]
        kind: LinkKind,
        /// The link's address or handle
        #[arg(long)]
        value: String,
        /// A label to show in place of the value
        #[arg(long)]
        label: Option<String>,
    },
    /// Change one link on the caller's profile
    Update {
        /// The link's identifier
        #[arg(value_name = "LINK_ID")]
        id: String,
        /// What kind of link this is
        #[arg(long)]
        kind: Option<LinkKind>,
        /// The link's address or handle
        #[arg(long)]
        value: Option<String>,
        /// A label to show in place of the value
        #[arg(long)]
        label: Option<String>,
        /// Where the link stands in the list, counting from 0
        #[arg(long)]
        sort_order: Option<u32>,
    },
    /// Remove one link from the caller's profile
    Remove {
        /// The link's identifier
        #[arg(value_name = "LINK_ID")]
        id: String,
    },
    /// Put the caller's links in the order given
    Reorder {
        /// Every link identifier the profile holds, exactly once, in the
        /// order they are to stand in
        #[arg(value_name = "LINK_ID", required = true)]
        order: Vec<String>,
    },
}

/// The links-update refusal (SPEC u272 Contract Surface): raised before
/// any request, so an update naming nothing sends nothing.
pub const UPDATE_NAMES_NOTHING: &str =
    "name at least one of --kind, --value, --label or --sort-order";

/// The reorder's own refusal: the entry takes every identifier exactly
/// once, so a repeat is refused here rather than spent on a round trip.
pub fn reorder_repeats_refusal(id: &str) -> String {
    format!("--reorder names '{id}' more than once")
}

/// The arm's own pre-request checks (SPEC u272 Behaviour, `cmd_links` 2).
pub fn check_action(action: &LinksAction) -> Result<(), CliError> {
    match action {
        LinksAction::Update {
            kind,
            value,
            label,
            sort_order,
            ..
        } => {
            if kind.is_none() && value.is_none() && label.is_none() && sort_order.is_none() {
                return Err(CliError::Config {
                    message: UPDATE_NAMES_NOTHING.to_string(),
                });
            }
            Ok(())
        }
        LinksAction::Reorder { order } => {
            let mut seen: Vec<&str> = Vec::with_capacity(order.len());
            for id in order {
                if seen.contains(&id.as_str()) {
                    return Err(CliError::Config {
                        message: reorder_repeats_refusal(id),
                    });
                }
                seen.push(id.as_str());
            }
            Ok(())
        }
        LinksAction::Add { .. } | LinksAction::Remove { .. } => Ok(()),
    }
}

pub async fn cmd_links(
    config: &Config,
    output: &Output,
    action: LinksAction,
) -> Result<(), CliError> {
    // 1 — read the stored credential, before any request.
    let token = TokenStore::new(config.credentials_path())
        .read()?
        .ok_or(CliError::AuthRequired)?;

    // 2 — the arm's own values.
    check_action(&action)?;

    // 3 — send the arm's request.
    let client = SynsClient::new(config.server_url())?;
    let (response, raw) = client.user_links(&token, &action).await?;

    // 4 — render the whole ordered list the answer carried, whichever
    //     arm reached it, or write the served body.
    if output.is_json() {
        output.json(&raw);
        return Ok(());
    }
    let rows: Vec<Vec<String>> = response
        .links
        .iter()
        .map(|link| {
            vec![
                link.id.clone(),
                link.kind.clone(),
                link.value.clone(),
                link.label.clone().unwrap_or_default(),
            ]
        })
        .collect();
    output.table(&["Link ID", "Kind", "Value", "Label"], rows);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::ValueEnum;

    // SPEC u272 Contract Surface, `LinkKind`: the eight registered
    // spellings and nothing beside them.
    #[test]
    fn the_eight_registered_kinds_are_the_whole_set() {
        let spellings: Vec<&str> = LinkKind::value_variants()
            .iter()
            .map(|k| k.as_wire_str())
            .collect();
        assert_eq!(
            spellings,
            vec![
                "website", "github", "x", "mastodon", "linkedin", "youtube", "bluesky", "generic",
            ]
        );
        for spelling in &spellings {
            assert!(
                LinkKind::from_str(spelling, true).is_ok(),
                "{spelling} is admitted"
            );
        }
        assert!(LinkKind::from_str("tumblr", true).is_err());
    }

    // SPEC u272 Behaviour, `cmd_links` 2: an update naming no changing
    // option is refused.
    #[test]
    fn an_update_naming_nothing_is_refused() {
        let err = check_action(&LinksAction::Update {
            id: "11111111-1111-1111-1111-111111111111".into(),
            kind: None,
            value: None,
            label: None,
            sort_order: None,
        })
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "configuration error: name at least one of --kind, --value, --label or --sort-order"
        );
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn an_update_naming_one_option_passes() {
        for action in [
            LinksAction::Update {
                id: "a".into(),
                kind: Some(LinkKind::X),
                value: None,
                label: None,
                sort_order: None,
            },
            LinksAction::Update {
                id: "a".into(),
                kind: None,
                value: Some("https://example.test/a".into()),
                label: None,
                sort_order: None,
            },
            LinksAction::Update {
                id: "a".into(),
                kind: None,
                value: None,
                label: Some("home".into()),
                sort_order: None,
            },
            LinksAction::Update {
                id: "a".into(),
                kind: None,
                value: None,
                label: None,
                sort_order: Some(0),
            },
        ] {
            assert!(check_action(&action).is_ok());
        }
    }

    // SPEC u272 Behaviour, `cmd_links` 2: a reorder naming one
    // identifier twice is refused before any request.
    #[test]
    fn a_reorder_naming_one_identifier_twice_is_refused() {
        let err = check_action(&LinksAction::Reorder {
            order: vec!["a".into(), "b".into(), "a".into()],
        })
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "configuration error: --reorder names 'a' more than once"
        );
        assert_eq!(err.exit_code(), 1);

        assert!(
            check_action(&LinksAction::Reorder {
                order: vec!["a".into(), "b".into(), "c".into()],
            })
            .is_ok()
        );
    }

    #[test]
    fn the_add_and_remove_arms_carry_no_pre_request_check() {
        assert!(
            check_action(&LinksAction::Add {
                kind: LinkKind::Website,
                value: "https://example.test".into(),
                label: None,
            })
            .is_ok()
        );
        assert!(check_action(&LinksAction::Remove { id: "a".into() }).is_ok());
    }

    // --- `cmd_links` (SPEC u272 Behaviour, `cmd_links` 1, 3 and 4) ---

    use crate::client::u272_bodies as B;
    use serial_test::serial;
    use wiremock::matchers::{header, method, path as path_matcher};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn seed(dir: &std::path::Path, token: Option<&str>) {
        if let Some(token) = token {
            TokenStore::new(dir.join("credentials.json"))
                .write(token)
                .unwrap();
        }
        std::env::set_current_dir(dir).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir) };
    }

    fn unseed() {
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };
    }

    // `cmd_links` 1: the credential is required ahead of every arm, and
    // of the arm's own checks.
    #[tokio::test]
    #[serial]
    async fn every_arm_requires_a_credential_before_any_request() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), None);
        let server = MockServer::start().await;
        let config = Config::new(Some(&server.uri())).unwrap();
        let output = Output::new(false);

        for action in [
            LinksAction::Add {
                kind: LinkKind::Website,
                value: "https://example.test".into(),
                label: None,
            },
            LinksAction::Update {
                id: "a".into(),
                kind: None,
                value: None,
                label: Some("l".into()),
                sort_order: None,
            },
            LinksAction::Remove { id: "a".into() },
            LinksAction::Reorder {
                order: vec!["a".into()],
            },
        ] {
            let err = cmd_links(&config, &output, action).await.unwrap_err();
            assert!(matches!(err, CliError::AuthRequired));
            assert_eq!(err.exit_code(), 1);
        }
        unseed();
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    // `cmd_links` 2: the update refusal stands ahead of the request.
    #[tokio::test]
    #[serial]
    async fn an_update_naming_nothing_sends_nothing() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), Some("u272-token"));
        let server = MockServer::start().await;
        let config = Config::new(Some(&server.uri())).unwrap();
        let output = Output::new(false);

        let err = cmd_links(
            &config,
            &output,
            LinksAction::Update {
                id: "11111111-1111-1111-1111-111111111111".into(),
                kind: None,
                value: None,
                label: None,
                sort_order: None,
            },
        )
        .await
        .unwrap_err();
        unseed();

        assert_eq!(
            err.to_string(),
            "configuration error: name at least one of --kind, --value, --label or --sort-order"
        );
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    // `cmd_links` 2: so does the reorder's duplicate refusal.
    #[tokio::test]
    #[serial]
    async fn a_reorder_repeating_an_identifier_sends_nothing() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), Some("u272-token"));
        let server = MockServer::start().await;
        let config = Config::new(Some(&server.uri())).unwrap();
        let output = Output::new(false);

        let err = cmd_links(
            &config,
            &output,
            LinksAction::Reorder {
                order: vec!["a".into(), "a".into()],
            },
        )
        .await
        .unwrap_err();
        unseed();

        assert_eq!(
            err.to_string(),
            "configuration error: --reorder names 'a' more than once"
        );
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    // `cmd_links` 3 and 4: each arm reaches its own address under the
    // credential, and one render is reached from all four.
    #[tokio::test]
    #[serial]
    async fn all_four_arms_reach_their_address_and_one_render() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), Some("u272-token"));
        let server = MockServer::start().await;
        for (verb, address, served) in [
            ("POST", "/api/v1/me/links", B::LINKS_CREATE),
            (
                "PATCH",
                "/api/v1/me/links/c3927ea2-a953-498b-8e83-86c870887758",
                B::LINKS_UPDATE,
            ),
            (
                "DELETE",
                "/api/v1/me/links/c3927ea2-a953-498b-8e83-86c870887758",
                B::LINKS_DELETE,
            ),
            ("POST", "/api/v1/me/links/reorder", B::LINKS_REORDER),
        ] {
            Mock::given(method(verb))
                .and(path_matcher(address))
                .and(header("authorization", "Bearer u272-token"))
                .respond_with(ResponseTemplate::new(200).set_body_string(served))
                .mount(&server)
                .await;
        }
        let config = Config::new(Some(&server.uri())).unwrap();
        let output = Output::new(false);
        let id = "c3927ea2-a953-498b-8e83-86c870887758";

        for action in [
            LinksAction::Add {
                kind: LinkKind::Generic,
                value: "https://u272.example.test/probe".into(),
                label: Some("u272 probe".into()),
            },
            LinksAction::Update {
                id: id.into(),
                kind: None,
                value: None,
                label: Some("u272 probe updated".into()),
                sort_order: None,
            },
            LinksAction::Reorder {
                order: vec![id.into(), "21474465-4502-4150-adf0-aa0b6a0cc4d5".into()],
            },
            LinksAction::Remove { id: id.into() },
        ] {
            let result = cmd_links(&config, &output, action).await;
            assert!(result.is_ok(), "got {:?}", result.err());
        }
        unseed();

        let requests = server.received_requests().await.unwrap();
        let reached: Vec<(String, String)> = requests
            .iter()
            .map(|r| (r.method.to_string(), r.url.path().to_string()))
            .collect();
        assert_eq!(
            reached,
            vec![
                ("POST".to_string(), "/api/v1/me/links".to_string()),
                ("PATCH".to_string(), format!("/api/v1/me/links/{id}")),
                ("POST".to_string(), "/api/v1/me/links/reorder".to_string()),
                ("DELETE".to_string(), format!("/api/v1/me/links/{id}")),
            ]
        );
    }
}

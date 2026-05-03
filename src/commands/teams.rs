use crate::auth::token::TokenStore;
use crate::client::{
    ChangeRoleRequest, CollaboratorRole, CreateTeamRequest, InviteRequest, SynsClient,
    TeamRepoAccessRequest, TeamRole, UpdateTeamRequest,
};
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::prompts::{ConfirmOutcome, confirm_or_yes};
use clap::Subcommand;
use console::style;
use serde_json::json;
use std::io::{IsTerminal, Write};

#[derive(Subcommand, Debug)]
pub enum TeamsAction {
    /// Create a new team
    Create {
        /// Team name
        name: String,
        /// Team description
        #[arg(long)]
        description: Option<String>,
    },
    /// Show team details
    Show {
        /// Team name (or owner/team-name to disambiguate)
        name: String,
    },
    /// Update team metadata
    Update {
        /// Team name (or owner/team-name to disambiguate)
        name: String,
        /// Rename the team
        #[arg(long)]
        rename: Option<String>,
        /// Set description
        #[arg(long)]
        description: Option<String>,
        /// Clear description
        #[arg(long)]
        clear_description: bool,
    },
    /// Delete a team (owner only)
    Delete {
        /// Team name (or owner/team-name to disambiguate)
        name: String,
        /// Skip confirmation prompt
        #[arg(long, short)]
        yes: bool,
    },
    /// List team members
    Members {
        /// Team name (or owner/team-name to disambiguate)
        name: String,
    },
    /// Invite a member to the team
    Invite {
        /// Team name (or owner/team-name to disambiguate)
        name: String,
        /// Email address of the person to invite
        email: String,
        /// Role to assign (admin, member)
        #[arg(long)]
        role: String,
    },
    /// List my pending invitations
    Invitations,
    /// Accept a pending invitation
    Accept {
        /// Invitation ID (UUID)
        invitation_id: String,
    },
    /// Decline a pending invitation
    Decline {
        /// Invitation ID (UUID)
        invitation_id: String,
    },
    /// Change a member's role
    Role {
        /// Team name (or owner/team-name to disambiguate)
        name: String,
        /// User ID of the member
        user_id: String,
        /// New role (admin, member)
        #[arg(long)]
        role: String,
    },
    /// Remove a member from the team
    Remove {
        /// Team name (or owner/team-name to disambiguate)
        name: String,
        /// User ID of the member to remove
        user_id: String,
        /// Skip confirmation prompt
        #[arg(long, short)]
        yes: bool,
    },
    /// Grant team access to a repository
    AddRepo {
        /// Team name (or owner/team-name to disambiguate)
        name: String,
        /// Repository (owner/name)
        repo: String,
        /// Access role (admin, write, read)
        #[arg(long)]
        role: String,
    },
    /// Revoke team access to a repository
    RemoveRepo {
        /// Team name (or owner/team-name to disambiguate)
        name: String,
        /// Repository (owner/name)
        repo: String,
        /// Skip confirmation prompt
        #[arg(long, short)]
        yes: bool,
    },
    /// List repositories the team has access to
    Repos {
        /// Team name (or owner/team-name to disambiguate)
        name: String,
    },
}

fn parse_team_role(role: &str) -> Result<TeamRole, CliError> {
    match role {
        "admin" => Ok(TeamRole::Admin),
        "member" => Ok(TeamRole::Member),
        _ => Err(CliError::Config {
            message: format!(
                "invalid role '{}' \u{2014} must be one of: admin, member",
                role
            ),
        }),
    }
}

fn parse_repo_access_role(role: &str) -> Result<CollaboratorRole, CliError> {
    match role {
        "admin" => Ok(CollaboratorRole::Admin),
        "write" => Ok(CollaboratorRole::Write),
        "read" => Ok(CollaboratorRole::Read),
        _ => Err(CliError::Config {
            message: format!(
                "invalid role '{}' \u{2014} must be one of: admin, write, read",
                role
            ),
        }),
    }
}

fn parse_repo_string(repo: &str) -> Result<(&str, &str), CliError> {
    match repo.split_once('/') {
        Some((owner, name)) if !owner.is_empty() && !name.is_empty() && !name.contains('/') => {
            Ok((owner, name))
        }
        _ => Err(CliError::Config {
            message: format!(
                "invalid repository '{}' \u{2014} must be in owner/name format",
                repo
            ),
        }),
    }
}

async fn resolve_team_id(
    client: &SynsClient,
    token: &str,
    name: &str,
    output: &Output,
) -> Result<String, CliError> {
    let response = client.list_teams(token).await?;

    let matches: Vec<_> = if let Some((owner_part, team_part)) = name.split_once('/') {
        response
            .data
            .into_iter()
            .filter(|t| t.owner.username == owner_part && t.name == team_part)
            .collect()
    } else {
        response
            .data
            .into_iter()
            .filter(|t| t.name == name)
            .collect()
    };

    match matches.len() {
        1 => Ok(matches.into_iter().next().unwrap().id),
        0 => Err(CliError::Config {
            message: format!("no team named '{}' found", name),
        }),
        _ if name.contains('/') => Err(CliError::Config {
            message: format!("no team named '{}' found", name),
        }),
        _ => {
            // CLI_IA.md line 822: in non-TTY or --json mode, do NOT prompt;
            // emit the list of matches and exit 1 with a single uniform error.
            if !std::io::stdin().is_terminal() || output.is_json() {
                let listed = matches
                    .iter()
                    .map(|t| format!("{}/{}", t.owner.username, name))
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(CliError::Config {
                    message: format!(
                        "ambiguous team name '{}'; matched: {}; pass 'owner/team-name' instead to disambiguate",
                        name, listed
                    ),
                });
            }
            eprintln!("Multiple teams named '{}':", name);
            for (i, team) in matches.iter().enumerate() {
                eprintln!("  {}. {} (owner: {})", i + 1, name, team.owner.username);
            }
            eprint!("Select a team [1-{}]: ", matches.len());
            std::io::stderr().flush().map_err(|e| CliError::Io {
                message: format!("could not read selection input: {e}"),
            })?;
            let mut input = String::new();
            std::io::stdin()
                .read_line(&mut input)
                .map_err(|e| CliError::Io {
                    message: format!("could not read selection input: {e}"),
                })?;
            let selection: usize = input.trim().parse().map_err(|_| CliError::Config {
                message: "invalid selection".to_string(),
            })?;
            if selection < 1 || selection > matches.len() {
                return Err(CliError::Config {
                    message: "invalid selection".to_string(),
                });
            }
            Ok(matches.into_iter().nth(selection - 1).unwrap().id)
        }
    }
}

fn display_team(output: &Output, team: &crate::client::TeamResponse) {
    if output.is_json() {
        output.json(&json!({
            "id": team.id,
            "name": team.name,
            "description": team.description,
            "owner": {
                "id": team.owner.id,
                "username": team.owner.username,
                "name": team.owner.name,
                "image": team.owner.image,
            },
            "memberCount": team.member_count,
            "role": format!("{:?}", team.role).to_lowercase(),
            "createdAt": team.created_at,
            "updatedAt": team.updated_at,
        }));
    } else {
        let rows = vec![
            vec!["Name".to_string(), team.name.clone()],
            vec![
                "Description".to_string(),
                team.description.as_deref().unwrap_or("(none)").to_string(),
            ],
            vec!["Owner".to_string(), team.owner.username.clone()],
            vec!["Members".to_string(), team.member_count.to_string()],
            vec![
                "Role".to_string(),
                format!("{:?}", team.role).to_lowercase(),
            ],
            vec!["Created".to_string(), team.created_at.clone()],
            vec!["Updated".to_string(), team.updated_at.clone()],
        ];
        output.table(&["Property", "Value"], rows);
    }
}

fn display_member(output: &Output, member: &crate::client::TeamMemberResponse) {
    if output.is_json() {
        output.json(&json!({
            "user": {
                "id": member.user.id,
                "username": member.user.username,
                "name": member.user.name,
                "email": member.user.email,
                "image": member.user.image,
            },
            "role": format!("{:?}", member.role).to_lowercase(),
            "joinedAt": member.joined_at,
        }));
    } else {
        let rows = vec![
            vec!["Username".to_string(), member.user.username.clone()],
            vec!["Name".to_string(), member.user.name.clone()],
            vec![
                "Email".to_string(),
                member.user.email.as_deref().unwrap_or("(none)").to_string(),
            ],
            vec![
                "Role".to_string(),
                format!("{:?}", member.role).to_lowercase(),
            ],
            vec!["Joined".to_string(), member.joined_at.clone()],
        ];
        output.table(&["Property", "Value"], rows);
    }
}

fn display_invitation(output: &Output, invitation: &crate::client::InvitationResponse) {
    if output.is_json() {
        output.json(&json!({
            "id": invitation.id,
            "team": {
                "id": invitation.team.id,
                "name": invitation.team.name,
                "description": invitation.team.description,
            },
            "email": invitation.email,
            "role": format!("{:?}", invitation.role).to_lowercase(),
            "invitedBy": invitation.invited_by.as_ref().map(|u| json!({
                "id": u.id,
                "username": u.username,
                "name": u.name,
                "image": u.image,
            })),
            "status": format!("{:?}", invitation.status).to_lowercase(),
            "expiresAt": invitation.expires_at,
            "createdAt": invitation.created_at,
        }));
    } else {
        let rows = vec![
            vec!["ID".to_string(), invitation.id.clone()],
            vec!["Team".to_string(), invitation.team.name.clone()],
            vec!["Email".to_string(), invitation.email.clone()],
            vec![
                "Role".to_string(),
                format!("{:?}", invitation.role).to_lowercase(),
            ],
            vec![
                "Invited By".to_string(),
                invitation
                    .invited_by
                    .as_ref()
                    .map(|u| u.username.clone())
                    .unwrap_or("(deleted)".to_string()),
            ],
            vec![
                "Status".to_string(),
                format!("{:?}", invitation.status).to_lowercase(),
            ],
            vec!["Expires".to_string(), invitation.expires_at.clone()],
        ];
        output.table(&["Property", "Value"], rows);
    }
}

pub async fn cmd_teams(
    config: &Config,
    output: &Output,
    action: Option<TeamsAction>,
) -> Result<(), CliError> {
    let client = SynsClient::new(config.server_url())?;
    let token = TokenStore::new(config.credentials_path())
        .read()?
        .ok_or(CliError::AuthRequired)?;

    match action {
        None => {
            let response = client.list_teams(&token).await?;
            if output.is_json() {
                output.json(&json!({
                    "teams": response.data.iter().map(|t| json!({
                        "id": t.id,
                        "name": t.name,
                        "description": t.description,
                        "owner": {
                            "id": t.owner.id,
                            "username": t.owner.username,
                            "name": t.owner.name,
                            "image": t.owner.image,
                        },
                        "memberCount": t.member_count,
                        "role": format!("{:?}", t.role).to_lowercase(),
                        "createdAt": t.created_at,
                        "updatedAt": t.updated_at,
                    })).collect::<Vec<_>>(),
                }));
            } else {
                let rows = response
                    .data
                    .iter()
                    .map(|t| {
                        vec![
                            t.name.clone(),
                            t.owner.username.clone(),
                            t.member_count.to_string(),
                            format!("{:?}", t.role).to_lowercase(),
                            t.updated_at.clone(),
                        ]
                    })
                    .collect();
                output.table(&["Name", "Owner", "Members", "Role", "Updated"], rows);
            }
        }
        Some(TeamsAction::Create { name, description }) => {
            let request = CreateTeamRequest { name, description };
            let response = client.create_team(&token, &request).await?;
            display_team(output, &response);
        }
        Some(TeamsAction::Show { name }) => {
            let team_id = resolve_team_id(&client, &token, &name, output).await?;
            let response = client.get_team(&token, &team_id).await?;
            display_team(output, &response);
        }
        Some(TeamsAction::Update {
            name,
            rename,
            description,
            clear_description,
        }) => {
            if rename.is_none() && description.is_none() && !clear_description {
                return Err(CliError::Config {
                    message: "no update flags provided \u{2014} use --rename, --description, or --clear-description".to_string(),
                });
            }
            if description.is_some() && clear_description {
                return Err(CliError::Config {
                    message: "cannot use --description and --clear-description together"
                        .to_string(),
                });
            }
            let team_id = resolve_team_id(&client, &token, &name, output).await?;
            let desc = if clear_description {
                Some(None)
            } else {
                description.map(Some)
            };
            let request = UpdateTeamRequest {
                name: rename,
                description: desc,
            };
            let response = client.update_team(&token, &team_id, &request).await?;
            display_team(output, &response);
        }
        Some(TeamsAction::Delete { name, yes }) => {
            let team_id = resolve_team_id(&client, &token, &name, output).await?;
            eprintln!("{}", style(format!(
                "WARNING: This will permanently delete team '{}' and all its memberships, invitations, and repository access entries.", name
            )).red().bold());
            eprintln!("This action cannot be undone.");
            let prompt = format!("Type the team name to confirm ('{}'): ", name);
            match confirm_or_yes(yes, &prompt)? {
                ConfirmOutcome::SkipPrompt => { /* proceed */ }
                ConfirmOutcome::Input(input) => {
                    if input.trim() != name {
                        eprintln!("Aborted \u{2014} input did not match team name.");
                        return Ok(());
                    }
                }
            }
            client.delete_team(&token, &team_id).await?;
            if output.is_json() {
                output.json(&json!({"deleted": true, "team": name}));
            } else {
                output.success(&format!("Team '{}' deleted.", name));
            }
        }
        Some(TeamsAction::Members { name }) => {
            let team_id = resolve_team_id(&client, &token, &name, output).await?;
            let response = client.list_members(&token, &team_id).await?;
            if output.is_json() {
                output.json(&json!({
                    "members": response.data.iter().map(|m| json!({
                        "user": {
                            "id": m.user.id,
                            "username": m.user.username,
                            "name": m.user.name,
                            "email": m.user.email,
                            "image": m.user.image,
                        },
                        "role": format!("{:?}", m.role).to_lowercase(),
                        "joinedAt": m.joined_at,
                    })).collect::<Vec<_>>(),
                }));
            } else {
                let rows = response
                    .data
                    .iter()
                    .map(|m| {
                        vec![
                            m.user.username.clone(),
                            m.user.name.clone(),
                            m.user.email.as_deref().unwrap_or("(none)").to_string(),
                            format!("{:?}", m.role).to_lowercase(),
                            m.joined_at.clone(),
                        ]
                    })
                    .collect();
                output.table(&["Username", "Name", "Email", "Role", "Joined"], rows);
            }
        }
        Some(TeamsAction::Invite { name, email, role }) => {
            let parsed_role = parse_team_role(&role)?;
            let team_id = resolve_team_id(&client, &token, &name, output).await?;
            let request = InviteRequest {
                email,
                role: parsed_role,
            };
            let response = client.invite_member(&token, &team_id, &request).await?;
            display_invitation(output, &response);
        }
        Some(TeamsAction::Invitations) => {
            let response = client.list_my_invitations(&token).await?;
            if output.is_json() {
                output.json(&json!({
                    "invitations": response.data.iter().map(|inv| json!({
                        "id": inv.id,
                        "team": {
                            "id": inv.team.id,
                            "name": inv.team.name,
                            "description": inv.team.description,
                        },
                        "email": inv.email,
                        "role": format!("{:?}", inv.role).to_lowercase(),
                        "invitedBy": inv.invited_by.as_ref().map(|u| json!({
                            "id": u.id,
                            "username": u.username,
                            "name": u.name,
                            "image": u.image,
                        })),
                        "status": format!("{:?}", inv.status).to_lowercase(),
                        "expiresAt": inv.expires_at,
                        "createdAt": inv.created_at,
                    })).collect::<Vec<_>>(),
                }));
            } else {
                let rows = response
                    .data
                    .iter()
                    .map(|inv| {
                        vec![
                            inv.id.clone(),
                            inv.team.name.clone(),
                            format!("{:?}", inv.role).to_lowercase(),
                            inv.invited_by
                                .as_ref()
                                .map(|u| u.username.clone())
                                .unwrap_or("(deleted)".to_string()),
                            inv.expires_at.clone(),
                        ]
                    })
                    .collect();
                output.table(&["ID", "Team", "Role", "Invited By", "Expires"], rows);
            }
        }
        Some(TeamsAction::Accept { invitation_id }) => {
            let response = client.accept_invitation(&token, &invitation_id).await?;
            display_member(output, &response);
        }
        Some(TeamsAction::Decline { invitation_id }) => {
            client.decline_invitation(&token, &invitation_id).await?;
            if output.is_json() {
                output.json(&json!({"declined": true, "invitationId": invitation_id}));
            } else {
                output.success(&format!("Declined invitation '{}'.", invitation_id));
            }
        }
        Some(TeamsAction::Role {
            name,
            user_id,
            role,
        }) => {
            let parsed_role = parse_team_role(&role)?;
            let team_id = resolve_team_id(&client, &token, &name, output).await?;
            let request = ChangeRoleRequest { role: parsed_role };
            let response = client
                .change_role(&token, &team_id, &user_id, &request)
                .await?;
            display_member(output, &response);
        }
        Some(TeamsAction::Remove { name, user_id, yes }) => {
            let team_id = resolve_team_id(&client, &token, &name, output).await?;
            let prompt = format!("Remove member '{}' from team '{}'? [y/N]: ", user_id, name);
            match confirm_or_yes(yes, &prompt)? {
                ConfirmOutcome::SkipPrompt => { /* proceed */ }
                ConfirmOutcome::Input(input) => {
                    let trimmed = input.trim().to_lowercase();
                    if trimmed != "y" && trimmed != "yes" {
                        eprintln!("Aborted.");
                        return Ok(());
                    }
                }
            }
            client.remove_member(&token, &team_id, &user_id).await?;
            if output.is_json() {
                output.json(&json!({"removed": true, "userId": user_id}));
            } else {
                output.success(&format!(
                    "Removed member '{}' from team '{}'.",
                    user_id, name
                ));
            }
        }
        Some(TeamsAction::AddRepo { name, repo, role }) => {
            let parsed_role = parse_repo_access_role(&role)?;
            let team_id = resolve_team_id(&client, &token, &name, output).await?;
            let (repo_owner, repo_name) = parse_repo_string(&repo)?;
            let request = TeamRepoAccessRequest { role: parsed_role };
            let response = client
                .add_team_repo(&token, &team_id, repo_owner, repo_name, &request)
                .await?;
            if output.is_json() {
                output.json(&json!({
                    "owner": response.owner,
                    "name": response.name,
                    "description": response.description,
                    "visibility": format!("{:?}", response.visibility).to_lowercase(),
                    "role": format!("{:?}", response.role).to_lowercase(),
                    "addedBy": response.added_by.as_ref().map(|u| json!({
                        "id": u.id,
                        "username": u.username,
                        "name": u.name,
                        "image": u.image,
                    })),
                    "addedAt": response.added_at,
                }));
            } else {
                let rows = vec![
                    vec![
                        "Repository".to_string(),
                        format!("{}/{}", response.owner, response.name),
                    ],
                    vec![
                        "Visibility".to_string(),
                        format!("{:?}", response.visibility).to_lowercase(),
                    ],
                    vec![
                        "Role".to_string(),
                        format!("{:?}", response.role).to_lowercase(),
                    ],
                    vec![
                        "Added By".to_string(),
                        response
                            .added_by
                            .as_ref()
                            .map(|u| u.username.clone())
                            .unwrap_or("(deleted)".to_string()),
                    ],
                    vec!["Added".to_string(), response.added_at.clone()],
                ];
                output.table(&["Property", "Value"], rows);
            }
        }
        Some(TeamsAction::RemoveRepo { name, repo, yes }) => {
            let (repo_owner, repo_name) = parse_repo_string(&repo)?;
            let team_id = resolve_team_id(&client, &token, &name, output).await?;
            let prompt = format!(
                "Remove repository access for '{}' from team '{}'? [y/N]: ",
                repo, name
            );
            match confirm_or_yes(yes, &prompt)? {
                ConfirmOutcome::SkipPrompt => { /* proceed */ }
                ConfirmOutcome::Input(input) => {
                    let trimmed = input.trim().to_lowercase();
                    if trimmed != "y" && trimmed != "yes" {
                        eprintln!("Aborted.");
                        return Ok(());
                    }
                }
            }
            client
                .remove_team_repo(&token, &team_id, repo_owner, repo_name)
                .await?;
            if output.is_json() {
                output.json(&json!({"removed": true, "team": name, "repo": repo}));
            } else {
                output.success(&format!(
                    "Removed repository access for '{}' from team '{}'.",
                    repo, name
                ));
            }
        }
        Some(TeamsAction::Repos { name }) => {
            let team_id = resolve_team_id(&client, &token, &name, output).await?;
            let response = client.list_team_repos(&token, &team_id).await?;
            if output.is_json() {
                output.json(&json!({
                    "repos": response.data.iter().map(|r| json!({
                        "owner": r.owner,
                        "name": r.name,
                        "description": r.description,
                        "visibility": format!("{:?}", r.visibility).to_lowercase(),
                        "role": format!("{:?}", r.role).to_lowercase(),
                        "addedBy": r.added_by.as_ref().map(|u| json!({
                            "id": u.id,
                            "username": u.username,
                            "name": u.name,
                            "image": u.image,
                        })),
                        "addedAt": r.added_at,
                    })).collect::<Vec<_>>(),
                }));
            } else {
                let rows = response
                    .data
                    .iter()
                    .map(|r| {
                        vec![
                            format!("{}/{}", r.owner, r.name),
                            format!("{:?}", r.visibility).to_lowercase(),
                            format!("{:?}", r.role).to_lowercase(),
                            r.added_by
                                .as_ref()
                                .map(|u| u.username.clone())
                                .unwrap_or("(deleted)".to_string()),
                            r.added_at.clone(),
                        ]
                    })
                    .collect();
                output.table(
                    &["Repository", "Visibility", "Role", "Added By", "Added"],
                    rows,
                );
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn resolve_team_id_finds_team_by_name() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/teams"))
            .and(header("Authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    {
                        "id": "uuid-alpha",
                        "name": "alpha",
                        "description": null,
                        "owner": { "id": "u1", "username": "alice", "name": "Alice", "image": null },
                        "memberCount": 3,
                        "role": "owner",
                        "createdAt": "2026-01-01T00:00:00Z",
                        "updatedAt": "2026-02-01T00:00:00Z"
                    },
                    {
                        "id": "uuid-beta",
                        "name": "beta",
                        "description": "Beta team",
                        "owner": { "id": "u2", "username": "bob", "name": "Bob", "image": null },
                        "memberCount": 1,
                        "role": "member",
                        "createdAt": "2026-01-15T00:00:00Z",
                        "updatedAt": "2026-01-15T00:00:00Z"
                    }
                ]
            })))
            .mount(&mock_server)
            .await;

        let client = SynsClient::new(&mock_server.uri()).unwrap();
        let output = Output::new(false);

        let result = resolve_team_id(&client, "test-token", "alpha", &output).await;
        assert_eq!(result.unwrap(), "uuid-alpha".to_string());
    }

    #[tokio::test]
    async fn disambiguation_returns_ambiguous_error_in_json_mode_with_multiple_matches() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/teams"))
            .and(header("Authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    {
                        "id": "uuid-alice-core",
                        "name": "core",
                        "description": null,
                        "owner": { "id": "u1", "username": "alice", "name": "Alice", "image": null },
                        "memberCount": 3,
                        "role": "owner",
                        "createdAt": "2026-01-01T00:00:00Z",
                        "updatedAt": "2026-02-01T00:00:00Z"
                    },
                    {
                        "id": "uuid-bob-core",
                        "name": "core",
                        "description": null,
                        "owner": { "id": "u2", "username": "bob", "name": "Bob", "image": null },
                        "memberCount": 1,
                        "role": "member",
                        "createdAt": "2026-01-15T00:00:00Z",
                        "updatedAt": "2026-01-15T00:00:00Z"
                    }
                ]
            })))
            .mount(&mock_server)
            .await;

        let client = SynsClient::new(&mock_server.uri()).unwrap();
        // JSON mode triggers the combined non-interactive predicate even if
        // stdin happens to be a TTY at test time.
        let output = Output::new(true);

        let result = resolve_team_id(&client, "test-token", "core", &output).await;
        let err = result.unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("ambiguous team name 'core'"),
            "expected ambiguous-team error, got: {message}"
        );
        assert!(
            message.contains("alice/core"),
            "expected alice/core in matches, got: {message}"
        );
        assert!(
            message.contains("bob/core"),
            "expected bob/core in matches, got: {message}"
        );
        assert!(
            message.contains("pass 'owner/team-name' instead to disambiguate"),
            "expected disambiguation hint, got: {message}"
        );
    }

    #[tokio::test]
    async fn disambiguation_with_single_match_returns_match_unchanged_in_json_mode() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/teams"))
            .and(header("Authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    {
                        "id": "uuid-only",
                        "name": "core",
                        "description": null,
                        "owner": { "id": "u1", "username": "alice", "name": "Alice", "image": null },
                        "memberCount": 3,
                        "role": "owner",
                        "createdAt": "2026-01-01T00:00:00Z",
                        "updatedAt": "2026-02-01T00:00:00Z"
                    }
                ]
            })))
            .mount(&mock_server)
            .await;

        let client = SynsClient::new(&mock_server.uri()).unwrap();
        let output = Output::new(true);

        let result = resolve_team_id(&client, "test-token", "core", &output).await;
        assert_eq!(result.unwrap(), "uuid-only".to_string());
    }

    #[tokio::test]
    async fn resolve_team_id_errors_on_no_match() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/teams"))
            .and(header("Authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    {
                        "id": "uuid-alpha",
                        "name": "alpha",
                        "description": null,
                        "owner": { "id": "u1", "username": "alice", "name": "Alice", "image": null },
                        "memberCount": 3,
                        "role": "owner",
                        "createdAt": "2026-01-01T00:00:00Z",
                        "updatedAt": "2026-02-01T00:00:00Z"
                    }
                ]
            })))
            .mount(&mock_server)
            .await;

        let client = SynsClient::new(&mock_server.uri()).unwrap();
        let output = Output::new(false);

        let result = resolve_team_id(&client, "test-token", "gamma", &output).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("no team named 'gamma' found"));
    }

    #[tokio::test]
    #[serial]
    async fn create_team_sends_correct_request() {
        let dir = tempfile::tempdir().unwrap();
        TokenStore::new(dir.path().join("credentials.json"))
            .write("test-token")
            .unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/v1/teams"))
            .and(header("Authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": "uuid-new",
                "name": "my-team",
                "description": null,
                "owner": { "id": "u1", "username": "alice", "name": "Alice", "image": null },
                "memberCount": 1,
                "role": "owner",
                "createdAt": "2026-03-01T00:00:00Z",
                "updatedAt": "2026-03-01T00:00:00Z"
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_teams(
            &config,
            &output,
            Some(TeamsAction::Create {
                name: "my-team".to_string(),
                description: None,
            }),
        )
        .await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }
}

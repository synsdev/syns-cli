//! `syns share PATH`, `syns share PATH --show` and `syns unshare PATH`
//! (SPEC u300): a folder of a holder shared under an identity of its own
//! named by its sharer, the identity a folder stands under read, and the
//! sharing stopped (`D-110`, `D-115`, `D-116`, `D-118`, `D-119`).

use std::io::{BufRead, IsTerminal, Write};

use serde_json::{Value, json};

use crate::auth::token::TokenStore;
use crate::client::{CollaboratorRole, RepoResponse, SynsClient};
use crate::commands::place::{counted_from, placement_path};
use crate::config::Config;
use crate::errors::{ApiErrorContext, CliError};
use crate::output::Output;
use crate::prompts::{ConfirmOutcome, confirm_or_yes};
use crate::repo::folder::{current_dir, resolve_folder_scope};

/// The holder a share, its lookup and its removal address, and the
/// folder's path from the holder's root (SPEC u300, `ShareTarget`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShareTarget {
    /// The holder's `OWNER/NAME`, lower-cased.
    pub holder: String,
    /// The folder's path from the holder's root.
    pub path: String,
}

/// What a share ended at (SPEC u300, `ShareOutcome`): the identity's
/// record, typed and as served, and whether this run's share made it
/// stand rather than the lookup finding it.
#[derive(Debug)]
pub(crate) struct ShareOutcome {
    pub repo: RepoResponse,
    pub raw: Value,
    pub created: bool,
}

/// The name prompt `share_with_names` asks again through: handed the
/// held `OWNER/NAME`, it answers another name or none.
pub(crate) type AskName<'a> = &'a mut dyn FnMut(&str) -> Option<String>;

/// The longest name the `repository name` field kind admits.
const NAME_MAX: usize = 100;

/// The device names no repository name may be (SPEC u300,
/// `share_name_problem`).
fn is_reserved(name: &str) -> bool {
    if matches!(name, "con" | "prn" | "aux" | "nul") {
        return true;
    }
    let bytes = name.as_bytes();
    bytes.len() == 4
        && (name.starts_with("com") || name.starts_with("lpt"))
        && bytes[3].is_ascii_digit()
}

/// The name kind line where `name` falls outside the `repository name`
/// field kind, and none otherwise (SPEC u300, `share_name_problem`).
pub fn share_name_problem(name: &str) -> Option<String> {
    let admitted =
        |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-');
    let opens = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let ends = name.chars().last().is_some_and(|c| !matches!(c, '.' | '-'));
    let fits = opens
        && ends
        && name.len() <= NAME_MAX
        && name.chars().all(admitted)
        && !name.contains("..")
        && !name.contains("--")
        && !is_reserved(name);
    (!fits).then(|| {
        format!(
            "a name holds lower-case letters, digits, ., _ and -, opens with a letter or digit, ends with neither . nor -, carries no .. or -- run and runs 1 to 100 characters, and is none of con, prn, aux, nul, com0 to com9 or lpt0 to lpt9 (got {name})"
        )
    })
}

/// The name a first share offers (SPEC u300, `offered_share_name`): the
/// holder's name, `-`, and the last segment of `path`, lower-cased; none
/// where that fails `share_name_problem`.
pub fn offered_share_name(holder: &str, path: &str) -> Option<String> {
    let holder_name = holder.rsplit('/').next().unwrap_or(holder);
    let last = path.rsplit('/').next().unwrap_or(path);
    let offer = format!("{holder_name}-{last}").to_lowercase();
    share_name_problem(&offer).is_none().then_some(offer)
}

/// Whether a typed path names the folder the run stands in (SPEC u300
/// Q-02, `bind_share_target` 2).
fn names_this_folder(typed: &str) -> bool {
    typed == "." || typed == "./"
}

/// The holder and the folder's holder path a share, its lookup and its
/// removal all address (SPEC u300 Behaviour, `bind_share_target`).
pub(crate) fn bind_share_target(typed: &str, repo: Option<&str>) -> Result<ShareTarget, CliError> {
    // 1 — `--repo` names the holder, the typed path its holder path.
    if let Some(repo) = repo {
        return Ok(ShareTarget {
            holder: repo.to_ascii_lowercase(),
            path: placement_path(typed)?,
        });
    }

    // 2 — a typed `.` inside a scoped folder names that folder (Q-02);
    // otherwise the path is counted as `cmd_place` 3 and 4 count theirs.
    let cwd = current_dir()?;
    if names_this_folder(typed)
        && let Some(scope) = resolve_folder_scope(&cwd)?
    {
        return Ok(ShareTarget {
            holder: scope.holder(),
            path: scope.path.clone(),
        });
    }
    let path = placement_path(typed)?;
    let counted = counted_from(&cwd)?;
    Ok(ShareTarget {
        holder: counted.holder.clone(),
        path: counted.repository_path(&path),
    })
}

/// The held line: the name the share asked for is held already.
fn held_line(held: &str) -> String {
    format!("{held} is already held; give another name")
}

/// Whether `err` is the refusal a lookup answers where no identity of the
/// folder stands.
fn is_not_shared(err: &CliError) -> bool {
    matches!(err, CliError::Api { status: Some(404), error, .. } if error == "not_found")
}

/// Whether `err` is a `conflict` naming no head — a name already held —
/// rather than the head moving under the share.
fn is_held_name(err: &CliError) -> bool {
    matches!(
        err,
        CliError::Api { status: Some(409), error, context }
            if error == "conflict" && !matches!(context, Some(ApiErrorContext::HeadMoved { .. }))
    )
}

/// Shares the target under `name`, reading the lookup again on every
/// held name and asking `ask` for another while the folder stands
/// unshared (SPEC u300 Behaviour, `share_with_names`).
pub(crate) async fn share_with_names(
    client: &SynsClient,
    token: &str,
    target: &ShareTarget,
    name: String,
    mut ask: Option<AskName<'_>>,
) -> Result<ShareOutcome, CliError> {
    let owner = target.holder.split('/').next().unwrap_or(&target.holder);
    let mut name = name;
    loop {
        // 1 — the share.
        let held = match client
            .share_folder(&target.holder, token, &target.path, &name)
            .await
        {
            Ok((repo, raw)) => {
                return Ok(ShareOutcome {
                    repo,
                    raw,
                    created: true,
                });
            }
            Err(err) if is_held_name(&err) => err,
            Err(err) => return Err(err),
        };

        // 2 — the lookup again: an identity standing ends shared.
        match client
            .get_share(&target.holder, Some(token), &target.path)
            .await
        {
            Ok((repo, raw)) => {
                return Ok(ShareOutcome {
                    repo,
                    raw,
                    created: false,
                });
            }
            Err(err) if is_not_shared(&err) => {}
            Err(err) => return Err(err),
        }

        // 3 — another name, where one can be asked for.
        let Some(ask) = ask.as_mut() else {
            return Err(held);
        };
        match ask(&format!("{owner}/{name}")) {
            Some(another) => name = another,
            None => return Err(held),
        }
    }
}

// ---- the prompts ------------------------------------------------------

/// Whether the name prompt can be raised for a caller holding `role` on
/// the holder: the prompt gate holding, and `may_prompt_for` the role.
fn prompt_can_be_raised(output: &Output, role: Option<&CollaboratorRole>) -> bool {
    terminal_prompt(output) && may_prompt_for(role)
}

/// Whether a caller holding `role` on the holder may be asked for a name:
/// `owner` or `admin` alone (SPEC u300 Behaviour, `cmd_share` 6).
fn may_prompt_for(role: Option<&CollaboratorRole>) -> bool {
    matches!(
        role,
        Some(CollaboratorRole::Owner | CollaboratorRole::Admin)
    )
}

/// The prompt gate before the role is known: standard input a terminal,
/// `--json` off, and `CI` absent whatever its value.
fn terminal_prompt(output: &Output) -> bool {
    std::io::stdin().is_terminal() && !output.is_json() && std::env::var_os("CI").is_none()
}

/// One line read from `input` after writing `prompt` on `diag`, none at
/// end of input or where the line cannot be read.
fn read_answer(input: &mut dyn BufRead, diag: &mut dyn Write, prompt: &str) -> Option<String> {
    write!(diag, "{prompt}").ok()?;
    diag.flush().ok()?;
    let mut line = String::new();
    match input.read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim().to_string()),
    }
}

/// The name prompt for the folder at `path` of `holder`, read from
/// `input` and written on `diag`: an empty answer takes `offer`, or asks
/// again where none stands; an answer `share_name_problem` refuses writes
/// its line and asks again; none at end of input.
fn ask_name(
    input: &mut dyn BufRead,
    diag: &mut dyn Write,
    path: &str,
    holder: &str,
    offer: Option<&str>,
) -> Option<String> {
    let prompt = match offer {
        Some(offer) => format!("name for {path} of {holder} [{offer}]: "),
        None => format!("name for {path} of {holder}: "),
    };
    loop {
        let answer = read_answer(input, diag, &prompt)?;
        if answer.is_empty() {
            match offer {
                Some(offer) => return Some(offer.to_string()),
                None => continue,
            }
        }
        match share_name_problem(&answer) {
            Some(line) => writeln!(diag, "{line}").ok()?,
            None => return Some(answer),
        }
    }
}

/// `ask_name` over the run's standard input and diagnostic stream.
fn ask_name_here(path: &str, holder: &str, offer: Option<&str>) -> Option<String> {
    ask_name(
        &mut std::io::stdin().lock(),
        &mut std::io::stderr(),
        path,
        holder,
        offer,
    )
}

// ---- the lines --------------------------------------------------------

/// The identity-holder line (SPEC u300 Q-01).
fn identity_holder_refusal(holder: &str) -> CliError {
    CliError::Config {
        message: format!(
            "{holder} is a shared folder's identity; --repo takes the repository the folder stands in"
        ),
    }
}

/// The refusal where no name can be taken (`inferred`).
fn no_name_refusal() -> CliError {
    CliError::Config {
        message: "no name to share under; pass --name NAME".to_string(),
    }
}

/// The served body as a JSON object, `extra` laid over it.
fn document(raw: &Value, extra: Value) -> Value {
    let mut document = match raw {
        Value::Object(map) => map.clone(),
        _ => serde_json::Map::new(),
    };
    if let Value::Object(extra) = extra {
        document.extend(extra);
    }
    Value::Object(document)
}

/// The holder's record, its `role` the caller's, refused where it is
/// itself a shared folder's identity (SPEC u300 Behaviour, `cmd_share` 4,
/// `cmd_share_show` 3).
async fn read_holder(
    client: &SynsClient,
    token: Option<&str>,
    holder: &str,
) -> Result<RepoResponse, CliError> {
    let record = client.get_repo(holder, token).await?;
    if record.shared_folder {
        return Err(identity_holder_refusal(holder));
    }
    Ok(record)
}

// ---- the commands -----------------------------------------------------

/// `syns share PATH [--name NAME] [--repo OWNER/NAME]` (SPEC u300
/// Behaviour, `cmd_share`).
pub async fn cmd_share(
    config: &Config,
    output: &Output,
    path: String,
    name: Option<String>,
    repo: Option<String>,
) -> Result<(), CliError> {
    // 1 — a typed name outside the kind, where no prompt can follow.
    if let Some(typed) = name.as_deref()
        && let Some(line) = share_name_problem(typed)
        && !terminal_prompt(output)
    {
        return Err(CliError::Config { message: line });
    }

    // 2 — the target.
    let target = bind_share_target(&path, repo.as_deref())?;

    // 3 — the credential.
    let token = TokenStore::new(config.credentials_path())
        .read()?
        .ok_or(CliError::AuthRequired)?;
    let client = SynsClient::new(config.server_url())?;

    // 4 — the holder, and the caller's role on it.
    let holder = read_holder(&client, Some(&token), &target.holder).await?;

    // 5 — an identity standing ends the run shared.
    let outcome = match client
        .get_share(&target.holder, Some(&token), &target.path)
        .await
    {
        Ok((repo, raw)) => ShareOutcome {
            repo,
            raw,
            created: false,
        },
        Err(err) if is_not_shared(&err) => {
            // 6 — the name.
            let can_prompt = prompt_can_be_raised(output, holder.role.as_ref());
            let name = match name {
                Some(typed) => match share_name_problem(&typed) {
                    None => typed,
                    Some(line) if can_prompt => {
                        eprintln!("{line}");
                        ask_name_here(&target.path, &target.holder, None)
                            .ok_or_else(no_name_refusal)?
                    }
                    Some(line) => return Err(CliError::Config { message: line }),
                },
                None => {
                    let offer = offered_share_name(&target.holder, &target.path);
                    if can_prompt {
                        ask_name_here(&target.path, &target.holder, offer.as_deref())
                            .ok_or_else(no_name_refusal)?
                    } else {
                        offer.ok_or_else(no_name_refusal)?
                    }
                }
            };

            // 7 — the share, asking again on a held name.
            let (path, holder_id) = (target.path.clone(), target.holder.clone());
            let mut again = move |held: &str| {
                eprintln!("{}", held_line(held));
                ask_name_here(&path, &holder_id, None)
            };
            let ask: Option<AskName<'_>> = if can_prompt { Some(&mut again) } else { None };
            share_with_names(&client, &token, &target, name, ask).await?
        }
        Err(err) => return Err(err),
    };

    // 8 — the document, or the report and the identity.
    let identity = format!("{}/{}", outcome.repo.owner, outcome.repo.name);
    if output.is_json() {
        output.json(&document(
            &outcome.raw,
            json!({
                "holder": target.holder,
                "path": target.path,
                "created": outcome.created,
            }),
        ));
        return Ok(());
    }
    if outcome.created {
        let scoped = resolve_folder_scope(&current_dir()?)
            .ok()
            .flatten()
            .is_some();
        if scoped {
            eprintln!("shared {} of {} as {identity}", target.path, target.holder);
        } else {
            eprintln!(
                "shared {} of {} as {identity}; add people with: syns collaborators add USER --role read --repo {identity}",
                target.path, target.holder
            );
        }
    } else {
        eprintln!(
            "{} of {} is already shared as {identity}",
            target.path, target.holder
        );
    }
    println!("{identity}");
    Ok(())
}

/// `syns share PATH --show [--repo OWNER/NAME]` (SPEC u300 Behaviour,
/// `cmd_share_show`): the lookup alone, nothing sent that changes
/// anything.
pub async fn cmd_share_show(
    config: &Config,
    output: &Output,
    path: String,
    repo: Option<String>,
) -> Result<(), CliError> {
    // 1 — the target.
    let target = bind_share_target(&path, repo.as_deref())?;

    // 2 — the stored credential, none where none stands or parses.
    let token = TokenStore::new(config.credentials_path())
        .read()
        .ok()
        .flatten();
    let client = SynsClient::new(config.server_url())?;

    // 3 — the holder, and the caller's role on it.
    let holder = read_holder(&client, token.as_deref(), &target.holder).await?;
    let holder_role = serde_json::to_value(&holder.role).unwrap_or(Value::Null);

    // 4 — the lookup.
    let standing = match client
        .get_share(&target.holder, token.as_deref(), &target.path)
        .await
    {
        Ok(found) => Some(found),
        Err(err) if is_not_shared(&err) => None,
        Err(err) => return Err(err),
    };

    // 5 — the document, or the report.
    match standing {
        Some((repo, raw)) => {
            let identity = format!("{}/{}", repo.owner, repo.name);
            if output.is_json() {
                output.json(&document(
                    &raw,
                    json!({
                        "holder": target.holder,
                        "path": target.path,
                        "holderRole": holder_role,
                        "shared": true,
                    }),
                ));
            } else {
                eprintln!(
                    "{} of {} is shared as {identity}",
                    target.path, target.holder
                );
                println!("{identity}");
            }
        }
        None => {
            let offer = offered_share_name(&target.holder, &target.path);
            if output.is_json() {
                output.json(&json!({
                    "holder": target.holder,
                    "path": target.path,
                    "holderRole": holder_role,
                    "shared": false,
                    "offeredName": offer,
                }));
            } else {
                match offer {
                    Some(offer) => eprintln!(
                        "{} of {} is not shared; syns share offers the name {offer}",
                        target.path, target.holder
                    ),
                    None => eprintln!("{} of {} is not shared", target.path, target.holder),
                }
            }
        }
    }
    Ok(())
}

/// `syns unshare PATH [--repo OWNER/NAME] [--yes]` (SPEC u300 Behaviour,
/// `cmd_unshare`).
pub async fn cmd_unshare(
    config: &Config,
    output: &Output,
    path: String,
    repo: Option<String>,
    yes: bool,
) -> Result<(), CliError> {
    // 1 — the target.
    let target = bind_share_target(&path, repo.as_deref())?;

    // 2 — the credential.
    let token = TokenStore::new(config.credentials_path())
        .read()?
        .ok_or(CliError::AuthRequired)?;
    let client = SynsClient::new(config.server_url())?;

    // 3 — the identity the folder stands shared under.
    let (identity, _) = client
        .get_share(&target.holder, Some(&token), &target.path)
        .await?;
    let unshare_document = |unshared: bool| {
        json!({
            "unshared": unshared,
            "holder": target.holder,
            "path": target.path,
            "owner": identity.owner,
            "name": identity.name,
        })
    };

    // 4 — the confirmation.
    let confirmed = match confirm_or_yes(
        yes,
        &format!(
            "Stop sharing {} of {} as {}/{}, removing its collaborators? [y/N]: ",
            target.path, target.holder, identity.owner, identity.name
        ),
    )? {
        ConfirmOutcome::SkipPrompt => true,
        ConfirmOutcome::Input(input) => {
            let answer = input.trim().to_lowercase();
            answer == "y" || answer == "yes"
        }
    };
    if !confirmed {
        eprintln!("Aborted.");
        if output.is_json() {
            output.json(&unshare_document(false));
        }
        return Ok(());
    }

    // 5 — the removal.
    client
        .unshare_folder(&target.holder, &token, &target.path)
        .await?;

    // 6 — the document, or the report.
    if output.is_json() {
        output.json(&unshare_document(true));
    } else {
        eprintln!(
            "stopped sharing {} of {}: {}/{} is retired and its collaborators removed",
            target.path, target.holder, identity.owner, identity.name
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // SPEC u300 Tests, the row of this name.
    #[test]
    fn share_name_problem_weighs_the_repository_name_kind() {
        assert_eq!(share_name_problem("docs-q3-plan"), None);
        let long = "a".repeat(101);
        for refused in [
            "Docs",
            "a--b",
            "a..b",
            "-a",
            "a.",
            "a/b",
            "con",
            "com7",
            long.as_str(),
        ] {
            assert_eq!(
                share_name_problem(refused),
                Some(format!(
                    "a name holds lower-case letters, digits, ., _ and -, opens with a letter or digit, ends with neither . nor -, carries no .. or -- run and runs 1 to 100 characters, and is none of con, prn, aux, nul, com0 to com9 or lpt0 to lpt9 (got {refused})"
                )),
                "{refused}"
            );
        }
    }

    // SPEC u300 Tests, the row of this name.
    #[test]
    fn offered_share_name_takes_the_last_segment_lower_cased() {
        assert_eq!(
            offered_share_name("alice/work", "clients/vela/Q3-Board").as_deref(),
            Some("work-q3-board")
        );
        assert_eq!(offered_share_name("alice/docs", "q3 plan"), None);
    }

    // SPEC u302 Behaviour, `bind_share_target` 1: inside a folder bound to
    // its identity, the holder its file names and the typed path joined
    // under its recorded path, a typed `.` naming the folder itself.
    #[test]
    #[serial_test::serial]
    fn bind_share_target_counts_from_the_holder_inside_an_identity_folder() {
        let tree = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(tree.path()).unwrap().join("q3-plan");
        std::fs::create_dir_all(dir.join("appendix")).unwrap();
        std::fs::write(
            dir.join(".syns.yaml"),
            "holder: alice/docs\npath: q3-plan\nshared_as: docs-q3-plan\n",
        )
        .unwrap();
        std::env::set_current_dir(&dir).unwrap();
        let appendix = bind_share_target("appendix", None);
        let this = bind_share_target(".", None);
        std::env::set_current_dir(std::env::temp_dir()).unwrap();
        let appendix = appendix.unwrap();
        assert_eq!(
            (appendix.holder.as_str(), appendix.path.as_str()),
            ("alice/docs", "q3-plan/appendix")
        );
        let this = this.unwrap();
        assert_eq!(
            (this.holder.as_str(), this.path.as_str()),
            ("alice/docs", "q3-plan")
        );
    }

    fn identity(name: &str) -> Value {
        json!({
            "owner": "alice", "name": name, "description": null,
            "commitSha": null, "status": "active", "author": null, "tags": [],
            "visibility": "private", "forkedFrom": null, "forkCount": 0,
            "fileCount": 0, "role": "owner", "sharedFolder": true,
            "createdAt": "2026-10-02T00:00:00Z", "updatedAt": "2026-10-02T00:00:00Z",
        })
    }

    fn q3_plan() -> ShareTarget {
        ShareTarget {
            holder: "alice/docs".to_string(),
            path: "q3-plan".to_string(),
        }
    }

    // SPEC u300 Tests, the row of this name.
    #[tokio::test]
    async fn the_ask_takes_another_name_where_the_name_is_held() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/repos/alice/docs/shares"))
            .and(body_partial_json(json!({"name": "docs-q3-plan"})))
            .respond_with(ResponseTemplate::new(409).set_body_json(json!({"error": "conflict"})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/repos/alice/docs/shares"))
            .and(body_partial_json(json!({"name": "docs-q3-plan-2"})))
            .respond_with(ResponseTemplate::new(201).set_body_json(identity("docs-q3-plan-2")))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/docs/shares/q3-plan"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({"error": "not_found"})))
            .mount(&server)
            .await;
        let client = SynsClient::new(&server.uri()).unwrap();
        let mut handed = Vec::new();
        let mut ask = |held: &str| {
            handed.push(held.to_string());
            Some("docs-q3-plan-2".to_string())
        };

        let outcome = share_with_names(
            &client,
            "t",
            &q3_plan(),
            "docs-q3-plan".to_string(),
            Some(&mut ask),
        )
        .await
        .unwrap();

        assert!(outcome.created);
        assert_eq!(outcome.repo.name, "docs-q3-plan-2");
        assert_eq!(handed, vec!["alice/docs-q3-plan".to_string()]);
        let names: Vec<Value> = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.method.as_str() == "POST")
            .map(|r| serde_json::from_slice::<Value>(&r.body).unwrap()["name"].clone())
            .collect();
        assert_eq!(names, vec![json!("docs-q3-plan"), json!("docs-q3-plan-2")]);
    }

    // SPEC u300 Tests, the row of this name.
    #[tokio::test]
    async fn a_moved_head_under_the_share_asks_no_other_name() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/repos/alice/docs/shares"))
            .respond_with(
                ResponseTemplate::new(409)
                    .set_body_json(json!({"error": "conflict", "currentSha": "h9"})),
            )
            .mount(&server)
            .await;
        let client = SynsClient::new(&server.uri()).unwrap();
        let mut calls = 0;
        let mut ask = |_: &str| {
            calls += 1;
            Some("other".to_string())
        };

        let result = share_with_names(
            &client,
            "t",
            &q3_plan(),
            "docs-q3-plan".to_string(),
            Some(&mut ask),
        )
        .await;

        match result {
            Err(CliError::Api {
                status: Some(409),
                error,
                ..
            }) => assert_eq!(error, "conflict"),
            other => panic!("expected the conflict refusal, got {other:?}"),
        }
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1, "one share request and no lookup");
        assert_eq!(requests[0].method.as_str(), "POST");
        assert_eq!(calls, 0);
    }

    // CR1-2: the name prompt, read from a given input.
    #[test]
    fn the_name_prompt_takes_the_offer_asks_again_and_ends_at_end_of_input() {
        let ask = |input: &str, offer: Option<&str>| {
            let mut diag = Vec::new();
            let answer = ask_name(
                &mut std::io::Cursor::new(input.as_bytes().to_vec()),
                &mut diag,
                "q3-plan",
                "alice/docs",
                offer,
            );
            (answer, String::from_utf8(diag).unwrap())
        };

        let (answer, diag) = ask("\n", Some("docs-q3-plan"));
        assert_eq!(answer.as_deref(), Some("docs-q3-plan"));
        assert_eq!(diag, "name for q3-plan of alice/docs [docs-q3-plan]: ");

        let (answer, diag) = ask("\nDocs\nq3\n", None);
        assert_eq!(answer.as_deref(), Some("q3"));
        let prompt = "name for q3-plan of alice/docs: ";
        assert_eq!(
            diag,
            format!(
                "{prompt}{prompt}{}\n{prompt}",
                share_name_problem("Docs").unwrap()
            )
        );

        assert_eq!(ask("", Some("docs-q3-plan")).0, None);
        assert_eq!(ask("Docs\n", None).0, None);
    }

    // CR1-2: the role half of the prompt gate.
    #[test]
    fn only_an_owner_or_an_admin_of_the_holder_is_asked_for_a_name() {
        assert!(may_prompt_for(Some(&CollaboratorRole::Owner)));
        assert!(may_prompt_for(Some(&CollaboratorRole::Admin)));
        assert!(!may_prompt_for(Some(&CollaboratorRole::Write)));
        assert!(!may_prompt_for(Some(&CollaboratorRole::Read)));
        assert!(!may_prompt_for(Some(&CollaboratorRole::Unknown)));
        assert!(!may_prompt_for(None));
    }
}

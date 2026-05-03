use crate::errors::CliError;
use std::io::Write;

/// Outcome of a destructive-confirmation prompt.
#[derive(Debug)]
pub enum ConfirmOutcome {
    /// `--yes` was passed; the caller proceeds without prompting.
    SkipPrompt,
    /// The user typed `s`; the caller compares it to its expected value.
    Input(String),
}

/// Prompt for destructive-confirmation, with `--yes` and `CI=true` short-circuits.
///
/// - When `yes` is true, returns `Ok(SkipPrompt)` without reading stdin.
/// - When `CI` env var is set and `yes` is false, returns
///   `Err(CliError::Config { message: "running under CI; pass --yes to confirm destructive operations" })`.
/// - Otherwise, writes `message` to stderr, flushes, reads a line from stdin,
///   and returns `Ok(Input(s))` where `s` is the raw line (untrimmed).
pub fn confirm_or_yes(yes: bool, message: &str) -> Result<ConfirmOutcome, CliError> {
    if yes {
        return Ok(ConfirmOutcome::SkipPrompt);
    }
    if std::env::var("CI").is_ok() {
        return Err(CliError::Config {
            message: "running under CI; pass --yes to confirm destructive operations".to_string(),
        });
    }
    eprint!("{message}");
    std::io::stderr().flush().map_err(|e| CliError::Io {
        message: format!("could not read confirmation input: {e}"),
    })?;
    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .map_err(|e| CliError::Io {
            message: format!("could not read confirmation input: {e}"),
        })?;
    Ok(ConfirmOutcome::Input(input))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial]
    fn confirm_or_yes_skip_when_yes_flag_set() {
        unsafe { std::env::remove_var("CI") };
        let outcome = confirm_or_yes(true, "ignored").unwrap();
        assert!(matches!(outcome, ConfirmOutcome::SkipPrompt));
    }

    #[test]
    #[serial]
    fn confirm_or_yes_errors_when_ci_set_and_no_yes() {
        unsafe { std::env::set_var("CI", "true") };
        let result = confirm_or_yes(false, "Type repo to confirm: ");
        unsafe { std::env::remove_var("CI") };
        let err = result.unwrap_err();
        match err {
            CliError::Config { message } => {
                assert_eq!(
                    message,
                    "running under CI; pass --yes to confirm destructive operations"
                );
            }
            other => panic!("expected Config, got {other:?}"),
        }
    }

    #[test]
    #[serial]
    fn confirm_or_yes_errors_when_ci_set_to_empty_string() {
        unsafe { std::env::set_var("CI", "") };
        let result = confirm_or_yes(false, "...");
        unsafe { std::env::remove_var("CI") };
        assert!(matches!(result, Err(CliError::Config { .. })));
    }

    #[test]
    #[serial]
    fn confirm_or_yes_short_circuits_yes_flag_even_when_ci_set() {
        unsafe { std::env::set_var("CI", "true") };
        let outcome = confirm_or_yes(true, "...").unwrap();
        unsafe { std::env::remove_var("CI") };
        assert!(matches!(outcome, ConfirmOutcome::SkipPrompt));
    }
}

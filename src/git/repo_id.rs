use sha2::{Sha256, Digest};
use crate::errors::CliError;
use crate::git::remote::normalize_remote;

const REPO_ID_LENGTH: usize = 16;

pub fn derive_repo_id(remote_url: &str) -> Result<String, CliError> {
    let normalized = normalize_remote(remote_url);
    if normalized.is_empty() || normalized == "https://" {
        return Err(CliError::Config {
            message: "cannot derive repo ID from empty remote URL".to_string(),
        });
    }
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    let result = hasher.finalize();
    let hex = format!("{:x}", result);
    Ok(hex[..REPO_ID_LENGTH].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_and_https_produce_same_id() {
        let ssh_id = derive_repo_id("git@github.com:user/repo.git").unwrap();
        let https_id = derive_repo_id("https://github.com/user/repo").unwrap();
        assert_eq!(ssh_id, https_id);
    }

    #[test]
    fn repo_id_is_deterministic() {
        let id1 = derive_repo_id("https://github.com/user/repo").unwrap();
        let id2 = derive_repo_id("https://github.com/user/repo").unwrap();
        assert_eq!(id1, id2);
    }

    #[test]
    fn repo_id_is_16_hex_chars() {
        let id = derive_repo_id("https://github.com/user/repo").unwrap();
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
    }

    #[test]
    fn repo_id_empty_input_returns_error() {
        let result = derive_repo_id("");
        assert!(result.is_err());
    }

    #[test]
    fn repo_id_degenerate_https_returns_error() {
        let result = derive_repo_id("https://");
        assert!(result.is_err());
    }
}

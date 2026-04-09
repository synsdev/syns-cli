use crate::errors::CliError;
use comfy_table::{presets, Attribute, Cell, Table};
use console::style;
use serde::Serialize;

#[derive(Clone)]
#[allow(dead_code)] // Methods used by downstream command units (U09+)
pub struct Output {
    json_mode: bool,
}

#[allow(dead_code)] // Methods used by downstream command units (U09+)
impl Output {
    pub fn new(json_mode: bool) -> Output {
        Output { json_mode }
    }

    pub fn is_json(&self) -> bool {
        self.json_mode
    }

    pub(crate) fn format_table(&self, headers: &[&str], rows: Vec<Vec<String>>) -> Option<String> {
        if self.json_mode {
            return None;
        }
        if headers.is_empty() && rows.is_empty() {
            return None;
        }
        let mut table = Table::new();
        table.load_preset(presets::UTF8_FULL_CONDENSED);
        table.set_header(
            headers
                .iter()
                .map(|h| Cell::new(h).add_attribute(Attribute::Bold)),
        );
        for row in rows {
            table.add_row(row);
        }
        Some(table.to_string())
    }

    pub(crate) fn format_json<T: Serialize>(&self, value: &T) -> String {
        match serde_json::to_string(value) {
            Ok(json) => json,
            Err(_) => r#"{"error": "serialization_failed"}"#.to_string(),
        }
    }

    pub(crate) fn format_error(&self, error: &CliError) -> String {
        if self.json_mode {
            serde_json::json!({"error": error.to_string()}).to_string()
        } else {
            format!("error: {error}")
        }
    }

    pub(crate) fn format_success(&self, message: &str) -> Option<String> {
        if self.json_mode {
            None
        } else {
            Some(message.to_string())
        }
    }

    pub fn table(&self, headers: &[&str], rows: Vec<Vec<String>>) {
        if let Some(text) = self.format_table(headers, rows) {
            println!("{text}");
        }
    }

    pub fn json<T: Serialize>(&self, value: &T) {
        match serde_json::to_string_pretty(value) {
            Ok(json) => println!("{json}"),
            Err(_) => eprintln!(r#"{{"error": "serialization_failed"}}"#),
        }
    }

    pub fn error(&self, error: &CliError) {
        let text = self.format_error(error);
        if self.json_mode {
            println!("{text}");
        } else {
            let styled = style(&text).red().bold();
            eprintln!("{styled}");
        }
    }

    pub fn success(&self, message: &str) {
        if let Some(text) = self.format_success(message) {
            let styled = style(&text).green();
            println!("{styled}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_table_normal_mode() {
        let output = Output::new(false);
        let result = output.format_table(
            &["Name", "Status"],
            vec![
                vec!["my-repo".into(), "active".into()],
                vec!["other-repo".into(), "draft".into()],
            ],
        );
        let text = result.unwrap();
        assert!(text.contains("Name"));
        assert!(text.contains("Status"));
        assert!(text.contains("my-repo"));
        assert!(text.contains("active"));
        assert!(text.contains("other-repo"));
        assert!(text.contains("draft"));
        assert!(text.contains('│') || text.contains('─'));
    }

    #[test]
    fn format_table_json_mode_is_noop() {
        let output = Output::new(true);
        let result = output.format_table(
            &["Name"],
            vec![vec!["value".into()]],
        );
        assert!(result.is_none());
    }

    #[test]
    fn format_table_empty_is_noop() {
        let output = Output::new(false);
        let result = output.format_table(&[], vec![]);
        assert!(result.is_none());
    }

    #[test]
    fn format_table_headers_only() {
        let output = Output::new(false);
        let result = output.format_table(&["Name", "Status"], vec![]);
        let text = result.unwrap();
        assert!(text.contains("Name"));
        assert!(text.contains("Status"));
    }

    #[test]
    fn format_json_valid() {
        let output = Output::new(true);
        let text = output.format_json(&serde_json::json!({"name": "my-repo", "status": "active"}));
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["name"], "my-repo");
        assert_eq!(parsed["status"], "active");
    }

    #[test]
    fn format_error_normal_mode() {
        let output = Output::new(false);
        let err = CliError::Config { message: "test error".into() };
        let text = output.format_error(&err);
        assert!(text.contains("error:"));
        assert!(text.contains("configuration error: test error"));
    }

    #[test]
    fn format_error_json_mode() {
        let output = Output::new(true);
        let err = CliError::Config { message: "test error".into() };
        let text = output.format_error(&err);
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["error"], "configuration error: test error");
    }

    #[test]
    fn format_success_normal_mode() {
        let output = Output::new(false);
        let result = output.format_success("done");
        assert_eq!(result, Some("done".to_string()));
    }

    #[test]
    fn format_success_json_mode_is_noop() {
        let output = Output::new(true);
        let result = output.format_success("done");
        assert!(result.is_none());
    }
}

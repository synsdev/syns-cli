use comfy_table::{presets, Attribute, Cell, Table};
use console::style;
use serde::Serialize;

use crate::errors::CliError;

#[derive(Clone)]
pub struct Output {
    json_mode: bool,
}

impl Output {
    pub fn new(json_mode: bool) -> Output {
        Output { json_mode }
    }

    pub fn is_json(&self) -> bool {
        self.json_mode
    }

    pub fn table(&self, headers: &[&str], rows: Vec<Vec<String>>) {
        if let Some(s) = self.format_table(headers, rows) {
            println!("{s}");
        }
    }

    pub fn json<T: Serialize>(&self, value: &T) {
        match self.format_json(value) {
            Ok(s) => println!("{s}"),
            Err(()) => eprintln!("{{\"error\": \"serialization_failed\"}}"),
        }
    }

    pub fn error(&self, error: &CliError) {
        if self.json_mode {
            println!("{}", serde_json::json!({"error": error.to_string()}));
        } else {
            let styled = style(format!("error: {error}")).red().bold();
            eprintln!("{styled}");
        }
    }

    pub fn success(&self, message: &str) {
        if !self.json_mode {
            let styled = style(message).green();
            println!("{styled}");
        }
    }

    pub(crate) fn format_table(
        &self,
        headers: &[&str],
        rows: Vec<Vec<String>>,
    ) -> Option<String> {
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

    pub(crate) fn format_json<T: Serialize>(&self, value: &T) -> Result<String, ()> {
        serde_json::to_string_pretty(value).map_err(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_table_contains_headers_and_data() {
        let output = Output::new(false);
        let result = output.format_table(
            &["Name", "Status"],
            vec![
                vec!["my-repo".into(), "active".into()],
                vec!["other-repo".into(), "draft".into()],
            ],
        );
        let s = result.expect("table should be Some");
        assert!(s.contains("Name"));
        assert!(s.contains("Status"));
        assert!(s.contains("my-repo"));
        assert!(s.contains("active"));
        assert!(s.contains("other-repo"));
        assert!(s.contains("draft"));
        assert!(s.contains('\u{2502}') || s.contains('\u{2500}'));
    }

    #[test]
    fn output_table_noop_in_json_mode() {
        let output = Output::new(true);
        let result = output.format_table(&["Name"], vec![vec!["x".into()]]);
        assert!(result.is_none());
    }

    #[test]
    fn output_json_formats_correctly() {
        let output = Output::new(true);
        let result =
            output.format_json(&serde_json::json!({"name": "my-repo", "status": "active"}));
        let s = result.expect("json should be Ok");
        let parsed: serde_json::Value = serde_json::from_str(&s).expect("should parse as JSON");
        assert_eq!(parsed["name"], "my-repo");
        assert_eq!(parsed["status"], "active");
    }

    #[test]
    fn output_table_empty_rows_shows_headers() {
        let output = Output::new(false);
        let result = output.format_table(&["Name", "Status"], vec![]);
        let s = result.expect("table should be Some with headers only");
        assert!(s.contains("Name"));
        assert!(s.contains("Status"));
    }

    #[test]
    fn output_table_empty_everything_is_noop() {
        let output = Output::new(false);
        let result = output.format_table(&[], vec![]);
        assert!(result.is_none());
    }
}

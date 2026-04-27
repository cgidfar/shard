use std::path::{Component, Path};

use crate::{Result, ShardError};

const WINDOWS_RESERVED_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

pub fn validate_repo_alias(alias: &str) -> Result<()> {
    validate_path_segment("repo alias", alias)
}

pub fn validate_workspace_name(name: &str) -> Result<()> {
    validate_path_segment("workspace name", name)
}

pub fn validate_session_id(id: &str) -> Result<()> {
    validate_path_segment("session id", id)
}

fn validate_path_segment(kind: &str, value: &str) -> Result<()> {
    let invalid = |reason: &str| ShardError::Other(format!("invalid {kind} '{value}': {reason}"));

    if value.is_empty() {
        return Err(invalid("must not be empty"));
    }
    if value.trim() != value {
        return Err(invalid("must not have leading or trailing whitespace"));
    }
    if value.ends_with('.') || value.ends_with(' ') {
        return Err(invalid("must not end with a space or period"));
    }
    if value == "." || value == ".." {
        return Err(invalid("must not be '.' or '..'"));
    }
    if value.contains('/') || value.contains('\\') {
        return Err(invalid("must be a single path segment"));
    }
    if value
        .chars()
        .any(|c| c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
    {
        return Err(invalid(
            "contains characters that are unsafe in Windows paths",
        ));
    }
    if Path::new(value).is_absolute() {
        return Err(invalid("must not be an absolute path"));
    }
    if !matches!(
        Path::new(value).components().collect::<Vec<_>>().as_slice(),
        [Component::Normal(_)]
    ) {
        return Err(invalid("must be a single normal path segment"));
    }

    let stem = value
        .split('.')
        .next()
        .unwrap_or(value)
        .to_ascii_uppercase();
    if WINDOWS_RESERVED_NAMES.iter().any(|name| *name == stem) {
        return Err(invalid("uses a reserved Windows device name"));
    }

    Ok(())
}

pub fn safe_workspace_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_dash = false;

    for ch in raw.chars() {
        let safe = match ch {
            '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*' if !ch.is_control() => '-',
            _ if ch.is_control() => '-',
            _ => ch,
        };
        if safe == '-' {
            if last_dash {
                continue;
            }
            last_dash = true;
        } else {
            last_dash = false;
        }
        out.push(safe);
    }

    let mut trimmed = out
        .trim_matches(|c| c == '-' || c == ' ' || c == '.')
        .to_string();
    if trimmed.is_empty() {
        trimmed = "workspace".to_string();
    }

    let stem = trimmed
        .split('.')
        .next()
        .unwrap_or(&trimmed)
        .to_ascii_uppercase();
    if WINDOWS_RESERVED_NAMES.iter().any(|name| *name == stem) {
        trimmed = format!("workspace-{trimmed}");
    }

    trimmed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_path_like_identifiers() {
        for value in [
            "",
            ".",
            "..",
            "a/b",
            "a\\b",
            "C:\\Windows",
            "C:relative",
            "name:",
            "trailing.",
            " trailing",
            "CON",
            "COM1.txt",
        ] {
            assert!(
                validate_repo_alias(value).is_err(),
                "expected invalid: {value:?}"
            );
        }
    }

    #[test]
    fn accepts_single_safe_segments() {
        for value in ["demo", "feature-a", "feature.a", "my repo", "ABC_123"] {
            assert!(
                validate_workspace_name(value).is_ok(),
                "expected valid: {value:?}"
            );
        }
    }

    #[test]
    fn safe_workspace_name_strips_path_separators_and_reserved_names() {
        assert_eq!(safe_workspace_name("feature/foo"), "feature-foo");
        assert_eq!(safe_workspace_name("feature\\foo"), "feature-foo");
        assert_eq!(safe_workspace_name("CON"), "workspace-CON");
        assert_eq!(safe_workspace_name("///"), "workspace");
    }
}

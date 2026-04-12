//! Shell utilities for session defaults.

use std::path::PathBuf;
use std::sync::OnceLock;

/// Default shell command for new sessions.
///
/// Prefers PowerShell 7 (pwsh.exe) if available, otherwise falls back to COMSPEC or cmd.exe.
pub fn default_command() -> Vec<String> {
    if which_exists("pwsh.exe") {
        vec!["pwsh.exe".into(), "-NoLogo".into()]
    } else {
        let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
        vec![shell]
    }
}

/// Check if an executable exists in PATH using native lookup (no subprocess).
pub fn which_exists(name: &str) -> bool {
    find_in_path(name).is_some()
}

/// Cached PATHEXT extensions (computed once).
fn pathext_extensions() -> &'static [String] {
    static EXTENSIONS: OnceLock<Vec<String>> = OnceLock::new();
    EXTENSIONS.get_or_init(|| {
        if cfg!(windows) {
            std::env::var("PATHEXT")
                .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
                .split(';')
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect()
        } else {
            Vec::new()
        }
    })
}

/// Find an executable in PATH without shelling out.
fn find_in_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;

    for dir in std::env::split_paths(&path_var) {
        // Check exact name first
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        // On Windows, try with each extension if name doesn't already have one
        if cfg!(windows) && !name.contains('.') {
            for ext in pathext_extensions() {
                let with_ext = dir.join(format!("{name}{ext}"));
                if with_ext.is_file() {
                    return Some(with_ext);
                }
            }
        }
    }
    None
}

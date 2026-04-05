use std::path::Path;
use std::process::Command;

use crate::{Result, ShardError};

/// Run a git command and return stdout on success.
pub fn run_git(args: &[&str], cwd: Option<&Path>) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    tracing::debug!("git {}", args.join(" "));

    let output = cmd.output().map_err(|e| ShardError::Git(format!("failed to run git: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ShardError::Git(format!(
            "git {} failed: {}",
            args.join(" "),
            stderr.trim()
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Clone a repo as a bare repository.
pub fn clone_bare(url: &str, dest: &Path) -> Result<()> {
    let dest_str = dest.to_str().ok_or_else(|| ShardError::Other("invalid path".into()))?;
    run_git(&["clone", "--bare", url, dest_str], None)?;
    Ok(())
}

/// Fetch latest changes in a repo (bare or regular checkout).
pub fn fetch(repo_dir: &Path) -> Result<()> {
    run_git(&["fetch", "--all", "--prune"], Some(repo_dir))?;
    Ok(())
}

/// Create a git worktree from a repo (bare or regular checkout).
///
/// If `new_branch` is Some, creates a new branch based on `start_point` (the branch arg).
/// Otherwise checks out the existing branch directly.
pub fn worktree_add(
    repo_dir: &Path,
    worktree_path: &Path,
    branch: &str,
    new_branch: Option<&str>,
) -> Result<()> {
    let wt = worktree_path.to_str().ok_or_else(|| ShardError::Other("invalid path".into()))?;
    match new_branch {
        Some(nb) => {
            run_git(&["worktree", "add", "-b", nb, wt, branch], Some(repo_dir))?;
        }
        None => {
            run_git(&["worktree", "add", wt, branch], Some(repo_dir))?;
        }
    }
    Ok(())
}

/// Remove a git worktree.
pub fn worktree_remove(repo_dir: &Path, worktree_path: &Path) -> Result<()> {
    let wt = worktree_path.to_str().ok_or_else(|| ShardError::Other("invalid path".into()))?;
    run_git(&["worktree", "remove", "--force", wt], Some(repo_dir))?;
    Ok(())
}

/// Prune stale worktree admin entries.
pub fn worktree_prune(repo_dir: &Path) -> Result<()> {
    run_git(&["worktree", "prune"], Some(repo_dir))?;
    Ok(())
}

/// List branches in a repo. Returns branch names without the refs/heads/ prefix.
pub fn list_branches(repo_dir: &Path) -> Result<Vec<String>> {
    let output = run_git(
        &["for-each-ref", "--format=%(refname:short)", "refs/heads/"],
        Some(repo_dir),
    )?;
    Ok(output.lines().map(|s| s.to_string()).filter(|s| !s.is_empty()).collect())
}

/// Get the current branch (HEAD target) of a repo.
pub fn default_branch(repo_dir: &Path) -> Result<String> {
    let output = run_git(&["symbolic-ref", "--short", "HEAD"], Some(repo_dir))?;
    Ok(output)
}

/// Add a pattern to `.git/info/exclude` (per-repo gitignore, not committed).
/// Idempotent — won't add if already present.
pub fn add_to_exclude(repo_dir: &Path, pattern: &str) -> Result<()> {
    let exclude_path = repo_dir.join(".git").join("info").join("exclude");
    if let Some(parent) = exclude_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let contents = std::fs::read_to_string(&exclude_path).unwrap_or_default();
    if contents.lines().any(|line| line.trim() == pattern) {
        return Ok(()); // Already present
    }

    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&exclude_path)?;
    writeln!(f, "{}", pattern)?;
    Ok(())
}

/// Remove a pattern from `.git/info/exclude`.
pub fn remove_from_exclude(repo_dir: &Path, pattern: &str) -> Result<()> {
    let exclude_path = repo_dir.join(".git").join("info").join("exclude");
    if !exclude_path.exists() {
        return Ok(());
    }

    let contents = std::fs::read_to_string(&exclude_path)?;
    let filtered: Vec<&str> = contents
        .lines()
        .filter(|line| line.trim() != pattern)
        .collect();
    std::fs::write(&exclude_path, filtered.join("\n") + "\n")?;
    Ok(())
}

/// Parse a git URL or path into optional (host, owner, name) components.
///
/// Supports:
///   https://github.com/owner/name.git
///   https://github.com/owner/name
///   git@github.com:owner/name.git
///   ssh://git@host/owner/name
///   C:\repos\local-project (local path — returns None for all)
pub fn parse_url(url: &str) -> (Option<String>, Option<String>, Option<String>) {
    // Try SSH shorthand: git@host:owner/name.git
    if let Some(rest) = url.strip_prefix("git@") {
        if let Some((host, path)) = rest.split_once(':') {
            let parts = parse_path_components(path);
            if let (Some(owner), Some(name)) = (parts.0, parts.1) {
                return (Some(host.to_string()), Some(owner), Some(name));
            }
        }
    }

    // Try URL with scheme: https://host/owner/name or ssh://git@host/owner/name
    if url.contains("://") {
        if let Some(after_scheme) = url.split("://").nth(1) {
            // Strip optional user@ prefix
            let after_user = if let Some((_user, rest)) = after_scheme.split_once('@') {
                rest
            } else {
                after_scheme
            };

            // Split into host and path
            if let Some((host, path)) = after_user.split_once('/') {
                let parts = parse_path_components(path);
                if let (Some(owner), Some(name)) = (parts.0, parts.1) {
                    return (Some(host.to_string()), Some(owner), Some(name));
                }
            }
        }
    }

    // Not a recognized URL pattern (local path, etc.)
    (None, None, None)
}

/// Extract owner and name from a URL path like "owner/name.git" or "owner/name"
fn parse_path_components(path: &str) -> (Option<String>, Option<String>) {
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    if parts.len() >= 2 {
        let owner = parts[0].to_string();
        let name = parts[1].trim_end_matches(".git").to_string();
        if !owner.is_empty() && !name.is_empty() {
            return (Some(owner), Some(name));
        }
    }
    (None, None)
}

/// Derive a default alias from a URL.
///
/// Uses the repo name if parseable, otherwise returns None.
pub fn default_alias(url: &str) -> Option<String> {
    let (_, _, name) = parse_url(url);
    if let Some(n) = name {
        return Some(n);
    }

    // For local paths, use the last path component
    let path = Path::new(url);
    path.file_name()
        .and_then(|f| f.to_str())
        .map(|s| s.trim_end_matches(".git").to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_https_url() {
        let (host, owner, name) = parse_url("https://github.com/penberg/swarm");
        assert_eq!(host.as_deref(), Some("github.com"));
        assert_eq!(owner.as_deref(), Some("penberg"));
        assert_eq!(name.as_deref(), Some("swarm"));
    }

    #[test]
    fn parse_https_url_with_git_suffix() {
        let (host, owner, name) = parse_url("https://github.com/penberg/swarm.git");
        assert_eq!(host.as_deref(), Some("github.com"));
        assert_eq!(owner.as_deref(), Some("penberg"));
        assert_eq!(name.as_deref(), Some("swarm"));
    }

    #[test]
    fn parse_ssh_shorthand() {
        let (host, owner, name) = parse_url("git@github.com:penberg/swarm.git");
        assert_eq!(host.as_deref(), Some("github.com"));
        assert_eq!(owner.as_deref(), Some("penberg"));
        assert_eq!(name.as_deref(), Some("swarm"));
    }

    #[test]
    fn parse_local_path() {
        let (host, owner, name) = parse_url(r"C:\repos\my-project");
        assert!(host.is_none());
        assert!(owner.is_none());
        assert!(name.is_none());
    }

    #[test]
    fn default_alias_from_https() {
        assert_eq!(default_alias("https://github.com/penberg/swarm"), Some("swarm".into()));
    }

    #[test]
    fn default_alias_from_local_path() {
        assert_eq!(default_alias(r"C:\repos\my-project"), Some("my-project".into()));
    }

    #[test]
    fn default_alias_from_ssh() {
        assert_eq!(default_alias("git@github.com:penberg/swarm.git"), Some("swarm".into()));
    }
}

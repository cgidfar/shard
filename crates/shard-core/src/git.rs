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

/// Fetch latest changes into a bare repo.
pub fn fetch(bare_repo: &Path) -> Result<()> {
    run_git(&["fetch", "--all", "--prune"], Some(bare_repo))?;
    Ok(())
}

/// Create a git worktree from a bare repo.
///
/// If `new_branch` is Some, creates a new branch based on `start_point` (the branch arg).
/// Otherwise checks out the existing branch directly.
pub fn worktree_add(
    bare_repo: &Path,
    worktree_path: &Path,
    branch: &str,
    new_branch: Option<&str>,
) -> Result<()> {
    let wt = worktree_path.to_str().ok_or_else(|| ShardError::Other("invalid path".into()))?;
    match new_branch {
        Some(nb) => {
            // git worktree add -b <new_branch> <path> <start_point>
            run_git(&["worktree", "add", "-b", nb, wt, branch], Some(bare_repo))?;
        }
        None => {
            run_git(&["worktree", "add", wt, branch], Some(bare_repo))?;
        }
    }
    Ok(())
}

/// Remove a git worktree.
pub fn worktree_remove(bare_repo: &Path, worktree_path: &Path) -> Result<()> {
    let wt = worktree_path.to_str().ok_or_else(|| ShardError::Other("invalid path".into()))?;
    run_git(&["worktree", "remove", "--force", wt], Some(bare_repo))?;
    Ok(())
}

/// List branches in a bare repo. Returns branch names without the refs/heads/ prefix.
pub fn list_branches(bare_repo: &Path) -> Result<Vec<String>> {
    let output = run_git(
        &["for-each-ref", "--format=%(refname:short)", "refs/heads/"],
        Some(bare_repo),
    )?;
    Ok(output.lines().map(|s| s.to_string()).filter(|s| !s.is_empty()).collect())
}

/// Get the default branch of a bare repo (HEAD target).
pub fn default_branch(bare_repo: &Path) -> Result<String> {
    let output = run_git(&["symbolic-ref", "--short", "HEAD"], Some(bare_repo))?;
    Ok(output)
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

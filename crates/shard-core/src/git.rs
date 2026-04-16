use std::path::Path;
use std::process::Command;

use crate::{Result, ShardError};

/// Run a git command and return stdout on success.
///
/// On Windows the command is spawned with `CREATE_NO_WINDOW` so no console
/// window flashes when called from a GUI-subsystem parent (the daemon, the
/// Tauri app). Without this flag, every `git` invocation briefly pops a
/// console window — visually disruptive on the reconcile tick's 30s
/// `git worktree list --porcelain` loop.
pub fn run_git(args: &[&str], cwd: Option<&Path>) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
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

/// Parsed HEAD state for a workspace.
///
/// - `branch = Some(name), detached = false` — symbolic ref to `refs/heads/<name>`
/// - `branch = None, detached = true` — bare SHA (rebase/bisect/detached HEAD)
/// - `sha` is always the best-effort resolved commit SHA when readable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadState {
    pub branch: Option<String>,
    pub sha: Option<String>,
    pub detached: bool,
}

/// Resolve the actual git directory for a workspace path.
///
/// Handles three shapes:
/// 1. A real repo — `<workspace>/.git` is a directory. Returns that directory.
/// 2. A linked worktree — `<workspace>/.git` is a file containing `gitdir: <path>`.
///    Returns the referenced path (which lives under the main repo's
///    `.git/worktrees/<name>/`).
/// 3. The workspace path *is* a gitdir (bare layout). Returns the path as-is.
///
/// Returns an error if nothing git-like is found. The returned path is the
/// directory whose `HEAD` file should be read and watched.
pub fn resolve_gitdir(workspace_path: &Path) -> Result<std::path::PathBuf> {
    let dot_git = workspace_path.join(".git");

    if dot_git.is_dir() {
        return Ok(dot_git);
    }

    if dot_git.is_file() {
        let contents = std::fs::read_to_string(&dot_git)
            .map_err(|e| ShardError::Git(format!("read .git file at {:?}: {e}", dot_git)))?;
        for line in contents.lines() {
            if let Some(rest) = line.strip_prefix("gitdir:") {
                let rel = rest.trim();
                let resolved = if std::path::Path::new(rel).is_absolute() {
                    std::path::PathBuf::from(rel)
                } else {
                    workspace_path.join(rel)
                };
                // Canonicalize best-effort; fall back to the raw join so that
                // subsequent reads / notify watches still have something
                // concrete to point at even if `..` segments stay unresolved.
                return Ok(std::fs::canonicalize(&resolved)
                    .map(strip_unc_prefix)
                    .unwrap_or(resolved));
            }
        }
        return Err(ShardError::Git(format!(
            ".git file at {:?} has no gitdir: line",
            dot_git
        )));
    }

    // Bare layout — the path itself looks like a gitdir.
    if workspace_path.join("HEAD").exists() {
        return Ok(workspace_path.to_path_buf());
    }

    Err(ShardError::Git(format!(
        "no git directory at {:?}",
        workspace_path
    )))
}

/// Strip the Windows `\\?\` extended-length prefix from a canonicalized path.
pub fn strip_unc_prefix(p: std::path::PathBuf) -> std::path::PathBuf {
    let s = p.to_string_lossy();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        std::path::PathBuf::from(stripped)
    } else {
        p
    }
}

/// Read and parse the HEAD file inside a gitdir.
///
/// Reads `<gitdir>/HEAD` directly instead of shelling out — this is on the
/// hot path for the WorkspaceMonitor and has to be cheap. Handles both
/// symbolic refs (`ref: refs/heads/main`) and bare SHAs.
///
/// For symbolic refs, also attempts to resolve the referenced SHA by reading
/// `<gitdir>/refs/heads/<branch>` or scanning `<gitdir>/packed-refs`. SHA
/// resolution is best-effort; a missing SHA does not fail the call.
pub fn read_head(gitdir: &Path) -> Result<HeadState> {
    let head_path = gitdir.join("HEAD");
    let raw = std::fs::read_to_string(&head_path)
        .map_err(|e| ShardError::Git(format!("read HEAD at {:?}: {e}", head_path)))?;
    let trimmed = raw.trim();

    if let Some(rest) = trimmed.strip_prefix("ref:") {
        let ref_name = rest.trim();
        let branch = ref_name
            .strip_prefix("refs/heads/")
            .map(|s| s.to_string());
        let sha = resolve_ref_sha(gitdir, ref_name);
        return Ok(HeadState {
            branch,
            sha,
            detached: false,
        });
    }

    // Bare SHA — detached HEAD (rebase, bisect, explicit checkout of a commit).
    if looks_like_sha(trimmed) {
        return Ok(HeadState {
            branch: None,
            sha: Some(trimmed.to_string()),
            detached: true,
        });
    }

    Err(ShardError::Git(format!(
        "unrecognized HEAD content in {:?}: {:?}",
        head_path, trimmed
    )))
}

fn looks_like_sha(s: &str) -> bool {
    s.len() >= 7 && s.len() <= 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Resolve a ref name (e.g. `refs/heads/main`) to a SHA inside `gitdir`.
/// Checks the loose ref file first, then falls back to `packed-refs`.
fn resolve_ref_sha(gitdir: &Path, ref_name: &str) -> Option<String> {
    let loose = gitdir.join(ref_name);
    if let Ok(content) = std::fs::read_to_string(&loose) {
        let trimmed = content.trim();
        if looks_like_sha(trimmed) {
            return Some(trimmed.to_string());
        }
    }

    let packed = gitdir.join("packed-refs");
    if let Ok(content) = std::fs::read_to_string(&packed) {
        for line in content.lines() {
            if line.starts_with('#') || line.starts_with('^') {
                continue;
            }
            if let Some((sha, name)) = line.split_once(' ') {
                if name == ref_name && looks_like_sha(sha) {
                    return Some(sha.to_string());
                }
            }
        }
    }

    None
}

/// A single entry returned by `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub path: std::path::PathBuf,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub detached: bool,
    /// True when git has flagged the entry as `prunable` (worktree dir missing
    /// or administratively broken). Used by the monitor's reconcile tick to
    /// distinguish Missing from Broken.
    pub prunable: bool,
}

/// Run `git worktree list --porcelain` and parse the output.
///
/// Called from the WorkspaceMonitor reconcile tick to cross-check the
/// daemon's idea of the world against git's own registration. An entry in
/// our DB that doesn't show up here is Broken; an entry git reports as
/// `prunable` is Missing.
pub fn worktree_list_porcelain(repo_dir: &Path) -> Result<Vec<WorktreeEntry>> {
    let output = run_git(&["worktree", "list", "--porcelain"], Some(repo_dir))?;
    Ok(parse_worktree_porcelain(&output))
}

fn parse_worktree_porcelain(output: &str) -> Vec<WorktreeEntry> {
    let mut entries = Vec::new();
    let mut cur: Option<WorktreeEntry> = None;

    for line in output.lines() {
        if line.is_empty() {
            if let Some(entry) = cur.take() {
                entries.push(entry);
            }
            continue;
        }

        if let Some(rest) = line.strip_prefix("worktree ") {
            if let Some(entry) = cur.take() {
                entries.push(entry);
            }
            cur = Some(WorktreeEntry {
                path: std::path::PathBuf::from(rest),
                head: None,
                branch: None,
                detached: false,
                prunable: false,
            });
        } else if let Some(entry) = cur.as_mut() {
            if let Some(sha) = line.strip_prefix("HEAD ") {
                entry.head = Some(sha.to_string());
            } else if let Some(br) = line.strip_prefix("branch ") {
                entry.branch = Some(br.strip_prefix("refs/heads/").unwrap_or(br).to_string());
            } else if line == "detached" {
                entry.detached = true;
            } else if line == "prunable" || line.starts_with("prunable ") {
                entry.prunable = true;
            }
        }
    }

    if let Some(entry) = cur.take() {
        entries.push(entry);
    }

    entries
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

    // ── HEAD parsing ────────────────────────────────────────────────────

    fn tempdir() -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "shard-core-git-tests-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn read_head_symbolic_ref() {
        let dir = tempdir();
        std::fs::write(dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let head = read_head(&dir).unwrap();
        assert_eq!(head.branch.as_deref(), Some("main"));
        assert!(!head.detached);
        assert!(head.sha.is_none()); // no loose ref file
    }

    #[test]
    fn read_head_symbolic_ref_with_loose_sha() {
        let dir = tempdir();
        std::fs::write(dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let refs_heads = dir.join("refs").join("heads");
        std::fs::create_dir_all(&refs_heads).unwrap();
        std::fs::write(
            refs_heads.join("main"),
            "abc123def4567890abc123def4567890abc12345\n",
        )
        .unwrap();

        let head = read_head(&dir).unwrap();
        assert_eq!(head.branch.as_deref(), Some("main"));
        assert_eq!(
            head.sha.as_deref(),
            Some("abc123def4567890abc123def4567890abc12345")
        );
        assert!(!head.detached);
    }

    #[test]
    fn read_head_symbolic_ref_with_packed_refs() {
        let dir = tempdir();
        std::fs::write(dir.join("HEAD"), "ref: refs/heads/feature\n").unwrap();
        std::fs::write(
            dir.join("packed-refs"),
            "# pack-refs with: peeled fully-peeled\n\
             abc123def4567890abc123def4567890abc12345 refs/heads/feature\n\
             def4567890abc123def4567890abc123def45678 refs/heads/other\n",
        )
        .unwrap();

        let head = read_head(&dir).unwrap();
        assert_eq!(head.branch.as_deref(), Some("feature"));
        assert_eq!(
            head.sha.as_deref(),
            Some("abc123def4567890abc123def4567890abc12345")
        );
    }

    #[test]
    fn read_head_detached() {
        let dir = tempdir();
        std::fs::write(
            dir.join("HEAD"),
            "abc123def4567890abc123def4567890abc12345\n",
        )
        .unwrap();

        let head = read_head(&dir).unwrap();
        assert!(head.branch.is_none());
        assert!(head.detached);
        assert_eq!(
            head.sha.as_deref(),
            Some("abc123def4567890abc123def4567890abc12345")
        );
    }

    // ── gitdir resolution ───────────────────────────────────────────────

    #[test]
    fn resolve_gitdir_plain_repo() {
        let dir = tempdir();
        let dot_git = dir.join(".git");
        std::fs::create_dir_all(&dot_git).unwrap();
        std::fs::write(dot_git.join("HEAD"), "ref: refs/heads/main\n").unwrap();

        let resolved = resolve_gitdir(&dir).unwrap();
        assert_eq!(resolved, dot_git);
    }

    #[test]
    fn resolve_gitdir_linked_worktree() {
        let dir = tempdir();
        let admin = dir.join("admin");
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::write(admin.join("HEAD"), "ref: refs/heads/feat\n").unwrap();

        let workspace = dir.join("worktree");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(
            workspace.join(".git"),
            format!("gitdir: {}\n", admin.to_string_lossy()),
        )
        .unwrap();

        let resolved = resolve_gitdir(&workspace).unwrap();
        // Canonicalization may strip \\?\ on Windows; compare as canonicalized.
        let expected = std::fs::canonicalize(&admin).map(strip_unc_prefix).unwrap();
        assert_eq!(resolved, expected);
    }

    #[test]
    fn resolve_gitdir_missing() {
        let dir = tempdir();
        // No .git file / dir and no HEAD file.
        assert!(resolve_gitdir(&dir).is_err());
    }

    // ── porcelain parsing ───────────────────────────────────────────────

    #[test]
    fn parse_worktree_list_porcelain_smoke() {
        let output = "worktree /path/to/main\n\
                      HEAD abc123def4567890abc123def4567890abc12345\n\
                      branch refs/heads/main\n\
                      \n\
                      worktree /path/to/feature\n\
                      HEAD def4567890abc123def4567890abc123def45678\n\
                      detached\n\
                      \n\
                      worktree /path/to/stale\n\
                      HEAD 0000000000000000000000000000000000000000\n\
                      prunable gitdir file points to non-existent location\n";

        let entries = parse_worktree_porcelain(output);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].branch.as_deref(), Some("main"));
        assert!(!entries[0].detached);
        assert!(entries[1].detached);
        assert!(entries[2].prunable);
    }
}

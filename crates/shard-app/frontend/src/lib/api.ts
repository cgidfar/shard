import { invoke, Channel } from "@tauri-apps/api/core";

// --- Types ---

export interface Repository {
  id: string;
  url: string;
  alias: string;
  host: string | null;
  owner: string | null;
  name: string | null;
  local_path: string | null;
  created_at: number;
}

export type WorkspaceHealth = "healthy" | "missing" | "broken";

export interface WorkspaceStatus {
  current_branch: string | null;
  head_sha: string | null;
  detached: boolean;
  health: WorkspaceHealth;
}

export interface Workspace {
  name: string;
  /** @deprecated Branch stored in SQLite — a stale snapshot from workspace
   *  creation. Use `status.current_branch` for the live value. */
  branch: string;
  path: string;
  is_base: boolean;
  /** True when Shard adopted this worktree (didn't create it). Remove
   *  is untrack-only — the directory is left intact. */
  is_external: boolean;
  created_at: number;
  /** Live-state overlay supplied by the daemon WorkspaceMonitor. `null`
   *  when the monitor has not yet reported a snapshot for this repo. */
  status: WorkspaceStatus | null;
}

export interface BranchInfo {
  name: string;
  is_head: boolean;
  checked_out_by: string | null;
  /** Set when this branch is currently checked out in an externally-managed
   *  worktree (one not tracked by Shard). The path can be passed straight to
   *  `adoptWorkspace` to register it. */
  external_path: string | null;
}

export type WorkspaceMode = "new_branch" | "existing_branch";

/** Mirror of `shard_core::identifiers::safe_workspace_name` so the dialog
 *  can preview the workspace name the daemon will derive from a branch
 *  (e.g. for collision checks against existing workspaces before submit).
 *  Keep in sync with the Rust impl. */
const WINDOWS_RESERVED_NAMES = new Set([
  "CON", "PRN", "AUX", "NUL",
  "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9",
  "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
]);
export function safeWorkspaceName(raw: string): string {
  let out = "";
  let lastDash = false;
  for (const ch of raw) {
    const code = ch.charCodeAt(0);
    const isControl = code < 0x20 || code === 0x7f;
    const safe = isControl || /[/\\<>:"|?*]/.test(ch) ? "-" : ch;
    if (safe === "-") {
      if (lastDash) continue;
      lastDash = true;
    } else {
      lastDash = false;
    }
    out += safe;
  }
  let trimmed = out.replace(/^[-. ]+|[-. ]+$/g, "");
  if (!trimmed) trimmed = "workspace";
  const stem = (trimmed.split(".")[0] || trimmed).toUpperCase();
  if (WINDOWS_RESERVED_NAMES.has(stem)) trimmed = `workspace-${trimmed}`;
  return trimmed;
}

export type Harness = "claude-code" | "codex";

export interface Session {
  id: string;
  workspace_name: string;
  command_json: string;
  transport_addr: string;
  log_path: string;
  supervisor_pid: number | null;
  child_pid: number | null;
  status: string;
  exit_code: number | null;
  created_at: number;
  stopped_at: number | null;
  label: string | null;
  harness: Harness | null;
}

export interface SessionInfo {
  repo: string;
  session: Session;
}

// --- Repo ---

export function listRepos(): Promise<Repository[]> {
  return invoke("list_repos");
}

export function addRepo(url: string, alias?: string): Promise<Repository> {
  return invoke("add_repo", { url, alias: alias ?? null });
}

export function syncRepo(alias: string): Promise<void> {
  return invoke("sync_repo", { alias });
}

export function removeRepo(alias: string): Promise<void> {
  return invoke("remove_repo", { alias });
}

// --- Workspace ---

export function listWorkspaces(repo: string): Promise<Workspace[]> {
  return invoke("list_workspaces", { repo });
}

export function createWorkspace(
  repo: string,
  name: string | undefined,
  mode: WorkspaceMode,
  branch: string | undefined,
): Promise<Workspace> {
  return invoke("create_workspace", {
    repo,
    name: name ?? null,
    mode,
    branch: branch ?? null,
  });
}

/** Adopt an existing external git worktree as a Shard workspace.
 *  `path` must already be registered with git as a worktree of `repo`. */
export function adoptWorkspace(
  repo: string,
  path: string,
  name?: string,
): Promise<Workspace> {
  return invoke("adopt_workspace", {
    repo,
    path,
    name: name ?? null,
  });
}

export function removeWorkspace(repo: string, name: string): Promise<void> {
  return invoke("remove_workspace", { repo, name });
}

export function listRepoBranches(repo: string): Promise<BranchInfo[]> {
  return invoke("list_repo_branches", { repo });
}

// --- Session ---

export function listSessions(
  repo?: string,
  workspace?: string
): Promise<SessionInfo[]> {
  return invoke("list_sessions", {
    repo: repo ?? null,
    workspace: workspace ?? null,
  });
}

export function createSession(
  repo: string,
  workspaceName: string,
  command?: string[],
  harness?: Harness
): Promise<Session> {
  return invoke("create_session", {
    repo,
    workspaceName,
    command: command ?? null,
    harness: harness ?? null,
  });
}

export function stopSession(id: string, force: boolean = false): Promise<void> {
  return invoke("stop_session", { id, force });
}

export function removeSession(id: string): Promise<void> {
  return invoke("remove_session", { id });
}

export function attachSession(
  id: string,
  channel: Channel<ArrayBuffer>
): Promise<void> {
  return invoke("attach_session", { id, channel });
}

export function writeToSession(id: string, data: Uint8Array): Promise<void> {
  return invoke("write_to_session", { id, data: Array.from(data) });
}

export function resizeSession(
  id: string,
  rows: number,
  cols: number
): Promise<void> {
  return invoke("resize_session", { id, rows, cols });
}

export function renameSession(id: string, label: string | null): Promise<void> {
  return invoke("rename_session", { id, label });
}

export function detachSession(id: string): Promise<void> {
  return invoke("detach_session", { id });
}

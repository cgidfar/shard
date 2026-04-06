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

export interface Workspace {
  name: string;
  branch: string;
  path: string;
  created_at: number;
}

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
  name?: string,
  branch?: string
): Promise<Workspace> {
  return invoke("create_workspace", {
    repo,
    name: name ?? null,
    branch: branch ?? null,
  });
}

export function removeWorkspace(repo: string, name: string): Promise<void> {
  return invoke("remove_workspace", { repo, name });
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
  command?: string[]
): Promise<Session> {
  return invoke("create_session", {
    repo,
    workspaceName,
    command: command ?? null,
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
  channel: Channel<Uint8Array>
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

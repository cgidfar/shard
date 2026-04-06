import "@xterm/xterm/css/xterm.css";
import { listen } from "@tauri-apps/api/event";
import { TitleBar } from "./components/TitleBar";
import { Sidebar } from "./components/Sidebar";
import { TerminalPane } from "./components/TerminalPane";
import { AddShardDialog } from "./components/AddShardDialog";
import { addRepo, createSession, createWorkspace, listRepos } from "./lib/api";

const titlebarEl = document.getElementById("titlebar")!;
const sidebarEl = document.getElementById("sidebar")!;
const terminalContainer = document.getElementById("terminal-container")!;

const dialog = new AddShardDialog();

async function openAddShardDialog() {
  const result = await dialog.open();
  if (!result) return;
  try {
    await addRepo(result.url, result.alias);
    await sidebar.refresh();
    await updateEmptyState();
  } catch (err) {
    alert(`Failed to add shard: ${err}`);
  }
}

const terminalPane = new TerminalPane(terminalContainer, {
  onAddShard: openAddShardDialog,
});

const titleBar = new TitleBar(titlebarEl, {
  onAddShard: openAddShardDialog,
  onToggleSidebar() {
    sidebarEl.classList.toggle("collapsed");
  },
});

const sidebar = new Sidebar(sidebarEl, {
  onSessionClick(repo: string, workspace: string, sessionId: string, sessionLabel: string) {
    openSession(repo, workspace, sessionId, sessionLabel);
  },
  onSessionClosed(sessionId: string) {
    terminalPane.close(sessionId);
    if (!terminalPane.getActiveId()) {
      terminalPane.showEmpty();
      titleBar.setBreadcrumb(null);
    }
  },
  async onCreateSession(repo: string, workspace: string) {
    try {
      const session = await createSession(repo, workspace);
      const cmd: string[] = JSON.parse(session.command_json);
      const label = cmd[0]?.split(/[/\\]/).pop() || "session";
      openSession(repo, workspace, session.id, label);
      sidebar.expandWorkspace(repo, workspace);
      sidebar.refresh();
    } catch (err) {
      console.error("Failed to create session:", err);
    }
  },
  async onCreateWorkspace(repo: string) {
    const name = prompt("Workspace name (branch will be created):");
    if (!name || !name.trim()) return;
    try {
      await createWorkspace(repo, name.trim());
      sidebar.refresh();
    } catch (err) {
      alert(`Failed to create workspace: ${err}`);
    }
  },
});

function openSession(repo: string, workspace: string, sessionId: string, sessionLabel: string) {
  terminalPane.open(sessionId);
  sidebar.setActiveSession(sessionId);
  titleBar.setBreadcrumb({
    repo,
    workspace,
    session: sessionLabel,
    status: "running",
  });
}

async function updateEmptyState() {
  const repos = await listRepos();
  terminalPane.setHasShards(repos.length > 0);
}

// Initial load
async function init() {
  await updateEmptyState();
  terminalPane.showEmpty();
  await sidebar.refresh();
}

init();

// Refresh sidebar when backend state changes
listen("sidebar-changed", () => sidebar.refresh());

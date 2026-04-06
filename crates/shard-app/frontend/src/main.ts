import "@xterm/xterm/css/xterm.css";
import { listen } from "@tauri-apps/api/event";
import { TitleBar } from "./components/TitleBar";
import { Sidebar } from "./components/Sidebar";
import { TerminalPane } from "./components/TerminalPane";
import { AddShardDialog } from "./components/AddShardDialog";
import { addRepo, createSession, createWorkspace, listRepos, stopSession, removeSession, removeWorkspace, syncRepo, removeRepo } from "./lib/api";
import { contextMenu, type MenuItemDef } from "./lib/ContextMenu";

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
    closeSession(sessionId);
  },
  async onCreateSession(repo: string, workspace: string) {
    try {
      await doCreateSession(repo, workspace);
    } catch (err) {
      console.error("Failed to create session:", err);
    }
  },
  async onCreateWorkspace(repo: string) {
    try {
      await doCreateWorkspace(repo);
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

function closeSession(sessionId: string) {
  terminalPane.close(sessionId);
  if (!terminalPane.getActiveId()) {
    terminalPane.showEmpty();
    titleBar.setBreadcrumb(null);
  }
}

async function doCreateSession(repo: string, workspace: string) {
  const session = await createSession(repo, workspace);
  const cmd: string[] = JSON.parse(session.command_json);
  const label = cmd[0]?.split(/[/\\]/).pop() || "session";
  openSession(repo, workspace, session.id, label);
  sidebar.expandWorkspace(repo, workspace);
  sidebar.refresh();
}

async function doCreateWorkspace(repo: string) {
  const name = prompt("Workspace name (branch will be created):");
  if (!name || !name.trim()) return;
  await createWorkspace(repo, name.trim());
  sidebar.refresh();
}

async function updateEmptyState() {
  const repos = await listRepos();
  terminalPane.setHasShards(repos.length > 0);
}

// ── Context menus ──

// Session right-click (most specific — register first)
contextMenu.register(".tree-item-session", (el): MenuItemDef[] => {
  const sessionId = el.dataset.sessionId!;
  const status = el.dataset.sessionStatus!;
  const isRunning = status === "running";

  const items: MenuItemDef[] = [];

  if (isRunning) {
    items.push({
      kind: "action",
      label: "Stop Session",
      danger: true,
      handler() {
        closeSession(sessionId);
        stopSession(sessionId)
          .then(() => removeSession(sessionId))
          .catch(() => removeSession(sessionId))
          .catch(() => {})
          .finally(() => sidebar.refresh());
      },
    });
  } else {
    items.push({
      kind: "action",
      label: "Remove Session",
      danger: true,
      handler() {
        closeSession(sessionId);
        removeSession(sessionId).then(() => sidebar.refresh());
      },
    });
  }

  items.push({ kind: "separator" });
  items.push({
    kind: "action",
    label: "Copy Session ID",
    handler() {
      navigator.clipboard.writeText(sessionId);
    },
  });

  return items;
});

// Workspace right-click
contextMenu.register(".tree-group-ws", (el): MenuItemDef[] => {
  const repo = el.dataset.repo!;
  const workspace = el.dataset.workspace!;
  return [
    {
      kind: "action",
      label: "New Session",
      handler() {
        doCreateSession(repo, workspace).catch((err) =>
          console.error("Failed to create session:", err));
      },
    },
    { kind: "separator" },
    {
      kind: "action",
      label: "Remove Workspace",
      danger: true,
      handler() {
        removeWorkspace(repo, workspace)
          .then(() => sidebar.refresh())
          .catch((err) => alert(`Failed to remove workspace: ${err}`));
      },
    },
  ];
});

// Repo right-click
contextMenu.register(".tree-group-repo", (el): MenuItemDef[] => {
  const repo = el.dataset.repo!;
  return [
    {
      kind: "action",
      label: "New Workspace",
      handler() {
        doCreateWorkspace(repo).catch((err) =>
          alert(`Failed to create workspace: ${err}`));
      },
    },
    {
      kind: "action",
      label: "Sync",
      handler() {
        syncRepo(repo).catch((err) => alert(`Failed to sync: ${err}`));
      },
    },
    { kind: "separator" },
    {
      kind: "action",
      label: "Remove Shard",
      danger: true,
      handler() {
        if (confirm(`Remove "${repo}" and all its workspaces?`)) {
          removeRepo(repo)
            .then(() => sidebar.refresh())
            .then(() => updateEmptyState())
            .catch((err) => alert(`Failed to remove: ${err}`));
        }
      },
    },
  ];
});

// Terminal right-click
contextMenu.register("#terminal-container", (): MenuItemDef[] => {
  const activeTerminal = terminalPane.getActiveTerminal();
  if (!activeTerminal) return [];

  const hasSelection = activeTerminal.hasSelection();
  return [
    {
      kind: "action",
      label: "Copy",
      disabled: !hasSelection,
      handler() {
        const text = activeTerminal.getSelection();
        navigator.clipboard.writeText(text);
        activeTerminal.clearSelection();
      },
    },
    {
      kind: "action",
      label: "Paste",
      async handler() {
        const text = await navigator.clipboard.readText();
        if (text) activeTerminal.paste(text);
      },
    },
    { kind: "separator" },
    {
      kind: "action",
      label: "Clear Terminal",
      handler() {
        activeTerminal.clear();
      },
    },
    {
      kind: "action",
      label: "Select All",
      handler() {
        activeTerminal.selectAll();
      },
    },
  ];
});

// Initial load
async function init() {
  await updateEmptyState();
  terminalPane.showEmpty();
  await sidebar.refresh();
}

init();

// Refresh sidebar when backend state changes
listen("sidebar-changed", () => sidebar.refresh());

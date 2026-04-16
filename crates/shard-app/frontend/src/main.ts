import "@xterm/xterm/css/xterm.css";
import { listen } from "@tauri-apps/api/event";
import { TitleBar, type Breadcrumb } from "./components/TitleBar";
import { Sidebar } from "./components/Sidebar";
import { TerminalPane } from "./components/TerminalPane";
import { AddShardDialog } from "./components/AddShardDialog";
import { addRepo, createSession, createWorkspace, listRepos, stopSession, removeSession, removeWorkspace, syncRepo, removeRepo, type WorkspaceStatus } from "./lib/api";
import { contextMenu, type MenuItemDef } from "./lib/ContextMenu";
import { labelFromCommand } from "./lib/titleFormat";
import { activityStore } from "./lib/activityStore";

const titlebarEl = document.getElementById("titlebar")!;
const sidebarEl = document.getElementById("sidebar")!;
const sidebarResizerEl = document.getElementById("sidebar-resizer")!;
const terminalContainer = document.getElementById("terminal-container")!;

// ── Sidebar resize ──
const SIDEBAR_WIDTH_KEY = "shard.sidebarWidth";
const SIDEBAR_MIN_WIDTH = 180;
const SIDEBAR_MAX_WIDTH = 500;

const savedWidth = Number(localStorage.getItem(SIDEBAR_WIDTH_KEY));
if (Number.isFinite(savedWidth) && savedWidth >= SIDEBAR_MIN_WIDTH && savedWidth <= SIDEBAR_MAX_WIDTH) {
  sidebarEl.style.width = `${savedWidth}px`;
}

sidebarResizerEl.addEventListener("mousedown", (e) => {
  e.preventDefault();
  const startX = e.clientX;
  const startWidth = sidebarEl.getBoundingClientRect().width;
  sidebarEl.classList.add("resizing");
  sidebarResizerEl.classList.add("resizing");
  document.body.style.cursor = "col-resize";
  document.body.style.userSelect = "none";

  const onMove = (ev: MouseEvent) => {
    const next = Math.max(
      SIDEBAR_MIN_WIDTH,
      Math.min(SIDEBAR_MAX_WIDTH, startWidth + (ev.clientX - startX)),
    );
    sidebarEl.style.width = `${next}px`;
  };

  const onUp = () => {
    window.removeEventListener("mousemove", onMove);
    window.removeEventListener("mouseup", onUp);
    sidebarEl.classList.remove("resizing");
    sidebarResizerEl.classList.remove("resizing");
    document.body.style.cursor = "";
    document.body.style.userSelect = "";
    localStorage.setItem(SIDEBAR_WIDTH_KEY, String(sidebarEl.getBoundingClientRect().width));
  };

  window.addEventListener("mousemove", onMove);
  window.addEventListener("mouseup", onUp);
});

const dialog = new AddShardDialog();

let currentBreadcrumb: Breadcrumb | null = null;

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

// Wire OSC title changes from terminal to sidebar + breadcrumb
terminalPane.onTitleChange = (sessionId, title) => {
  sidebar.notifyTitleChange(sessionId, title);
  if (sessionId === terminalPane.getActiveId() && currentBreadcrumb) {
    // Only update breadcrumb if session has no user-set label
    const resolvedLabel = sidebar.resolveLabel(sessionId);
    currentBreadcrumb = { ...currentBreadcrumb, session: resolvedLabel };
    titleBar.setBreadcrumb(currentBreadcrumb);
  }
};

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
  onLabelChanged(sessionId: string, label: string) {
    if (sessionId === terminalPane.getActiveId() && currentBreadcrumb) {
      currentBreadcrumb = { ...currentBreadcrumb, session: label };
      titleBar.setBreadcrumb(currentBreadcrumb);
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
  currentBreadcrumb = {
    repo,
    workspace,
    session: sessionLabel,
    status: "running",
  };
  titleBar.setBreadcrumb(currentBreadcrumb);
}

function closeSession(sessionId: string) {
  activityStore.remove(sessionId);
  terminalPane.close(sessionId);
  if (!terminalPane.getActiveId()) {
    terminalPane.showEmpty();
    currentBreadcrumb = null;
    titleBar.setBreadcrumb(null);
  }
}

async function doCreateSession(repo: string, workspace: string) {
  const session = await createSession(repo, workspace);
  const label = labelFromCommand(session.command_json);
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

  const items: MenuItemDef[] = [
    {
      kind: "action",
      label: "Rename",
      handler() {
        sidebar.startRename(sessionId);
      },
    },
    { kind: "separator" },
  ];

  if (isRunning) {
    items.push({
      kind: "action",
      label: "Stop Session",
      danger: true,
      handler() {
        sidebar.beginStopSession(sessionId);
        closeSession(sessionId);
        stopSession(sessionId)
          .then(() => removeSession(sessionId))
          .catch(() => removeSession(sessionId))
          .catch(() => {})
          .finally(() => {
            sidebar.endStopSession(sessionId);
            sidebar.refresh();
          });
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
  const unhealthy = sidebar.isWorkspaceUnhealthy(repo, workspace);

  const items: MenuItemDef[] = [];
  if (!unhealthy) {
    items.push({
      kind: "action",
      label: "New Session",
      handler() {
        doCreateSession(repo, workspace).catch((err) =>
          console.error("Failed to create session:", err));
      },
    });
    items.push({ kind: "separator" });
  }
  items.push({
    kind: "action",
    label: "Remove Workspace",
    danger: true,
    handler() {
      removeWorkspace(repo, workspace)
        .then(() => sidebar.refresh())
        .catch((err) => alert(`Failed to remove workspace: ${err}`));
    },
  });
  return items;
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

// Refresh sidebar when backend structural state changes (add/remove)
listen("sidebar-changed", () => sidebar.refresh());

// Targeted workspace-status patch: the daemon WorkspaceMonitor has observed
// external git activity (branch flip, worktree deletion) or completed a
// reconcile pass. Apply a single-row update instead of a full refresh so
// frequent branch flips during rebases don't repaint the whole tree.
listen<{ repo: string; workspace: string; status: WorkspaceStatus | null }>(
  "workspace-status-changed",
  ({ payload }) => {
    sidebar.patchWorkspaceStatus(payload.repo, payload.workspace, payload.status);
  }
);

// Relay activity state from supervisor to the store
listen<{ id: string; state: "active" | "idle" | "blocked" }>("session-activity", ({ payload }) => {
  const isFocused = payload.id === terminalPane.getActiveId();
  activityStore.notify(payload.id, payload.state, isFocused);
});

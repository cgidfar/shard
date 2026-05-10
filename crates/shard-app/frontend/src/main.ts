import "@xterm/xterm/css/xterm.css";
import { listen } from "@tauri-apps/api/event";
import { TitleBar, type Breadcrumb } from "./components/TitleBar";
import { Sidebar } from "./components/Sidebar";
import { TerminalPane } from "./components/TerminalPane";
import { AddShardDialog } from "./components/AddShardDialog";
import { AddWorkspaceDialog } from "./components/AddWorkspaceDialog";
import { addRepo, adoptWorkspace, createSession, createWorkspace, focusSessionWindow, listRepos, listSessionInputOwners, openNewWindow, stopSession, removeSession, removeWorkspace, syncRepo, removeRepo, type WorkspaceStatus } from "./lib/api";
import { contextMenu, type MenuItemDef } from "./lib/ContextMenu";
import { labelFromCommand } from "./lib/titleFormat";
import { activityStore } from "./lib/activityStore";
import { windowState } from "./lib/windowState";
import type { SessionInputStateEvent } from "./lib/terminal";

const titlebarEl = document.getElementById("titlebar")!;
const sidebarEl = document.getElementById("sidebar")!;
const sidebarResizerEl = document.getElementById("sidebar-resizer")!;
const terminalContainer = document.getElementById("terminal-container")!;

// ── Sidebar resize ──
// Per-window storage so two open windows don't fight over the same key.
// Resolved lazily in `init()` once `windowState` has its label; until then
// the saved width is just the unscoped legacy default if anything.
let sidebarWidthKey = "shard.sidebarWidth";
const SIDEBAR_MIN_WIDTH = 180;
const SIDEBAR_MAX_WIDTH = 500;

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
    localStorage.setItem(sidebarWidthKey, String(sidebarEl.getBoundingClientRect().width));
  };

  window.addEventListener("mousemove", onMove);
  window.addEventListener("mouseup", onUp);
});

const dialog = new AddShardDialog();
const workspaceDialog = new AddWorkspaceDialog();

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
  onOwnershipConflict(sessionId: string, ownerLabel: string) {
    focusSessionWindow(ownerLabel, sessionId).catch((err) =>
      console.warn(`focus_session_window(${ownerLabel}, ${sessionId}) failed:`, err),
    );
  },
  onInputStateChange(state: SessionInputStateEvent) {
    applySidebarOwnership(state);
  },
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
  onToggleSidebar() {
    sidebarEl.classList.toggle("collapsed");
  },
  onNewWindow() {
    openNewWindow().catch((err) =>
      console.warn("open_new_window failed:", err),
    );
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
  onRemoveWorkspace(repo: string, workspace: string, sessionIds: string[]) {
    doRemoveWorkspace(repo, workspace, sessionIds);
  },
  onAddShard: openAddShardDialog,
  onSessionFocusElsewhere(sessionId: string, ownerLabel: string) {
    focusSessionWindow(ownerLabel, sessionId).catch((err) =>
      console.warn(`focus_session_window(${ownerLabel}, ${sessionId}) failed:`, err),
    );
  },
});

/**
 * Reflect the current input owner into the sidebar's "owned elsewhere"
 * overlay. The supervisor broadcasts `InputOwnerChanged` to every
 * connected client (every window's monitor + any window's attach reader),
 * so each window receives the same `session-input-state` events and
 * computes its own UI state independently.
 */
function applySidebarOwnership(state: SessionInputStateEvent) {
  if (!windowState.ready) return;
  // GUI owner with a label that isn't us → mark elsewhere.
  if (
    state.owner_kind === "gui" &&
    state.owner_label &&
    state.owner_label !== windowState.label
  ) {
    sidebar.setOwnedElsewhere(state.id, state.owner_label);
    return;
  }
  // Owner is a CLI agent or no owner — clear (CLI takeover greys out the
  // local terminal via `.input-disabled`, but we don't show an
  // "elsewhere" sidebar overlay for it; the affordance is "click to
  // focus another window," which doesn't apply to CLI processes).
  sidebar.setOwnedElsewhere(state.id, null);
}

async function hydrateSidebarOwnership() {
  const states = await listSessionInputOwners();
  for (const state of states) applySidebarOwnership(state);
}

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

function focusLocalSession(sessionId: string) {
  if (!terminalPane.show(sessionId)) return;

  const location = sidebar.getSessionLocation(sessionId);
  if (location) {
    sidebar.setActiveSession(sessionId);
    currentBreadcrumb = {
      repo: location.repo,
      workspace: location.workspace,
      session: location.label,
      status: location.status,
    };
    titleBar.setBreadcrumb(currentBreadcrumb);
  }
}

function closeSession(sessionId: string) {
  const wasActive = terminalPane.getActiveId() === sessionId;
  activityStore.remove(sessionId);
  terminalPane.close(sessionId);
  if (wasActive) {
    sidebar.setActiveSession(null);
  }
  if (!terminalPane.getActiveId()) {
    terminalPane.showEmpty();
    currentBreadcrumb = null;
    titleBar.setBreadcrumb(null);
  }
}

async function doRemoveWorkspace(
  repo: string,
  workspace: string,
  sessionIds = sidebar.getSessionIdsForWorkspace(repo, workspace),
) {
  for (const sessionId of new Set(sessionIds)) {
    closeSession(sessionId);
  }

  try {
    await removeWorkspace(repo, workspace);
    await sidebar.refresh();
    await updateEmptyState();
  } catch (err) {
    await sidebar.refresh();
    alert(`Failed to remove workspace: ${err}`);
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
  const result = await workspaceDialog.open(repo);
  if (!result) return;
  try {
    if (result.adoptPath) {
      // Branch was checked out in an externally-managed worktree. Tell
      // the daemon to track it without creating a new worktree. Don't
      // pass `result.name` — for existing-branch mode it's the raw branch
      // name (may contain `/`), and the daemon derives a safe workspace
      // name from the branch when `name` is omitted.
      await adoptWorkspace(repo, result.adoptPath);
    } else {
      await createWorkspace(repo, result.name, result.mode, result.branch);
    }
    sidebar.refresh();
  } catch (err) {
    alert(`Failed to create workspace: ${err}`);
  }
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
      doRemoveWorkspace(repo, workspace);
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
  // Resolve our window label first — every backend call routes through
  // this. Once the label is known, swap in the per-window sidebar-width
  // key and apply the saved value (if any).
  await windowState.init();
  sidebarWidthKey = `shard.sidebarWidth.${windowState.label}`;
  const savedWidth = Number(localStorage.getItem(sidebarWidthKey));
  if (
    Number.isFinite(savedWidth) &&
    savedWidth >= SIDEBAR_MIN_WIDTH &&
    savedWidth <= SIDEBAR_MAX_WIDTH
  ) {
    sidebarEl.style.width = `${savedWidth}px`;
  }

  await updateEmptyState();
  terminalPane.showEmpty();
  await sidebar.refresh();
  await hydrateSidebarOwnership();
}

init();

// Refresh sidebar when backend structural state changes (add/remove)
listen("sidebar-changed", () => sidebar.refresh());

listen<{ id: string; status: string; code: number }>("terminal-ended", ({ payload }) => {
  activityStore.remove(payload.id);
  if (payload.id === terminalPane.getActiveId() && currentBreadcrumb) {
    currentBreadcrumb = { ...currentBreadcrumb, status: payload.status };
    titleBar.setBreadcrumb(currentBreadcrumb);
  }
  sidebar.refresh();
});

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

// Global ownership listener. The terminal pane's local listener handles
// ownership for sessions this window has open; this one updates sidebar
// state for sessions this window is only monitoring (e.g. when window-2
// attaches a session that window-1 has visible in its sidebar but isn't
// terminal-mounted, window-1 still needs to grey it out).
listen<SessionInputStateEvent>("session-input-state", ({ payload }) => {
  applySidebarOwnership(payload);
});

listen<{ id: string }>("focus-session", ({ payload }) => {
  focusLocalSession(payload.id);
});

// Ctrl+N opens a new window. Document-level so it fires regardless of
// which child element has focus (xterm.js would otherwise swallow it).
document.addEventListener("keydown", (e) => {
  if (e.ctrlKey && !e.shiftKey && !e.altKey && (e.key === "n" || e.key === "N")) {
    e.preventDefault();
    openNewWindow().catch((err) =>
      console.warn("Ctrl+N open_new_window failed:", err),
    );
  }
});

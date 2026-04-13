import {
  listRepos,
  listWorkspaces,
  listSessions,
  removeSession,
  renameSession,
  stopSession,
  type Repository,
  type Workspace,
  type SessionInfo,
} from "../lib/api";
import { labelFromCommand } from "../lib/titleFormat";
import { activityStore, type DisplayState } from "../lib/activityStore";
import { createStatusIndicator } from "../lib/statusIndicator";

const ICON_FOLDER =
  `<svg class="tree-icon" width="13" height="13" viewBox="0 0 14 14" fill="none" aria-hidden="true"><path d="M1.6 4.2 L1.6 10.5 Q1.6 11 2.1 11 L11.9 11 Q12.4 11 12.4 10.5 L12.4 5.9 Q12.4 5.4 11.9 5.4 L6.6 5.4 L5.1 3.9 L2.1 3.9 Q1.6 3.9 1.6 4.4 Z" stroke="currentColor" stroke-width="1" stroke-linejoin="round"/></svg>`;

const ICON_HOME =
  `<svg class="tree-icon" width="13" height="13" viewBox="0 0 14 14" fill="none" aria-hidden="true"><path d="M2 7 L7 2.5 L12 7 L12 11.5 Q12 12 11.5 12 L8.5 12 L8.5 9 L5.5 9 L5.5 12 L2.5 12 Q2 12 2 11.5 Z" stroke="currentColor" stroke-width="1" stroke-linejoin="round"/></svg>`;

const ICON_BRANCH =
  `<svg class="tree-icon" width="13" height="13" viewBox="0 0 14 14" fill="none" aria-hidden="true"><circle cx="4" cy="3.2" r="1.4" stroke="currentColor" stroke-width="1" fill="none"/><circle cx="4" cy="10.8" r="1.4" stroke="currentColor" stroke-width="1" fill="none"/><circle cx="10" cy="7" r="1.4" stroke="currentColor" stroke-width="1" fill="none"/><path d="M4 4.6 L4 9.4" stroke="currentColor" stroke-width="1"/><path d="M4 7 L8.6 7" stroke="currentColor" stroke-width="1"/></svg>`;

export interface SidebarCallbacks {
  onSessionClick: (repo: string, workspace: string, sessionId: string, sessionLabel: string) => void;
  onSessionClosed: (sessionId: string) => void;
  onCreateSession: (repo: string, workspace: string) => void;
  onCreateWorkspace: (repo: string) => void;
  onLabelChanged?: (sessionId: string, label: string) => void;
}

interface RepoTree {
  repo: Repository;
  workspaces: {
    workspace: Workspace;
    sessions: SessionInfo[];
  }[];
}

export class Sidebar {
  private el: HTMLElement;
  private callbacks: SidebarCallbacks;
  private tree: RepoTree[] = [];
  private activeSessionId: string | null = null;
  private confirmingStopId: string | null = null;
  private pendingStopIds: Set<string> = new Set();
  private expandState: Map<string, boolean> = new Map();
  private refreshing = false;
  private renamingSessionId: string | null = null;
  private draftLabel: string = "";
  private dynamicTitles: Map<string, string> = new Map();
  private pendingRefresh = false;

  constructor(el: HTMLElement, callbacks: SidebarCallbacks) {
    this.el = el;
    this.callbacks = callbacks;
    activityStore.onChange((id, state) => this.updateActivityIndicator(id, state));
  }

  expandWorkspace(repo: string, workspace: string) {
    this.expandState.set(`repo:${repo}`, true);
    this.expandState.set(`ws:${repo}:${workspace}`, true);
  }

  setActiveSession(sessionId: string | null) {
    this.activeSessionId = sessionId;
    if (sessionId) activityStore.clearAttention(sessionId);
    this.render();
  }

  /** Hide a session row immediately (e.g. while a stop op is in flight). */
  beginStopSession(sessionId: string) {
    this.pendingStopIds.add(sessionId);
    this.render();
  }

  /** Clear the hidden state. If the stop failed and the session still exists,
   *  the next refresh will surface it again. */
  endStopSession(sessionId: string) {
    this.pendingStopIds.delete(sessionId);
  }

  /** Resolve the display label for a session.
   *  Priority: user label > dynamic OSC title > command-based fallback. */
  resolveLabel(sessionId: string): string {
    const si = this.findSessionInfo(sessionId);
    if (!si) return "session";
    return this.resolveLabelForSession(si);
  }

  /** Notify that a terminal's OSC title has changed. */
  notifyTitleChange(sessionId: string, title: string) {
    this.dynamicTitles.set(sessionId, title);
    // Targeted DOM update — avoid full re-render for frequent title changes
    const si = this.findSessionInfo(sessionId);
    if (si && si.session.label == null) {
      const row = this.el.querySelector(
        `[data-session-id="${sessionId}"] .tree-label`
      ) as HTMLElement | null;
      if (row && this.renamingSessionId !== sessionId) {
        row.textContent = title;
      }
    }
  }

  /** Enter inline rename mode for a session. */
  startRename(sessionId: string) {
    const si = this.findSessionInfo(sessionId);
    if (!si) return;
    this.renamingSessionId = sessionId;
    this.draftLabel = this.resolveLabelForSession(si);
    this.render();
  }

  async refresh() {
    if (this.refreshing) return;
    // Queue refresh if rename is active — flush after commit/cancel
    if (this.renamingSessionId !== null) {
      this.pendingRefresh = true;
      return;
    }
    this.refreshing = true;
    try {
      const repos = await listRepos();
      const tree: RepoTree[] = [];

      for (const repo of repos) {
        const workspaces = await listWorkspaces(repo.alias);
        const sessions = await listSessions(repo.alias);

        const wsTree = workspaces.map((ws) => ({
          workspace: ws,
          sessions: sessions.filter(
            (s) => s.session.workspace_name === ws.name
          ),
        }));

        tree.push({ repo, workspaces: wsTree });
      }

      this.tree = tree;
      this.render();
    } finally {
      this.refreshing = false;
    }
  }

  private resolveLabelForSession(si: SessionInfo): string {
    if (si.session.label != null && si.session.label.trim() !== "") {
      return si.session.label;
    }
    const dynamic = this.dynamicTitles.get(si.session.id);
    if (dynamic) return dynamic;
    return labelFromCommand(si.session.command_json);
  }

  private findSessionInfo(sessionId: string): SessionInfo | undefined {
    for (const { workspaces } of this.tree) {
      for (const { sessions } of workspaces) {
        const found = sessions.find((s) => s.session.id === sessionId);
        if (found) return found;
      }
    }
    return undefined;
  }

  private async commitRename() {
    const id = this.renamingSessionId;
    const label = this.draftLabel.trim();
    this.renamingSessionId = null;
    this.draftLabel = "";
    if (id) {
      await renameSession(id, label || null).catch(() => {});
    }
    // Flush any queued refresh, otherwise just re-render from cached data
    if (this.pendingRefresh) {
      this.pendingRefresh = false;
      await this.refresh();
    } else {
      // refresh() to pick up the new label from DB
      await this.refresh();
    }

    // Notify label changed so breadcrumb can update
    if (id && this.callbacks.onLabelChanged) {
      this.callbacks.onLabelChanged(id, this.resolveLabel(id));
    }
  }

  private cancelRename() {
    this.renamingSessionId = null;
    this.draftLabel = "";
    if (this.pendingRefresh) {
      this.pendingRefresh = false;
      this.refresh();
    } else {
      this.render();
    }
  }

  /** Targeted DOM update for activity state changes — avoids full re-render. */
  private updateActivityIndicator(sessionId: string, state: DisplayState) {
    const indicator = this.el.querySelector(
      `[data-session-id="${sessionId}"] .status-indicator`
    ) as HTMLElement | null;
    if (indicator) {
      indicator.dataset.activityState = state;
    }
  }

  private findActiveWorkspace(): { repo: string; workspace: string } | null {
    if (!this.activeSessionId) return null;
    for (const { repo, workspaces } of this.tree) {
      for (const { workspace, sessions } of workspaces) {
        if (sessions.some((s) => s.session.id === this.activeSessionId)) {
          return { repo: repo.alias, workspace: workspace.name };
        }
      }
    }
    return null;
  }

  private render() {
    this.el.innerHTML = "";

    if (this.tree.length === 0) {
      const empty = document.createElement("div");
      empty.className = "sidebar-empty";
      empty.innerHTML = `
        <span class="sidebar-empty-title">No shards yet</span>
        <span class="sidebar-empty-hint">Add a local folder or clone a remote repo to get started</span>
      `;
      this.el.appendChild(empty);
      return;
    }

    const activeWs = this.findActiveWorkspace();

    for (let ri = 0; ri < this.tree.length; ri++) {
      const { repo, workspaces } = this.tree[ri];
      const repoKey = `repo:${repo.alias}`;
      if (!this.expandState.has(repoKey)) this.expandState.set(repoKey, true);

      // Repo row
      const repoGroup = document.createElement("div");
      repoGroup.className = "tree-group tree-group-repo";
      if (ri > 0) repoGroup.classList.add("repo-group-spaced");
      repoGroup.dataset.repo = repo.alias;
      repoGroup.title = repo.local_path || repo.url;
      repoGroup.insertAdjacentHTML("beforeend", ICON_FOLDER);

      const repoLabelWrap = document.createElement("span");
      repoLabelWrap.className = "tree-label-wrap";

      const repoLabel = document.createElement("span");
      repoLabel.className = "tree-label";
      repoLabel.textContent = repo.alias;
      repoLabelWrap.appendChild(repoLabel);

      const repoArrow = document.createElement("span");
      repoArrow.className = "tree-arrow";
      repoArrow.textContent = this.expandState.get(repoKey) ? "▼" : "▶";
      repoLabelWrap.appendChild(repoArrow);

      repoGroup.appendChild(repoLabelWrap);

      const addWsBtn = document.createElement("button");
      addWsBtn.className = "tree-action";
      addWsBtn.textContent = "+";
      addWsBtn.title = "New workspace";
      addWsBtn.addEventListener("click", (e) => {
        e.stopPropagation();
        this.callbacks.onCreateWorkspace(repo.alias);
      });
      repoGroup.appendChild(addWsBtn);

      const repoChildren = document.createElement("div");
      repoChildren.className = this.expandState.get(repoKey) ? "tree-children open" : "tree-children";

      repoGroup.addEventListener("click", (e) => {
        if ((e.target as HTMLElement).closest(".tree-action")) return;
        const open = !this.expandState.get(repoKey);
        this.expandState.set(repoKey, open);
        repoChildren.className = open ? "tree-children open" : "tree-children";
        repoArrow.textContent = open ? "▼" : "▶";
      });

      this.el.appendChild(repoGroup);

      // Sort workspaces: base first, then alphabetical
      const sortedWs = [...workspaces].sort((a, b) => {
        if (a.workspace.is_base !== b.workspace.is_base) return a.workspace.is_base ? -1 : 1;
        return a.workspace.name.localeCompare(b.workspace.name);
      });

      for (const { workspace, sessions } of sortedWs) {
        const wsKey = `ws:${repo.alias}:${workspace.name}`;
        if (!this.expandState.has(wsKey)) this.expandState.set(wsKey, sessions.length > 0);

        const isActiveWs =
          activeWs?.repo === repo.alias && activeWs?.workspace === workspace.name;

        const wsGroup = document.createElement("div");
        wsGroup.className = "tree-group tree-group-ws";
        if (isActiveWs) wsGroup.classList.add("active-ws");
        wsGroup.dataset.repo = repo.alias;
        wsGroup.dataset.workspace = workspace.name;
        wsGroup.title = workspace.path;
        wsGroup.insertAdjacentHTML("beforeend", workspace.is_base ? ICON_HOME : ICON_BRANCH);

        const wsLabelWrap = document.createElement("span");
        wsLabelWrap.className = "tree-label-wrap";

        const wsLabel = document.createElement("span");
        wsLabel.className = "tree-label";
        wsLabel.textContent = workspace.name;
        wsLabelWrap.appendChild(wsLabel);

        const wsArrow = document.createElement("span");
        wsArrow.className = "tree-arrow";
        wsArrow.textContent = this.expandState.get(wsKey) ? "▼" : "▶";
        if (sessions.length > 0) wsLabelWrap.appendChild(wsArrow);

        wsGroup.appendChild(wsLabelWrap);

        const addSessionBtn = document.createElement("button");
        addSessionBtn.className = "tree-action";
        addSessionBtn.textContent = "+";
        addSessionBtn.title = "New session";
        addSessionBtn.addEventListener("click", (e) => {
          e.stopPropagation();
          this.callbacks.onCreateSession(repo.alias, workspace.name);
        });
        wsGroup.appendChild(addSessionBtn);

        const wsChildren = document.createElement("div");
        wsChildren.className = this.expandState.get(wsKey) ? "tree-children open" : "tree-children";

        wsGroup.addEventListener("click", (e) => {
          if ((e.target as HTMLElement).closest(".tree-action")) return;
          const open = !this.expandState.get(wsKey);
          this.expandState.set(wsKey, open);
          wsChildren.className = open ? "tree-children open" : "tree-children";
          wsArrow.textContent = open ? "▼" : "▶";
        });

        repoChildren.appendChild(wsGroup);

        // Sessions
        for (const si of sessions) {
          if (this.pendingStopIds.has(si.session.id)) continue;
          const isRunning = si.session.status === "running";
          const isDead = ["failed", "exited", "stopped"].includes(si.session.status);
          const isActive = si.session.id === this.activeSessionId;
          const isConfirming = si.session.id === this.confirmingStopId;
          const isRenaming = si.session.id === this.renamingSessionId;
          const resolvedLabel = this.resolveLabelForSession(si);

          const sessionRow = document.createElement("div");
          sessionRow.className = `tree-item tree-item-session${isActive ? " active" : ""}${isDead ? " dead" : ""}`;
          sessionRow.dataset.repo = si.repo;
          sessionRow.dataset.workspace = workspace.name;
          sessionRow.dataset.sessionId = si.session.id;
          sessionRow.dataset.sessionStatus = si.session.status;
          sessionRow.dataset.sessionLabel = resolvedLabel;

          if (isConfirming) {
            // Inline stop confirmation
            sessionRow.className = "tree-item tree-item-session confirming";

            const label = document.createElement("span");
            label.className = "tree-label";
            label.textContent = "Stop session?";

            const stopBtn = document.createElement("button");
            stopBtn.className = "confirm-stop";
            stopBtn.textContent = "Stop";
            stopBtn.addEventListener("click", (e) => {
              e.stopPropagation();
              this.confirmingStopId = null;
              this.pendingStopIds.add(si.session.id);
              this.callbacks.onSessionClosed(si.session.id);
              this.render();
              stopSession(si.session.id)
                .then(() => removeSession(si.session.id))
                .catch(() => removeSession(si.session.id))
                .catch(() => {}) // remove might fail too, that's ok
                .finally(() => {
                  this.pendingStopIds.delete(si.session.id);
                  this.refresh();
                });
            });

            const cancelBtn = document.createElement("button");
            cancelBtn.className = "confirm-cancel";
            cancelBtn.textContent = "No";
            cancelBtn.addEventListener("click", (e) => {
              e.stopPropagation();
              this.confirmingStopId = null;
              this.render();
            });

            sessionRow.appendChild(label);
            sessionRow.appendChild(stopBtn);
            sessionRow.appendChild(cancelBtn);
          } else if (isRenaming) {
            // Inline rename
            sessionRow.className = `tree-item tree-item-session${isActive ? " active" : ""} renaming`;
            const input = document.createElement("input");
            input.className = "rename-input";
            input.type = "text";
            input.value = this.draftLabel;
            let committed = false;
            input.addEventListener("input", () => {
              this.draftLabel = input.value;
            });
            input.addEventListener("keydown", (e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                committed = true;
                this.commitRename();
              } else if (e.key === "Escape") {
                e.preventDefault();
                committed = true;
                this.cancelRename();
              }
              e.stopPropagation();
            });
            input.addEventListener("blur", () => {
              if (!committed && this.renamingSessionId === si.session.id) this.cancelRename();
            });
            sessionRow.appendChild(input);
            requestAnimationFrame(() => {
              input.focus();
              input.select();
            });
          } else {
            // Only show activity state for running sessions — dead sessions
            // may have stale entries in the store
            const activity = isRunning ? activityStore.get(si.session.id) : undefined;
            sessionRow.appendChild(createStatusIndicator(si.session.status, activity));

            const label = document.createElement("span");
            label.className = "tree-label";
            label.textContent = resolvedLabel;
            label.addEventListener("dblclick", (e) => {
              e.stopPropagation();
              this.startRename(si.session.id);
            });
            sessionRow.appendChild(label);

            const closeBtn = document.createElement("button");
            closeBtn.className = "tree-action tree-action-close";
            closeBtn.textContent = "×";

            sessionRow.style.cursor = "pointer";
            sessionRow.addEventListener("click", (e) => {
              if ((e.target as HTMLElement).closest(".tree-action")) return;
              this.callbacks.onSessionClick(si.repo, workspace.name, si.session.id, resolvedLabel);
            });

            if (isRunning) {
              closeBtn.title = "Stop session";
              closeBtn.addEventListener("click", (e) => {
                e.stopPropagation();
                this.confirmingStopId = si.session.id;
                this.render();
              });
            } else {
              closeBtn.title = "Remove session";
              closeBtn.addEventListener("click", (e) => {
                e.stopPropagation();
                removeSession(si.session.id).then(() => this.refresh());
              });
            }

            sessionRow.appendChild(closeBtn);
          }

          wsChildren.appendChild(sessionRow);
        }

        repoChildren.appendChild(wsChildren);
      }

      this.el.appendChild(repoChildren);
    }
  }
}

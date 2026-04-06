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
  private stoppingId: string | null = null;
  private expandState: Map<string, boolean> = new Map();
  private refreshing = false;
  private renamingSessionId: string | null = null;
  private draftLabel: string = "";
  private dynamicTitles: Map<string, string> = new Map();
  private pendingRefresh = false;

  constructor(el: HTMLElement, callbacks: SidebarCallbacks) {
    this.el = el;
    this.callbacks = callbacks;
  }

  expandWorkspace(repo: string, workspace: string) {
    this.expandState.set(`repo:${repo}`, true);
    this.expandState.set(`ws:${repo}:${workspace}`, true);
  }

  setActiveSession(sessionId: string | null) {
    this.activeSessionId = sessionId;
    this.render();
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

    for (let ri = 0; ri < this.tree.length; ri++) {
      const { repo, workspaces } = this.tree[ri];

      if (ri > 0) {
        const divider = document.createElement("div");
        divider.className = "sidebar-divider";
        this.el.appendChild(divider);
      }

      // Repo group
      const repoGroup = document.createElement("div");
      repoGroup.className = "tree-group tree-group-repo";
      repoGroup.dataset.repo = repo.alias;
      const repoKey = `repo:${repo.alias}`;
      if (!this.expandState.has(repoKey)) this.expandState.set(repoKey, true);

      const repoArrow = document.createElement("span");
      repoArrow.className = "tree-arrow";
      repoArrow.textContent = this.expandState.get(repoKey) ? "▼" : "▶";

      const repoLabel = document.createElement("span");
      repoLabel.className = "tree-label";
      repoLabel.textContent = repo.alias;

      const addWsBtn = document.createElement("button");
      addWsBtn.className = "tree-action";
      addWsBtn.textContent = "+";
      addWsBtn.title = "New workspace";
      addWsBtn.addEventListener("click", (e) => {
        e.stopPropagation();
        this.callbacks.onCreateWorkspace(repo.alias);
      });

      repoGroup.appendChild(repoArrow);
      repoGroup.appendChild(repoLabel);
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

      for (const { workspace, sessions } of workspaces) {
        // Workspace row
        const wsGroup = document.createElement("div");
        wsGroup.className = "tree-group tree-group-ws";
        wsGroup.dataset.repo = repo.alias;
        wsGroup.dataset.workspace = workspace.name;
        const wsKey = `ws:${repo.alias}:${workspace.name}`;
        if (!this.expandState.has(wsKey)) this.expandState.set(wsKey, sessions.length > 0);

        const wsArrow = document.createElement("span");
        wsArrow.className = "tree-arrow";
        wsArrow.textContent = this.expandState.get(wsKey) ? "▼" : "▶";

        // Default workspace indicator
        const isBase = workspace.name === "main" || workspace.name === "master";
        if (isBase) {
          const pin = document.createElement("span");
          pin.className = "ws-pin";
          pin.innerHTML = `<svg width="10" height="10" viewBox="0 0 10 10"><circle cx="5" cy="5" r="2" fill="none" stroke="currentColor" stroke-width="1.2"/><circle cx="5" cy="5" r="0.8" fill="currentColor"/></svg>`;
          wsGroup.appendChild(wsArrow);
          wsGroup.appendChild(pin);
        } else {
          wsGroup.appendChild(wsArrow);
        }

        const wsLabel = document.createElement("span");
        wsLabel.className = "tree-label";
        wsLabel.textContent = workspace.name;

        const addSessionBtn = document.createElement("button");
        addSessionBtn.className = "tree-action";
        addSessionBtn.textContent = "+";
        addSessionBtn.title = "New session";
        addSessionBtn.addEventListener("click", (e) => {
          e.stopPropagation();
          this.callbacks.onCreateSession(repo.alias, workspace.name);
        });

        wsGroup.appendChild(wsLabel);
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

          if (si.session.id === this.stoppingId) {
            // Stopping in progress — show feedback
            sessionRow.className = "tree-item tree-item-session confirming";
            const label = document.createElement("span");
            label.className = "tree-label";
            label.textContent = "Stopping...";
            sessionRow.appendChild(label);
          } else if (isConfirming) {
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
              this.stoppingId = si.session.id;
              this.callbacks.onSessionClosed(si.session.id);
              this.render();
              stopSession(si.session.id)
                .then(() => removeSession(si.session.id))
                .catch(() => removeSession(si.session.id))
                .catch(() => {}) // remove might fail too, that's ok
                .finally(() => {
                  this.stoppingId = null;
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
            const dot = document.createElement("span");
            dot.className = `status-dot ${si.session.status}`;
            sessionRow.appendChild(dot);

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

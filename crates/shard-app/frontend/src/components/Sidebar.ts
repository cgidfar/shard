import {
  listRepos,
  listWorkspaces,
  listSessions,
  removeSession,
  stopSession,
  type Repository,
  type Workspace,
  type SessionInfo,
} from "../lib/api";

export interface SidebarCallbacks {
  onSessionClick: (repo: string, sessionId: string, sessionLabel: string) => void;
  onCreateSession: (repo: string, workspace: string) => void;
  onCreateWorkspace: (repo: string) => void;
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

  constructor(el: HTMLElement, callbacks: SidebarCallbacks) {
    this.el = el;
    this.callbacks = callbacks;
  }

  setActiveSession(sessionId: string | null) {
    this.activeSessionId = sessionId;
    this.render();
  }

  async refresh() {
    const repos = await listRepos();
    this.tree = [];

    for (const repo of repos) {
      const workspaces = await listWorkspaces(repo.alias);
      const sessions = await listSessions(repo.alias);

      const wsTree = workspaces.map((ws) => ({
        workspace: ws,
        sessions: sessions.filter(
          (s) => s.session.workspace_name === ws.name
        ),
      }));

      this.tree.push({ repo, workspaces: wsTree });
    }

    this.render();
  }

  private deriveSessionLabel(si: SessionInfo): string {
    try {
      const cmd: string[] = JSON.parse(si.session.command_json);
      const exe = cmd[0]?.split(/[/\\]/).pop() || "session";
      return exe;
    } catch {
      return "session";
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
      let repoOpen = true;

      const repoArrow = document.createElement("span");
      repoArrow.className = "tree-arrow";
      repoArrow.textContent = "▼";

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
      repoChildren.className = "tree-children open";

      repoGroup.addEventListener("click", (e) => {
        if ((e.target as HTMLElement).closest(".tree-action")) return;
        repoOpen = !repoOpen;
        repoChildren.className = repoOpen ? "tree-children open" : "tree-children";
        repoArrow.textContent = repoOpen ? "▼" : "▶";
      });

      this.el.appendChild(repoGroup);

      for (const { workspace, sessions } of workspaces) {
        // Workspace row
        const wsGroup = document.createElement("div");
        wsGroup.className = "tree-group tree-group-ws";
        let wsOpen = true;

        const wsArrow = document.createElement("span");
        wsArrow.className = "tree-arrow";
        wsArrow.textContent = sessions.length > 0 ? "▼" : "▶";

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
        wsChildren.className = sessions.length > 0 ? "tree-children open" : "tree-children";

        wsGroup.addEventListener("click", (e) => {
          if ((e.target as HTMLElement).closest(".tree-action")) return;
          wsOpen = !wsOpen;
          wsChildren.className = wsOpen ? "tree-children open" : "tree-children";
          wsArrow.textContent = wsOpen ? "▼" : "▶";
        });

        repoChildren.appendChild(wsGroup);

        // Sessions
        for (const si of sessions) {
          const isRunning = si.session.status === "running";
          const isDead = ["failed", "exited", "stopped"].includes(si.session.status);
          const isActive = si.session.id === this.activeSessionId;
          const isConfirming = si.session.id === this.confirmingStopId;

          const sessionRow = document.createElement("div");
          sessionRow.className = `tree-item tree-item-session${isActive ? " active" : ""}${isDead ? " dead" : ""}`;

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
              stopSession(si.session.id)
                .then(() => this.refresh())
                .catch(() => {
                  // Supervisor likely dead (orphaned session) — force remove
                  removeSession(si.session.id).then(() => this.refresh());
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
          } else {
            const dot = document.createElement("span");
            dot.className = `status-dot ${si.session.status}`;
            sessionRow.appendChild(dot);

            const label = document.createElement("span");
            label.className = "tree-label";
            label.textContent = this.deriveSessionLabel(si);
            sessionRow.appendChild(label);

            const closeBtn = document.createElement("button");
            closeBtn.className = "tree-action tree-action-close";
            closeBtn.textContent = "×";

            if (isRunning) {
              sessionRow.style.cursor = "pointer";
              sessionRow.addEventListener("click", (e) => {
                if ((e.target as HTMLElement).closest(".tree-action")) return;
                this.callbacks.onSessionClick(si.repo, si.session.id, this.deriveSessionLabel(si));
              });

              closeBtn.title = "Stop session";
              closeBtn.addEventListener("click", (e) => {
                e.stopPropagation();
                this.confirmingStopId = si.session.id;
                this.render();
              });
            } else {
              // Dead session — click to view output, × removes
              sessionRow.style.cursor = "pointer";
              sessionRow.addEventListener("click", (e) => {
                if ((e.target as HTMLElement).closest(".tree-action")) return;
                this.callbacks.onSessionClick(si.repo, si.session.id, this.deriveSessionLabel(si));
              });

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

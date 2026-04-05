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
  onSessionClick: (repo: string, sessionId: string) => void;
  onCreateSession: (repo: string, workspace: string) => void;
  onCreateWorkspace: (repo: string) => void;
  onAddRepo: () => void;
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

  constructor(el: HTMLElement, callbacks: SidebarCallbacks) {
    this.el = el;
    this.callbacks = callbacks;
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

  private render() {
    this.el.innerHTML = "";

    // Header
    const header = document.createElement("div");
    header.className = "sidebar-header";
    const headerLabel = document.createElement("span");
    headerLabel.textContent = "Shard";
    header.appendChild(headerLabel);

    const addRepoBtn = document.createElement("button");
    addRepoBtn.className = "btn";
    addRepoBtn.textContent = "+";
    addRepoBtn.title = "Add repository";
    addRepoBtn.addEventListener("click", () => this.callbacks.onAddRepo());
    header.appendChild(addRepoBtn);

    this.el.appendChild(header);

    if (this.tree.length === 0) {
      const empty = document.createElement("div");
      empty.style.padding = "16px";
      empty.style.color = "var(--text-muted)";
      empty.style.fontSize = "12px";
      empty.textContent =
        "No repositories. Use shardctl to add repos and create sessions.";
      this.el.appendChild(empty);
      return;
    }

    for (const { repo, workspaces } of this.tree) {
      const section = document.createElement("div");
      section.className = "sidebar-section";

      // Repo group header
      const repoGroup = document.createElement("div");
      repoGroup.className = "tree-group";
      repoGroup.innerHTML = `<span>\u25B6</span> <span>${repo.alias}</span>`;
      let open = true;

      // Add workspace button on the repo row
      const addWsBtn = document.createElement("button");
      addWsBtn.className = "btn";
      addWsBtn.textContent = "+";
      addWsBtn.title = "New workspace";
      addWsBtn.style.marginLeft = "auto";
      addWsBtn.addEventListener("click", (e) => {
        e.stopPropagation();
        this.callbacks.onCreateWorkspace(repo.alias);
      });
      repoGroup.appendChild(addWsBtn);

      const children = document.createElement("div");
      children.className = "tree-children open";

      repoGroup.addEventListener("click", (e) => {
        if ((e.target as HTMLElement).closest(".btn")) return;
        open = !open;
        children.className = open ? "tree-children open" : "tree-children";
        repoGroup.querySelector("span")!.textContent = open
          ? "\u25BC"
          : "\u25B6";
      });
      repoGroup.querySelector("span")!.textContent = "\u25BC";

      for (const { workspace, sessions } of workspaces) {
        // Workspace sub-group
        const wsItem = document.createElement("div");
        wsItem.className = "tree-group";
        wsItem.style.paddingLeft = "28px";
        wsItem.style.fontSize = "12px";

        const wsLabel = document.createElement("span");
        wsLabel.textContent = workspace.name;
        wsItem.appendChild(wsLabel);

        // Add session button
        const addBtn = document.createElement("button");
        addBtn.className = "btn";
        addBtn.textContent = "+";
        addBtn.title = "New session";
        addBtn.style.marginLeft = "auto";
        addBtn.addEventListener("click", (e) => {
          e.stopPropagation();
          this.callbacks.onCreateSession(repo.alias, workspace.name);
        });
        wsItem.appendChild(addBtn);

        children.appendChild(wsItem);

        // Sessions
        for (const si of sessions) {
          const isRunning = si.session.status === "running";
          const isDead =
            si.session.status === "failed" ||
            si.session.status === "exited" ||
            si.session.status === "stopped";

          const sessionItem = document.createElement("div");
          sessionItem.className = "tree-item";
          sessionItem.style.paddingLeft = "44px";

          const dot = document.createElement("span");
          dot.className = `status-dot ${si.session.status}`;
          sessionItem.appendChild(dot);

          const cmd: string[] = JSON.parse(si.session.command_json);
          const label = document.createElement("span");
          label.textContent = `${si.session.id.slice(0, 8)} ${cmd.join(" ")}`;
          label.style.overflow = "hidden";
          label.style.textOverflow = "ellipsis";
          label.style.flex = "1";
          sessionItem.appendChild(label);

          if (isRunning) {
            // Click to attach
            sessionItem.style.cursor = "pointer";
            sessionItem.addEventListener("click", () => {
              this.callbacks.onSessionClick(si.repo, si.session.id);
            });

            // Stop button
            const stopBtn = document.createElement("button");
            stopBtn.className = "btn";
            stopBtn.textContent = "\u25A0";
            stopBtn.title = "Stop session";
            stopBtn.style.fontSize = "10px";
            stopBtn.addEventListener("click", (e) => {
              e.stopPropagation();
              stopSession(si.session.id).then(() => this.refresh());
            });
            sessionItem.appendChild(stopBtn);
          } else if (isDead) {
            // Dim dead sessions
            sessionItem.style.opacity = "0.6";

            // Remove button
            const removeBtn = document.createElement("button");
            removeBtn.className = "btn";
            removeBtn.textContent = "\u00D7";
            removeBtn.title = "Remove session";
            removeBtn.addEventListener("click", (e) => {
              e.stopPropagation();
              removeSession(si.session.id).then(() => this.refresh());
            });
            sessionItem.appendChild(removeBtn);
          }

          children.appendChild(sessionItem);
        }
      }

      section.appendChild(repoGroup);
      section.appendChild(children);
      this.el.appendChild(section);
    }
  }
}

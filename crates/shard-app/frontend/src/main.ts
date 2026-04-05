import "@xterm/xterm/css/xterm.css";
import { Sidebar } from "./components/Sidebar";
import { TabBar } from "./components/TabBar";
import { TerminalPane } from "./components/TerminalPane";
import { addRepo, createSession, createWorkspace } from "./lib/api";

const sidebarEl = document.getElementById("sidebar")!;
const tabBarEl = document.getElementById("tab-bar")!;
const terminalContainer = document.getElementById("terminal-container")!;

const terminalPane = new TerminalPane(terminalContainer);

const tabBar = new TabBar(tabBarEl, {
  onSelect(sessionId: string) {
    terminalPane.hideEmpty();
    terminalPane.show(sessionId);
    tabBar.setActive(sessionId);
  },
  onClose(sessionId: string) {
    terminalPane.close(sessionId);
    tabBar.removeTab(sessionId);
    if (!tabBar.getActiveId()) {
      terminalPane.showEmpty();
    }
  },
});

const sidebar = new Sidebar(sidebarEl, {
  onSessionClick(_repo: string, sessionId: string) {
    openSession(_repo, sessionId);
  },
  async onCreateSession(repo: string, workspace: string) {
    try {
      const session = await createSession(repo, workspace);
      openSession(repo, session.id);
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
  async onAddRepo() {
    const url = prompt(
      "Git URL, SSH address, or local path:\n\n" +
        "Examples:\n" +
        "  https://github.com/user/repo\n" +
        "  git@github.com:user/repo.git\n" +
        "  C:\\Projects\\my-repo"
    );
    if (!url || !url.trim()) return;

    const alias = prompt(
      "Short alias (leave blank to auto-derive from URL):"
    );

    try {
      await addRepo(url.trim(), alias?.trim() || undefined);
      sidebar.refresh();
    } catch (err) {
      alert(`Failed to add repo: ${err}`);
    }
  },
});

function openSession(repo: string, sessionId: string) {
  const shortId = sessionId.slice(0, 8);
  tabBar.addTab(sessionId, `${repo}:${shortId}`);
  terminalPane.hideEmpty();
  terminalPane.open(sessionId);
}

// Initial load
terminalPane.showEmpty();
sidebar.refresh();

// Refresh sidebar periodically to pick up external changes
setInterval(() => sidebar.refresh(), 5000);

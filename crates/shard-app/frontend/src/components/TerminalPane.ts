import { createTerminalSession, type TerminalSession } from "../lib/terminal";

export interface TerminalPaneCallbacks {
  onAddShard: () => void;
}

export class TerminalPane {
  private container: HTMLElement;
  private callbacks: TerminalPaneCallbacks;
  private sessions: Map<string, { el: HTMLDivElement; session: TerminalSession }> =
    new Map();
  private activeId: string | null = null;
  private hasShards: boolean = false;

  constructor(container: HTMLElement, callbacks: TerminalPaneCallbacks) {
    this.container = container;
    this.callbacks = callbacks;
  }

  setHasShards(has: boolean) {
    this.hasShards = has;
  }

  open(sessionId: string) {
    if (this.sessions.has(sessionId)) {
      this.show(sessionId);
      return;
    }

    const el = document.createElement("div");
    el.style.position = "absolute";
    el.style.inset = "0";
    el.style.display = "none";
    this.container.appendChild(el);

    const session = createTerminalSession(sessionId, el);
    this.sessions.set(sessionId, { el, session });
    this.show(sessionId);
  }

  show(sessionId: string) {
    if (this.activeId && this.sessions.has(this.activeId)) {
      this.sessions.get(this.activeId)!.el.style.display = "none";
    }

    this.activeId = sessionId;
    const entry = this.sessions.get(sessionId);
    if (entry) {
      entry.el.style.display = "block";
      entry.session.fitAddon.fit();
      entry.session.terminal.focus();
    }
    this.hideEmpty();
  }

  close(sessionId: string) {
    const entry = this.sessions.get(sessionId);
    if (!entry) return;

    entry.session.dispose();
    entry.el.remove();
    this.sessions.delete(sessionId);

    if (this.activeId === sessionId) {
      this.activeId = null;
    }
  }

  getActiveId(): string | null {
    return this.activeId;
  }

  showEmpty() {
    if (this.activeId && this.sessions.has(this.activeId)) {
      this.sessions.get(this.activeId)!.el.style.display = "none";
    }
    this.activeId = null;

    let empty = this.container.querySelector(".empty-state") as HTMLElement;
    if (!empty) {
      empty = document.createElement("div");
      empty.className = "empty-state";
      this.container.appendChild(empty);
    }

    if (!this.hasShards) {
      // First launch / no shards
      empty.innerHTML = `
        <div class="empty-welcome">Welcome to Shard</div>
        <div class="empty-subtitle">Your agentic workspaces, all in one place</div>
        <button class="empty-cta" id="empty-add-shard">+ Add your first shard</button>
        <div class="empty-cli-hint">or from the command line</div>
        <div class="empty-cli"><code>shardctl repo add C:\\Projects\\my-repo</code></div>
      `;
      empty.querySelector("#empty-add-shard")?.addEventListener("click", () => {
        this.callbacks.onAddShard();
      });
    } else {
      // Has shards but no session selected
      empty.innerHTML = `
        <div class="empty-state-title">No session open</div>
        <div class="empty-state-hint">Click a session in the sidebar or create a new one</div>
      `;
    }

    empty.style.display = "flex";
  }

  hideEmpty() {
    const empty = this.container.querySelector(".empty-state");
    if (empty) {
      (empty as HTMLElement).style.display = "none";
    }
  }
}

import { createTerminalSession, type TerminalSession } from "../lib/terminal";

export class TerminalPane {
  private container: HTMLElement;
  private sessions: Map<string, { el: HTMLDivElement; session: TerminalSession }> =
    new Map();
  private activeId: string | null = null;

  constructor(container: HTMLElement) {
    this.container = container;
  }

  open(sessionId: string) {
    if (this.sessions.has(sessionId)) {
      this.show(sessionId);
      return;
    }

    // Create a wrapper div for this terminal
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
    // Hide current
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

  hasSession(sessionId: string): boolean {
    return this.sessions.has(sessionId);
  }

  showEmpty() {
    if (this.activeId && this.sessions.has(this.activeId)) {
      this.sessions.get(this.activeId)!.el.style.display = "none";
    }
    this.activeId = null;

    // Show empty state if no terminals are open
    if (this.sessions.size === 0) {
      let empty = this.container.querySelector(".empty-state");
      if (!empty) {
        empty = document.createElement("div");
        empty.className = "empty-state";
        empty.innerHTML = `
          <div class="empty-state-title">No session open</div>
          <div>Click a session in the sidebar or create a new one</div>
        `;
        this.container.appendChild(empty);
      }
      (empty as HTMLElement).style.display = "flex";
    }
  }

  hideEmpty() {
    const empty = this.container.querySelector(".empty-state");
    if (empty) {
      (empty as HTMLElement).style.display = "none";
    }
  }
}

import { Terminal } from "@xterm/xterm";
import {
  createTerminalSession,
  type SessionInputStateEvent,
  type TerminalSession,
} from "../lib/terminal";

export interface TerminalPaneCallbacks {
  onAddShard: () => void;
  /**
   * Fires when the user opens a session that another window already holds.
   * The host should focus that window — `terminal.ts` has already aborted
   * its own attach attempt by the time this fires.
   */
  onOwnershipConflict?: (sessionId: string, ownerWindowLabel: string) => void;
  /** Per-session input-state event relay for sidebar overlays. */
  onInputStateChange?: (state: SessionInputStateEvent) => void;
}

export class TerminalPane {
  private container: HTMLElement;
  private callbacks: TerminalPaneCallbacks;
  private sessions: Map<string, { el: HTMLDivElement; session: TerminalSession }> =
    new Map();
  private activeId: string | null = null;
  private hasShards: boolean = false;
  private dynamicTitles: Map<string, string> = new Map();

  /** Called when a session's terminal title changes via OSC sequences. */
  onTitleChange?: (sessionId: string, title: string) => void;

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

    const previousId =
      this.activeId && this.activeId !== sessionId ? this.activeId : null;
    const el = document.createElement("div");
    el.style.position = "absolute";
    el.style.inset = "0";
    el.style.display = "none";
    this.container.appendChild(el);

    const session = createTerminalSession(sessionId, el, {
      onTitleChange: (title) => {
        this.dynamicTitles.set(sessionId, title);
        this.onTitleChange?.(sessionId, title);
      },
      onAttached: () => {
        if (previousId && this.activeId === sessionId) {
          this.close(previousId);
        }
      },
      onOwnershipConflict: (ownerWindowLabel) => {
        // The session is alive in another window. Tear down our local
        // entry — terminal.ts already aborted the attach — so the
        // sidebar can re-render without an "open" highlight. If we had
        // an active terminal before the attempted switch, restore it
        // because the switch did not actually happen.
        this.close(sessionId);
        if (previousId && this.sessions.has(previousId)) {
          this.show(previousId);
        } else if (!this.activeId) {
          this.showEmpty();
        }
        this.callbacks.onOwnershipConflict?.(sessionId, ownerWindowLabel);
      },
      onInputStateChange: (state) => {
        this.callbacks.onInputStateChange?.(state);
      },
    });
    this.sessions.set(sessionId, { el, session });
    this.show(sessionId);
  }

  show(sessionId: string): boolean {
    const entry = this.sessions.get(sessionId);
    if (!entry) return false;

    if (this.activeId && this.sessions.has(this.activeId)) {
      this.sessions.get(this.activeId)!.el.style.display = "none";
    }

    this.activeId = sessionId;
    entry.el.style.display = "block";
    entry.session.fitAddon.fit();
    entry.session.terminal.focus();
    this.hideEmpty();
    return true;
  }

  close(sessionId: string) {
    const entry = this.sessions.get(sessionId);
    if (!entry) return;

    entry.session.dispose();
    entry.el.remove();
    this.sessions.delete(sessionId);
    this.dynamicTitles.delete(sessionId);

    if (this.activeId === sessionId) {
      this.activeId = null;
    }
  }

  getActiveId(): string | null {
    return this.activeId;
  }

  getActiveTerminal(): Terminal | null {
    if (!this.activeId) return null;
    return this.sessions.get(this.activeId)?.session.terminal ?? null;
  }

  getDynamicTitle(sessionId: string): string | undefined {
    return this.dynamicTitles.get(sessionId);
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

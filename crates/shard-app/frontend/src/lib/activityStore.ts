/** Supervisor-pushed activity state (Active | Idle | Blocked). */
export type SupervisorState = "active" | "idle" | "blocked";

/**
 * Frontend-resolved display state for the sidebar indicator.
 *
 * - active:          agent is working (green spinning)
 * - idle:            agent finished, nothing happening (faint green still)
 * - blocked:         agent waiting for permission (amber spinning)
 * - needs-attention: agent transitioned from active/blocked to idle while unfocused (amber pulsing)
 */
export type DisplayState = "active" | "idle" | "blocked" | "needs-attention";

type Listener = (id: string, state: DisplayState) => void;

interface SessionState {
  /** Raw state from the supervisor — never mutated locally. */
  supervisor: SupervisorState;
  /** Set when a meaningful transition happened while the tab was unfocused. */
  attention: boolean;
}

/**
 * Per-session activity state store with separate supervisor and attention tracking.
 *
 * The supervisor state is authoritative and comes from harness hooks via the
 * transport protocol. The attention flag is a local UI concern — it tracks
 * whether the user has "seen" a state change.
 *
 * Display derivation:
 *   supervisor=active                  → active  (always, regardless of attention)
 *   supervisor=blocked                 → blocked (always — agent needs the user)
 *   supervisor=idle + attention=true   → needs-attention
 *   supervisor=idle + attention=false  → idle
 */
class SessionActivityStore {
  private states = new Map<string, SessionState>();
  private listeners: Listener[] = [];

  /** Called when a `session-activity` Tauri event arrives. */
  notify(
    id: string,
    supervisorState: SupervisorState,
    isFocused: boolean
  ): void {
    const current = this.states.get(id);
    const prev = current ? this.resolve(current) : undefined;

    let attention = current?.attention ?? false;

    if (supervisorState === "active") {
      // Active clears attention — the agent is working again
      attention = false;
    } else if (
      !isFocused &&
      (current?.supervisor === "active" || current?.supervisor === "blocked")
    ) {
      // Transitioned away from active/blocked while unfocused → attention.
      // active→idle: agent finished working, user should know.
      // blocked→idle: permission was handled, agent moved on.
      attention = true;
    }

    const next: SessionState = { supervisor: supervisorState, attention };
    this.states.set(id, next);

    const display = this.resolve(next);
    if (display !== prev) {
      for (const fn_ of this.listeners) fn_(id, display);
    }
  }

  /** Clear attention when the user opens/focuses a session tab. */
  clearAttention(id: string): void {
    const current = this.states.get(id);
    if (!current || !current.attention) return;

    const updated = { ...current, attention: false };
    this.states.set(id, updated);

    const prev = this.resolve(current);
    const next = this.resolve(updated);
    if (next !== prev) {
      for (const fn_ of this.listeners) fn_(id, next);
    }
  }

  get(id: string): DisplayState | undefined {
    const s = this.states.get(id);
    return s ? this.resolve(s) : undefined;
  }

  remove(id: string): void {
    this.states.delete(id);
  }

  /** Subscribe to display state changes. Returns an unsubscribe function. */
  onChange(fn_: Listener): () => void {
    this.listeners.push(fn_);
    return () => {
      this.listeners = this.listeners.filter((f) => f !== fn_);
    };
  }

  private resolve(s: SessionState): DisplayState {
    if (s.supervisor === "active") return "active";
    if (s.supervisor === "blocked") return "blocked";
    // supervisor === "idle"
    return s.attention ? "needs-attention" : "idle";
  }
}

export const activityStore = new SessionActivityStore();

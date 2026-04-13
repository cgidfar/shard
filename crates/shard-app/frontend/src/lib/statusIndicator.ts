import type { DisplayState } from "./activityStore";

/**
 * Build a session status indicator — a 12px dot + ring widget used in both
 * the sidebar tree and the titlebar breadcrumb.
 *
 * Styling lives in app.css under `.status-indicator`, driven by the
 * `data-lifecycle-status` and `data-activity-state` attributes.
 */
export function createStatusIndicator(
  lifecycleStatus: string,
  activityState?: DisplayState,
): HTMLSpanElement {
  const indicator = document.createElement("span");
  indicator.className = "status-indicator";
  indicator.dataset.lifecycleStatus = lifecycleStatus;
  if (activityState) {
    indicator.dataset.activityState = activityState;
  }

  const ring = document.createElement("span");
  ring.className = "status-ring";
  const innerDot = document.createElement("span");
  innerDot.className = "status-dot-inner";

  indicator.appendChild(ring);
  indicator.appendChild(innerDot);

  return indicator;
}

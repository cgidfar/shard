import { getCurrentWindow } from "@tauri-apps/api/window";
import { createStatusIndicator } from "../lib/statusIndicator";

export interface TitleBarCallbacks {
  onToggleSidebar: () => void;
}

export interface Breadcrumb {
  repo: string;
  workspace: string;
  session: string;
  status: string;
}

const ICON_TOGGLE_SIDEBAR =
  `<svg width="14" height="14" viewBox="0 0 14 14"><rect x="1" y="2" width="12" height="10" rx="1.5" fill="none" stroke="currentColor" stroke-width="1.2"/><line x1="5" y1="2" x2="5" y2="12" stroke="currentColor" stroke-width="1.2"/></svg>`;

const ICON_BACK =
  `<svg width="13" height="13" viewBox="0 0 13 13" fill="none"><path d="M8 3L4.5 6.5L8 10" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round"/></svg>`;

const ICON_FORWARD =
  `<svg width="13" height="13" viewBox="0 0 13 13" fill="none"><path d="M5 3L8.5 6.5L5 10" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round"/></svg>`;

const ICON_SEARCH =
  `<svg width="13" height="13" viewBox="0 0 13 13" fill="none"><circle cx="5.75" cy="5.75" r="3.25" stroke="currentColor" stroke-width="1.2"/><path d="M8.25 8.25L10.5 10.5" stroke="currentColor" stroke-width="1.2" stroke-linecap="round"/></svg>`;

export class TitleBar {
  private el: HTMLElement;
  private callbacks: TitleBarCallbacks;
  private breadcrumbEl: HTMLElement;

  constructor(el: HTMLElement, callbacks: TitleBarCallbacks) {
    this.el = el;
    this.callbacks = callbacks;
    this.breadcrumbEl = document.createElement("div");
    this.breadcrumbEl.className = "titlebar-breadcrumb";
    this.breadcrumbEl.setAttribute("data-tauri-drag-region", "");
    this.render();
  }

  setBreadcrumb(crumb: Breadcrumb | null) {
    if (!crumb) {
      this.breadcrumbEl.textContent = "";
      return;
    }
    this.breadcrumbEl.innerHTML = "";

    const parts = [
      { text: crumb.repo, cls: "breadcrumb-muted" },
      { text: crumb.workspace, cls: "breadcrumb-muted" },
      { text: crumb.session, cls: "breadcrumb-active" },
    ];

    for (let i = 0; i < parts.length; i++) {
      if (i > 0) {
        const sep = document.createElement("span");
        sep.className = "breadcrumb-sep";
        sep.textContent = "›";
        this.breadcrumbEl.appendChild(sep);
      }
      const span = document.createElement("span");
      span.className = parts[i].cls;
      span.textContent = parts[i].text;
      this.breadcrumbEl.appendChild(span);
    }

    const indicator = createStatusIndicator(crumb.status);
    indicator.style.marginLeft = "6px";
    this.breadcrumbEl.appendChild(indicator);
  }

  private render() {
    this.el.innerHTML = "";

    const left = document.createElement("div");
    left.className = "titlebar-left";

    const toggleBtn = document.createElement("button");
    toggleBtn.className = "titlebar-btn";
    toggleBtn.title = "Toggle sidebar";
    toggleBtn.innerHTML = ICON_TOGGLE_SIDEBAR;
    toggleBtn.addEventListener("click", () => this.callbacks.onToggleSidebar());

    const brand = document.createElement("span");
    brand.className = "titlebar-brand";
    brand.textContent = "Shard";

    const navGroup = document.createElement("div");
    navGroup.className = "titlebar-nav";
    for (const { icon, title, cls } of [
      { icon: ICON_BACK, title: "Back", cls: "" },
      { icon: ICON_FORWARD, title: "Forward", cls: "is-disabled" },
      { icon: ICON_SEARCH, title: "Search", cls: "" },
    ]) {
      const btn = document.createElement("button");
      btn.className = `titlebar-btn titlebar-btn-placeholder ${cls}`.trim();
      btn.title = title;
      btn.disabled = true;
      btn.innerHTML = icon;
      navGroup.appendChild(btn);
    }

    left.appendChild(toggleBtn);
    left.appendChild(brand);
    left.appendChild(navGroup);

    const right = document.createElement("div");
    right.className = "titlebar-controls";

    for (const { label, action, cls } of [
      { label: "─", action: () => getCurrentWindow().minimize(), cls: "" },
      { label: "☐", action: () => getCurrentWindow().toggleMaximize(), cls: "" },
      { label: "×", action: () => getCurrentWindow().close(), cls: "titlebar-close" },
    ]) {
      const btn = document.createElement("button");
      btn.className = `titlebar-control ${cls}`;
      btn.textContent = label;
      btn.addEventListener("click", action);
      right.appendChild(btn);
    }

    this.el.appendChild(left);
    this.el.appendChild(this.breadcrumbEl);
    this.el.appendChild(right);
  }
}

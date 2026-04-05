import { getCurrentWindow } from "@tauri-apps/api/window";

export interface TitleBarCallbacks {
  onAddShard: () => void;
  onToggleSidebar: () => void;
}

export interface Breadcrumb {
  repo: string;
  workspace: string;
  session: string;
  status: string;
}

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

    const dot = document.createElement("span");
    dot.className = `status-dot ${crumb.status}`;
    dot.style.marginLeft = "6px";
    this.breadcrumbEl.appendChild(dot);
  }

  private render() {
    this.el.innerHTML = "";

    // Left: toggle + branding + add
    const left = document.createElement("div");
    left.className = "titlebar-left";

    const toggleBtn = document.createElement("button");
    toggleBtn.className = "titlebar-btn";
    toggleBtn.title = "Toggle sidebar";
    toggleBtn.innerHTML = `<svg width="14" height="14" viewBox="0 0 14 14"><rect x="1" y="2" width="12" height="10" rx="1.5" fill="none" stroke="currentColor" stroke-width="1.2"/><line x1="5" y1="2" x2="5" y2="12" stroke="currentColor" stroke-width="1.2"/></svg>`;
    toggleBtn.addEventListener("click", () => this.callbacks.onToggleSidebar());

    const brand = document.createElement("span");
    brand.className = "titlebar-brand";
    brand.textContent = "Shard";

    const addBtn = document.createElement("button");
    addBtn.className = "titlebar-add";
    addBtn.title = "Add shard";
    addBtn.textContent = "+";
    addBtn.addEventListener("click", () => this.callbacks.onAddShard());

    left.appendChild(toggleBtn);
    left.appendChild(brand);
    left.appendChild(addBtn);

    // Right: window controls
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

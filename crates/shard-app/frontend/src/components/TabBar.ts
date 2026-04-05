export interface Tab {
  sessionId: string;
  label: string;
}

export interface TabBarCallbacks {
  onSelect: (sessionId: string) => void;
  onClose: (sessionId: string) => void;
}

export class TabBar {
  private el: HTMLElement;
  private callbacks: TabBarCallbacks;
  private tabs: Tab[] = [];
  private activeId: string | null = null;

  constructor(el: HTMLElement, callbacks: TabBarCallbacks) {
    this.el = el;
    this.callbacks = callbacks;
  }

  addTab(sessionId: string, label: string) {
    if (this.tabs.find((t) => t.sessionId === sessionId)) {
      this.setActive(sessionId);
      return;
    }
    this.tabs.push({ sessionId, label });
    this.setActive(sessionId);
  }

  removeTab(sessionId: string) {
    const idx = this.tabs.findIndex((t) => t.sessionId === sessionId);
    if (idx === -1) return;
    this.tabs.splice(idx, 1);

    if (this.activeId === sessionId) {
      const next = this.tabs[Math.min(idx, this.tabs.length - 1)];
      this.activeId = next?.sessionId ?? null;
      if (this.activeId) {
        this.callbacks.onSelect(this.activeId);
      }
    }
    this.render();
  }

  setActive(sessionId: string) {
    this.activeId = sessionId;
    this.render();
  }

  getActiveId(): string | null {
    return this.activeId;
  }

  private render() {
    this.el.innerHTML = "";

    for (const tab of this.tabs) {
      const tabEl = document.createElement("div");
      tabEl.className = `tab${tab.sessionId === this.activeId ? " active" : ""}`;

      const label = document.createElement("span");
      label.textContent = tab.label;
      tabEl.appendChild(label);

      const close = document.createElement("button");
      close.className = "tab-close";
      close.textContent = "\u00D7";
      close.addEventListener("click", (e) => {
        e.stopPropagation();
        this.callbacks.onClose(tab.sessionId);
      });
      tabEl.appendChild(close);

      tabEl.addEventListener("click", () => {
        this.callbacks.onSelect(tab.sessionId);
      });

      this.el.appendChild(tabEl);
    }
  }
}

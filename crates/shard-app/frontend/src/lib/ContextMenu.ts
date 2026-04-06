export interface MenuItemDef {
  kind: "action" | "separator";
  label?: string;
  disabled?: boolean;
  danger?: boolean;
  handler?: () => void;
}

export type MenuItemProvider = (target: HTMLElement) => MenuItemDef[];

interface Registration {
  selector: string;
  provider: MenuItemProvider;
}

export class ContextMenu {
  private menuEl: HTMLDivElement;
  private registrations: Registration[] = [];
  private visible = false;

  constructor() {
    this.menuEl = document.createElement("div");
    this.menuEl.className = "context-menu";
    document.body.appendChild(this.menuEl);

    // Single global contextmenu handler — suppress default everywhere
    document.addEventListener("contextmenu", (e) => this.onContextMenu(e), true);
    document.addEventListener("click", () => this.hide());
    document.addEventListener("keydown", (e) => {
      if (e.key === "Escape") this.hide();
    });
    window.addEventListener("blur", () => this.hide());
  }

  /** Register a context menu for elements matching a CSS selector.
   *  More-specific selectors should be registered first. */
  register(selector: string, provider: MenuItemProvider) {
    this.registrations.push({ selector, provider });
  }

  /** Show the menu at (x, y) with the given items. */
  show(x: number, y: number, items: MenuItemDef[]) {
    this.render(items);
    this.menuEl.style.display = "block";
    this.visible = true;
    // Position after display:block so offsetWidth/Height reflect actual size
    this.position(x, y);
  }

  hide() {
    if (!this.visible) return;
    this.menuEl.style.display = "none";
    this.visible = false;
  }

  private onContextMenu(e: MouseEvent) {
    e.preventDefault();
    this.hide();

    const target = e.target as HTMLElement;
    for (const reg of this.registrations) {
      const matched = target.closest(reg.selector) as HTMLElement | null;
      if (matched) {
        const items = reg.provider(matched);
        if (items.length > 0) {
          this.show(e.clientX, e.clientY, items);
        }
        return;
      }
    }
    // No match — default menu is already suppressed
  }

  private render(items: MenuItemDef[]) {
    this.menuEl.innerHTML = "";
    for (const item of items) {
      if (item.kind === "separator") {
        const sep = document.createElement("div");
        sep.className = "context-menu-separator";
        this.menuEl.appendChild(sep);
        continue;
      }

      const row = document.createElement("div");
      row.className = "context-menu-item";
      if (item.disabled) row.classList.add("disabled");
      if (item.danger) row.classList.add("danger");
      row.textContent = item.label ?? "";

      if (!item.disabled && item.handler) {
        row.addEventListener("click", (e) => {
          e.stopPropagation();
          this.hide();
          item.handler!();
        });
      }

      this.menuEl.appendChild(row);
    }
  }

  private position(x: number, y: number) {
    // Place at cursor, then clamp to viewport
    const w = this.menuEl.offsetWidth;
    const h = this.menuEl.offsetHeight;
    const vw = window.innerWidth;
    const vh = window.innerHeight;

    let left = x;
    let top = y;

    if (left + w > vw) left = Math.max(0, x - w);
    if (top + h > vh) top = Math.max(0, y - h);

    this.menuEl.style.left = `${left}px`;
    this.menuEl.style.top = `${top}px`;
  }
}

export const contextMenu = new ContextMenu();

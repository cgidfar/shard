import { open as openDialog } from "@tauri-apps/plugin-dialog";

export interface AddShardResult {
  url: string;
  alias: string | undefined;
}

export class AddShardDialog {
  private overlay: HTMLElement;
  private mode: "local" | "remote" = "local";
  private pathInput!: HTMLInputElement;
  private aliasInput!: HTMLInputElement;
  private urlInput!: HTMLInputElement;
  private localFields!: HTMLElement;
  private remoteFields!: HTMLElement;
  private localToggle!: HTMLElement;
  private remoteToggle!: HTMLElement;
  private resolve: ((result: AddShardResult | null) => void) | null = null;

  constructor() {
    this.overlay = document.createElement("div");
    this.overlay.className = "dialog-overlay";
    this.overlay.addEventListener("click", (e) => {
      if (e.target === this.overlay) this.close(null);
    });
    this.buildDialog();
  }

  open(): Promise<AddShardResult | null> {
    document.body.appendChild(this.overlay);
    this.reset();
    // Focus first input after render
    requestAnimationFrame(() => {
      if (this.mode === "local") this.pathInput.focus();
      else this.urlInput.focus();
    });
    return new Promise((resolve) => {
      this.resolve = resolve;
    });
  }

  private close(result: AddShardResult | null) {
    this.overlay.remove();
    this.resolve?.(result);
    this.resolve = null;
  }

  private reset() {
    this.pathInput.value = "";
    this.aliasInput.value = "";
    this.aliasInput.placeholder = "";
    this.urlInput.value = "";
    this.setMode("local");
  }

  private setMode(mode: "local" | "remote") {
    this.mode = mode;
    this.localToggle.classList.toggle("active", mode === "local");
    this.remoteToggle.classList.toggle("active", mode === "remote");
    this.localFields.style.display = mode === "local" ? "flex" : "none";
    this.remoteFields.style.display = mode === "remote" ? "flex" : "none";
  }

  private deriveAlias(path: string): string {
    const parts = path.replace(/\\/g, "/").split("/").filter(Boolean);
    return parts[parts.length - 1] || "";
  }

  private async browsePath() {
    try {
      const selected = await openDialog({ directory: true, multiple: false });
      if (selected) {
        this.pathInput.value = selected as string;
        this.aliasInput.placeholder = this.deriveAlias(selected as string);
      }
    } catch {
      // Dialog cancelled or not available
    }
  }

  private submit() {
    const url = this.mode === "local" ? this.pathInput.value.trim() : this.urlInput.value.trim();
    if (!url) return;
    const alias = this.aliasInput.value.trim() || undefined;
    this.close({ url, alias });
  }

  private buildDialog() {
    const dialog = document.createElement("div");
    dialog.className = "dialog";

    // Header
    const header = document.createElement("div");
    header.className = "dialog-header";
    const title = document.createElement("span");
    title.className = "dialog-title";
    title.textContent = "Add Shard";
    const closeBtn = document.createElement("button");
    closeBtn.className = "dialog-close";
    closeBtn.textContent = "×";
    closeBtn.addEventListener("click", () => this.close(null));
    header.appendChild(title);
    header.appendChild(closeBtn);

    // Mode toggle
    const toggle = document.createElement("div");
    toggle.className = "dialog-toggle";

    this.localToggle = document.createElement("button");
    this.localToggle.className = "dialog-toggle-btn active";
    this.localToggle.textContent = "Local";
    this.localToggle.addEventListener("click", () => this.setMode("local"));

    this.remoteToggle = document.createElement("button");
    this.remoteToggle.className = "dialog-toggle-btn";
    this.remoteToggle.textContent = "Remote";
    this.remoteToggle.addEventListener("click", () => this.setMode("remote"));

    toggle.appendChild(this.localToggle);
    toggle.appendChild(this.remoteToggle);

    // Local fields
    this.localFields = document.createElement("div");
    this.localFields.className = "dialog-fields";

    const pathGroup = this.createFieldGroup("Path");
    const pathRow = document.createElement("div");
    pathRow.className = "dialog-input-row";
    this.pathInput = document.createElement("input");
    this.pathInput.className = "dialog-input mono";
    this.pathInput.placeholder = "C:\\Projects\\my-repo";
    this.pathInput.addEventListener("input", () => {
      this.aliasInput.placeholder = this.deriveAlias(this.pathInput.value);
    });
    const browseBtn = document.createElement("button");
    browseBtn.className = "dialog-browse";
    browseBtn.textContent = "Browse";
    browseBtn.addEventListener("click", () => this.browsePath());
    pathRow.appendChild(this.pathInput);
    pathRow.appendChild(browseBtn);
    pathGroup.appendChild(pathRow);
    this.localFields.appendChild(pathGroup);

    // Remote fields
    this.remoteFields = document.createElement("div");
    this.remoteFields.className = "dialog-fields";
    this.remoteFields.style.display = "none";

    const urlGroup = this.createFieldGroup("URL");
    this.urlInput = document.createElement("input");
    this.urlInput.className = "dialog-input mono";
    this.urlInput.placeholder = "https://github.com/user/repo";
    this.urlInput.addEventListener("input", () => {
      this.aliasInput.placeholder = this.deriveAlias(this.urlInput.value.replace(/\.git$/, ""));
    });
    urlGroup.appendChild(this.urlInput);
    this.remoteFields.appendChild(urlGroup);

    // Alias (shared)
    const aliasGroup = this.createFieldGroup("Alias", "optional");
    this.aliasInput = document.createElement("input");
    this.aliasInput.className = "dialog-input";
    const aliasHint = document.createElement("span");
    aliasHint.className = "dialog-hint";
    aliasHint.textContent = "Auto-derived from path. Override for a shorter name.";
    aliasGroup.appendChild(this.aliasInput);
    aliasGroup.appendChild(aliasHint);

    // Footer
    const footer = document.createElement("div");
    footer.className = "dialog-footer";

    const cancelBtn = document.createElement("button");
    cancelBtn.className = "dialog-btn-ghost";
    cancelBtn.textContent = "Cancel";
    cancelBtn.addEventListener("click", () => this.close(null));

    const submitBtn = document.createElement("button");
    submitBtn.className = "dialog-btn-primary";
    submitBtn.textContent = "Add Shard";
    submitBtn.addEventListener("click", () => this.submit());

    footer.appendChild(cancelBtn);
    footer.appendChild(submitBtn);

    // Assemble
    const body = document.createElement("div");
    body.className = "dialog-body";
    body.appendChild(this.localFields);
    body.appendChild(this.remoteFields);
    body.appendChild(aliasGroup);

    dialog.appendChild(header);
    dialog.appendChild(toggle);
    dialog.appendChild(body);
    dialog.appendChild(footer);
    this.overlay.appendChild(dialog);

    // Enter to submit
    dialog.addEventListener("keydown", (e) => {
      if (e.key === "Enter") this.submit();
      if (e.key === "Escape") this.close(null);
    });
  }

  private createFieldGroup(label: string, suffix?: string): HTMLElement {
    const group = document.createElement("div");
    group.className = "dialog-field-group";
    const labelRow = document.createElement("div");
    labelRow.className = "dialog-label-row";
    const labelEl = document.createElement("label");
    labelEl.className = "dialog-label";
    labelEl.textContent = label;
    labelRow.appendChild(labelEl);
    if (suffix) {
      const suf = document.createElement("span");
      suf.className = "dialog-label-suffix";
      suf.textContent = suffix;
      labelRow.appendChild(suf);
    }
    group.appendChild(labelRow);
    return group;
  }
}

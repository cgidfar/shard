import {
  listRepoBranches,
  listWorkspaces,
  safeWorkspaceName,
  type BranchInfo,
  type WorkspaceMode,
} from "../lib/api";

export interface AddWorkspaceResult {
  name: string;
  mode: WorkspaceMode;
  branch: string;
  /** When set, the user picked a branch checked out in an externally-managed
   *  worktree. The caller should route to `adoptWorkspace(repo, adoptPath, ...)`
   *  instead of `createWorkspace(...)`. Always paired with `mode === "existing_branch"`. */
  adoptPath?: string;
}

export class AddWorkspaceDialog {
  private overlay: HTMLElement;
  private mode: WorkspaceMode = "new_branch";
  private branches: BranchInfo[] = [];
  private existingWorkspaceNames: string[] = [];
  private nameGroup!: HTMLElement;
  private nameInput!: HTMLInputElement;
  private baseSelect!: HTMLSelectElement;
  private existingSelect!: HTMLSelectElement;
  private newFields!: HTMLElement;
  private existingFields!: HTMLElement;
  private newToggle!: HTMLButtonElement;
  private existingToggle!: HTMLButtonElement;
  private warnEl!: HTMLElement;
  private submitBtn!: HTMLButtonElement;
  private resolve: ((result: AddWorkspaceResult | null) => void) | null = null;

  constructor() {
    this.overlay = document.createElement("div");
    this.overlay.className = "dialog-overlay";
    this.overlay.addEventListener("click", (e) => {
      if (e.target === this.overlay) this.close(null);
    });
    this.buildDialog();
  }

  async open(repo: string): Promise<AddWorkspaceResult | null> {
    // Guard against double-open (rapid keyboard). Current promise wins.
    if (this.resolve) return null;

    try {
      const [branches, workspaces] = await Promise.all([
        listRepoBranches(repo),
        listWorkspaces(repo).catch(() => []),
      ]);
      this.branches = branches;
      this.existingWorkspaceNames = workspaces.map((ws) => ws.name);
    } catch {
      this.branches = [];
      this.existingWorkspaceNames = [];
    }

    this.reset();
    document.body.appendChild(this.overlay);
    requestAnimationFrame(() => this.nameInput.focus());
    requestAnimationFrame(() => this.nameInput.select());
    return new Promise((resolve) => {
      this.resolve = resolve;
    });
  }

  private close(result: AddWorkspaceResult | null) {
    this.overlay.remove();
    const r = this.resolve;
    this.resolve = null;
    r?.(result);
  }

  private reset() {
    this.populateSelect(this.baseSelect, this.branches, /*skipOccupied*/ false);
    this.populateSelect(this.existingSelect, this.branches, /*skipOccupied*/ false);
    const headIdx = this.branches.findIndex((b) => b.is_head);
    if (headIdx >= 0) this.baseSelect.selectedIndex = headIdx;
    this.nameInput.value = this.generateName();
    this.setMode("new_branch");
  }

  private setMode(mode: WorkspaceMode) {
    this.mode = mode;
    this.newToggle.classList.toggle("active", mode === "new_branch");
    this.existingToggle.classList.toggle("active", mode === "existing_branch");
    this.newFields.style.display = mode === "new_branch" ? "flex" : "none";
    this.existingFields.style.display = mode === "existing_branch" ? "flex" : "none";
    // In existing-branch mode the workspace name is the branch name (git
    // only allows one checkout per branch), so the name field is hidden.
    this.nameGroup.style.display = mode === "new_branch" ? "flex" : "none";
    this.validate();
  }

  private validate() {
    const reasons: string[] = [];

    if (this.branches.length === 0) {
      reasons.push("no-branches");
    }

    let warn: string | null = null;
    let adoptable = false;
    if (this.mode === "existing_branch") {
      const sel = this.selectedBranch(this.existingSelect);
      if (!sel) {
        reasons.push("no-branch");
      } else if (sel.external_path) {
        // Branch is checked out in a worktree Shard didn't create. Allow
        // submit; the caller will route to `adoptWorkspace` via `adoptPath`
        // on the result.
        adoptable = true;
        warn = `External worktree at ${sel.external_path}. Click Adopt to track it in Shard.`;
        // Compare against the sanitized form the daemon will derive — for
        // a branch like `feature/foo`, the workspace will land as
        // `feature-foo` and would otherwise collide silently on submit.
        const derivedName = safeWorkspaceName(sel.name);
        if (this.existingWorkspaceNames.includes(derivedName)) {
          reasons.push("name-collision");
          warn = `A workspace named "${derivedName}" already exists.`;
        }
      } else if (sel.checked_out_by) {
        reasons.push("occupied");
        warn = `Already checked out in workspace "${sel.checked_out_by}". Choose a different branch.`;
      } else if (this.existingWorkspaceNames.includes(sel.name)) {
        reasons.push("name-collision");
        warn = `A workspace named "${sel.name}" already exists.`;
      }
    } else {
      const name = this.nameInput.value.trim();
      if (!name) {
        reasons.push("name");
      } else if (/[<>:"/\\|?*\x00-\x1f]/.test(name)) {
        reasons.push("invalid-chars");
      }
      if (!this.selectedBranch(this.baseSelect)) {
        reasons.push("no-base");
      }
    }

    this.warnEl.textContent = warn ?? "";
    this.warnEl.classList.toggle("dialog-hint--warn", !adoptable);
    this.warnEl.style.display = warn ? "block" : "none";

    this.submitBtn.textContent = adoptable ? "Adopt Workspace" : "Create Workspace";
    this.submitBtn.disabled = reasons.length > 0;
  }

  private submit() {
    if (this.submitBtn.disabled) return;
    const picked =
      this.mode === "new_branch"
        ? this.selectedBranch(this.baseSelect)
        : this.selectedBranch(this.existingSelect);
    if (!picked) return;
    const name =
      this.mode === "new_branch" ? this.nameInput.value.trim() : picked.name;
    const adoptPath =
      this.mode === "existing_branch" && picked.external_path
        ? picked.external_path
        : undefined;
    this.close({ name, mode: this.mode, branch: picked.name, adoptPath });
  }

  private selectedBranch(select: HTMLSelectElement): BranchInfo | null {
    const value = select.value;
    if (!value) return null;
    return this.branches.find((b) => b.name === value) ?? null;
  }

  private populateSelect(select: HTMLSelectElement, branches: BranchInfo[], skipOccupied: boolean) {
    select.replaceChildren();
    if (branches.length === 0) {
      const opt = document.createElement("option");
      opt.value = "";
      opt.textContent = "— no branches —";
      opt.disabled = true;
      opt.selected = true;
      select.appendChild(opt);
      return;
    }
    for (const b of branches) {
      if (skipOccupied && b.checked_out_by) continue;
      const opt = document.createElement("option");
      opt.value = b.name;
      const tags: string[] = [];
      if (b.is_head) tags.push("HEAD");
      if (b.checked_out_by) tags.push(`in use: ${b.checked_out_by}`);
      opt.textContent = tags.length ? `${b.name}  (${tags.join(", ")})` : b.name;
      select.appendChild(opt);
    }
  }

  private generateName(): string {
    const d = new Date();
    const yyyy = d.getFullYear();
    const mm = String(d.getMonth() + 1).padStart(2, "0");
    const dd = String(d.getDate()).padStart(2, "0");
    const taken = new Set<string>([
      ...this.branches.map((b) => b.name),
      ...this.existingWorkspaceNames,
    ]);
    // Random suffix makes collisions vanishingly rare even across back-to-back
    // creates, avoiding a race where list_branches hasn't caught up yet.
    for (let i = 0; i < 20; i++) {
      const suffix = Math.floor(Math.random() * 0x10000)
        .toString(16)
        .padStart(4, "0");
      const candidate = `ws-${yyyy}-${mm}-${dd}-${suffix}`;
      if (!taken.has(candidate)) return candidate;
    }
    return `ws-${yyyy}-${mm}-${dd}-${Date.now().toString(16).slice(-6)}`;
  }

  private buildDialog() {
    const dialog = document.createElement("div");
    dialog.className = "dialog";

    // Header
    const header = document.createElement("div");
    header.className = "dialog-header";
    const title = document.createElement("span");
    title.className = "dialog-title";
    title.textContent = "New Workspace";
    const closeBtn = document.createElement("button");
    closeBtn.className = "dialog-close";
    closeBtn.textContent = "×";
    closeBtn.addEventListener("click", () => this.close(null));
    header.appendChild(title);
    header.appendChild(closeBtn);

    // Mode toggle
    const toggle = document.createElement("div");
    toggle.className = "dialog-toggle";
    this.newToggle = document.createElement("button");
    this.newToggle.className = "dialog-toggle-btn active";
    this.newToggle.textContent = "New Branch";
    this.newToggle.addEventListener("click", () => this.setMode("new_branch"));
    this.existingToggle = document.createElement("button");
    this.existingToggle.className = "dialog-toggle-btn";
    this.existingToggle.textContent = "Existing Branch";
    this.existingToggle.addEventListener("click", () => this.setMode("existing_branch"));
    toggle.appendChild(this.newToggle);
    toggle.appendChild(this.existingToggle);

    // Name field (only used in New Branch mode — existing mode uses the
    // branch name directly since git allows one checkout per branch).
    this.nameGroup = this.createFieldGroup("Name");
    this.nameInput = document.createElement("input");
    this.nameInput.className = "dialog-input mono";
    this.nameInput.placeholder = "ws-2026-04-16-a7f2";
    this.nameInput.addEventListener("input", () => this.validate());
    this.nameGroup.appendChild(this.nameInput);
    const nameHint = document.createElement("span");
    nameHint.className = "dialog-hint";
    nameHint.textContent =
      "Used as the workspace directory and the new branch name.";
    this.nameGroup.appendChild(nameHint);

    // New-branch fields
    this.newFields = document.createElement("div");
    this.newFields.className = "dialog-fields";
    const baseGroup = this.createFieldGroup("Base Branch");
    this.baseSelect = document.createElement("select");
    this.baseSelect.className = "dialog-input mono";
    baseGroup.appendChild(this.baseSelect);
    this.newFields.appendChild(baseGroup);

    // Existing-branch fields
    this.existingFields = document.createElement("div");
    this.existingFields.className = "dialog-fields";
    this.existingFields.style.display = "none";
    const existingGroup = this.createFieldGroup("Branch");
    this.existingSelect = document.createElement("select");
    this.existingSelect.className = "dialog-input mono";
    this.existingSelect.addEventListener("change", () => this.validate());
    existingGroup.appendChild(this.existingSelect);
    this.warnEl = document.createElement("span");
    this.warnEl.className = "dialog-hint dialog-hint--warn";
    this.warnEl.style.display = "none";
    existingGroup.appendChild(this.warnEl);
    this.existingFields.appendChild(existingGroup);

    // Footer
    const footer = document.createElement("div");
    footer.className = "dialog-footer";
    const cancelBtn = document.createElement("button");
    cancelBtn.className = "dialog-btn-ghost";
    cancelBtn.textContent = "Cancel";
    cancelBtn.addEventListener("click", () => this.close(null));
    this.submitBtn = document.createElement("button");
    this.submitBtn.className = "dialog-btn-primary";
    this.submitBtn.textContent = "Create Workspace";
    this.submitBtn.addEventListener("click", () => this.submit());
    footer.appendChild(cancelBtn);
    footer.appendChild(this.submitBtn);

    // Assemble
    const body = document.createElement("div");
    body.className = "dialog-body";
    body.appendChild(this.nameGroup);
    body.appendChild(this.newFields);
    body.appendChild(this.existingFields);

    dialog.appendChild(header);
    dialog.appendChild(toggle);
    dialog.appendChild(body);
    dialog.appendChild(footer);
    this.overlay.appendChild(dialog);

    dialog.addEventListener("keydown", (e) => {
      if (e.key === "Escape") {
        this.close(null);
        return;
      }
      // Enter on a button must run the button's own action, not submit
      // the form. Only treat Enter as submit when focus is on an
      // actual form field.
      if (e.key === "Enter" && !(e.target instanceof HTMLButtonElement)) {
        this.submit();
      }
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

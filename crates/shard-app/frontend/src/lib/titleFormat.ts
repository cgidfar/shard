/** Strip .exe extension (case-insensitive). */
export function stripExe(name: string): string {
  return name.replace(/\.exe$/i, "");
}

/** Derive a display label from command_json. */
export function labelFromCommand(commandJson: string): string {
  try {
    const cmd: string[] = JSON.parse(commandJson);
    const exe = cmd[0]?.split(/[/\\]/).pop() ?? "session";
    return stripExe(exe);
  } catch {
    return "session";
  }
}

/** Format an OSC terminal title for display.
 *  Only strips to last path component if it looks like a path. */
export function formatOscTitle(raw: string): string {
  const trimmed = raw.trim();
  if (!trimmed) return "";
  // If it looks like an absolute path, take the last component
  if (/^[A-Z]:[/\\]/i.test(trimmed) || trimmed.startsWith("/")) {
    const part = trimmed.split(/[/\\]/).filter(Boolean).pop() ?? trimmed;
    return stripExe(part);
  }
  // Otherwise use as-is (e.g. "nvim foo.rs", "user@host: repo")
  return trimmed;
}

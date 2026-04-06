import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import { Channel } from "@tauri-apps/api/core";
import { attachSession, writeToSession, resizeSession, detachSession } from "./api";
import { formatOscTitle } from "./titleFormat";

export interface TerminalSession {
  terminal: Terminal;
  fitAddon: FitAddon;
  dispose: () => void;
}

export interface TerminalSessionOptions {
  onTitleChange?: (title: string) => void;
}

export function createTerminalSession(
  sessionId: string,
  container: HTMLElement,
  options: TerminalSessionOptions = {}
): TerminalSession {
  const terminal = new Terminal({
    fontFamily: "'Geist Mono', 'Cascadia Code', 'Consolas', monospace",
    fontSize: 14,
    theme: {
      background: "#0f0d0a",
      foreground: "#e8ddd0",
      cursor: "#e8956a",
      selectionBackground: "#302820",
      black: "#302820",
      red: "#e06c75",
      green: "#a3d977",
      yellow: "#f0c75e",
      blue: "#7ab0d6",
      magenta: "#c895bf",
      cyan: "#7ec4a8",
      white: "#b0a08c",
      brightBlack: "#6b5d4f",
      brightRed: "#e88a92",
      brightGreen: "#b8e494",
      brightYellow: "#f5d67a",
      brightBlue: "#96c4e6",
      brightMagenta: "#daaed3",
      brightCyan: "#98d6bc",
      brightWhite: "#e8ddd0",
    },
    cursorBlink: true,
    allowProposedApi: true,
  });

  const fitAddon = new FitAddon();
  terminal.loadAddon(fitAddon);

  // Let browser handle clipboard shortcuts instead of xterm consuming them
  terminal.attachCustomKeyEventHandler((e: KeyboardEvent) => {
    if (e.type !== "keydown") return true;
    if (e.ctrlKey && !e.shiftKey && !e.altKey) {
      if (e.key === "c" && terminal.hasSelection()) return false;
      if (e.key === "v") return false;
    }
    if (e.ctrlKey && e.shiftKey && !e.altKey) {
      if (e.key === "C" || e.key === "V") return false;
    }
    return true;
  });

  terminal.open(container);

  // Load WebGL addon for GPU-accelerated rendering
  try {
    const webglAddon = new WebglAddon();
    terminal.loadAddon(webglAddon);
  } catch {
    console.warn("WebGL addon failed to load, falling back to canvas renderer");
  }

  fitAddon.fit();

  // Forward OSC title changes (e.g. shell sets CWD as window title)
  if (options.onTitleChange) {
    terminal.onTitleChange((raw) => {
      const formatted = formatOscTitle(raw);
      if (formatted) options.onTitleChange!(formatted);
    });
  }

  // Set up data channel from backend
  const channel = new Channel<Uint8Array>();
  channel.onmessage = (data: Uint8Array) => {
    terminal.write(data);
  };

  // Attach to session (sends Resume, starts receiving output)
  attachSession(sessionId, channel).catch((err) => {
    terminal.write(`\r\n\x1b[31mFailed to attach: ${err}\x1b[0m\r\n`);
  });

  // Forward user input to backend
  let disconnected = false;
  terminal.onData((data: string) => {
    if (disconnected) return;
    const encoder = new TextEncoder();
    writeToSession(sessionId, encoder.encode(data)).catch(() => {
      if (!disconnected) {
        disconnected = true;
        terminal.write("\r\n\x1b[31mSession ended.\x1b[0m\r\n");
      }
    });
  });

  // Handle resize
  const resizeObserver = new ResizeObserver(() => {
    fitAddon.fit();
    const dims = fitAddon.proposeDimensions();
    if (dims) {
      resizeSession(sessionId, dims.rows, dims.cols).catch(() => {});
    }
  });
  resizeObserver.observe(container);

  // Also send initial size
  const initialDims = fitAddon.proposeDimensions();
  if (initialDims) {
    resizeSession(sessionId, initialDims.rows, initialDims.cols).catch(() => {});
  }

  function dispose() {
    resizeObserver.disconnect();
    detachSession(sessionId).catch(() => {});
    terminal.dispose();
  }

  return { terminal, fitAddon, dispose };
}

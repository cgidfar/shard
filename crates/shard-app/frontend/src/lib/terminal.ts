import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import { Channel } from "@tauri-apps/api/core";
import { attachSession, writeToSession, resizeSession, detachSession } from "./api";

export interface TerminalSession {
  terminal: Terminal;
  fitAddon: FitAddon;
  dispose: () => void;
}

export function createTerminalSession(
  sessionId: string,
  container: HTMLElement
): TerminalSession {
  const terminal = new Terminal({
    fontFamily: "'Cascadia Code', 'Consolas', 'Courier New', monospace",
    fontSize: 14,
    theme: {
      background: "#11111b",
      foreground: "#cdd6f4",
      cursor: "#f5e0dc",
      selectionBackground: "#45475a",
      black: "#45475a",
      red: "#f38ba8",
      green: "#a6e3a1",
      yellow: "#f9e2af",
      blue: "#89b4fa",
      magenta: "#f5c2e7",
      cyan: "#94e2d5",
      white: "#bac2de",
      brightBlack: "#585b70",
      brightRed: "#f38ba8",
      brightGreen: "#a6e3a1",
      brightYellow: "#f9e2af",
      brightBlue: "#89b4fa",
      brightMagenta: "#f5c2e7",
      brightCyan: "#94e2d5",
      brightWhite: "#a6adc8",
    },
    cursorBlink: true,
    allowProposedApi: true,
  });

  const fitAddon = new FitAddon();
  terminal.loadAddon(fitAddon);

  terminal.open(container);

  // Load WebGL addon for GPU-accelerated rendering
  try {
    const webglAddon = new WebglAddon();
    terminal.loadAddon(webglAddon);
  } catch {
    console.warn("WebGL addon failed to load, falling back to canvas renderer");
  }

  fitAddon.fit();

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
  terminal.onData((data: string) => {
    const encoder = new TextEncoder();
    writeToSession(sessionId, encoder.encode(data)).catch(() => {});
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

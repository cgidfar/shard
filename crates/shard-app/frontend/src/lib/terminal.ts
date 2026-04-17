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
  const isWindowsHost = navigator.userAgent.includes("Windows");
  const terminal = new Terminal({
    fontFamily: "'Geist Mono', 'Cascadia Code', 'Consolas', monospace",
    fontSize: 14,
    theme: {
      background: "#0b0c0f",
      foreground: "#ffffff",
      cursor: "#c4758a",
      selectionBackground: "#292b30",
      black: "#292b30",
      red: "#d44e4e",
      green: "#4aba78",
      yellow: "#eac26c",
      blue: "#7ab0d6",
      magenta: "#c884a8",
      cyan: "#7ec4a8",
      white: "#9296a0",
      brightBlack: "#51555e",
      brightRed: "#e07070",
      brightGreen: "#73ce95",
      brightYellow: "#f5d684",
      brightBlue: "#96c4e6",
      brightMagenta: "#e0abcc",
      brightCyan: "#98d6bc",
      brightWhite: "#ffffff",
    },
    cursorBlink: true,
    allowProposedApi: true,
    // ConPTY does not behave like a Unix PTY around wrap/reflow. Tell xterm.js
    // so it enables the same Windows-specific heuristics VS Code depends on.
    windowsPty: isWindowsHost ? { backend: "conpty" } : undefined,
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

  // Route wheel scrolling through scrollLines() rather than xterm's pixel-based
  // viewport.scrollTop update. The default path accumulates sub-pixel drift
  // (xtermjs/xterm.js#4959) that eventually pushes the last row out of view;
  // scrollLines snaps scrollTop to ydisp*rowHeight exactly.
  let wheelPartialScroll = 0;
  terminal.attachCustomWheelEventHandler((ev: WheelEvent) => {
    if (ev.deltaY === 0 || ev.shiftKey) return true;
    // Alt-screen apps (less, vim) expect wheel→arrow translation from xterm.
    if (terminal.buffer.active.type === "alternate") return true;

    const opts = terminal.options;
    const scrollSensitivity = opts.scrollSensitivity ?? 1;
    const fastSensitivity = opts.fastScrollSensitivity ?? 5;
    const fastMod = opts.fastScrollModifier;
    const isFast =
      (fastMod === "alt" && ev.altKey) ||
      (fastMod === "ctrl" && ev.ctrlKey) ||
      (fastMod === "shift" && ev.shiftKey);
    let amount = ev.deltaY * scrollSensitivity * (isFast ? fastSensitivity : 1);

    if (ev.deltaMode === WheelEvent.DOM_DELTA_PIXEL) {
      const rowHeight = (terminal as unknown as { _core?: any })._core
        ?._renderService?.dimensions?.css?.cell?.height;
      if (!rowHeight) return true;
      amount /= rowHeight;
      wheelPartialScroll += amount;
      amount = Math.floor(Math.abs(wheelPartialScroll)) * Math.sign(wheelPartialScroll);
      wheelPartialScroll %= 1;
    } else if (ev.deltaMode === WheelEvent.DOM_DELTA_PAGE) {
      amount *= terminal.rows;
    }

    const lines = Math.trunc(amount);
    if (lines !== 0) {
      ev.preventDefault();
      terminal.scrollLines(lines);
    }
    return false;
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

  // Set up data channel from backend (binary transfer via Tauri Response)
  const channel = new Channel<ArrayBuffer>();
  channel.onmessage = (data: ArrayBuffer) => {
    terminal.write(new Uint8Array(data));
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
        // Restore main screen, show cursor, reset SGR (NOT full RIS — preserves scrollback)
        terminal.write("\x1b[?1049l\x1b[?25h\x1b[0m");
        terminal.write("\r\n\x1b[31mSession ended.\x1b[0m\r\n");
      }
    });
  });

  // Handle resize
  let lastRows = 0;
  let lastCols = 0;
  let resizeTimer: ReturnType<typeof setTimeout> | null = null;

  const resizeObserver = new ResizeObserver(() => {
    // Debounce BOTH fit() and resizeSession() together. Calling fit()
    // immediately reflows xterm.js locally, but ConPTY later sends its
    // own redraw after resize — the two independent reflows create
    // duplicate/garbled content. By deferring fit() until the drag
    // settles, xterm.js and ConPTY resize simultaneously.
    if (resizeTimer) clearTimeout(resizeTimer);
    resizeTimer = setTimeout(() => {
      fitAddon.fit();
      const dims = fitAddon.proposeDimensions();
      if (dims && (dims.rows !== lastRows || dims.cols !== lastCols)) {
        lastRows = dims.rows;
        lastCols = dims.cols;
        resizeSession(sessionId, dims.rows, dims.cols).catch(() => {});
      }
    }, 150);
  });
  resizeObserver.observe(container);

  // Also send initial size
  const initialDims = fitAddon.proposeDimensions();
  if (initialDims) {
    lastRows = initialDims.rows;
    lastCols = initialDims.cols;
    resizeSession(sessionId, initialDims.rows, initialDims.cols).catch(() => {});
  }

  function dispose() {
    if (resizeTimer) clearTimeout(resizeTimer);
    resizeObserver.disconnect();
    detachSession(sessionId).catch(() => {});
    terminal.dispose();
  }

  return { terminal, fitAddon, dispose };
}

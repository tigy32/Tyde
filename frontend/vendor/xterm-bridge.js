// Tyde2 xterm bridge.
//
// A tiny JS layer that wraps xterm.js so the Rust/WASM frontend can talk to a
// real terminal emulator through a minimal, serializable API.
//
// Loaded via a plain <script> tag in index.html. Depends on window.Terminal
// (xterm.js) and window.FitAddon (xterm-addon-fit) already being present.
//
// Exposed globally as window.TydeTerm, with methods:
//   create(id, container, onData, onResize) -> creates an xterm attached to container
//   write(id, data)                         -> writes a text chunk
//   dispose(id)                             -> tears the emulator down
//   fit(id)                                 -> fits to the current container size
//   attach(id, container)                   -> (re)attach an existing emulator to a DOM node
//   detach(id)                              -> detach from the current DOM node but keep state alive
//   size(id)                                -> { cols, rows } of the current emulator
//   focus(id)
//
// The bridge owns a Map of terminal-id -> { term, fit, container }. It is
// connection-scoped; callers are responsible for disposing before drop.

(function () {
    if (window.TydeTerm) {
        return;
    }

    const THEME = {
        background: "#0e0e0e",
        foreground: "#d4d4d4",
        cursor: "#d4d4d4",
        cursorAccent: "#0e0e0e",
        selectionBackground: "#264f78",
        black: "#000000",
        red: "#cd3131",
        green: "#4ec9b0",
        yellow: "#e5e510",
        blue: "#2472c8",
        magenta: "#bc3fbc",
        cyan: "#11a8cd",
        white: "#e5e5e5",
        brightBlack: "#666666",
        brightRed: "#f14c4c",
        brightGreen: "#23d18b",
        brightYellow: "#f5f543",
        brightBlue: "#3b8eea",
        brightMagenta: "#d670d6",
        brightCyan: "#29b8db",
        brightWhite: "#ffffff",
    };

    const terminals = new Map();

    function ensureLibs() {
        if (typeof window.Terminal !== "function") {
            throw new Error("xterm.js not loaded (window.Terminal is undefined)");
        }
    }

    function create(id, container, onData, onResize) {
        ensureLibs();
        if (terminals.has(id)) {
            // idempotent: re-attach if caller recreates on the same id
            return attach(id, container);
        }

        const term = new window.Terminal({
            fontFamily: "Menlo, Monaco, 'Courier New', monospace",
            fontSize: 13,
            lineHeight: 1.2,
            cursorBlink: true,
            cursorStyle: "block",
            scrollback: 10000,
            allowProposedApi: true,
            theme: THEME,
            convertEol: false,
        });

        let fit = null;
        if (typeof window.FitAddon === "object" && typeof window.FitAddon.FitAddon === "function") {
            fit = new window.FitAddon.FitAddon();
            term.loadAddon(fit);
        } else if (typeof window.FitAddon === "function") {
            fit = new window.FitAddon();
            term.loadAddon(fit);
        }

        term.open(container);
        if (term.textarea) {
            term.textarea.setAttribute('spellcheck', 'false');
            term.textarea.setAttribute('autocorrect', 'off');
            term.textarea.setAttribute('autocapitalize', 'none');
            term.textarea.setAttribute('autocomplete', 'off');
        }
        if (fit) {
            try {
                fit.fit();
            } catch (_) {}
        }

        if (typeof onData === "function") {
            term.onData((data) => {
                try {
                    onData(data);
                } catch (e) {
                    console.error("TydeTerm onData handler threw", e);
                }
            });
        }

        if (typeof onResize === "function") {
            term.onResize(({ cols, rows }) => {
                try {
                    onResize(cols, rows);
                } catch (e) {
                    console.error("TydeTerm onResize handler threw", e);
                }
            });
        }

        let observer = null;
        if (typeof window.ResizeObserver === "function" && fit) {
            observer = new window.ResizeObserver(() => {
                try {
                    fit.fit();
                } catch (_) {}
            });
            observer.observe(container);
        }

        terminals.set(id, { term, fit, container, observer });
        return { cols: term.cols, rows: term.rows };
    }

    function write(id, data) {
        const entry = terminals.get(id);
        if (!entry) return;
        entry.term.write(data);
    }

    function dispose(id) {
        const entry = terminals.get(id);
        if (!entry) return;
        if (entry.observer) {
            try {
                entry.observer.disconnect();
            } catch (_) {}
        }
        try {
            entry.term.dispose();
        } catch (_) {}
        terminals.delete(id);
    }

    function fit(id) {
        const entry = terminals.get(id);
        if (!entry || !entry.fit) return null;
        try {
            entry.fit.fit();
            return { cols: entry.term.cols, rows: entry.term.rows };
        } catch (_) {
            return null;
        }
    }

    function attach(id, container) {
        const entry = terminals.get(id);
        if (!entry) return null;
        // xterm.js does not support reparenting via open() once attached, so the
        // caller should avoid detaching in the first place. We keep attach as a
        // best-effort no-op if the DOM node is already the container, which is
        // the common Leptos-Show case.
        if (entry.container === container) {
            if (entry.fit) {
                try {
                    entry.fit.fit();
                } catch (_) {}
            }
            return { cols: entry.term.cols, rows: entry.term.rows };
        }
        // Move the xterm root element into the new container.
        while (container.firstChild) {
            container.removeChild(container.firstChild);
        }
        const el = entry.term.element;
        if (el && el.parentElement !== container) {
            container.appendChild(el);
        }
        entry.container = container;
        if (entry.fit) {
            try {
                entry.fit.fit();
            } catch (_) {}
        }
        return { cols: entry.term.cols, rows: entry.term.rows };
    }

    function detach(id) {
        const entry = terminals.get(id);
        if (!entry) return;
        entry.container = null;
    }

    function size(id) {
        const entry = terminals.get(id);
        if (!entry) return null;
        return { cols: entry.term.cols, rows: entry.term.rows };
    }

    function focus(id) {
        const entry = terminals.get(id);
        if (!entry) return;
        try {
            entry.term.focus();
        } catch (_) {}
    }

    window.TydeTerm = {
        create,
        write,
        dispose,
        fit,
        attach,
        detach,
        size,
        focus,
    };
})();

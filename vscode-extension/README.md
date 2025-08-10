# Codex VS Code Extension

Bridge VS Code and Codex via a simple, reactive IPC channel. The extension adds commands and a status bar button to:

- Open a new editor group
- Launch a terminal in the editor area (new group)
- Run Codex in that terminal at the workspace root
- Stream selection context to Codex and open native diffs for proposed changes

## Usage

- Press `Cmd+Shift+P` (macOS) or `Ctrl+Shift+P` (Windows/Linux) and run: `Codex: Open New Group and Run`.
- Or click the Codex action in the editor title bar (top-right).

### Highlighted selection auto-context

- Highlight text anywhere (files, notebooks, Output). Codex shows a live hint with the number of selected lines.
- When you send your next message, Codex automatically includes the current selection as a fenced code block with a helpful header.
- For non-file editors (like Output), the header uses a friendly label (e.g., `Output: Codex Extension`) instead of internal URIs.

## Installation

- From the marketplace: install “Codex” (`openai.codex-vscode`).
- Or simply run `codex` in a VS Code terminal: the CLI auto‑installs/updates the marketplace extension when needed.

To install a locally packaged build, see Development below.

## Development

- From VS Code, run `F5` to launch an Extension Development Host and test.
- To package for installation: `pnpm i && pnpm run vsce:package` (requires `@vscode/vsce`).
- Install the built `.vsix` via: `code --install-extension <file>.vsix`.

### Self-tests (no network, no separate runner)

- Open the repo in VS Code and press F5 to run the Dev Host.
- In the Dev Host, run the command: `Codex: Run Extension Self-Tests`.
- Note: This command appears only in the Extension Development Host (debug). It is hidden in regular VS Code to avoid clutter.
- What’s covered (mirrors real usage in the extension):
  - Updates: opens native VS Code diffs (green/red). Uses the extension’s `applyUnifiedDiff` + `openDiffs` logic.
  - Adds/Deletes: open meaningful colored diffs (empty ↔ content) so visuals are clear. Layout (split vs inline) follows your editor setting.
  - Proposed buffers: always computed and recorded so assertions are deterministic.
- Behavior and guardrails:
  - Tests do not change your VS Code settings (no workspace/global writes).
  - No file watchers are used; tests drive the same diff-opening code paths as production and verify in‑memory buffers.
  - Editors are closed at the start of each run and a single “Codex Self-Tests” output channel is reused to keep results tidy.
  - Results appear in the “Codex Self-Tests” Output panel plus a pass/fail notification. Artifacts are cleaned up on success; on failure, the path is printed for inspection.

Internals:
- The self-tests live in `vscode-extension/self-tests.js` and are registered from `extension.js` with a small services object. This keeps the production code (providers, diff logic) reusable by the tests.

### Using a locally built Codex

- The extension prefers a workspace-local binary at `codex-rs/target/debug/codex`.
- If not found, it falls back to `codex` on your PATH.
- You can override with `CODEX_BIN=/absolute/path/to/codex` in your environment.
- The Codex terminal is launched with `CODEX_BIN` set, so `echo $CODEX_BIN` in that terminal shows the chosen binary.

Build locally:

```
cd codex-rs
just fmt && just fix
cargo build -p codex-cli
```

## Notes

- The run command prefers the active editor's workspace folder as CWD; if none exists, it uses the terminal's default folder.
- The selection injection includes a header and fenced code block, attached to your next sent message.
- Ensure the `codex` binary is on your PATH.

## Live Selection Hint

- Selection updates stream over IPC to Codex as you select text in any editor (files, notebooks, Output, untitled, etc.).
- Codex shows a live hint under the composer like: `N lines highlighted in VS Code`.
- Selection events are reactive; there is no polling.

## Proposed Diffs View

- Codex sends proposed edits over IPC (`apply_patch` / `turn_diff`). The extension opens native VS Code diffs with clear titles.
- Updates: diff the on-disk file against a virtual proposed buffer. Adds/Deletes: show meaningful colored diffs (empty ↔ content) via virtual documents.
- VS Code controls layout (split vs inline) via your `diffEditor.renderSideBySide` setting.
- Approvals still happen in the Codex terminal UI; upon successful apply, the extension closes any opened proposal tabs.
- First opened diff is brought to the front explicitly; proposal tabs remain open (non‑preview) for review.

## Configuration

No user-configurable settings are required. The extension opens native diffs and keeps proposal tabs open for review by default.

## Logging & IPC

- Output channel: “Codex Extension” with timestamped logs for lifecycle, selections, diffs, and errors.
- IPC server: UNIX socket at `~/.codex/ide/codex-ipc.sock` (or Windows named pipe). The extension advertises its `ipc_path` in `~/.codex/ide/extension.json`.
- Privacy: selection content is sent to Codex over local IPC as you select; it’s only included in a message when you submit one.

### IPC Flow (at a glance)

```
Codex (Rust TUI)  ⇄  VS Code Extension (Node)
        │                  │
        │  NDJSON over     │  Listens on UNIX socket (~/.codex/ide/codex-ipc.sock)
        │  Unix socket /   │  or Windows named pipe.
        │  named pipe      │
        │                  │
  Sends: selection,        Receives: apply_patch/turn_diff → opens native diffs
         apply_patch,      Sends: selection events → live hint under composer
         turn_diff,        Closes: proposal tabs after successful apply
         patch_applied
```

## Troubleshooting

- Check logs: View → Output → “Codex Extension”. Errors and IPC events are timestamped.
- Verify IPC: open `~/.codex/ide/extension.json` and confirm `ipc_path` exists; on UNIX, ensure the socket path is present.
- Ensure `codex` binary: either on `PATH`, or set `CODEX_BIN=/absolute/path/to/codex`.
- In Dev Host: run “Codex: Run Extension Self-Tests” to validate diff opening and IPC end‑to‑end.
- Dev Host convenience: JSON parse or server errors also surface as lightweight warnings in the status area.

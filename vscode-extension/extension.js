// Minimal JS extension to open a new editor group and run Codex in a terminal
// No build step required

const vscode = require('vscode');
const fs = require('fs');
const net = require('net');
const path = require('path');
const os = require('os');

// Module-level handles so we can clean up on deactivate
let gIpcServer = undefined;
let gIpcPath = undefined;

let codexTerminal = undefined;
const CODEX_HOME = process.env.CODEX_HOME || path.join(os.homedir(), '.codex');
const IDE_DIR = path.join(CODEX_HOME, 'ide');
const EXT_MARKER_FILE = path.join(IDE_DIR, 'extension.json');

function ensureIdeDir() {
  try { fs.mkdirSync(IDE_DIR, { recursive: true }); } catch {}
}

function isExecutable(filePath) {
  try {
    fs.accessSync(filePath, fs.constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

async function resolveCodexBinary(cwd) {
  // 1) Explicit override via env
  const envBin = process.env.CODEX_BIN;
  if (envBin && isExecutable(envBin)) return envBin;

  // 2) Local debug build inside this repo
  //    Prefer cwd (workspace root) to locate repo-relative path
  try {
    const root = cwd || process.cwd();
    const exeName = process.platform === 'win32' ? 'codex.exe' : 'codex';
    const candidates = [
      path.join(root, 'codex-rs', 'target', 'debug', exeName), // monorepo root
      path.join(root, 'target', 'debug', exeName),              // opened at codex-rs/
    ];
    for (const p of candidates) {
      if (isExecutable(p)) return p;
    }
  } catch {}

  // 3) Fallback to PATH
  return 'codex';
}

/**
 * @param {vscode.ExtensionContext} context
 */
function activate(context) {
  const log = vscode.window.createOutputChannel('Codex Extension');
  const logLine = (msg) => { try { log.appendLine(`[${new Date().toISOString()}] ${msg}`); } catch {} };
  logLine('[Codex] activate()');
  // Determine dev/test mode and set a context key for menus
  const devMode = context.extensionMode && context.extensionMode !== vscode.ExtensionMode.Production;
  try { vscode.commands.executeCommand('setContext', 'codex.devMode', devMode); } catch {}
  // Prepare IPC server for Codex integration (duplex, NDJSON)
  ensureIdeDir();
  let ipcPath = undefined;
  if (process.platform === 'win32') {
    ipcPath = `\\\\.\\pipe\\codex-ipc-${process.pid}`;
  } else {
    ipcPath = path.join(IDE_DIR, 'codex-ipc.sock');
    try { if (fs.existsSync(ipcPath)) fs.unlinkSync(ipcPath); } catch {}
  }
  let ipcSocket = undefined; // last accepted connection (single client)
  let ipcServer = undefined;
  try {
    logLine(`[Codex] starting IPC server at ${ipcPath}`);
    ipcServer = net.createServer((socket) => {
      ipcSocket = socket;
      socket.setEncoding('utf8');
      logLine('[Codex] IPC client connected');
      let buf = '';
      socket.on('data', async (chunk) => {
        buf += chunk;
        let idx;
        while ((idx = buf.indexOf('\n')) >= 0) {
          const line = buf.slice(0, idx);
          buf = buf.slice(idx + 1);
          if (!line.trim()) continue;
          try {
            const msg = JSON.parse(line);
            // Handle proposal/status inbound from Codex
            if (msg && (msg.type === 'apply_patch' || msg.type === 'apply_patch_raw' || msg.type === 'turn_diff')) {
              try {
                const cnt = msg && msg.changes ? Object.keys(msg.changes).length : 0;
                logLine(`[Codex] IPC recv: ${msg.type} title="${msg.title || ''}" changes=${cnt}`);
              } catch {}
              const persist = true; // keep diff tabs open for review
              const label = typeof msg.title === 'string' && msg.title.trim() ? msg.title.trim() : (new Date()).toLocaleTimeString();
              openedDiffPairs = [];
              try {
                const pairs = await openDiffs(msg, persist, label);
                if (pairs && pairs.length) {
                  openedDiffPairs = pairs;
                  logLine(`[Codex] IPC opened ${pairs.length} diff tab(s)`);
                } else {
                  logLine('[Codex] IPC opened 0 diff tabs');
                }
              } catch (e) {
                console.error('Codex IPC: open diffs error', e);
                try { logLine(`[Codex] IPC open diffs error: ${String(e)}`); } catch {}
              }
            } else if (msg && (msg.type === 'patch_applied' || msg.type === 'patch_status')) {
              try { logLine(`[Codex] IPC recv: ${msg.type} success=${msg.success}`); } catch {}
              if (msg.success) {
                closeCodexEditors();
                openedDiffPairs = [];
              }
            }
          } catch (e) {
            try { logLine(`[Codex] IPC parse error: ${String(e)}`); } catch {}
            if (devMode) {
              try { vscode.window.showWarningMessage(`Codex IPC parse error: ${String(e)}`); } catch {}
            }
          }
        }
      });
      const onClose = (ev) => { if (ipcSocket === socket) { ipcSocket = undefined; logLine('[Codex] IPC client disconnected'); } };
      const onError = (err) => { if (ipcSocket === socket) { ipcSocket = undefined; logLine(`[Codex] IPC client error: ${String(err)}`); } };
      socket.on('close', onClose);
      socket.on('error', onError);
    });
    ipcServer.on('error', (e) => {
      try { logLine(`[Codex] IPC server error: ${String(e)}`);} catch {}
      if (devMode) {
        try { vscode.window.showWarningMessage(`Codex IPC server error: ${String(e)}`); } catch {}
      }
    });
    ipcServer.listen(ipcPath, () => { try { logLine('[Codex] IPC server listening'); } catch {} });
    gIpcServer = ipcServer;
    gIpcPath = ipcPath;
  } catch (e) {
    try { logLine(`[Codex] IPC server failed: ${String(e)}`); } catch {}
  }
  // Mark extension as active for Codex to detect, including IPC path
  try {
    fs.writeFileSync(EXT_MARKER_FILE, JSON.stringify({ ts: Date.now(), pid: process.pid, version: '0.0.1', ipc_path: ipcPath }));
    try { logLine(`[Codex] extension marker updated (ipc_path=${ipcPath})`);} catch {}
  } catch {}
  const commandId = 'codex.openNewGroupAndRun';

  const disposable = vscode.commands.registerCommand(commandId, async () => {
    // Prefer the workspace folder of the active editor; fallback to first folder
    let cwd;
    const activeEditor = vscode.window.activeTextEditor;
    if (activeEditor) {
      const wsFolder = vscode.workspace.getWorkspaceFolder(activeEditor.document.uri);
      if (wsFolder) cwd = wsFolder.uri.fsPath;
    }
    if (!cwd) {
      const folders = vscode.workspace.workspaceFolders;
      if (folders && folders.length > 0) {
        cwd = folders[0].uri.fsPath;
      }
    }
    // Create a new editor group on the right
    try {
      await vscode.commands.executeCommand('workbench.action.newGroupRight');
    } catch (err) {
      console.error('Codex: failed to create a new group', err);
    }

    // Resolve binary first so we can expose it via env for the terminal
    const codexBin = await resolveCodexBinary(cwd);

    // Create a terminal in the editor area (new group on the right)
    const location = { viewColumn: vscode.ViewColumn.Active };
    const env = { CODEX_BIN: codexBin, CODEX_HOME };
    // Prepend directory of codexBin to PATH so `which codex` reflects the chosen binary
    const dir = codexBin.includes(path.sep) ? path.dirname(codexBin) : undefined;
    if (dir) {
      env.PATH = `${dir}${path.delimiter}${process.env.PATH || ''}`;
    }
    const term = cwd
      ? vscode.window.createTerminal({ name: 'Codex', cwd, location, env })
      : vscode.window.createTerminal({ name: 'Codex', location, env });
    term.show(true);
    term.sendText(codexBin);
    codexTerminal = term;
  });

  context.subscriptions.push(disposable);

  // Track last selection signature to avoid spammy logs and writes
  let lastSelectionSig = undefined;

  // Compute a friendly label for the current editor selection source
  function selectionLabelForDocument(document) {
    try {
      const uri = document?.uri;
      if (!uri) return 'Selection';
      if (uri.scheme === 'file') {
        return vscode.workspace.asRelativePath(uri, false);
      }
      if (uri.scheme === 'untitled') {
        return 'Untitled';
      }
      if (uri.scheme === 'vscode-notebook-cell') {
        return 'Notebook Cell';
      }
      if (uri.scheme === 'output') {
        const base = path.basename(uri.path || '');
        const chan = base && base.includes('-') ? base.substring(base.lastIndexOf('-') + 1) : 'Output';
        return `Output: ${chan}`;
      }
      // Fallback: Capitalize scheme name
      const s = String(uri.scheme || '').trim();
      return s ? (s.charAt(0).toUpperCase() + s.slice(1)) : 'Selection';
    } catch {
      return 'Selection';
    }
  }

  // Command: paste current selection into Codex composer (no newline)
  const pasteCmd = vscode.commands.registerCommand('codex.pasteSelectionToCodex', async () => {
    // Resolve target terminal: prefer existing codexTerminal; else find by name
    let term = codexTerminal;
    if (!term) {
      term = vscode.window.terminals.find(t => t.name === 'Codex');
    }
    if (!term) {
      const choice = await vscode.window.showInformationMessage(
        'Codex terminal not found. Start Codex?',
        'Start Codex',
        'Cancel'
      );
      if (choice === 'Start Codex') {
        await vscode.commands.executeCommand(commandId);
        // try again to find terminal
        term = codexTerminal || vscode.window.terminals.find(t => t.name === 'Codex');
      }
    }
    if (!term) {
      return; // user cancelled or still not available
    }

    const editor = vscode.window.activeTextEditor;
    if (!editor) {
      vscode.window.showWarningMessage('No active editor to take selection from.');
      return;
    }

    const sel = editor.selection;
    if (sel.isEmpty) {
      vscode.window.showWarningMessage('No selection. Select text to send to Codex.');
      return;
    }

    const document = editor.document;
    const selectedText = document.getText(sel);
    const lang = document.languageId || '';
    const relPath = selectionLabelForDocument(document);
    const startLine = sel.start.line + 1;
    const endLine = sel.end.line + 1;

    // Build a helpful snippet to inject without submitting
    const header = `From ${relPath}:L${startLine}-L${endLine}\n`;
    const fenceLang = lang && /^[\w-]+$/.test(lang) ? lang : '';
    const block = '```' + fenceLang + '\n' + selectedText.replaceAll('```', '\u0060\u0060\u0060') + '\n```\n';
    const snippet = header + block;

    term.show(true);
    term.sendText(snippet, false); // do not add newline; user submits in Codex
  });
  context.subscriptions.push(pasteCmd);

  // Self-tests are registered from a separate module for clarity and reuse
  // Moved to the end of activate() after all services are defined

  // track terminal closures
  context.subscriptions.push(vscode.window.onDidCloseTerminal(t => {
    if (t === codexTerminal) {
      codexTerminal = undefined;
    }
  }));

  // Listen to selection changes and update selection.json
  context.subscriptions.push(vscode.window.onDidChangeTextEditorSelection(e => {
    const editor = e.textEditor;
    if (!editor) return;
    const sel = editor.selection;
    let totalLines = 0;
    let totalChars = 0;
    let text = '';
    let relPath = '';
    let languageId = editor.document.languageId || '';
    let startLine = 0;
    let endLine = 0;
    if (sel && !sel.isEmpty) {
      totalLines = Math.abs(sel.end.line - sel.start.line) + 1;
      text = editor.document.getText(sel);
      totalChars = text.length;
      startLine = sel.start.line + 1;
      endLine = sel.end.line + 1;
      relPath = selectionLabelForDocument(editor.document);
    }
    // Write extended payload including text and metadata
    ensureIdeDir();
    const payload = {
      lines: totalLines,
      characters: totalChars,
      ts: Date.now(),
      text,
      path: relPath,
      languageId,
      startLine,
      endLine
    };
    try {
      // Only act when selection actually changed to avoid spam
      const sig = `${payload.path || ''}|${payload.startLine || 0}|${payload.endLine || 0}|${payload.lines}|${payload.characters}`;
      if (sig === lastSelectionSig) return;
      lastSelectionSig = sig;

      // Push over IPC only
      const msg = { type: 'selection', ...payload };
      try {
        if (ipcSocket && !ipcSocket.destroyed) {
          ipcSocket.write(JSON.stringify(msg) + '\n');
          if (payload.lines === 0 && payload.characters === 0) {
            logLine('[Codex] selection cleared');
          } else {
            logLine(`[Codex] selection -> IPC: lines=${payload.lines} chars=${payload.characters} path=${payload.path || '<none>'}`);
          }
        } else {
          logLine('[Codex] selection dropped (no IPC client)');
        }
      } catch (err) {
        try { logLine(`[Codex] selection IPC write failed: ${String(err)}`);} catch {}
      }
    } catch (e) {
      console.error('Codex: failed to write selection info', e);
      try { logLine(`[Codex] selection handler error: ${String(e)}`);} catch {}
    }
  }));

  // No temp files are needed for diffs; we use virtual providers for proposed/empty content.

  // During self-tests we suppress watcher-driven UI opens to avoid duplicates
  let suppressWatcherOpen = false;

  // Virtual provider for newly added files (so users can preview content)
  const newFileEmitter = new vscode.EventEmitter();
  context.subscriptions.push(newFileEmitter);
  const newFileScheme = 'codex-new';
  const newFileContents = new Map(); // key: uri.path, value: file content
  context.subscriptions.push(
    vscode.workspace.registerTextDocumentContentProvider(newFileScheme, {
      onDidChange: newFileEmitter.event,
      provideTextDocumentContent: (uri) => newFileContents.get(uri.path) || ''
    })
  );

  // Virtual provider for proposed content (updates) to show vscode.diff
  const proposedEmitter = new vscode.EventEmitter();
  context.subscriptions.push(proposedEmitter);
  const proposedScheme = 'codex-proposed';
  const proposedContents = new Map(); // key: abs path or normalized path
  context.subscriptions.push(
    vscode.workspace.registerTextDocumentContentProvider(proposedScheme, {
      onDidChange: proposedEmitter.event,
      provideTextDocumentContent: (uri) => proposedContents.get(uri.path) || ''
    })
  );
  // Track opened diff pairs for optional auto-close
  let openedDiffPairs = [];
  // Empty provider to diff new files against
  const emptyScheme = 'codex-empty';
  context.subscriptions.push(
    vscode.workspace.registerTextDocumentContentProvider(emptyScheme, {
      onDidChange: undefined,
      provideTextDocumentContent: () => ''
    })
  );

  // Normalize paths and change shapes for cleaner handling
  function resolveAbsPath(root, p) {
    let abs = p;
    if (!path.isAbsolute(abs) && root) abs = path.join(root, p);
    return abs;
  }

  function normalizeChange(change) {
    if (!change || typeof change !== 'object') return { kind: 'unknown' };
    const kindRaw = change.type || Object.keys(change)[0] || '';
    const kind = String(kindRaw).toLowerCase();
    const updateObj = change.Update || change.update || change;
    const addObj = change.Add || change.add || change;
    const delObj = change.Delete || change.delete || change;
    if (kind === 'update' || typeof updateObj.unified_diff === 'string') {
      return { kind: 'update', unifiedDiff: updateObj.unified_diff, movePath: updateObj.move_path };
    }
    if (kind === 'add' || typeof addObj.content === 'string') {
      return { kind: 'add', content: addObj.content };
    }
    if (kind === 'delete' || delObj === null) {
      return { kind: 'delete' };
    }
    return { kind: 'unknown' };
  }

  function closeCodexEditors() {
    try {
      const groups = vscode.window.tabGroups.all || [];
      const toClose = [];
      for (const g of groups) {
        for (const tab of g.tabs) {
          const input = tab.input;
          // TabInputText
          const sch = input && input.uri && input.uri.scheme;
          if (sch === proposedScheme || sch === emptyScheme) {
            toClose.push(tab);
            continue;
          }
          // TabInputTextDiff (original/modified)
          const origSch = input && input.original && input.original.scheme;
          const modSch = input && input.modified && input.modified.scheme;
          if (origSch === proposedScheme || origSch === emptyScheme || modSch === proposedScheme || modSch === emptyScheme) {
            toClose.push(tab);
          }
        }
      }
      if (toClose.length > 0) {
        logLine(`[Codex] closing ${toClose.length} proposal tab(s)`);
        vscode.window.tabGroups.close(toClose, true);
      } else {
        logLine('[Codex] no proposal tabs to close');
      }
    } catch (e) {
      console.error('Codex: failed to close proposal tabs', e);
    }
  }

  // Apply a minimal unified diff to original text and return proposed text
  function applyUnifiedDiff(original, unified) {
    const origLines = original.split('\n');
    let iOrig = 0; // 0-based index into origLines
    const out = [];
    const lines = unified.split('\n');
    let idx = 0;
    // Skip headers if present
    while (idx < lines.length && (lines[idx].startsWith('---') || lines[idx].startsWith('+++'))) idx++;
    const hunkHeader = /^@@\s*-([0-9]+)(?:,([0-9]+))?\s*\+([0-9]+)(?:,([0-9]+))?\s*@@/;
    while (idx < lines.length) {
      const m = lines[idx].match(hunkHeader);
      if (!m) { idx++; continue; }
      idx++;
      const origStart = parseInt(m[1], 10) || 1; // 1-based
      // Append unchanged lines before hunk
      const target = Math.max(0, origStart - 1);
      while (iOrig < target) { out.push(origLines[iOrig++] || ''); }
      // Process hunk lines until next hunk or end
      while (idx < lines.length && !lines[idx].startsWith('@@')) {
        const ln = lines[idx];
        if (ln.startsWith(' ')) {
          out.push(origLines[iOrig++] || '');
        } else if (ln.startsWith('-')) {
          iOrig++; // skip deletion
        } else if (ln.startsWith('+')) {
          out.push(ln.slice(1));
        } else if (ln.startsWith('\\ No newline at end of file')) {
          // ignore
        } else {
          // treat as context
          out.push(origLines[iOrig++] || '');
        }
        idx++;
      }
    }
    // Append any remaining original lines
    while (iOrig < origLines.length) { out.push(origLines[iOrig++] || ''); }
    return out.join('\n');
  }

  async function openDiffs(json, persistTabs, label) {
    if (!json || typeof json !== 'object') return;
    const ws = vscode.workspace.workspaceFolders && vscode.workspace.workspaceFolders[0];
    const root = (typeof json.cwd === 'string' && json.cwd) || (ws ? ws.uri.fsPath : undefined);
    try { logLine(`[Codex] openDiffs root=${root || '<none>'} from=${json && json.type}`);} catch {}
    const pairs = [];
    if (json.type === 'apply_patch' && json.changes) {
      for (const [p, raw] of Object.entries(json.changes)) {
        const ch = normalizeChange(raw);
        const abs = resolveAbsPath(root, p);
        try { logLine(`[Codex] change: path=${p} kind=${ch.kind} abs=${abs}`);} catch {}
        try {
          const base = path.basename(p);
          const title = label ? `${label} • ${base}` : `Proposed • ${p}`;
          const normKey = path.isAbsolute(p) ? p.replace(/\\/g, '/') : ('/' + p).replace(/\\/g, '/');
          if (ch.kind === 'update') {
            if (!ch.unifiedDiff) { try { logLine('[Codex]  skip: no unifiedDiff'); } catch {} continue; }
            if (!fs.existsSync(abs)) { try { logLine('[Codex]  skip: left file does not exist'); } catch {} continue; }
            const orig = fs.readFileSync(abs, 'utf8');
            const proposed = applyUnifiedDiff(orig, ch.unifiedDiff);
            const left = vscode.Uri.file(abs);
            const right = vscode.Uri.from({ scheme: proposedScheme, path: normKey });
            try { proposedContents.set(normKey, proposed); } catch {}
            proposedEmitter.fire(right);
            const showOpts = { preview: !persistTabs, preserveFocus: false, viewColumn: vscode.ViewColumn.Active };
            await vscode.commands.executeCommand('vscode.diff', left, right, title, showOpts);
            if (persistTabs) { await vscode.commands.executeCommand('workbench.action.keepEditor'); }
            pairs.push({ left, right, title, groupIndex: 0 });
            try { logLine('[Codex]  opened diff tab (update)'); } catch {}
          } else if (ch.kind === 'add') {
            const proposed = ch.content || '';
            const left = vscode.Uri.from({ scheme: emptyScheme, path: normKey });
            const right = vscode.Uri.from({ scheme: proposedScheme, path: normKey });
            proposedContents.set(normKey, proposed);
            proposedEmitter.fire(right);
            const showOpts = { preview: !persistTabs, preserveFocus: false, viewColumn: vscode.ViewColumn.Active };
            await vscode.commands.executeCommand('vscode.diff', left, right, title, showOpts);
            if (persistTabs) { await vscode.commands.executeCommand('workbench.action.keepEditor'); }
            pairs.push({ left, right, title, groupIndex: 0 });
            try { logLine('[Codex]  opened diff tab (add)'); } catch {}
          } else if (ch.kind === 'delete') {
            let left;
            if (fs.existsSync(abs)) {
              left = vscode.Uri.file(abs);
            } else if (devMode) {
              // Dev fallback: show a placeholder left so tests still open a diff tab
              const placeholder = 'deleted file (original content unavailable)\n';
              proposedContents.set(normKey, placeholder);
              left = vscode.Uri.from({ scheme: proposedScheme, path: normKey });
              proposedEmitter.fire(left);
              try { logLine('[Codex]  delete fallback: placeholder left (dev mode)'); } catch {}
            } else {
              try { logLine('[Codex]  skip delete: file does not exist'); } catch {}
              continue;
            }
            const right = vscode.Uri.from({ scheme: emptyScheme, path: normKey });
            const showOpts = { preview: !persistTabs, preserveFocus: false, viewColumn: vscode.ViewColumn.Active };
            await vscode.commands.executeCommand('vscode.diff', left, right, title, showOpts);
            if (persistTabs) { await vscode.commands.executeCommand('workbench.action.keepEditor'); }
            pairs.push({ left, right, title, groupIndex: 0 });
            try { logLine('[Codex]  opened diff tab (delete)'); } catch {}
          } else {
            try { logLine('[Codex]  skip: unknown change kind'); } catch {}
          }
        } catch (e) {
          console.error('Codex: failed to compute proposed diff for', p, e);
        }
      }
    } else if (json.type === 'apply_patch_raw' && typeof json.patch === 'string') {
      // Parse simple apply_patch format for Update sections only (adds/deletes: unified doc only)
      const lines = json.patch.split('\n');
      let i = 0;
      function normPath(p) { return ('/' + p).replace(/\\/g, '/'); }
      while (i < lines.length) {
        const header = lines[i];
        const m = header && header.startsWith('*** ')
          ? header.match(/^\*\*\*\s+(Add File|Update File|Delete File):\s+(.+)$/)
          : null;
        if (!m) { i++; continue; }
        const kind = m[1];
        const rel = m[2].trim();
        i++;
        const body = [];
        while (i < lines.length && !lines[i].startsWith('*** ')) {
          body.push(lines[i++]);
        }
        // Resolve abs path for existing file
        let abs = rel;
        if (!path.isAbsolute(abs) && root) abs = path.join(root, rel);
        if (kind === 'Update File') {
          if (fs.existsSync(abs)) {
            const left = vscode.Uri.file(abs);
            const bodyText = body.join('\n');
            let proposed = bodyText;
            try {
              const orig = fs.readFileSync(abs, 'utf8');
              // If hunk header looks like a real unified hunk with numbers, try to apply
              if (/@@\s*-?\d/.test(bodyText)) {
                proposed = applyUnifiedDiff(orig, bodyText);
              } else {
                // Heuristic: if body contains only added lines ("+...") and no deletions,
                // assume it's an insertion; show original plus added lines as proposed.
                const plus = body.filter(l => l.startsWith('+')).map(l => l.slice(1)).join('\n');
                const hasMinus = body.some(l => l.startsWith('-'));
                if (!hasMinus && plus.trim().length > 0) {
                  proposed = orig.replace(/\n?$/, '\n') + plus + '\n';
                }
              }
            } catch {}
            const norm = normPath(rel);
            const right = vscode.Uri.from({ scheme: proposedScheme, path: norm });
            // Always store proposed buffer for tests/inspection
            try { proposedContents.set(norm, proposed); } catch {}
            proposedEmitter.fire(right);
            const base = path.basename(rel);
            const title = label ? `${label} • ${base}` : `Proposed • ${rel}`;
            const showOpts = { preview: !persistTabs, preserveFocus: false, viewColumn: vscode.ViewColumn.Active };
            await vscode.commands.executeCommand('vscode.diff', left, right, title, showOpts);
            if (persistTabs) {
              await vscode.commands.executeCommand('workbench.action.keepEditor');
            }
            pairs.push({ left, right, title, groupIndex: 0 });
          }
        }
      }
    }
    // Bring the first diff to front explicitly, in case later opens stole focus.
    try {
      if (pairs.length > 0) {
        const first = pairs[0];
        const showOpts = { preview: !persistTabs, preserveFocus: false, viewColumn: vscode.ViewColumn.Active };
        await vscode.commands.executeCommand('vscode.diff', first.left, first.right, first.title, showOpts);
      }
    } catch (e) {
      console.error('Codex: failed to focus first diff', e);
    }
    return pairs;
  }

  // Register self-tests after all services are defined (dev/test only)
  try {
    if (devMode) {
      const selfTests = require('./self-tests');
      const services = {
        setWatcherSuppressed: (v) => { suppressWatcherOpen = !!v; },
        applyUnifiedDiff,
        openDiffs,
        proposedContents,
      };
      selfTests.registerSelfTests(context, services);
    } else {
      // Provide a friendly stub to avoid "command not found" if invoked
      const stub = vscode.commands.registerCommand('codex.runSelfTests', () => {
        vscode.window.showInformationMessage('Codex self-tests are available only in Extension Development Host.');
      });
      context.subscriptions.push(stub);
    }
  } catch (e) {
    console.error('Codex: failed to register self-tests', e);
  }

  // Group management helpers no longer needed; diffs open in active group
}

function deactivate() {
  try { if (gIpcServer) { gIpcServer.close(); } } catch {}
  try {
    if (gIpcPath && process.platform !== 'win32') {
      if (fs.existsSync(gIpcPath)) fs.unlinkSync(gIpcPath);
    }
  } catch {}
  // No tmp diff folders to clean; diffs use virtual providers.
}

module.exports = { activate, deactivate };

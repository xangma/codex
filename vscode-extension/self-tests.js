// Extracted self-tests for the Codex VS Code extension
// These tests exercise the same diff-opening logic used by the extension.

const vscode = require('vscode');
const fs = require('fs');
const path = require('path');
const os = require('os');
const net = require('net');

let OUT_CHAN_SINGLETON = undefined;

function registerSelfTests(context, services) {
  const cmd = vscode.commands.registerCommand('codex.runSelfTests', async () => {
    services.setWatcherSuppressed(true);

    const tmpRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'codex-tests-'));
    const out = (OUT_CHAN_SINGLETON ||= vscode.window.createOutputChannel('Codex Self-Tests'));
    try { out.clear(); } catch {}
    out.show(true);
    try { await vscode.commands.executeCommand('workbench.action.closeAllEditors'); } catch {}
    const results = [];

    function assert(cond, msg) {
      if (cond) {
        results.push({ ok: true, msg });
        out.appendLine(`PASS: ${msg}`);
      } else {
        results.push({ ok: false, msg });
        out.appendLine(`FAIL: ${msg}`);
      }
    }

    function hasProposed(pathEndsWith, expectedContains) {
      const needle = '/' + pathEndsWith.replace(/\\/g, '/');
      let content;
      for (const [k, v] of services.proposedContents.entries()) {
        if (k.endsWith(needle)) { content = v; break; }
      }
      if (!content) return false;
      if (expectedContains && !content.includes(expectedContains)) return false;
      return true;
    }

    // Establish IPC connection to the extension (duplex NDJSON)
    const CODEX_HOME = process.env.CODEX_HOME || path.join(os.homedir(), '.codex');
    const IDE_DIR = path.join(CODEX_HOME, 'ide');
    const EXT_MARKER_FILE = path.join(IDE_DIR, 'extension.json');
    let ipcPath;
    try {
      const marker = JSON.parse(fs.readFileSync(EXT_MARKER_FILE, 'utf8'));
      ipcPath = marker && marker.ipc_path;
      out.appendLine(`IPC path: ${ipcPath || '<none>'}`);
    } catch {}
    let ipcSocket;
    function sendIpc(obj) {
      return new Promise((resolve, reject) => {
        if (!ipcSocket || ipcSocket.destroyed) return reject(new Error('IPC not connected'));
        try {
          ipcSocket.write(JSON.stringify(obj) + '\n');
          resolve();
        } catch (e) {
          reject(e);
        }
      });
    }
    if (ipcPath) {
      try {
        ipcSocket = net.createConnection({ path: ipcPath });
        ipcSocket.setEncoding('utf8');
        await new Promise((res, rej) => {
          ipcSocket.once('connect', res);
          ipcSocket.once('error', rej);
        });
        out.appendLine('IPC connected');
      } catch (e) {
        out.appendLine(`IPC connect failed: ${e}`);
      }
    }

    // Test 1: File creation (via IPC add)
    try {
      const createPath = path.join(tmpRoot, 'create_1.txt');
      const content = 'line1\nline2\n';
      const json1 = { type: 'apply_patch', changes: { [createPath]: { add: { content } } }, title: 'Test 1 – Create' };
      if (ipcSocket && !ipcSocket.destroyed) {
        await sendIpc(json1);
      } else {
        await services.openDiffs(json1, true, 'Test 1 – Create');
      }
      let tries = 10, seen = false;
      while (tries-- > 0) {
        if (hasProposed('create_1.txt', 'line2')) { seen = true; break; }
        await new Promise(r => setTimeout(r, 200));
      }
      assert(seen, 'Opened diff for file creation (add)');
    } catch (e) {
      assert(false, `Creation diff threw error: ${e}`);
    }

    // Test 2: Line addition → split diff (via IPC)
    try {
      const addLinePath = path.join(tmpRoot, 'add_line.txt');
      fs.writeFileSync(addLinePath, 'alpha\nbeta\ngamma\n');
      const unified = ['--- a/x', '+++ b/x', '@@ -1,3 +1,4 @@', ' alpha', ' beta', '+beta2', ' gamma', ''].join('\n');
      const json2 = { type: 'apply_patch', changes: { [addLinePath]: { unified_diff: unified } }, title: 'Test 2 – Add Line' };
      if (ipcSocket && !ipcSocket.destroyed) {
        await sendIpc(json2);
      } else {
        await services.openDiffs(json2, true, 'Test 2 – Add Line');
      }
      let tries = 10, seen = false;
      while (tries-- > 0) {
        if (hasProposed('add_line.txt', 'beta2')) { seen = true; break; }
        await new Promise(r => setTimeout(r, 200));
      }
      assert(seen, 'Computed proposed buffer for line addition');
    } catch (e) {
      assert(false, `Line addition diff threw error: ${e}`);
    }

    // Test 3: Line modification → split diff (via IPC)
    try {
      const modPath = path.join(tmpRoot, 'modify.txt');
      fs.writeFileSync(modPath, 'one\ntwo\nthree\n');
      const unified2 = ['--- a/y', '+++ b/y', '@@ -1,3 +1,3 @@', ' one', '-two', '+two2', ' three', ''].join('\n');
      const json3 = { type: 'apply_patch', changes: { [modPath]: { unified_diff: unified2 } }, title: 'Test 3 – Modify Line' };
      if (ipcSocket && !ipcSocket.destroyed) {
        await sendIpc(json3);
      } else {
        await services.openDiffs(json3, true, 'Test 3 – Modify Line');
      }
      let tries = 10, seen = false;
      while (tries-- > 0) {
        if (hasProposed('modify.txt', 'two2')) { seen = true; break; }
        await new Promise(r => setTimeout(r, 200));
      }
      assert(seen, 'Computed proposed buffer for line modification');
    } catch (e) {
      assert(false, `Line modification diff threw error: ${e}`);
    }

    // Test 4: Line removal → split diff (via IPC)
    try {
      const remPath = path.join(tmpRoot, 'remove_line.txt');
      fs.writeFileSync(remPath, 'aa\nbb\ncc\n');
      const unified3 = ['--- a/z', '+++ b/z', '@@ -1,3 +1,2 @@', ' aa', '-bb', ' cc', ''].join('\n');
      const json4 = { type: 'apply_patch', changes: { [remPath]: { unified_diff: unified3 } }, title: 'Test 4 – Remove Line' };
      if (ipcSocket && !ipcSocket.destroyed) {
        await sendIpc(json4);
      } else {
        await services.openDiffs(json4, true, 'Test 4 – Remove Line');
      }
      let tries = 10, seen = false;
      while (tries-- > 0) {
        if (hasProposed('remove_line.txt') && !hasProposed('remove_line.txt', 'bb\n')) { seen = true; break; }
        await new Promise(r => setTimeout(r, 200));
      }
      assert(seen, 'Computed proposed buffer for line removal');
    } catch (e) {
      assert(false, `Line removal diff threw error: ${e}`);
    }

    // Test 6: Multiple updates in one message → first diff should be focused
    try {
      const f1 = path.join(tmpRoot, 'multi1.txt');
      const f2 = path.join(tmpRoot, 'multi2.txt');
      fs.writeFileSync(f1, 'a\nb\n');
      fs.writeFileSync(f2, 'x\ny\n');
      const u1 = ['--- a/p', '+++ b/p', '@@ -1,2 +1,2 @@', ' a', '-b', '+b2', ''].join('\n');
      const u2 = ['--- a/q', '+++ b/q', '@@ -1,2 +1,3 @@', ' x', ' y', '+z', ''].join('\n');
      const label6 = 'Test 6 – Multi';
      const json6 = { type: 'apply_patch', title: label6, changes: { [f1]: { unified_diff: u1 }, [f2]: { unified_diff: u2 } } };
      if (ipcSocket && !ipcSocket.destroyed) {
        await sendIpc(json6);
      } else {
        await services.openDiffs(json6, true, label6);
      }
      // Give VS Code a moment to update active tab
      await new Promise(r => setTimeout(r, 150));
      const activeTab = vscode.window.tabGroups?.activeTabGroup?.activeTab;
      const expected = `${label6} • ${path.basename(f1)}`;
      const focusedOk = !!activeTab && typeof activeTab.label === 'string' && activeTab.label === expected;
      assert(focusedOk, `First diff focused (expected label: ${expected})`);
    } catch (e) {
      assert(false, `Multi-update focus check threw error: ${e}`);
    }

    // Test 5: File removal (via IPC delete)
    try {
      const delPath = path.join(tmpRoot, 'delete_1.txt');
      fs.writeFileSync(delPath, 'x1\nx2\n');
      const json5 = { type: 'apply_patch', changes: { [delPath]: { delete: {} } }, title: 'Test 5 – Delete File' };
      await new Promise(r => setTimeout(r, 50));
      if (ipcSocket && !ipcSocket.destroyed) {
        await sendIpc(json5);
      } else {
        await services.openDiffs(json5, true, 'Test 5 – Delete File');
      }
      // No proposed buffer for deletes; assert that the file still exists (pre-condition)
      assert(fs.existsSync(delPath), 'Delete test precondition satisfied');
    } catch (e) {
      assert(false, `Deletion diff threw error: ${e}`);
    }

    const pass = results.filter(r => r.ok).length;
    const fail = results.length - pass;
    const summary = `Self-tests completed: ${pass} passed, ${fail} failed`;
    out.appendLine('');
    out.appendLine(summary);
    if (fail === 0) {
      // Clean up artifacts on success
      try { fs.rmSync(tmpRoot, { recursive: true, force: true }); } catch {}
      out.appendLine('Artifacts cleaned up');
      vscode.window.showInformationMessage(summary);
    } else {
      out.appendLine(`Artifacts kept for inspection: ${tmpRoot}`);
      vscode.window.showErrorMessage(summary + ` — artifacts: ${tmpRoot}`);
    }

    services.setWatcherSuppressed(false);
    try { if (ipcSocket && !ipcSocket.destroyed) { ipcSocket.end(); ipcSocket.destroy(); } } catch {}
  });
  context.subscriptions.push(cmd);
}

module.exports = { registerSelfTests };

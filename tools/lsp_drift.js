// Drift gate for the language server.
//
// Drives a real `glua_ls` over stdio as VS Code does: real debounce, slices,
// threads, watched-file events and diagnostics as the server delivers them.
// After each scenario it compares the diagnostics a client would show for
// every workspace file with a cold `glua_check` of the text the scenario ends
// on. Any difference is drift the user would see.
//
// A server that advertises `diagnosticProvider` is pulled, as VS Code pulls
// it; one that does not is followed by its `publishDiagnostics` alone, where
// the latest publish per file is what the client shows. A push scenario waits
// for fresh workspace analysis, then for --quiet-ms without a publish. The
// load scenario also waits for the server's `startupComplete` status first.
//
// Scenarios:
//   load     the workspace as loaded
//   typing   the edit typed one character at a time, a keystroke every
//            --keystroke-ms with a --pause-ms pause every --pause-every
//            keystrokes, so keystrokes land both within the debounce and in
//            the middle of a re-analysis
//   revert   the original text back in one change, as an undo
//   delete   the file closed and deleted on disk, through a watched-file
//            event; a push server must clear the deleted file's diagnostics
//   recreate the file re-created, through a watched-file event
//
// Runs on temporary copies of the workspace; the codebase is never touched.
//
// Usage:
//   LSP_CODEBASE=/path/to/workspace LSP_ANNOTATIONS=/path/to/annotations \
//   node tools/lsp_drift.js --file relative/path.lua \
//       --edit-find TEXT --edit-replace TEXT [--reference cold.json]
//       [--keystroke-ms 40] [--pause-ms 700] [--pause-every 4] [--quiet-ms 5000]
//
// --reference is `glua_check -f json` output for the unmodified codebase from
// the same build (the corpus diff makes one); without it the gate runs that
// check itself. LSP_SERVER and GLUA_CHECK override the binaries (default
// target/release). Exits 1 on any difference.
'use strict';

const { spawn } = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');
const {
    sleep, requireDir, fileUri, startServer, contentChanges,
} = require('./lsp_client');

function parseArgs(argv) {
    const opts = {
        file: null, editFind: null, editReplace: null, reference: null,
        keystrokeMs: 40, pauseMs: 700, pauseEvery: 4, quietMs: 5000,
    };
    for (let i = 2; i < argv.length; i++) {
        const a = argv[i];
        if (a === '--file') opts.file = argv[++i];
        else if (a === '--edit-find') opts.editFind = argv[++i];
        else if (a === '--edit-replace') opts.editReplace = argv[++i];
        else if (a === '--reference') opts.reference = argv[++i];
        else if (a === '--keystroke-ms') opts.keystrokeMs = Number(argv[++i]);
        else if (a === '--pause-ms') opts.pauseMs = Number(argv[++i]);
        else if (a === '--pause-every') opts.pauseEvery = Number(argv[++i]);
        else if (a === '--quiet-ms') opts.quietMs = Number(argv[++i]);
        else throw new Error(`unknown argument: ${a}`);
    }
    if (!opts.file || !opts.editFind || opts.editReplace === null) {
        throw new Error('--file, --edit-find and --edit-replace are required');
    }
    return opts;
}

function exe(name) {
    return process.platform === 'win32' ? `${name}.exe` : name;
}

/** Copies `from` to `to` without `.git`. */
function copyWorkspace(from, to) {
    fs.cpSync(from, to, {
        recursive: true,
        filter: (src) => path.basename(src) !== '.git',
    });
}

/**
 * The texts typing `find` into `replace` passes through: the part of `find`
 * that differs deleted at once, then the differing part of `replace` typed a
 * character at a time. The last is the edited text.
 */
function typedTexts(text, find, replace) {
    const at = text.indexOf(find);
    if (at < 0) throw new Error(`--edit-find text not present in ${JSON.stringify(find)}`);
    const from = [...find];
    const to = [...replace];
    let prefix = 0;
    while (prefix < from.length && prefix < to.length && from[prefix] === to[prefix]) prefix++;
    let suffix = 0;
    while (suffix < from.length - prefix && suffix < to.length - prefix
        && from[from.length - 1 - suffix] === to[to.length - 1 - suffix]) suffix++;
    const before = text.slice(0, at);
    const after = text.slice(at + find.length);
    const head = to.slice(0, prefix).join('');
    const tail = to.slice(to.length - suffix).join('');
    const typed = to.slice(prefix, to.length - suffix);
    const texts = [];
    if (from.length - suffix > prefix || typed.length === 0) texts.push(before + head + tail + after);
    for (let end = 1; end <= typed.length; end++) {
        texts.push(before + head + typed.slice(0, end).join('') + tail + after);
    }
    return texts;
}

/** LSP `ServerCancelled`. */
const SERVER_CANCELLED = -32802;

/** Pulls sent for one request before its cancellations fail the run. */
const PULL_ATTEMPTS = 10;

async function settlePush(client, quietMs) {
    if (!client.statusAt.has('startupComplete')) {
        await client.waitForStatus('startupComplete', 300000);
    }
    const barrier = await client.request('workspace/symbol', { query: '__gluals_drift_barrier__' });
    if (barrier.message.error) throw new Error(JSON.stringify(barrier.message.error));
    await client.waitForPublishQuiet(quietMs, Date.now());
}

/** Child processes still running, stopped before the scratch folder is removed. */
const children = new Set();

/** Resolves once `proc` has exited. */
function exited(proc) {
    if (proc.exitCode !== null || proc.signalCode !== null) return Promise.resolve();
    return new Promise((resolve) => proc.once('exit', resolve));
}

/** Runs `glua_check` on `dir` and returns its diagnostics by file. */
function coldCheck(glua, dir, annotations, output) {
    return new Promise((resolve, reject) => {
        const proc = spawn(glua, [dir, '-f', 'json', '--output', output, '--gmod-annotations', annotations],
            { stdio: ['ignore', 'ignore', 'pipe'] });
        children.add(proc);
        proc.once('exit', () => children.delete(proc));
        let stderr = '';
        proc.stderr.on('data', (chunk) => { stderr += chunk; });
        proc.on('exit', () => {
            if (!fs.existsSync(output)) {
                reject(new Error(`glua_check wrote no output for ${dir}: ${stderr.slice(-500)}`));
                return;
            }
            resolve(output);
        });
    });
}

/** A path relative to `root`, normalised for comparison. */
function relativeKey(root, file) {
    const relative = path.relative(root, file).replace(/\\/g, '/');
    return process.platform === 'win32' ? relative.toLowerCase() : relative;
}

function uriToPath(uri) {
    let p = decodeURIComponent(uri.replace(/^file:\/\//, ''));
    if (/^\/[A-Za-z]:/.test(p)) p = p.slice(1);
    return path.resolve(p);
}

function diagnosticKey(d) {
    const code = d.code === undefined || d.code === null ? '<none>' : String(d.code);
    const r = d.range;
    return `${d.severity || 4} [${code}] ${r.start.line + 1}:${r.start.character + 1}-`
        + `${r.end.line + 1}:${r.end.character + 1} ${String(d.message).replace(/[\r\n]+/g, ' ')}`;
}

/** `glua_check` JSON as sorted diagnostic keys by relative file. */
function loadCold(root, output) {
    const byFile = new Map();
    for (const entry of JSON.parse(fs.readFileSync(output, 'utf8'))) {
        const keys = entry.diagnostics.map(diagnosticKey).sort();
        if (keys.length) byFile.set(relativeKey(root, entry.file), keys);
    }
    return byFile;
}

/** Compares what the client shows with the cold check; prints and counts differences. */
function compare(label, shown, cold) {
    const lines = [];
    for (const file of new Set([...shown.keys(), ...cold.keys()])) {
        const a = shown.get(file) || [];
        const b = cold.get(file) || [];
        const count = (keys) => keys.reduce((m, k) => m.set(k, (m.get(k) || 0) + 1), new Map());
        const ca = count(a);
        const cb = count(b);
        for (const [k, n] of cb) for (let i = (ca.get(k) || 0); i < n; i++) lines.push(`  MISSING ${file} ${k}`);
        for (const [k, n] of ca) for (let i = (cb.get(k) || 0); i < n; i++) lines.push(`  EXTRA   ${file} ${k}`);
    }
    if (lines.length === 0) {
        console.log(`[${label}] +0 diagnostics`);
        return 0;
    }
    console.log(`[${label}] ${lines.length} difference(s) from the cold check`);
    for (const line of lines.sort().slice(0, 40)) console.log(line);
    return lines.length;
}

async function main() {
    const opts = parseArgs(process.argv);
    const codebase = requireDir(process.env.LSP_CODEBASE, 'LSP_CODEBASE');
    const annotations = requireDir(process.env.LSP_ANNOTATIONS, 'LSP_ANNOTATIONS');
    const release = path.resolve(__dirname, '..', 'target', 'release');
    const server = process.env.LSP_SERVER || path.join(release, exe('glua_ls'));
    const glua = process.env.GLUA_CHECK || path.join(release, exe('glua_check'));
    for (const binary of [server, glua]) {
        if (!fs.existsSync(binary)) throw new Error(`binary not found: ${binary}`);
    }
    const started = Date.now();
    const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'glua_drift_'));
    // Each copy keeps the codebase's folder name, which names a gamemode rooted there.
    const work = path.join(scratch, 'work', path.basename(path.resolve(codebase)));
    const edited = path.join(scratch, 'edited', path.basename(path.resolve(codebase)));
    const deleted = path.join(scratch, 'deleted', path.basename(path.resolve(codebase)));
    try {
        copyWorkspace(codebase, work);
        copyWorkspace(codebase, edited);
        copyWorkspace(codebase, deleted);
        const relative = opts.file.replace(/\\/g, '/');
        const target = path.join(work, relative);
        const original = fs.readFileSync(target, 'utf8');
        const texts = typedTexts(original, opts.editFind, opts.editReplace);
        fs.writeFileSync(path.join(edited, relative), texts[texts.length - 1]);
        fs.rmSync(path.join(deleted, relative));

        // Cold checks run one at a time next to the server, which bounds memory
        // at two analyses.
        const originalCold = opts.reference
            ? Promise.resolve(loadCold(codebase, opts.reference))
            : coldCheck(glua, work, annotations, path.join(scratch, 'original.json'))
                .then((output) => loadCold(work, output));
        const editedCold = originalCold.then(() =>
            coldCheck(glua, edited, annotations, path.join(scratch, 'edited.json'))
                .then((output) => loadCold(edited, output)));
        const deletedCold = editedCold.then(() =>
            coldCheck(glua, deleted, annotations, path.join(scratch, 'deleted.json'))
                .then((output) => loadCold(deleted, output)));

        const { proc, client, loadMs, push, incremental } = await startServer(server, work, annotations);
        children.add(proc);
        console.log(`[load] workspace loaded in ${(loadMs / 1000).toFixed(1)}s`
            + ` (diagnostics by ${push ? 'push' : 'pull'}, ${incremental ? 'incremental' : 'full'} text sync)`);

        // What the client shows: the latest report per file, as VS Code keeps
        // it. A pull replaces the files it reports and leaves the rest; a
        // publish replaces its file, and an empty one clears it.
        const shown = new Map();
        const resultIds = new Map();
        const apply = (uri, report) => {
            if (!report || report.kind === 'unchanged') return;
            const file = relativeKey(work, uriToPath(uri));
            const keys = (report.items || []).map(diagnosticKey).sort();
            if (keys.length) shown.set(file, keys); else shown.delete(file);
            if (report.resultId) resultIds.set(uri, report.resultId);
        };
        // Publishes can land before this listener does; the client keeps them.
        for (const [published, entry] of client.publishes) {
            apply(published, { kind: 'full', items: entry.diagnostics });
        }
        client.onNotification = ((previous) => (message) => {
            if (message.method === 'textDocument/publishDiagnostics') {
                apply(message.params.uri, { kind: 'full', items: message.params.diagnostics });
            }
            if (previous) previous(message);
        })(client.onNotification);

        // A diagnostic pull the server cancels with `retriggerRequest` is sent
        // again, as VS Code does. Any other error fails the run, and so does a
        // pull cancelled every time, which is what a dead debounce loop answers.
        const pull = async (method, params) => {
            for (let attempt = 1; ; attempt++) {
                const response = await client.request(method, params());
                const error = response.message.error;
                if (!error) return response.message.result;
                const retrigger = error.code === SERVER_CANCELLED && error.data && error.data.retriggerRequest;
                if (!retrigger || attempt === PULL_ATTEMPTS) {
                    throw new Error(`${method} failed after ${attempt} attempt(s): ${JSON.stringify(error)}`);
                }
                await sleep(200);
            }
        };

        const open = new Set();
        // A pull server has settled once a workspace pull, which waits for
        // fresh analysis, is followed by no refresh request for --quiet-ms;
        // then one more pull picks up anything a missed refresh armed.
        const settlePull = async () => {
            for (const uri of open) {
                const result = await pull('textDocument/diagnostic', () => ({
                    textDocument: { uri }, previousResultId: resultIds.get(uri),
                }));
                apply(uri, result);
            }
            const pullWorkspace = async () => {
                const result = await pull('workspace/diagnostic', () => ({
                    previousResultIds: [...resultIds].map(([uri, value]) => ({ uri, value })),
                }));
                for (const item of (result && result.items) || []) {
                    apply(item.uri, item);
                }
            };
            for (;;) {
                const refresh = client.waitForServerRequest('workspace/diagnostic/refresh', opts.quietMs)
                    .catch(() => null);
                await pullWorkspace();
                if (!(await refresh)) break;
            }
            await pullWorkspace();
        };
        const settle = push ? () => settlePush(client, opts.quietMs) : settlePull;

        let differences = 0;
        await settle();
        differences += compare('load', shown, await originalCold);

        const uri = fileUri(target);
        let version = 1;
        client.notify('textDocument/didOpen', {
            textDocument: { uri, languageId: 'lua', version, text: original },
        });
        open.add(uri);
        let shownText = original;
        const change = (text) => {
            version += 1;
            client.notify('textDocument/didChange', {
                textDocument: { uri, version },
                contentChanges: contentChanges(shownText, text, incremental),
            });
            shownText = text;
        };
        for (let i = 0; i < texts.length; i++) {
            change(texts[i]);
            await sleep((i + 1) % opts.pauseEvery === 0 ? opts.pauseMs : opts.keystrokeMs);
        }
        await settle();
        differences += compare(`typing (${texts.length} keystrokes)`, shown, await editedCold);

        change(original);
        await settle();
        differences += compare('revert', shown, await originalCold);

        client.notify('textDocument/didClose', { textDocument: { uri } });
        open.delete(uri);
        fs.rmSync(target);
        client.notify('workspace/didChangeWatchedFiles', { changes: [{ uri, type: 3 }] });
        await settle();
        const deletedFile = relativeKey(work, target);
        if (push) {
            // The server clears a deleted file with an empty publish; anything
            // still shown for it would stay in the editor.
            const stale = shown.get(deletedFile) || [];
            if (stale.length) {
                console.log(`[delete] ${stale.length} diagnostic(s) still shown for the deleted file`);
                for (const key of stale.slice(0, 40)) console.log(`  STALE   ${deletedFile} ${key}`);
            }
            differences += stale.length;
        } else {
            // The server forgets a deleted file's report; the editor drops it
            // and the result id it held.
            shown.delete(deletedFile);
            resultIds.delete(uri);
        }
        const shownElsewhere = new Map([...shown].filter(([file]) => file !== deletedFile));
        differences += compare('delete', shownElsewhere, await deletedCold);
        fs.writeFileSync(target, original);
        client.notify('workspace/didChangeWatchedFiles', { changes: [{ uri, type: 1 }] });
        await settle();
        differences += compare('recreate', shown, await originalCold);

        console.log(`[time] total ${((Date.now() - started) / 1000).toFixed(1)}s`);
        return differences;
    } finally {
        // Windows keeps a folder until every process holding files in it has
        // exited; a failed removal is reported without hiding the run's result.
        const running = [...children];
        for (const proc of running) proc.kill();
        await Promise.all(running.map(exited));
        try {
            fs.rmSync(scratch, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
        } catch (error) {
            console.error(`warning: could not remove ${scratch}: ${error.message}`);
        }
    }
}

module.exports = { settlePush };

if (require.main === module) main().then((differences) => {
    if (differences > 0) {
        console.error(`\nFAILED: ${differences} difference(s) from the cold check`);
        process.exit(1);
    }
}).catch((error) => {
    console.error(String(error && error.stack ? error.stack : error));
    process.exit(1);
});

// Interactive latency harness for the language server.
//
// Drives a real `glua_ls` binary over stdio with the capabilities and the
// cancellation behaviour vscode-languageclient actually uses, then reports how
// long the requests a user waits on take. Unit tests cannot catch what this
// measures: the cost of a request is dominated by whether a reindex is pending,
// which only shows up against a real workspace.
//
// Usage:
//   LSP_CODEBASE=/path/to/workspace \
//   LSP_ANNOTATIONS=/path/to/annotations/output \
//   node tools/lsp_latency.js [--json] [--runs N] [--file relative/path.lua]
//                             [--edit code|comment]
//                             [--edit-find TEXT --edit-replace TEXT]
//                             [--completion-find TEXT]
//
// --edit-find/--edit-replace swap one snippet back and forth on every edit, so
// a specific real change to real code can be reproduced. --completion-find puts
// the cursor immediately after a given snippet instead of at the first member
// access in the file. Together they reproduce one user's exact complaint.
//
// --edit code (default) inserts a global function declaration, so each edit
// really changes what the file exports and the full dependency ripple runs.
// --edit comment inserts a comment line instead: cheaper, and useful for
// isolating offset-shift cost, but an optimisation that only helps this case
// has not made real typing any faster.
//
// LSP_SERVER overrides the binary (default: target/dist/glua_ls[.exe] if built,
// else target/release/glua_ls[.exe] — see defaultServerPath, and prefer `dist`).
// LSP_SERVER_ARGS passes extra space-separated arguments to the server, e.g.
//   LSP_SERVER_ARGS='--log-level debug' to profile a slow path.
// --file defaults to the largest .lua file in the workspace, which is the
// pessimistic case and keeps runs comparable without naming a file per repo.
//
// Exits non-zero on a correctness check, not on a latency number: a cancelled
// diagnostic pull answered with an empty full report, or a mid-edit completion
// that disagrees with the settled one.
'use strict';

const { spawn } = require('child_process');
const fs = require('fs');
const path = require('path');

// ---------------------------------------------------------------- config ---

function parseArgs(argv) {
    const opts = {
        json: false, runs: 5, file: null, edit: 'code',
        editFind: null, editReplace: null, completionFind: null,
    };
    for (let i = 2; i < argv.length; i++) {
        const a = argv[i];
        if (a === '--json') opts.json = true;
        else if (a === '--runs') opts.runs = Number(argv[++i]);
        else if (a === '--file') opts.file = argv[++i];
        else if (a === '--edit') opts.edit = argv[++i];
        else if (a === '--edit-find') opts.editFind = argv[++i];
        else if (a === '--edit-replace') opts.editReplace = argv[++i];
        else if (a === '--completion-find') opts.completionFind = argv[++i];
        else throw new Error(`unknown argument: ${a}`);
    }
    if (opts.edit !== 'code' && opts.edit !== 'comment') {
        throw new Error('--edit must be "code" or "comment"');
    }
    if ((opts.editFind === null) !== (opts.editReplace === null)) {
        throw new Error('--edit-find and --edit-replace must be given together');
    }
    if (opts.editFind === '') throw new Error('--edit-find must not be empty');
    if (!Number.isFinite(opts.runs) || opts.runs < 1) {
        throw new Error('--runs must be a positive integer');
    }
    return opts;
}

/**
 * Prefers the `dist` profile, which is what ships. `release` lacks its thin LTO
 * and single codegen unit, so measuring it reports numbers no user experiences —
 * an easy mistake to make for a whole session before noticing.
 */
function defaultServerPath() {
    const exe = process.platform === 'win32' ? 'glua_ls.exe' : 'glua_ls';
    const dist = path.resolve(__dirname, '..', 'target', 'dist', exe);
    if (fs.existsSync(dist)) return dist;
    return path.resolve(__dirname, '..', 'target', 'release', exe);
}

function requireDir(value, name) {
    if (!value) throw new Error(`${name} is required`);
    if (!fs.existsSync(value)) throw new Error(`${name} does not exist: ${value}`);
    return path.resolve(value);
}

function largestLuaFile(root) {
    let best = null;
    const walk = (dir) => {
        let entries;
        try { entries = fs.readdirSync(dir, { withFileTypes: true }); } catch { return; }
        for (const e of entries) {
            if (e.name === '.git' || e.name === 'node_modules') continue;
            const p = path.join(dir, e.name);
            if (e.isDirectory()) walk(p);
            else if (e.name.endsWith('.lua')) {
                const size = fs.statSync(p).size;
                if (!best || size > best.size) best = { path: p, size };
            }
        }
    };
    walk(root);
    if (!best) throw new Error(`no .lua files found under ${root}`);
    return best.path;
}

function fileUri(p) {
    const resolved = path.resolve(p).replace(/\\/g, '/');
    const withSlash = resolved.startsWith('/') ? resolved : `/${resolved}`;
    return `file://${encodeURI(withSlash).replace(/#/g, '%23').replace(/\?/g, '%3F')}`;
}

// ------------------------------------------------------------ lsp client ---

class LspClient {
    constructor(proc) {
        this.proc = proc;
        this.buffer = Buffer.alloc(0);
        this.nextId = 1;
        this.pending = new Map();
        this.onNotification = null;
        this.serverRequests = new Set();
        proc.stdout.on('data', (chunk) => this._receive(chunk));
        // The server's phase profiler writes to stderr, so LSP_SERVER_STDERR
        // makes those numbers reachable instead of dropping them on the floor.
        const stderrPath = process.env.LSP_SERVER_STDERR;
        if (stderrPath) fs.writeFileSync(stderrPath, '');
        proc.stderr.on('data', (chunk) => {
            if (stderrPath) fs.appendFileSync(stderrPath, chunk);
        });
    }

    _receive(chunk) {
        this.buffer = Buffer.concat([this.buffer, chunk]);
        for (;;) {
            const headerEnd = this.buffer.indexOf('\r\n\r\n');
            if (headerEnd < 0) return;
            const header = this.buffer.slice(0, headerEnd).toString('ascii');
            const match = /Content-Length: (\d+)/i.exec(header);
            if (!match) return;
            const length = Number(match[1]);
            const bodyStart = headerEnd + 4;
            if (this.buffer.length < bodyStart + length) return;
            const body = this.buffer.slice(bodyStart, bodyStart + length).toString('utf8');
            this.buffer = this.buffer.slice(bodyStart + length);
            try { this._dispatch(JSON.parse(body)); } catch { /* ignore malformed frame */ }
        }
    }

    _dispatch(message) {
        if (message.id !== undefined && message.method) {
            // Server-initiated request. Record it and answer so the server is
            // never left waiting on us. `workspace/configuration` has to come
            // back as one entry per requested item; null is not a valid result
            // and would have the server fall back to something a real client
            // never makes it use.
            this.serverRequests.add(message.method);
            const result = message.method === 'workspace/configuration'
                ? ((message.params && message.params.items) || []).map(() => ({}))
                : null;
            this._write({ jsonrpc: '2.0', id: message.id, result });
            return;
        }
        if (message.id !== undefined) {
            const resolve = this.pending.get(message.id);
            if (resolve) { this.pending.delete(message.id); resolve(message); }
            return;
        }
        if (this.onNotification) this.onNotification(message);
    }

    _write(payload) {
        const body = Buffer.from(JSON.stringify(payload), 'utf8');
        this.proc.stdin.write(`Content-Length: ${body.length}\r\n\r\n`);
        this.proc.stdin.write(body);
    }

    notify(method, params) {
        this._write({ jsonrpc: '2.0', method, params });
    }

    /** Returns a promise carrying the response, the elapsed ms, and its id. */
    request(method, params) {
        const id = this.nextId++;
        const startedAt = Date.now();
        const promise = new Promise((resolve) => {
            this.pending.set(id, (message) =>
                resolve({ message, ms: Date.now() - startedAt, id }));
        });
        this._write({ jsonrpc: '2.0', id, method, params });
        return Object.assign(promise, { id });
    }

    cancel(id) {
        this.notify('$/cancelRequest', { id });
    }
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/**
 * The subset of VS Code's capabilities that changes server behaviour on the
 * paths this tool measures. Keep in step with vscode-languageclient: the point
 * is to exercise what the real client exercises.
 */
function clientCapabilities() {
    return {
        general: {
            staleRequestSupport: {
                cancel: true,
                retryOnContentModified: [
                    'textDocument/semanticTokens/full',
                    'textDocument/semanticTokens/range',
                    'textDocument/semanticTokens/full/delta',
                ],
            },
        },
        window: { workDoneProgress: true },
        workspace: {
            applyEdit: true,
            configuration: true,
            workspaceFolders: true,
            diagnostics: { refreshSupport: true },
            semanticTokens: { refreshSupport: true },
            inlayHint: { refreshSupport: true },
            codeLens: { refreshSupport: true },
            didChangeWatchedFiles: { dynamicRegistration: true },
        },
        textDocument: {
            synchronization: { didSave: true },
            diagnostic: { dynamicRegistration: true, relatedDocumentSupport: true },
            completion: { completionItem: { tagSupport: { valueSet: [1] } } },
            hover: {},
            definition: {},
            semanticTokens: {
                requests: { full: true },
                tokenTypes: [], tokenModifiers: [], formats: ['relative'],
            },
        },
    };
}

// ------------------------------------------------------------- reporting ---

function summarise(samples) {
    if (samples.length === 0) return null;
    const sorted = [...samples].sort((a, b) => a - b);
    const at = (q) => sorted[Math.min(sorted.length - 1, Math.floor(q * sorted.length))];
    return { runs: sorted.length, min: sorted[0], median: at(0.5), max: sorted[sorted.length - 1] };
}

function describeReport(result) {
    if (!result) return { kind: 'none' };
    if (result.kind === 'unchanged') return { kind: 'unchanged', resultId: result.resultId };
    return { kind: 'full', count: (result.items || []).length, resultId: result.resultId };
}

// ------------------------------------------------------------- scenarios ---

/**
 * Finds a position just after a `.` on a member access, which is the case that
 * matters most: it forces the server to resolve a receiver type through the
 * index rather than listing globals.
 *
 * `self.` is preferred because resolving it exercises the class/type indexes
 * rather than a module table, which is where a stale index shows up first.
 */
function memberAccessPosition(text) {
    const lines = text.split('\n');
    const find = (pattern) => {
        for (let line = 0; line < lines.length; line++) {
            const match = pattern.exec(lines[line]);
            if (match) {
                return { line, character: match.index + match[0].indexOf('.') + 1 };
            }
        }
        return null;
    };
    return find(/\bself\.[A-Za-z_]/)
        || find(/[A-Za-z_][A-Za-z0-9_]*\.[A-Za-z_]/)
        || { line: 0, character: 0 };
}

async function main() {
    const opts = parseArgs(process.argv);
    const codebase = requireDir(process.env.LSP_CODEBASE, 'LSP_CODEBASE');
    const annotations = requireDir(process.env.LSP_ANNOTATIONS, 'LSP_ANNOTATIONS');
    const server = process.env.LSP_SERVER || defaultServerPath();
    if (!fs.existsSync(server)) {
        throw new Error(`server binary not found: ${server}\nBuild it with: cargo build -p glua_ls --release`);
    }
    const target = opts.file ? path.resolve(codebase, opts.file) : largestLuaFile(codebase);
    if (!fs.existsSync(target)) throw new Error(`target file not found: ${target}`);

    const extraArgs = (process.env.LSP_SERVER_ARGS || '').split(' ').filter(Boolean);
    const proc = spawn(server, [
        '--communication', 'stdio',
        '--gmod-annotations-path', annotations,
        ...extraArgs,
    ], { stdio: ['pipe', 'pipe', 'pipe'] });

    const client = new LspClient(proc);
    const report = {
        workspace: codebase,
        file: path.relative(codebase, target),
        server,
        runs: opts.runs,
        measurements: {},
        checks: {},
    };

    let workspaceLoadedAt = null;
    client.onNotification = (message) => {
        if (message.method === 'gluals/serverStatus'
            && message.params && message.params.state === 'workspaceLoaded') {
            workspaceLoadedAt = Date.now();
        }
    };

    const startedAt = Date.now();
    await client.request('initialize', {
        processId: process.pid,
        rootUri: fileUri(codebase),
        workspaceFolders: [{ uri: fileUri(codebase), name: path.basename(codebase) }],
        capabilities: clientCapabilities(),
        initializationOptions: {},
        clientInfo: { name: 'Visual Studio Code', version: '1.95.0' },
    });
    client.notify('initialized', {});

    const loadDeadline = Date.now() + 300000;
    while (!workspaceLoadedAt && Date.now() < loadDeadline) await sleep(100);
    if (!workspaceLoadedAt) {
        proc.kill();
        throw new Error('workspace never finished loading (5 minute timeout)');
    }
    report.measurements.workspaceLoad = { runs: 1, min: workspaceLoadedAt - startedAt, median: workspaceLoadedAt - startedAt, max: workspaceLoadedAt - startedAt };

    const uri = fileUri(target);
    const original = fs.readFileSync(target, 'utf8');
    let text = original;
    let version = 1;
    client.notify('textDocument/didOpen', {
        textDocument: { uri, languageId: 'lua', version, text },
    });
    await sleep(1500);

    // `--completion-find` puts the cursor immediately after a given snippet, so
    // a specific completion can be reproduced instead of whatever member access
    // happens to come first in the file.
    const completionPosition = (currentText) => {
        if (opts.completionFind === null) return memberAccessPosition(currentText);
        const index = currentText.indexOf(opts.completionFind);
        if (index < 0) {
            throw new Error(`--completion-find text not present in document: ${JSON.stringify(opts.completionFind)}`);
        }
        const lines = currentText.slice(0, index + opts.completionFind.length).split('\n');
        return { line: lines.length - 1, character: lines[lines.length - 1].length };
    };

    const editOffset = text.indexOf('\n') + 1;
    const settledCompletion = [];
    const typingCompletion = [];
    const settledDiagnostic = [];
    const typingHover = [];
    const typingCompletionConcurrent = [];
    const editToFresh = [];
    const cancelledPulls = [];
    const completionDrift = [];
    let previousResultId;

    const labelsOf = (items) =>
        (Array.isArray(items) ? items : []).map((item) => item.label);

    // Recomputed per call: every edit inserts a line, so a position captured
    // once would drift and silently start measuring an empty completion.
    const completionAt = async () => client.request('textDocument/completion', {
        textDocument: { uri },
        position: completionPosition(text),
        context: { triggerKind: 2, triggerCharacter: '.' },
    });

    let editSerial = 0;
    const editDocument = () => {
        version += 1;
        editSerial += 1;

        // `--edit-find`/`--edit-replace` reproduce one specific edit, swapping
        // back and forth so every keystroke is a real change to real code. Use
        // it to measure the edit a user actually complained about.
        if (opts.editFind !== null) {
            const [from, to] = editSerial % 2 === 1
                ? [opts.editFind, opts.editReplace]
                : [opts.editReplace, opts.editFind];
            if (!text.includes(from)) {
                throw new Error(`--edit-find text not present in document: ${JSON.stringify(from)}`);
            }
            text = text.replace(from, to);
            client.notify('textDocument/didChange', {
                textDocument: { uri, version },
                contentChanges: [{ text }],
            });
            return;
        }

        // `--edit comment` keeps the edit syntactically inert, which makes runs
        // comparable but is also the case an early-cutoff optimisation can make
        // fast without helping anyone. `--edit code` (the default) declares a
        // global function instead, so the edit really does change what the file
        // exports and the whole dependency ripple has to run.
        const inserted = opts.edit === 'comment'
            ? '-- perf\n'
            : `function _PerfProbe${editSerial}(a) return a end\n`;
        text = text.slice(0, editOffset) + inserted + text.slice(editOffset);
        client.notify('textDocument/didChange', {
            textDocument: { uri, version },
            contentChanges: [{ text }],
        });
    };

    // A settled measurement is only meaningful once nothing is pending. An
    // uncancelled diagnostic pull returns exactly when the analysis is fresh,
    // so it is the cheapest way to wait for quiescence without guessing.
    const waitUntilQuiet = async () => {
        await client.request('textDocument/diagnostic', {
            textDocument: { uri }, previousResultId,
        });
    };

    for (let run = 0; run < opts.runs; run++) {
        await waitUntilQuiet();

        // Settled: no pending edit, so this is the pure compute cost.
        const settled = await completionAt();
        settledCompletion.push(settled.ms);
        const items = settled.message.result
            ? (settled.message.result.items || settled.message.result)
            : [];
        report.checks.completionItemCount = Array.isArray(items) ? items.length : 0;

        const diagnostic = await client.request('textDocument/diagnostic', {
            textDocument: { uri }, previousResultId,
        });
        settledDiagnostic.push(diagnostic.ms);
        const described = describeReport(diagnostic.message.result);
        if (described.resultId) previousResultId = described.resultId;
        report.checks.diagnosticCount = described.count ?? report.checks.diagnosticCount;

        // While typing: the request the user actually waits on. Its result is
        // compared against the settled one, because the whole risk of answering
        // before a reindex finishes is answering *differently* — a thinner or
        // wrong list is the failure mode, not a slow one.
        editDocument();
        const typing = await completionAt();
        typingCompletion.push(typing.ms);
        const typingItems = typing.message.result
            ? (typing.message.result.items || typing.message.result)
            : [];
        const settledLabels = new Set(labelsOf(items));
        const typingLabels = new Set(labelsOf(typingItems));
        const missing = [...settledLabels].filter((l) => !typingLabels.has(l));
        const extra = [...typingLabels].filter((l) => !settledLabels.has(l));
        completionDrift.push({ missing: missing.length, extra: extra.length,
            sampleMissing: missing.slice(0, 5) });

        // Completion is not the only thing gated on freshness. Hover is issued
        // from the same keystroke and measured separately, so a fix that makes
        // completion answer early but leaves every other position-based feature
        // parked shows up here instead of looking like a win.
        editDocument();
        const [hovered, completed] = await Promise.all([
            client.request('textDocument/hover', {
                textDocument: { uri }, position: completionPosition(text),
            }),
            completionAt(),
        ]);
        typingHover.push(hovered.ms);
        typingCompletionConcurrent.push(completed.ms);

        // Keystroke to the first answer any index-reading handler can give.
        editDocument();
        const fresh = await client.request('textDocument/diagnostic', {
            textDocument: { uri }, previousResultId,
        });
        editToFresh.push(fresh.ms);

        // A pull cancelled mid-flight must never come back as an empty full
        // report — that is what clears the file's diagnostics in VS Code.
        editDocument();
        const doomed = client.request('textDocument/diagnostic', {
            textDocument: { uri }, previousResultId,
        });
        await sleep(20);
        client.cancel(doomed.id);
        const cancelled = await doomed;
        const shape = describeReport(cancelled.message.result);
        cancelledPulls.push({
            emptyFullReport: shape.kind === 'full' && shape.count === 0,
            errorCode: cancelled.message.error && cancelled.message.error.code,
        });
    }

    report.measurements.completionSettled = summarise(settledCompletion);
    report.measurements.completionWhileTyping = summarise(typingCompletion);
    report.measurements.diagnosticSettled = summarise(settledDiagnostic);
    report.measurements.hoverWhileTyping = summarise(typingHover);
    report.measurements.completionWhileTypingConcurrent = summarise(typingCompletionConcurrent);
    report.measurements.editToFreshAnswer = summarise(editToFresh);
    report.checks.emptyFullReportsOnCancel =
        cancelledPulls.filter((p) => p.emptyFullReport).length;
    // A mid-edit completion that differs from the settled one is a correctness
    // regression, however fast it came back.
    report.checks.completionDriftWhileTyping = {
        worstMissing: Math.max(0, ...completionDrift.map((d) => d.missing)),
        worstExtra: Math.max(0, ...completionDrift.map((d) => d.extra)),
        sampleMissing: (completionDrift.find((d) => d.missing > 0) || {}).sampleMissing || [],
    };
    report.checks.serverInitiatedRequests = [...client.serverRequests].sort();

    proc.kill();

    if (opts.json) {
        console.log(JSON.stringify(report, null, 2));
        return failedChecks(report);
    }

    const rows = [
        ['workspace load', report.measurements.workspaceLoad],
        ['completion (settled)', report.measurements.completionSettled],
        ['completion (while typing)', report.measurements.completionWhileTyping],
        ['diagnostic (settled)', report.measurements.diagnosticSettled],
        ['hover (while typing)', report.measurements.hoverWhileTyping],
        ['completion (concurrent)', report.measurements.completionWhileTypingConcurrent],
        ['edit -> fresh answer', report.measurements.editToFreshAnswer],
    ];
    console.log(`workspace : ${report.workspace}`);
    console.log(`file      : ${report.file}`);
    console.log(`server    : ${report.server}`);
    console.log(`runs      : ${report.runs}\n`);
    console.log('                             min      median       max');
    for (const [label, stats] of rows) {
        if (!stats) continue;
        const fmt = (n) => `${n}ms`.padStart(10);
        console.log(`${label.padEnd(28)}${fmt(stats.min)}${fmt(stats.median)}${fmt(stats.max)}`);
    }
    console.log(`\ncompletion items          : ${report.checks.completionItemCount}`);
    console.log(`diagnostics               : ${report.checks.diagnosticCount}`);
    console.log(`empty reports on cancel   : ${report.checks.emptyFullReportsOnCancel}`
        + (report.checks.emptyFullReportsOnCancel === 0 ? '  (good)' : '  (BAD: clears the file)'));
    const drift = report.checks.completionDriftWhileTyping;
    const driftOk = drift.worstMissing === 0 && drift.worstExtra === 0;
    console.log(`completion drift mid-edit : -${drift.worstMissing} / +${drift.worstExtra}`
        + (driftOk ? '  (good)' : `  (differs from settled: ${drift.sampleMissing.join(', ')})`));

    return failedChecks(report);
}

// The correctness checks, as opposed to the timings. Timings are reported for
// comparison and never fail; these two are defects whatever the latency was.
function failedChecks(report) {
    const failures = [];
    if (report.checks.emptyFullReportsOnCancel > 0) {
        failures.push(`${report.checks.emptyFullReportsOnCancel} cancelled diagnostic pull(s) `
            + 'came back as an empty full report, which clears the file in the editor');
    }
    const drift = report.checks.completionDriftWhileTyping;
    if (drift.worstMissing > 0 || drift.worstExtra > 0) {
        failures.push(`completion mid-edit differed from settled by -${drift.worstMissing}`
            + ` / +${drift.worstExtra} items`);
    }
    return failures;
}

main().then((failures) => {
    if (!failures || failures.length === 0) return;
    console.error('\nFAILED:');
    for (const failure of failures) console.error(`  - ${failure}`);
    process.exit(1);
}).catch((error) => {
    console.error(String(error && error.message ? error.message : error));
    process.exit(1);
});

// Interactive latency harness for glua_ls.
'use strict';

const { spawn } = require('child_process');
const fs = require('fs');
const path = require('path');

// LSP ContentModified. The client is expected to re-send rather than wait.
const CONTENT_MODIFIED = -32801;

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

/** Prefers dist profile. */
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
            // Answer server requests.
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

/** VS Code capabilities for server. */
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

/** Finds position after '.' for completion. */
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

    // Completion position via --completion-find.
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
    const highlightAfterEdit = [];
    const highlightRetries = [];
    const editToFresh = [];
    const cancelledPulls = [];
    const completionDrift = [];
    let previousResultId;

    const labelsOf = (items) =>
        (Array.isArray(items) ? items : []).map((item) => item.label);

    // Completion position recomputed per call.
    const completionAt = async () => client.request('textDocument/completion', {
        textDocument: { uri },
        position: completionPosition(text),
        context: { triggerKind: 2, triggerCharacter: '.' },
    });

    let editSerial = 0;
    const editDocument = () => {
        version += 1;
        editSerial += 1;

        // Specific edit via --edit-find.
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

        // Insert edit per --edit mode.
        const inserted = opts.edit === 'comment'
            ? '-- perf\n'
            : `function _PerfProbe${editSerial}(a) return a end\n`;
        text = text.slice(0, editOffset) + inserted + text.slice(editOffset);
        client.notify('textDocument/didChange', {
            textDocument: { uri, version },
            contentChanges: [{ text }],
        });
    };

    // Wait for analysis to settle.
    const waitUntilQuiet = async () => {
        await client.request('textDocument/diagnostic', {
            textDocument: { uri }, previousResultId,
        });
    };

    for (let run = 0; run < opts.runs; run++) {
        await waitUntilQuiet();

        // Measure without pending edits.
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

        // Measure while typing.
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

        // Measure hover separately.
        editDocument();
        const [hovered, completed] = await Promise.all([
            client.request('textDocument/hover', {
                textDocument: { uri }, position: completionPosition(text),
            }),
            completionAt(),
        ]);
        typingHover.push(hovered.ms);
        typingCompletionConcurrent.push(completed.ms);

        // Measure semantic tokens.
        editDocument();
        const highlightStarted = Date.now();
        let highlightRetried = 0;
        for (;;) {
            const tokens = await client.request('textDocument/semanticTokens/full', {
                textDocument: { uri },
            });
            const code = tokens.message.error && tokens.message.error.code;
            if (code !== CONTENT_MODIFIED) break;
            highlightRetried++;
            await sleep(50);
        }
        highlightAfterEdit.push(Date.now() - highlightStarted);
        highlightRetries.push(highlightRetried);

        // Keystroke to the first answer any index-reading handler can give.
        editDocument();
        const fresh = await client.request('textDocument/diagnostic', {
            textDocument: { uri }, previousResultId,
        });
        editToFresh.push(fresh.ms);

        // Verify cancelled pull does not drop diagnostics.
        editDocument();
        const doomed = client.request('textDocument/diagnostic', {
            textDocument: { uri }, previousResultId,
        });
        await sleep(20);
        client.cancel(doomed.id);
        const cancelled = await doomed;
        const shape = describeReport(cancelled.message.result);
        cancelledPulls.push({
            thinFullReport: shape.kind === 'full'
                && shape.count < report.checks.diagnosticCount,
            errorCode: cancelled.message.error && cancelled.message.error.code,
        });
    }

    report.measurements.completionSettled = summarise(settledCompletion);
    report.measurements.completionWhileTyping = summarise(typingCompletion);
    report.measurements.diagnosticSettled = summarise(settledDiagnostic);
    report.measurements.hoverWhileTyping = summarise(typingHover);
    report.measurements.completionWhileTypingConcurrent = summarise(typingCompletionConcurrent);
    report.measurements.highlightAfterEdit = summarise(highlightAfterEdit);
    report.checks.highlightRetries = highlightRetries.reduce((a, b) => a + b, 0);
    report.measurements.editToFreshAnswer = summarise(editToFresh);
    report.checks.thinReportsOnCancel =
        cancelledPulls.filter((p) => p.thinFullReport).length;
    // Check mid-edit completion matches baseline.
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
        ['highlight after edit', report.measurements.highlightAfterEdit],
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
    console.log(`thin reports on cancel    : ${report.checks.thinReportsOnCancel}`
        + (report.checks.thinReportsOnCancel === 0 ? '  (good)' : '  (BAD: drops diagnostics)'));
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
    if (report.checks.thinReportsOnCancel > 0) {
        failures.push(`${report.checks.thinReportsOnCancel} cancelled diagnostic pull(s) came `
            + 'back thinner than the settled report, which drops diagnostics in the editor');
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

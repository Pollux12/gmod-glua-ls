// A minimal LSP client over stdio for the tools that drive a real `glua_ls`,
// with the capabilities VS Code's language client sends.
'use strict';

const fs = require('fs');
const path = require('path');

class LspClient {
    constructor(proc) {
        this.proc = proc;
        this.buffer = Buffer.alloc(0);
        this.nextId = 1;
        this.pending = new Map();
        this.onNotification = null;
        this.serverRequests = new Set();
        this.requestWaiters = new Map();
        this.statusAt = new Map();
        this.publishes = new Map();
        this.lastPublishAt = -Infinity;
        this.publishWaiters = new Set();
        proc.stdout.on('data', (chunk) => this._receive(chunk));
        // The server's phase profiler writes to stderr, so LSP_SERVER_STDERR
        // makes those numbers reachable instead of dropping them on the floor.
        const stderrPath = process.env.LSP_SERVER_STDERR;
        if (stderrPath) fs.writeFileSync(stderrPath, '');
        // LSP_SERVER_STDERR_STAMP=1 prefixes each line with the epoch ms it arrived at.
        const stamp = process.env.LSP_SERVER_STDERR_STAMP === '1';
        let partial = '';
        proc.stderr.on('data', (chunk) => {
            if (!stderrPath) return;
            if (!stamp) return fs.appendFileSync(stderrPath, chunk);
            const lines = (partial + chunk).split('\n');
            partial = lines.pop();
            const at = Date.now();
            if (lines.length) fs.appendFileSync(stderrPath, lines.map((line) => `${at} ${line}\n`).join(''));
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
            const waiter = this.requestWaiters.get(message.method);
            if (waiter) { this.requestWaiters.delete(message.method); waiter(message); }
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
        if (message.method === 'gluals/serverStatus' && message.params
            && !this.statusAt.has(message.params.state)) {
            this.statusAt.set(message.params.state, Date.now());
        }
        if (message.method === 'textDocument/publishDiagnostics' && message.params) {
            this._recordPublish(message.params);
        }
        if (this.onNotification) this.onNotification(message);
    }

    _recordPublish(params) {
        const at = Date.now();
        const entry = { diagnostics: params.diagnostics || [], version: params.version, at };
        this.publishes.set(params.uri, entry);
        this.lastPublishAt = at;
        for (const waiter of this.publishWaiters) {
            if (waiter.uri === params.uri && waiter.accept(entry)) {
                this.publishWaiters.delete(waiter);
                waiter.resolve(entry);
            }
        }
    }

    _write(payload) {
        const body = Buffer.from(JSON.stringify(payload), 'utf8');
        this.proc.stdin.write(`Content-Length: ${body.length}\r\n\r\n`);
        this.proc.stdin.write(body);
    }

    notify(method, params) {
        this._write({ jsonrpc: '2.0', method, params });
    }

    /** Resolves when the server next initiates `method` (or rejects on timeout). */
    waitForServerRequest(method, timeoutMs) {
        return new Promise((resolve, reject) => {
            const timer = setTimeout(
                () => { this.requestWaiters.delete(method); reject(new Error(`timeout waiting for ${method}`)); },
                timeoutMs,
            );
            this.requestWaiters.set(method, (message) => { clearTimeout(timer); resolve(message); });
        });
    }

    /** Resolves once the server has reported `state` in `gluals/serverStatus` (or rejects on timeout). */
    async waitForStatus(state, timeoutMs) {
        const deadline = Date.now() + timeoutMs;
        while (!this.statusAt.has(state)) {
            if (Date.now() >= deadline) throw new Error(`timeout waiting for server status ${state}`);
            await sleep(100);
        }
        return this.statusAt.get(state);
    }

    /** Resolves once no `textDocument/publishDiagnostics` has arrived for `quietMs` since `since`. */
    async waitForPublishQuiet(quietMs, since = -Infinity) {
        for (;;) {
            const idle = Date.now() - Math.max(this.lastPublishAt, since);
            if (idle >= quietMs) return;
            await sleep(quietMs - idle);
        }
    }

    /**
     * Resolves with the first publish for `uri` from now on that `accept`
     * takes, or with null once publishes go quiet for `quietMs` without one.
     * The entry's `at` is when it arrived, so polling does not skew timings.
     */
    async nextPublish(uri, accept, quietMs) {
        const since = Date.now();
        let found = null;
        const waiter = { uri, accept, resolve: (entry) => { found = entry; } };
        this.publishWaiters.add(waiter);
        try {
            while (!found) {
                const idle = Date.now() - Math.max(this.lastPublishAt, since);
                if (idle >= quietMs) break;
                await sleep(Math.min(quietMs - idle, 25));
            }
        } finally {
            this.publishWaiters.delete(waiter);
        }
        return found;
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
 * paths these tools drive. Keep in step with vscode-languageclient: the point
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

/**
 * Prefers the `dist` profile, which is what ships. `release` lacks its thin LTO
 * and single codegen unit, so measuring it reports numbers no user experiences.
 * A `dist` build older than the `release` one predates the code under test, so
 * the newer of the two is used.
 */
function defaultServerPath() {
    const exe = process.platform === 'win32' ? 'glua_ls.exe' : 'glua_ls';
    const dist = path.resolve(__dirname, '..', 'target', 'dist', exe);
    const release = path.resolve(__dirname, '..', 'target', 'release', exe);
    const builtAt = (file) => (fs.existsSync(file) ? fs.statSync(file).mtimeMs : -Infinity);
    return builtAt(dist) >= builtAt(release) ? dist : release;
}

function requireDir(value, name) {
    if (!value) throw new Error(`${name} is required`);
    if (!fs.existsSync(value)) throw new Error(`${name} does not exist: ${value}`);
    return path.resolve(value);
}

function fileUri(p) {
    const resolved = path.resolve(p).replace(/\\/g, '/');
    const withSlash = resolved.startsWith('/') ? resolved : `/${resolved}`;
    return `file://${encodeURI(withSlash).replace(/#/g, '%23').replace(/\?/g, '%3F')}`;
}

/**
 * Starts the server on stdio and initializes it as VS Code does. `push` is
 * whether the server delivers diagnostics only by `publishDiagnostics`: a
 * server that answers pulls advertises `diagnosticProvider`.
 */
async function startServer(server, workspace, annotations) {
    const { spawn } = require('child_process');
    const extraArgs = (process.env.LSP_SERVER_ARGS || '').split(' ').filter(Boolean);
    const proc = spawn(server, [
        '--communication', 'stdio',
        '--gmod-annotations-path', annotations,
        ...extraArgs,
    ], { stdio: ['pipe', 'pipe', 'pipe'] });
    const client = new LspClient(proc);
    const startedAt = Date.now();
    const initialized = await client.request('initialize', {
        processId: process.pid,
        rootUri: fileUri(workspace),
        workspaceFolders: [{ uri: fileUri(workspace), name: path.basename(workspace) }],
        capabilities: clientCapabilities(),
        initializationOptions: {},
        clientInfo: { name: 'Visual Studio Code', version: '1.95.0' },
    });
    client.notify('initialized', {});
    const result = initialized.message.result || {};
    const capabilities = result.capabilities || {};
    let loadedAt;
    try {
        loadedAt = await client.waitForStatus('workspaceLoaded', 300000);
    } catch {
        proc.kill();
        throw new Error('workspace never finished loading (5 minute timeout)');
    }
    const sync = capabilities.textDocumentSync;
    const incremental = (typeof sync === 'number' ? sync : sync?.change) === 2;
    return {
        proc, client, loadMs: loadedAt - startedAt, capabilities,
        push: !capabilities.diagnosticProvider, incremental,
    };
}

/**
 * The didChange `contentChanges` taking `before` to `after`: the full text, or
 * with incremental sync one ranged change over the span that differs, as VS
 * Code sends a keystroke. Positions count UTF-16 units and never split a
 * surrogate pair or a CRLF.
 */
function contentChanges(before, after, incremental) {
    if (!incremental) return [{ text: after }];
    const splits = (text, i) => {
        if (i <= 0 || i >= text.length) return false;
        const prev = text.charCodeAt(i - 1);
        return (prev === 13 && text.charCodeAt(i) === 10) || (prev >= 0xd800 && prev <= 0xdbff);
    };
    const limit = Math.min(before.length, after.length);
    let start = 0;
    while (start < limit && before[start] === after[start]) start++;
    while (splits(before, start) || splits(after, start)) start--;
    let suffix = 0;
    while (suffix < limit - start
        && before[before.length - 1 - suffix] === after[after.length - 1 - suffix]) suffix++;
    while (splits(before, before.length - suffix) || splits(after, after.length - suffix)) suffix--;
    const position = (offset) => {
        const lineStart = offset === 0 ? 0 : before.lastIndexOf('\n', offset - 1) + 1;
        let line = 0;
        for (let i = before.indexOf('\n'); i !== -1 && i < offset; i = before.indexOf('\n', i + 1)) line++;
        return { line, character: offset - lineStart };
    };
    return [{
        range: { start: position(start), end: position(before.length - suffix) },
        text: after.slice(start, after.length - suffix),
    }];
}

module.exports = {
    LspClient, sleep, clientCapabilities, defaultServerPath, requireDir, fileUri, startServer,
    contentChanges,
};

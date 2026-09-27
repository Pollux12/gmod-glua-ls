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

function fileUri(p) {
    const resolved = path.resolve(p).replace(/\\/g, '/');
    const withSlash = resolved.startsWith('/') ? resolved : `/${resolved}`;
    return `file://${encodeURI(withSlash).replace(/#/g, '%23').replace(/\?/g, '%3F')}`;
}

/** Starts the server on stdio and initializes it as VS Code does. */
async function startServer(server, workspace, annotations) {
    const { spawn } = require('child_process');
    const extraArgs = (process.env.LSP_SERVER_ARGS || '').split(' ').filter(Boolean);
    const proc = spawn(server, [
        '--communication', 'stdio',
        '--gmod-annotations-path', annotations,
        ...extraArgs,
    ], { stdio: ['pipe', 'pipe', 'pipe'] });
    const client = new LspClient(proc);
    let loadedAt = null;
    const previous = client.onNotification;
    client.onNotification = (message) => {
        if (message.method === 'gluals/serverStatus'
            && message.params && message.params.state === 'workspaceLoaded') {
            loadedAt = Date.now();
        }
        if (previous) previous(message);
    };
    const startedAt = Date.now();
    await client.request('initialize', {
        processId: process.pid,
        rootUri: fileUri(workspace),
        workspaceFolders: [{ uri: fileUri(workspace), name: path.basename(workspace) }],
        capabilities: clientCapabilities(),
        initializationOptions: {},
        clientInfo: { name: 'Visual Studio Code', version: '1.95.0' },
    });
    client.notify('initialized', {});
    const deadline = Date.now() + 300000;
    while (!loadedAt && Date.now() < deadline) await sleep(100);
    if (!loadedAt) {
        proc.kill();
        throw new Error('workspace never finished loading (5 minute timeout)');
    }
    return { proc, client, loadMs: loadedAt - startedAt };
}

module.exports = {
    LspClient, sleep, clientCapabilities, defaultServerPath, requireDir, fileUri, startServer,
};

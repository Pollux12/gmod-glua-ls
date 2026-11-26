// Long-running memory soak & spike harness for `glua_ls`.
//
// Drives a real `glua_ls` binary over stdio with VS Code capabilities AND the
// custom `gluals/*` requests `vscode-gmod-glua-ls` actually sends, while a
// high-frequency sampler records both settled memory and transient peak spikes
// per cycle. Also tests idle time, file open/close churn, watched file
// create/modify/delete, and config reloads.
//
// Usage:
//   LSP_CODEBASE=/path/to/workspace \
//   LSP_ANNOTATIONS=/path/to/annotations/output \
//   node tools/lsp_memory_soak.js [--cycles 60] [--idle-sec 15]
//       [--sample-ms 100] [--settle-every 5] [--reopen-every 5]
//       [--watcher-every 10] [--config-every 20] [--quiet-ms 1500] [--json]
//
// Edits to existing files are in-memory (`didChange`) and reverted before
// closing; watched-file churn uses a temporary `_gluals_soak_tmp_<pid>.lua`
// file that is always deleted on exit. The workspace is left unmodified.
'use strict';

const fs = require('fs');
const path = require('path');
const { execFileSync } = require('child_process');
const {
    sleep, defaultServerPath, requireDir, fileUri, startServer, contentChanges,
} = require('./lsp_client');

function parseArgs(argv) {
    const opts = {
        cycles: 60,
        idleSec: 15,
        sampleMs: 100,
        settleEvery: 5,
        reopenEvery: 5,
        watcherEvery: 10,
        configEvery: 20,
        quietMs: 1500,
        json: false,
    };
    for (let i = 2; i < argv.length; i++) {
        const a = argv[i];
        if (a === '--cycles') opts.cycles = Number(argv[++i]);
        else if (a === '--idle-sec') opts.idleSec = Number(argv[++i]);
        else if (a === '--sample-ms') opts.sampleMs = Number(argv[++i]);
        else if (a === '--settle-every') opts.settleEvery = Number(argv[++i]);
        else if (a === '--reopen-every') opts.reopenEvery = Number(argv[++i]);
        else if (a === '--watcher-every') opts.watcherEvery = Number(argv[++i]);
        else if (a === '--config-every') opts.configEvery = Number(argv[++i]);
        else if (a === '--quiet-ms') opts.quietMs = Number(argv[++i]);
        else if (a === '--json') opts.json = true;
        else throw new Error(`unknown argument: ${a}`);
    }
    return opts;
}

/** Reads process memory in MiB (Linux /proc/<pid>/status, Windows PowerShell). */
function readProcessMem(pid) {
    if (process.platform === 'linux') {
        try {
            const status = fs.readFileSync(`/proc/${pid}/status`, 'utf8');
            const kb = (name) => {
                const m = new RegExp(`^${name}:\\s+(\\d+)\\s+kB`, 'm').exec(status);
                return m ? Number(m[1]) / 1024 : null;
            };
            let pss = null;
            let privDirty = null;
            try {
                const rollup = fs.readFileSync(`/proc/${pid}/smaps_rollup`, 'utf8');
                const rkb = (name) => {
                    const m = new RegExp(`^${name}:\\s+(\\d+)\\s+kB`, 'm').exec(rollup);
                    return m ? Number(m[1]) / 1024 : null;
                };
                pss = rkb('Pss');
                privDirty = rkb('Private_Dirty');
            } catch { /* ignore */ }
            const rss = kb('VmRSS');
            const anon = kb('RssAnon');
            const hwm = kb('VmHWM');
            const threads = (() => {
                const m = /^Threads:\s+(\d+)/m.exec(status);
                return m ? Number(m[1]) : null;
            })();
            return { rss, privateMb: privDirty ?? anon ?? rss, pss, hwm, threads };
        } catch {
            return null;
        }
    }
    if (process.platform === 'win32') {
        try {
            const out = execFileSync(
                'powershell',
                [
                    '-NoProfile', '-Command',
                    `$p = Get-Process -Id ${pid} -ErrorAction Stop; "$($p.WorkingSet64),$($p.PrivateMemorySize64),$($p.PeakWorkingSet64),$($p.Threads.Count)"`,
                ],
                { encoding: 'utf8', timeout: 3000 },
            ).trim();
            const [ws, priv, peakWs, threads] = out.split(',').map(Number);
            return {
                rss: ws / (1024 * 1024),
                privateMb: priv / (1024 * 1024),
                pss: null,
                hwm: peakWs / (1024 * 1024),
                threads,
            };
        } catch {
            return null;
        }
    }
    return null;
}

/** Collects candidate Lua files: a mix of large files and shared/autorun files. */
function pickTargetFiles(root, maxFiles = 6) {
    const all = [];
    const walk = (dir) => {
        let entries;
        try { entries = fs.readdirSync(dir, { withFileTypes: true }); } catch { return; }
        for (const e of entries) {
            if (e.name === '.git' || e.name === 'node_modules') continue;
            const p = path.join(dir, e.name);
            if (e.isDirectory()) walk(p);
            else if (e.name.endsWith('.lua')) {
                try {
                    const stat = fs.statSync(p);
                    if (stat.size >= 512 && stat.size <= 512 * 1024) {
                        all.push({ path: p, rel: path.relative(root, p).replace(/\\/g, '/'), size: stat.size });
                    }
                } catch { /* ignore */ }
            }
        }
    };
    walk(root);
    if (all.length === 0) throw new Error(`no .lua files found under ${root}`);

    const picked = [];
    const seen = new Set();
    const add = (item) => {
        if (!item || seen.has(item.path) || picked.length >= maxFiles) return;
        seen.add(item.path);
        picked.push(item);
    };

    // Prioritize shared/init/autorun files (trigger cross-file invalidations)
    const shared = all
        .filter((f) => /(?:^|\/)(?:shared|sh_|init|autorun)/i.test(f.rel))
        .sort((a, b) => b.size - a.size);
    add(shared[0]);
    add(shared[1]);

    // Plus the largest general Lua files
    const bySize = [...all].sort((a, b) => b.size - a.size);
    for (const f of bySize) {
        add(f);
        if (picked.length >= maxFiles) break;
    }
    return picked;
}

function findHoverPos(text) {
    const lines = text.split('\n');
    for (let line = 0; line < lines.length; line++) {
        const m = /\b([A-Za-z_][A-Za-z0-9_]*)\s*[.:]\s*([A-Za-z_][A-Za-z0-9_]*)/.exec(lines[line]);
        if (m) {
            return {
                dotPos: { line, character: m.index + m[1].length + 1 },
                hoverPos: { line, character: m.index + m[1].length + 2 },
            };
        }
    }
    return { dotPos: { line: 0, character: 0 }, hoverPos: { line: 0, character: 0 } };
}

function fmt(n) {
    return n == null || !Number.isFinite(n) ? 'n/a' : `${n.toFixed(0)} MiB`;
}

async function main() {
    const opts = parseArgs(process.argv);
    const workspace = requireDir(process.env.LSP_CODEBASE, 'LSP_CODEBASE');
    const annotations = requireDir(process.env.LSP_ANNOTATIONS, 'LSP_ANNOTATIONS');
    const server = process.env.LSP_SERVER || defaultServerPath();
    if (!fs.existsSync(server)) throw new Error(`server binary not found: ${server}`);

    const targets = pickTargetFiles(workspace, 6).map((f) => {
        const original = fs.readFileSync(f.path, 'utf8');
        const positions = findHoverPos(original);
        return {
            ...f,
            uri: fileUri(f.path),
            original,
            current: original,
            version: 0,
            open: false,
            ...positions,
        };
    });

    const tmpWatcherFile = path.join(workspace, `_gluals_soak_tmp_${process.pid}.lua`);
    const tmpWatcherUri = fileUri(tmpWatcherFile);

    console.error(`=== GLuaLS Memory Soak ===`);
    console.error(`Server:      ${server}`);
    console.error(`Workspace:   ${workspace}`);
    console.error(`Annotations: ${annotations}`);
    console.error(`Targets (${targets.length}): ${targets.map((t) => `${t.rel} (${(t.size / 1024).toFixed(1)} KB)`).join(', ')}`);
    console.error(`Cycles: ${opts.cycles}, Idle: ${opts.idleSec}s, Sample: ${opts.sampleMs}ms\n`);

    const startedAt = Date.now();
    const { proc, client, loadMs, incremental } = await startServer(server, workspace, annotations);
    const pid = proc.pid;

    // High-frequency background memory sampler (captures transient peaks).
    let intervalPeakRss = 0;
    let intervalPeakPriv = 0;
    let globalPeakRss = 0;
    let globalPeakPriv = 0;
    const pollSample = () => {
        const m = readProcessMem(pid);
        if (!m) return;
        if (m.rss != null) {
            if (m.rss > intervalPeakRss) intervalPeakRss = m.rss;
            if (m.rss > globalPeakRss) globalPeakRss = m.rss;
        }
        if (m.privateMb != null) {
            if (m.privateMb > intervalPeakPriv) intervalPeakPriv = m.privateMb;
            if (m.privateMb > globalPeakPriv) globalPeakPriv = m.privateMb;
        }
    };
    const samplerTimer = setInterval(pollSample, opts.sampleMs);

    const records = [];
    const recordPoint = (label, cycle) => {
        pollSample();
        const m = readProcessMem(pid) || {};
        const rec = {
            cycle,
            label,
            elapsedSec: Number(((Date.now() - startedAt) / 1000).toFixed(1)),
            rss: m.rss ?? null,
            privateMb: m.privateMb ?? null,
            pss: m.pss ?? null,
            hwm: m.hwm ?? null,
            cyclePeakRss: intervalPeakRss || m.rss || null,
            cyclePeakPriv: intervalPeakPriv || m.privateMb || null,
            threads: m.threads ?? null,
        };
        records.push(rec);
        intervalPeakRss = 0;
        intervalPeakPriv = 0;
        if (!opts.json) {
            console.error(
                `[${String(rec.elapsedSec).padStart(6)}s] ${label.padEnd(20)} ` +
                `RSS=${fmt(rec.rss).padStart(9)}  Priv=${fmt(rec.privateMb).padStart(9)}  ` +
                `PeakRSS=${fmt(rec.cyclePeakRss).padStart(9)}  HWM=${fmt(rec.hwm).padStart(9)}  ` +
                `Thr=${rec.threads ?? '?'}`,
            );
        }
        return rec;
    };

    const openFile = (t) => {
        if (t.open) return;
        t.version += 1;
        t.current = t.original;
        t.open = true;
        client.notify('textDocument/didOpen', {
            textDocument: { uri: t.uri, languageId: 'lua', version: t.version, text: t.current },
        });
    };

    const editFile = (t, nextText) => {
        if (!t.open) openFile(t);
        const changes = contentChanges(t.current, nextText, incremental);
        t.version += 1;
        t.current = nextText;
        client.notify('textDocument/didChange', {
            textDocument: { uri: t.uri, version: t.version },
            contentChanges: changes,
        });
    };

    const closeFile = (t) => {
        if (!t.open) return;
        if (t.current !== t.original) editFile(t, t.original);
        t.open = false;
        client.notify('textDocument/didClose', { textDocument: { uri: t.uri } });
    };

    const safeReq = async (method, params, timeoutMs = 10000) => {
        const req = client.request(method, params);
        const res = await Promise.race([
            req,
            sleep(timeoutMs).then(() => { client.cancel(req.id); return null; }),
        ]);
        return res;
    };

    const triggerFile = process.env.GLUALS_SAMPLE_TRIGGER_FILE || null;
    const triggerSample = async (label) => {
        if (!triggerFile) return;
        fs.writeFileSync(triggerFile, label);
        const deadline = Date.now() + 15000;
        while (fs.existsSync(triggerFile) && Date.now() < deadline) {
            await sleep(50);
        }
        await sleep(150);
    };

    try {
        recordPoint('workspaceLoaded', 0);

        // Wait for startupComplete + initial workspace diagnostics to settle.
        if (!client.statusAt.has('startupComplete')) {
            await client.waitForStatus('startupComplete', 300000);
        }
        await client.waitForPublishQuiet(opts.quietMs, Date.now());
        recordPoint('startupSettled', 0);
        await triggerSample('startup');

        // Initial explorer request (as VS Code GMod Explorer sends on startup).
        await safeReq('gluals/gmodScriptedClassesV2', {});

        // Open initial targets.
        for (const t of targets) openFile(t);
        await sleep(300);
        recordPoint('filesOpened', 0);

        for (let cycle = 1; cycle <= opts.cycles; cycle++) {
            const t = targets[(cycle - 1) % targets.length];
            openFile(t);

            // 1. Burst of realistic incremental edits (new global fn + member + brief syntax error + fix).
            const base = t.original;
            const step1 = `${base}\nfunction _SOAK_GLOBAL_${cycle}(ply)\n    local x = ply:Nick()\n    return x\nend\n`;
            const step2 = `${step1}_SOAK_TBL_${cycle} = { val = ${cycle}, name = "soak_${cycle}" }\nlocal _broken = _SOAK_TBL_${cycle}.\n`;
            const step3 = `${step1}_SOAK_TBL_${cycle} = { val = ${cycle}, name = "soak_${cycle}" }\nlocal _ok = _SOAK_TBL_${cycle}.val\n`;

            editFile(t, step1);
            await sleep(60);
            // Mid-edit requests (hover + completion + annotator + semanticTokens) while debounce/reindex runs.
            const midHover = safeReq('textDocument/hover', { textDocument: { uri: t.uri }, position: t.hoverPos });
            const midAnnot = safeReq('gluals/annotator', { uri: t.uri });
            await sleep(40);
            editFile(t, step2);
            const midComp = safeReq('textDocument/completion', {
                textDocument: { uri: t.uri },
                position: { line: step2.split('\n').length - 2, character: 36 },
            });
            await sleep(120);
            editFile(t, step3);
            await Promise.all([midHover, midAnnot, midComp]);

            // 2. Post-edit VS Code feature requests.
            await Promise.all([
                safeReq('textDocument/hover', { textDocument: { uri: t.uri }, position: t.hoverPos }),
                safeReq('textDocument/completion', { textDocument: { uri: t.uri }, position: t.dotPos }),
                safeReq('textDocument/semanticTokens/full', { textDocument: { uri: t.uri } }),
                safeReq('textDocument/inlayHint', {
                    textDocument: { uri: t.uri },
                    range: { start: { line: 0, character: 0 }, end: { line: 200, character: 0 } },
                }),
                safeReq('textDocument/codeLens', { textDocument: { uri: t.uri } }),
                safeReq('gluals/annotator', { uri: t.uri }),
            ]);

            // 3. Revert back to original text (like undo / clean edit).
            editFile(t, t.original);
            client.notify('textDocument/didSave', { textDocument: { uri: t.uri } });

            // 4. Periodic close & reopen churn.
            if (opts.reopenEvery > 0 && cycle % opts.reopenEvery === 0) {
                closeFile(t);
                await sleep(100);
                openFile(t);
                await safeReq('gluals/gmodScriptedClassesV2', {});
            }

            // 5. Periodic watched-file create / change / delete churn.
            if (opts.watcherEvery > 0 && cycle % opts.watcherEvery === 0) {
                fs.writeFileSync(
                    tmpWatcherFile,
                    `-- soak cycle ${cycle}\nfunction _SOAK_WATCH_${cycle}() return ${cycle} end\n`,
                );
                client.notify('workspace/didChangeWatchedFiles', {
                    changes: [{ uri: tmpWatcherUri, type: 1 }], // Created
                });
                await sleep(250);
                fs.writeFileSync(
                    tmpWatcherFile,
                    `-- soak cycle ${cycle} v2\nfunction _SOAK_WATCH_${cycle}() return ${cycle + 1} end\n`,
                );
                client.notify('workspace/didChangeWatchedFiles', {
                    changes: [{ uri: tmpWatcherUri, type: 2 }], // Changed
                });
                await sleep(250);
                fs.unlinkSync(tmpWatcherFile);
                client.notify('workspace/didChangeWatchedFiles', {
                    changes: [{ uri: tmpWatcherUri, type: 3 }], // Deleted
                });
                await sleep(250);
            }

            // 6. Periodic config reload churn.
            if (opts.configEvery > 0 && cycle % opts.configEvery === 0) {
                client.notify('workspace/didChangeConfiguration', { settings: {} });
                await sleep(400);
            }

            // 7. Settle & record memory.
            if (cycle === 1 || cycle === opts.cycles || (opts.settleEvery > 0 && cycle % opts.settleEvery === 0)) {
                await client.waitForPublishQuiet(opts.quietMs, Date.now());
                recordPoint(`cycle-${cycle}-settled`, cycle);
                if (cycle === 5 || cycle === opts.cycles) {
                    await triggerSample(`cycle-${cycle}`);
                }
            }
        }

        // Close all open files and settle.
        for (const t of targets) closeFile(t);
        await client.waitForPublishQuiet(opts.quietMs, Date.now());
        recordPoint('allClosed', opts.cycles);

        // Idle time soak (check for time-based leaks or allocator decay).
        if (opts.idleSec > 0) {
            const stepSec = Math.min(5, opts.idleSec);
            for (let s = stepSec; s <= opts.idleSec; s += stepSec) {
                await sleep(stepSec * 1000);
                recordPoint(`idle-${s}s`, opts.cycles);
            }
            await triggerSample('after-idle');
        }

        // Summary trend analysis (settled cycle points only).
        const settled = records.filter((r) => r.label.startsWith('cycle-') && r.privateMb != null);
        let growthMb = null;
        let mbPerCycle = null;
        if (settled.length >= 2) {
            const first = settled[0];
            const last = settled[settled.length - 1];
            growthMb = last.privateMb - first.privateMb;
            const dCycles = Math.max(1, last.cycle - first.cycle);
            mbPerCycle = growthMb / dCycles;
        }

        const summary = {
            loadMs,
            cycles: opts.cycles,
            globalPeakRssMb: Number(globalPeakRss.toFixed(1)),
            globalPeakPrivMb: Number(globalPeakPriv.toFixed(1)),
            settledGrowthPrivMb: growthMb != null ? Number(growthMb.toFixed(1)) : null,
            mbPerCycle: mbPerCycle != null ? Number(mbPerCycle.toFixed(2)) : null,
            records,
        };

        if (opts.json) {
            console.log(JSON.stringify(summary, null, 2));
        } else {
            console.error('\n=== Summary ===');
            console.error(`Load time:              ${loadMs} ms`);
            console.error(`Global peak RSS:        ${fmt(globalPeakRss)}`);
            console.error(`Global peak Private:    ${fmt(globalPeakPriv)}`);
            if (growthMb != null) {
                console.error(`Settled Private delta:  ${growthMb >= 0 ? '+' : ''}${growthMb.toFixed(1)} MiB (${mbPerCycle.toFixed(2)} MiB/cycle)`);
            }
        }
    } finally {
        clearInterval(samplerTimer);
        if (fs.existsSync(tmpWatcherFile)) {
            try { fs.unlinkSync(tmpWatcherFile); } catch { /* ignore */ }
        }
        try { await Promise.race([client.request('shutdown', null), sleep(2000)]); } catch { /* ignore */ }
        try { client.notify('exit', null); } catch { /* ignore */ }
        proc.kill();
    }
}

main().catch((err) => {
    console.error(err);
    process.exit(1);
});

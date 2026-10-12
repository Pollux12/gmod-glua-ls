// Diffs two `glua_check -f json` outputs by file-relative diagnostics.
// usage: node tools/diagnostics_diff.js reference.json current.json [limit]
// Exits 1 when they differ. Files are matched by path relative to the
// workspace root each file names, so outputs of copies compare.
'use strict';

const fs = require('fs');

const SEVERITY = { 1: 'error', 2: 'warning', 3: 'info', 4: 'hint' };

function load(file) {
    const entries = JSON.parse(fs.readFileSync(file, 'utf8'));
    const paths = entries.map((entry) => entry.file.replace(/\\/g, '/'));
    // The longest common directory prefix is the workspace root.
    let root = paths.length ? paths[0].slice(0, paths[0].lastIndexOf('/') + 1) : '';
    for (const p of paths) while (!p.startsWith(root)) root = root.slice(0, root.lastIndexOf('/', root.length - 2) + 1);
    const byFile = new Map();
    entries.forEach((entry, i) => {
        const keys = entry.diagnostics.map((d) => {
            const { start, end } = d.range;
            const range = `${start.line + 1}:${start.character + 1}-${end.line + 1}:${end.character + 1}`;
            return `${range} ${SEVERITY[d.severity] ?? d.severity} [${d.code}] ${String(d.message).replace(/[\r\n]+/g, ' ')}`;
        });
        byFile.set(paths[i].slice(root.length).toLowerCase(), keys);
    });
    return byFile;
}

const [reference, current, limit = '40'] = process.argv.slice(2);
const a = load(reference);
const b = load(current);
const lines = [];
for (const file of [...new Set([...a.keys(), ...b.keys()])].sort()) {
    const count = (keys) => (keys || []).reduce((m, k) => m.set(k, (m.get(k) || 0) + 1), new Map());
    const ca = count(a.get(file));
    const cb = count(b.get(file));
    for (const [k, n] of ca) for (let i = (cb.get(k) || 0); i < n; i++) lines.push(`- ${file}:${k}`);
    for (const [k, n] of cb) for (let i = (ca.get(k) || 0); i < n; i++) lines.push(`+ ${file}:${k}`);
}
console.log(lines.length === 0 ? 'IDENTICAL' : `${lines.length} differences`);
for (const line of lines.slice(0, Number(limit))) console.log(line.slice(0, 300));
process.exit(lines.length === 0 ? 0 : 1);

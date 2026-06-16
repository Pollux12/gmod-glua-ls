// Compares two glua_check JSON diagnostic outputs.
// Usage: node diag_diff.js baseline.json candidate.json
const fs = require('fs');
function load(p) {
  const arr = JSON.parse(fs.readFileSync(p, 'utf8'));
  // arr: [{ uri?, diagnostics: [...] }, ...]  (per-file)
  const set = new Map(); // key -> count
  let total = 0;
  for (const file of arr) {
    const uri = file.uri || file.path || file.file || '';
    for (const d of (file.diagnostics || [])) {
      const r = d.range || {};
      const s = r.start || {}, e = r.end || {};
      const key = [uri, d.code, d.severity, s.line, s.character, e.line, e.character, d.message].join('|');
      set.set(key, (set.get(key) || 0) + 1);
      total++;
    }
  }
  return { set, total };
}
const [, , aPath, bPath] = process.argv;
const a = load(aPath), b = load(bPath);
let removed = 0, added = 0;
const removedByCode = {}, addedByCode = {};
const removedSamples = [], addedSamples = [];
for (const [k, c] of a.set) {
  if (!b.set.has(k)) { removed += c; const code = k.split('|')[1]; removedByCode[code] = (removedByCode[code]||0)+c; if (removedSamples.length<25) removedSamples.push(k); }
}
for (const [k, c] of b.set) {
  if (!a.set.has(k)) { added += c; const code = k.split('|')[1]; addedByCode[code] = (addedByCode[code]||0)+c; if (addedSamples.length<25) addedSamples.push(k); }
}
console.log(`baseline total=${a.total} candidate total=${b.total} (delta ${b.total-a.total})`);
console.log(`REMOVED (in baseline, not candidate): ${removed}`);
console.log('  by code:', JSON.stringify(removedByCode));
console.log(`ADDED (in candidate, not baseline): ${added}`);
console.log('  by code:', JSON.stringify(addedByCode));
if (removedSamples.length) { console.log('\n--- REMOVED samples ---'); removedSamples.forEach(s=>console.log('  '+s.slice(0,180))); }
if (addedSamples.length) { console.log('\n--- ADDED samples ---'); addedSamples.forEach(s=>console.log('  '+s.slice(0,180))); }

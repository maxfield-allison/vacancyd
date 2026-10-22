import { readFile, writeFile } from 'node:fs/promises';

// The renderer uses the daemon's whitelist; an unsupported Rust type must
// fail the build instead of becoming an independently guessed wire field.
const source = await readFile('../../src/settings.rs', 'utf8');
function typeScript(type) {
  const simple = { String: 'string', bool: 'boolean', u64: 'number', f64: 'number' };
  if (simple[type]) return simple[type];
  const option = /^Option<(.+)>$/.exec(type);
  if (option) return `${typeScript(option[1])} | null`;
  const vector = /^Vec<(.+)>$/.exec(type);
  if (vector) return `(${typeScript(vector[1])})[]`;
  if (names.has(type)) return type;
  throw new Error(`Unsupported settings wire type: ${type}`);
}
const structs = [...source.matchAll(/pub struct (\w+) \{([^}]+)\}/g)];
const names = new Set(structs.map(match => match[1]));
if (!names.has('SettingsSnapshot') || !names.has('SettingsUpdate')) throw new Error('Missing settings contract');
let output = '// Generated from src/settings.rs by generate-settings.mjs.\n';
for (const [, name, body] of structs) {
  const fields = [...body.matchAll(/pub (\w+): ([^,\n]+),/g)];
  if (!fields.length) throw new Error(`Missing fields in ${name}`);
  output += `export type ${name} = {\n${fields.map(([, field, type]) => `  ${field}: ${typeScript(type)};`).join('\n')}\n};\n\n`;
}
const destination = 'src/settings-wire.ts';
if (process.argv.includes('--check')) {
  if (await readFile(destination, 'utf8') !== output) throw new Error('Settings wire types are stale; run npm run build');
} else {
  await writeFile(destination, output);
}

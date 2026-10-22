// Identify a built console frontend by its contents: `node dist-digest.mjs [dir]`
// prints every file's SHA-256 under its forward-slash path and one SHA-256 over
// that sorted listing. Two builds with the same digest hold the same bytes, so
// the digest shows that the frontend a release embeds is the one that was tested.
import { createHash } from 'node:crypto';
import { readdir, readFile } from 'node:fs/promises';
import { join, relative, sep } from 'node:path';

const root = process.argv[2] ?? 'dist';

async function walk(directory, found) {
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) await walk(path, found);
    else if (entry.isFile()) found.push(path);
    else throw new Error(`${path} is neither a regular file nor a directory`);
  }
  return found;
}

const files = {};
for (const path of await walk(root, [])) {
  const name = relative(root, path).split(sep).join('/');
  files[name] = createHash('sha256').update(await readFile(path)).digest('hex');
}
const names = Object.keys(files).sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
if (names.length === 0) throw new Error(`${root} holds no files`);
const listing = names.map((name) => `${files[name]}  ${name}\n`).join('');
const sorted = Object.fromEntries(names.map((name) => [name, files[name]]));
const sha256 = createHash('sha256').update(listing).digest('hex');
process.stdout.write(JSON.stringify({ sha256, files: sorted }, null, 2) + '\n');

import { copyFile, cp, mkdir } from 'node:fs/promises';
await mkdir('dist', { recursive: true });
for (const name of ['index.html', 'style.css']) await copyFile(name, `dist/${name}`);
await cp('assets', 'dist/assets', { recursive: true });
for (const name of ['mark-idle.svg', 'mark-lit.svg']) {
  await copyFile(`../../docs/assets/brand/${name}`, `dist/assets/${name}`);
}

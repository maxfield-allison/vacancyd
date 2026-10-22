// Compile the existing SVG mark into Windows ICO sizes; no alternate artwork.
import { chromium } from '@playwright/test';
import { readFile, writeFile, mkdir } from 'node:fs/promises';
const mark = await readFile('../../docs/assets/brand/mark-lit.svg', 'utf8');
// docs/brand.md: below 24px, drop the rail and use the plate alone. The crop
// is the framed plate's outer edge, squared around its centre.
const rail = /<g id="rail"[^>]*>[\s\S]*?<\/g>\s*/;
if (!rail.test(mark)) throw new Error('mark-lit.svg has no <g id="rail"> to drop at small sizes');
const plate = mark.replace(rail, '').replace('viewBox="0 0 64 64"', 'viewBox="6.5 9.5 51 51"');
const browser = await chromium.launch();
try {
  const page = await browser.newPage();
  const images = [];
  for (const size of [16, 24, 32, 48, 64, 128, 256]) {
    const svg = size < 24 ? plate : mark;
    const encoded = await page.evaluate(async ({ svg, size }) => {
      const img = new Image();
      img.src = 'data:image/svg+xml;base64,' + btoa(unescape(encodeURIComponent(svg)));
      await img.decode();
      const canvas = document.createElement('canvas');
      canvas.width = canvas.height = size;
      canvas.getContext('2d').drawImage(img, 0, 0, size, size);
      return canvas.toDataURL('image/png').split(',')[1];
    }, { svg, size });
    images.push({ size, bytes: Buffer.from(encoded, 'base64') });
  }
  const header = Buffer.alloc(6 + 16 * images.length);
  header.writeUInt16LE(1, 2); header.writeUInt16LE(images.length, 4);
  let offset = header.length;
  images.forEach(({ size, bytes }, index) => {
    const base = 6 + index * 16;
    header[base] = header[base + 1] = size === 256 ? 0 : size;
    header.writeUInt16LE(1, base + 4); header.writeUInt16LE(32, base + 6);
    header.writeUInt32LE(bytes.length, base + 8); header.writeUInt32LE(offset, base + 12);
    offset += bytes.length;
  });
  await mkdir('../icons', { recursive: true });
  await writeFile('../icons/icon.ico', Buffer.concat([header, ...images.map(i => i.bytes)]));
} finally { await browser.close(); }

// Export original product artwork with the bundled brand font and mark.
import { chromium } from "@playwright/test";
import { readFile, writeFile } from "node:fs/promises";
const font = (await readFile("assets/fonts/Archivo.ttf")).toString("base64");
const mark = (await readFile("../../docs/assets/brand/mark-lit.svg")).toString(
  "base64",
);
const browser = await chromium.launch();
try {
  const page = await browser.newPage({
    viewport: { width: 1200, height: 630 },
    deviceScaleFactor: 1,
  });
  await page.setContent(`<!doctype html><html lang="en"><style>
  @font-face{font-family:Archivo;src:url(data:font/ttf;base64,${font})}*{box-sizing:border-box}
  body{margin:0;width:1200px;height:630px;background:#0b0f17;color:white;font-family:Archivo,sans-serif;padding:64px 76px}
  header{display:flex;align-items:center;gap:18px;font-size:34px;font-weight:700;letter-spacing:-.02em}header img{width:56px;height:56px}
  h1{font-size:94px;letter-spacing:-.06em;line-height:1.04;margin:52px 0 24px;font-weight:700}h1 span{color:#ffb454}
  p{font-size:24px;color:#94a3b8;margin:0;line-height:1.5}footer{position:absolute;bottom:54px;font-size:18px;color:#94a3b8;letter-spacing:.02em}
  </style><header><img alt="" src="data:image/svg+xml;base64,${mark}">vacancyd</header>
  <h1>Spare compute.<br><span>On your terms.</span></h1><p>Keep your workstation. Lend its spare capacity for local AI.</p>
  <footer>vacancyd.com &nbsp; · &nbsp; Open source. Apache-2.0.</footer></html>`);
  await page.evaluate(() => document.fonts.ready);
  await page.screenshot({ path: "../../website/assets/social-card.png" });
  await writeFile(
    "../../website/assets/artwork-provenance.json",
    JSON.stringify(
      {
        artwork: "Original vacancyd typography and existing project mark",
        font: "Bundled Archivo, SIL Open Font License 1.1",
        renderer: "capture-brand.mjs with Chromium",
        dimensions: { width: 1200, height: 630 },
        license: "Apache-2.0; bundled fonts retain OFL-1.1",
      },
      null,
      2,
    ) + "\n",
  );
} finally {
  await browser.close();
}

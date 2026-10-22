import { test, expect } from '@playwright/test';
import { readFile } from 'node:fs/promises';

const defaultSettings = JSON.parse(await readFile('../../tests/fixtures/settings/default.json', 'utf8'));

async function fixture(name) {
  const status = JSON.parse(await readFile(`../../tests/fixtures/status/${name}.json`, 'utf8'));
  for (const resource of status.resources) {
    resource.required ??= true;
    resource.required_by ??= resource.required ? [status.upstreams[0]?.id ?? 'configured-route'] : [];
    resource.adapter_mapping ??= null;
  }
  for (const upstream of status.upstreams) {
    upstream.listener ??= { state: 'unknown', quality: 'unknown', age_s: null, last_error: null };
  }
  return status;
}
async function open(page, poll) {
  await page.addInitScript(value => { window.__TAURI__ = { core: { invoke: async () => value } }; }, poll);
  await page.goto('/');
}

for (const [name, title, tone] of [
  ['lending', 'Room for borrowed work', 'lending'],
  ['owner-veto', 'Owner policy pauses new work', 'withheld'],
  ['disabled', 'Lending is paused', 'withheld'],
  ['stale', 'Waiting for trustworthy readings', 'fault'],
  ['unknown', 'Waiting for trustworthy readings', 'fault'],
  ['calibrating', 'Preparing safe lending', 'withheld'],
  ['upstream-refusal', 'Holding spare capacity', 'withheld'],
  ['owner-veto-exempt', 'Only exempt routes are lending', 'lending'],
]) {
  test(`renders ${name} without changing daemon decisions`, async ({ page }) => {
    await open(page, { state: 'online', status: await fixture(name), console_version: '0.1.0' });
    await expect(page.locator('#decision-title')).toHaveText(title);
    await expect(page.locator('#decision')).toHaveAttribute('data-tone', tone);
    if (name === 'lending') {
      await expect(page.locator('#decision .vacancy-sign')).toHaveAttribute('data-vacancy', 'true');
      await expect(page.locator('#decision .vacancy-sign')).toHaveAccessibleName('Vacancy');
      await expect(page.locator('#decision .sign-no')).toHaveCSS('color', 'rgb(91, 73, 52)');
      await expect(page.locator('#sign')).toHaveAttribute('src', 'assets/mark-lit.svg');
    }
    if (name === 'owner-veto') {
      await expect(page.locator('#decision .vacancy-sign')).toHaveAttribute('data-vacancy', 'false');
      await expect(page.locator('#decision .vacancy-sign')).toHaveAccessibleName('No vacancy');
      await expect(page.locator('#decision .sign-no')).toHaveCSS('color', 'rgb(255, 180, 84)');
    }
    if (['stale', 'unknown'].includes(name)) {
      await expect(page.locator('#resources .measurement')).toHaveText('Unknown');
      await expect(page.locator('#resources progress')).toHaveCount(0);
    }
    if (name === 'calibrating') await expect(page.locator('#resources')).toContainText('Calibrating:');
  });
}

test('an exempt route lending through the owner veto is named, never hidden behind "the machine is yours"', async ({ page }) => {
  await page.setViewportSize({ width: 1180, height: 740 });
  await open(page, { state: 'online', status: await fixture('owner-veto-exempt'), console_version: '1.0.0' });
  await expect(page.locator('#decision-title')).toHaveText('Only exempt routes are lending');
  await expect(page.locator('#decision-title')).not.toHaveText('Owner policy pauses new work');
  await expect(page.locator('#decision-detail')).toHaveText('Owner policy pauses 1 route (owner active). chatterbox is exempt and keeps lending on its own hardware. See Workloads.');
  await expect(page.locator('#decision .vacancy-sign')).toHaveAttribute('data-vacancy', 'true');
  await expect(page.locator('#announcement')).toContainText('Only exempt routes are lending');
  await expect(page.locator('#veto-exemptions')).toBeVisible();
  await expect(page.locator('#veto-exemptions')).toHaveText('Owner presence never pauses chatterbox. It is set to owner_scope = "resource" and yields only to its own hardware. See Workloads.');
  // The overview still fits a normal desktop without scrolling.
  expect(await page.locator('main').evaluate(e => e.scrollHeight <= e.clientHeight)).toBe(true);
  await page.evaluate(() => document.fonts.ready);
  await page.screenshot({ path: 'test-results/console-owner-veto-exempt.png' });
  await page.getByRole('link', { name: 'Workloads', exact: true }).click();
  const exempt = page.locator('#upstreams article').filter({ has: page.getByRole('heading', { name: 'chatterbox' }) });
  const host = page.locator('#upstreams article').filter({ has: page.getByRole('heading', { name: 'inference' }) });
  await expect(exempt.locator('.fact').filter({ hasText: 'OWNER PRESENCE' }).locator('strong')).toHaveText('Exempt');
  await expect(exempt).toContainText('Keeps lending while you use this machine (owner_scope = "resource"). Owner pressure on gpu1 still refuses and reclaims it.');
  await expect(exempt).not.toContainText('Nothing measures');
  await expect(host).not.toContainText('Exempt');
  await expect(host).toContainText('owner active');
  await page.screenshot({ path: 'test-results/console-owner-veto-exempt-workloads.png' });
  expect(await page.locator('body').textContent()).not.toMatch(/[–—]/);
});

test('an exempt route holding work under the veto is still reported as lending', async ({ page }) => {
  const status = await fixture('owner-veto-exempt');
  const chatterbox = status.upstreams[1];
  Object.assign(chatterbox, { vacancy: false, refusal: { reason: 'resource_busy', detail: 'a job still owns this resource', retry_after_s: 2 } });
  status.work = [{ id: 4, upstream: 'chatterbox', model: null, resources: ['gpu1'], age_s: 3, completion: 'response_close', phase: 'forwarding', tracking_id: null, model_state: 'unknown' }];
  await open(page, { state: 'online', status, console_version: '1.0.0' });
  await expect(page.locator('#decision-title')).toHaveText('Only exempt routes are lending');
  await expect(page.locator('#decision')).toHaveAttribute('data-tone', 'withheld');
  await expect(page.locator('#decision .vacancy-sign')).toHaveAttribute('data-vacancy', 'false');
});

test('an idle exempt route leaves the machine yours but stays named', async ({ page }) => {
  const status = await fixture('owner-veto-exempt');
  Object.assign(status.upstreams[1], { vacancy: false, refusal: { reason: 'resource_pressured', detail: "resource 'gpu1' has 41.0% non-lender pressure (threshold 30.0%)", retry_after_s: 30 } });
  await open(page, { state: 'online', status, console_version: '1.0.0' });
  await expect(page.locator('#decision-title')).toHaveText('Owner policy pauses new work');
  await expect(page.locator('#decision-detail')).toHaveText('owner active. chatterbox is exempt from owner presence but not lending now.');
  await expect(page.locator('#veto-exemptions')).toBeVisible();
});

test('without the veto, the lending summary says which route owner protection does not cover', async ({ page }) => {
  const status = await fixture('owner-veto-exempt');
  status.owner_veto = { active: false, cause: null, exempt_upstreams: ['chatterbox'] };
  Object.assign(status.upstreams[0], { vacancy: true, refusal: null });
  await open(page, { state: 'online', status, console_version: '1.0.0' });
  await expect(page.locator('#decision-title')).toHaveText('Room for borrowed work');
  await expect(page.locator('#decision-detail')).toHaveText('The default budget fits for inference and chatterbox (2 of 2 routes). Model requirements or target health can still prevent work. See Workloads. Owner protection covers every route except chatterbox.');
  await expect(page.locator('#veto-exemptions')).toBeVisible();
});

test('an exempt route on unmeasured hardware says nothing reclaims it there', async ({ page }) => {
  const status = await fixture('owner-veto-exempt');
  const memory = { ...structuredClone(status.resources[1]), id: 'memory', kind: 'system-memory', label: 'System memory', pressure: null, adapter_mapping: null, required_by: ['chatterbox'] };
  status.resources[1].pressure = null;
  status.resources.push(memory);
  await open(page, { state: 'online', status, console_version: '1.0.0' });
  await page.getByRole('link', { name: 'Workloads', exact: true }).click();
  const exempt = page.locator('#upstreams article').filter({ has: page.getByRole('heading', { name: 'chatterbox' }) });
  await expect(exempt).toContainText('Nothing measures your use of gpu1 and memory, so only free capacity limits it there.');
  await expect(exempt).not.toContainText('Owner pressure on');
});

test('host-only configurations show no exemption note', async ({ page }) => {
  await open(page, { state: 'online', status: await fixture('owner-veto'), console_version: '1.0.0' });
  await expect(page.locator('#decision-title')).toHaveText('Owner policy pauses new work');
  await expect(page.locator('#decision-detail')).toHaveText('owner active');
  await expect(page.locator('#veto-exemptions')).toBeHidden();
  await page.getByRole('link', { name: 'Workloads', exact: true }).click();
  await expect(page.locator('#upstreams')).not.toContainText('Exempt');
});

test('partial admission names the available route while voice remains refused', async ({ page }) => {
  const status = await fixture('lending');
  const helper = status.upstreams[0];
  helper.id = 'helper';
  status.upstreams.push({ ...structuredClone(helper), id: 'voice', vacancy: false,
    refusal: { reason: 'insufficient_capacity', detail: 'Breeze needs 11500 MiB free.', retry_after_s: 30 } });
  await open(page, { state: 'online', status, console_version: '1.0.0' });
  await expect(page.locator('#decision-detail')).toContainText('The default budget fits for helper (1 of 2 routes).');
  await page.getByRole('link', { name: 'Workloads', exact: true }).click();
  const voice = page.locator('#upstreams article').filter({ has: page.getByRole('heading', { name: 'voice', exact: true }) });
  await expect(voice.locator('.route-decision')).toHaveText('Breeze needs 11500 MiB free.');
  await expect(voice.locator('.vacancy-sign')).toHaveAttribute('data-vacancy', 'false');
});

test('admission capacity does not promise a healthy voice target', async ({ page }) => {
  const status = await fixture('lending');
  status.upstreams[0].id = 'voice';
  status.upstreams[0].health = { quality: 'unknown', age_s: null, last_error: 'Target is unreachable.', http_status: null };
  await open(page, { state: 'online', status, console_version: '1.0.0' });
  await page.getByRole('link', { name: 'Workloads', exact: true }).click();
  await expect(page.locator('#upstreams .route-decision')).toHaveText('The default budget fits. Target health is unconfirmed; requests may fail.');
  await expect(page.locator('#upstreams')).toContainText('Target is unreachable.');
});

test('the exemption note fits a narrow window', async ({ page }) => {
  await page.setViewportSize({ width: 480, height: 320 });
  await open(page, { state: 'online', status: await fixture('owner-veto-exempt'), console_version: '1.0.0' });
  await expect(page.locator('#veto-exemptions')).toBeAttached();
  await page.locator('#veto-exemptions').scrollIntoViewIfNeeded();
  await expect(page.locator('#veto-exemptions')).toBeInViewport();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
});

test('an unconfirmed session reads as unknown with its reason, never as someone present', async ({ page }) => {
  const status = await fixture('owner-veto');
  const basis = 'logind: session view not yet confirmed by a second identical read';
  Object.assign(status.presence, { session: 'unknown', session_authority: 'ambiguous', basis });
  status.owner_veto = { active: true, cause: basis };
  status.session_authority.state = 'ambiguous';
  await open(page, { state: 'online', status, console_version: '1.0.0' });
  const presence = page.locator('#protection .fact').filter({ hasText: 'OWNER PRESENCE' });
  await expect(presence.locator('strong')).toHaveText('unknown');
  await expect(presence).toContainText(basis);
  await expect(presence).not.toContainText('active');
  await expect(page.locator('#decision-title')).toHaveText('Owner policy pauses new work');
  await expect(page.locator('#protection')).toContainText('ambiguous');
});

test('a known session keeps the idle reading as its detail', async ({ page }) => {
  const status = await fixture('owner-veto');
  Object.assign(status.presence, { session: 'active', idle_seconds: 70, basis: 'logind: session 2 active and unlocked; idle lending: no input for 70 s (owner.idle_lends_after_s = 60)' });
  await open(page, { state: 'online', status, console_version: '1.0.0' });
  const presence = page.locator('#protection .fact').filter({ hasText: 'OWNER PRESENCE' });
  await expect(presence.locator('strong')).toHaveText('active');
  await expect(presence).toContainText('Idle for 70s');
});

test('ready GPU memory with unavailable utilization says so instead of a meter', async ({ page }) => {
  const status = await fixture('lending');
  const gpu = status.resources.find(r => r.kind === 'nvidia-gpu');
  gpu.utilization_pct = null;
  await open(page, { state: 'online', status, console_version: '1.0.0' });
  await page.getByRole('link', { name: 'Hardware', exact: true }).click();
  const card = page.locator('#resources article').first();
  await expect(card.locator('.measurement')).not.toHaveText('Unknown');
  await expect(card).toContainText('GPU utilization unavailable');
  await expect(card.locator('progress')).toHaveCount(0);
});

test('cards replace the waiting placeholders, including after an outage', async ({ page }) => {
  const status = await fixture('lending');
  await open(page, { state: 'online', status, console_version: '0.1.0' });
  await expect(page.locator('#upstreams article')).toHaveCount(1);
  await expect(page.locator('#upstreams .empty')).toHaveCount(0);
  await expect(page.locator('#resources .empty')).toHaveCount(0);
  await page.evaluate(() => { window.__TAURI__.core.invoke = async () => ({ state: 'unavailable', reason: 'Service stopped.' }); });
  await expect(page.locator('#upstreams .empty')).toHaveCount(1);
  await page.evaluate(value => { window.__TAURI__.core.invoke = async () => value; }, { state: 'online', status, console_version: '0.1.0' });
  await expect(page.locator('#upstreams article')).toHaveCount(1);
  await expect(page.locator('#upstreams .empty')).toHaveCount(0);
  await expect(page.locator('#resources .empty')).toHaveCount(0);
});

test('outage clears last-good hardware and workload state', async ({ page }) => {
  await open(page, { state: 'online', status: await fixture('lending'), console_version: '0.1.0' });
  await expect(page.locator('#resources .measurement')).toHaveCount(1);
  await page.evaluate(() => { window.__TAURI__.core.invoke = async () => ({ state: 'unavailable', reason: 'Service stopped.' }); });
  await expect(page.locator('#decision-title')).toHaveText('Status unavailable');
  // Nothing is answering, so the sign is dark rather than lit with stale meaning.
  await expect(page.locator('#sign')).toHaveAttribute('src', 'assets/mark-idle.svg');
  await expect(page.locator('#resources .measurement')).toHaveCount(0);
  await expect(page.locator('#upstreams .badge')).toHaveCount(0);
});

test('incompatible schema shows an explicit recovery action', async ({ page }) => {
  await open(page, { state: 'incompatible', schema_version: 999 });
  await expect(page.locator('#decision-detail')).toContainText('Install matching service and console versions');
  await expect(page.locator('#resources .measurement')).toHaveCount(0);
});

test('untrusted labels remain text and never create markup or requests', async ({ page }) => {
  const status = await fixture('lending');
  status.resources[0].label = '<img src="https://example.invalid/track" onerror="window.injected=true">';
  const external = [];
  page.on('request', request => { if (!request.url().startsWith('http://127.0.0.1:38110/')) external.push(request.url()); });
  await open(page, { state: 'online', status, console_version: '0.1.0' });
  await expect(page.locator('#resources h3')).toHaveText(`${status.resources[0].label} (gpu0)`);
  await expect(page.locator('#resources img')).toHaveCount(0);
  expect(external).toEqual([]);
});

test('ready status and owner veto outrank unused unknown hardware', async ({ page }) => {
  const status = await fixture('owner-veto');
  const unused = structuredClone(status.resources[0]);
  Object.assign(unused, {
    id: 'spare-gpu', label: status.resources[0].label, required: false, required_by: [],
    enabled: true, lendable: false,
    capacity: { quality: 'unknown', age_s: null, last_error: 'not sampled because unused' },
    pressure: { ...unused.pressure, quality: 'unknown', last_error: 'not collected' },
    adapter_mapping: { quality: 'unknown', age_s: null, last_error: 'not collected', pci: null, luid: null, source: null },
  });
  status.resources.push(unused);
  await open(page, { state: 'online', status, console_version: '1.1.0' });
  await expect(page.locator('#decision-title')).toHaveText('Owner policy pauses new work');
  await expect(page.locator('#announcement')).toContainText('Owner policy pauses new work');
  await page.getByRole('link', { name: 'Hardware', exact: true }).click();
  const card = page.getByRole('article', { name: 'Example GPU (spare-gpu) resource' });
  await expect(card).toContainText('NOT USED');
  await expect(card).toContainText('No borrowing route uses this resource.');
  await expect(card.locator('.resource-control')).toHaveCount(0);
});

test('duplicate GPU labels expose stable identity, mapping and distinct controls', async ({ page }) => {
  const status = await fixture('lending');
  const first = status.resources[0];
  first.id = 'gpu-a';
  first.required_by = ['inference-a'];
  first.adapter_mapping = { ...first.adapter_mapping, pci: '0000:01:00.0', source: 'nvidia_smi' };
  const second = structuredClone(first);
  second.id = 'gpu-b';
  second.required_by = ['inference-b'];
  second.adapter_mapping = { ...second.adapter_mapping, pci: '0000:02:00.0' };
  status.resources = [first, second];
  await open(page, { state: 'online', status, console_version: '1.1.0' });
  await page.getByRole('link', { name: 'Hardware', exact: true }).click();
  await expect(page.locator('#resources h3')).toHaveText(['Example GPU (gpu-a)', 'Example GPU (gpu-b)']);
  await expect(page.getByRole('article', { name: 'Example GPU (gpu-a) resource' })).toContainText('PCI 0000:01:00.0');
  await expect(page.getByRole('article', { name: 'Example GPU (gpu-b) resource' })).toContainText('PCI 0000:02:00.0');
  await expect(page.getByRole('button', { name: 'Disable sharing: Example GPU (gpu-a)' })).toHaveAttribute('data-resource', 'gpu-a');
  await expect(page.getByRole('button', { name: 'Disable sharing: Example GPU (gpu-b)' })).toHaveAttribute('data-resource', 'gpu-b');
});

test('a learned threshold below its target says it is rising and how much it has seen', async ({ page }) => {
  const status = await fixture('lending');
  Object.assign(status.resources[0].pressure, { threshold_pct: 34.2, target_pct: 40, learned_s: 187_200 });
  await open(page, { state: 'online', status, console_version: '1.1.0' });
  await expect(page.locator('#resources')).toContainText('Threshold 34%, rising toward 40% (learned from 52 h).');
  // Nearly there: whole percentages would read "40%, rising toward 40%".
  Object.assign(status.resources[0].pressure, { threshold_pct: 39.6, target_pct: 40 });
  await open(page, { state: 'online', status, console_version: '1.1.0' });
  await expect(page.locator('#resources')).toContainText('Threshold 39.6%, rising toward 40.0% (learned from 52 h).');
  await expect(page.locator('#resources')).not.toContainText('40%, rising toward 40%');
  // At its target, nothing is rising.
  Object.assign(status.resources[0].pressure, { threshold_pct: 40, target_pct: 40, calibration_hint: 'learned load is above the calibration cap; target held at cap' });
  await open(page, { state: 'online', status, console_version: '1.1.0' });
  await expect(page.locator('#resources')).toContainText('40% threshold');
  await expect(page.locator('#resources')).not.toContainText('rising toward');
  await expect(page.locator('#resources')).toContainText('learned load is above the calibration cap; target held at cap');
});

test('bounded mapping and pressure errors do not replace the daemon verdict', async ({ page }) => {
  const status = await fixture('lending');
  const longError = `collector failed ${'x'.repeat(300)}`;
  status.resources[0].adapter_mapping = { quality: 'unknown', age_s: null, last_error: longError, pci: null, luid: null, source: null };
  status.resources[0].pressure = { ...status.resources[0].pressure, quality: 'unknown', last_error: longError };
  await open(page, { state: 'online', status, console_version: '1.1.0' });
  await expect(page.locator('#decision-title')).toHaveText('Room for borrowed work');
  await expect(page.locator('#resources')).toContainText('Adapter mapping: collector failed');
  await expect(page.locator('#resources')).toContainText('Owner pressure: collector failed');
  await expect(page.locator('#resources')).not.toContainText(longError);
  await expect(page.locator('#resources .hint').first()).toContainText('…');
});

test('listener states describe direct-access boundary without inferring internet exposure', async ({ page }) => {
  const status = await fixture('lending');
  const base = status.upstreams[0];
  status.upstreams = [
    { ...structuredClone(base), id: 'local', listener: { state: 'loopback_only', quality: 'ready', age_s: 1, last_error: null } },
    { ...structuredClone(base), id: 'lan', listener: { state: 'listens_on_network', quality: 'ready', age_s: 1, last_error: null } },
    { ...structuredClone(base), id: 'unverified', listener: { state: 'unknown', quality: 'unknown', age_s: null, last_error: 'listener probe unavailable' } },
    { ...structuredClone(base), id: 'typo', listener: { state: 'network_bound', quality: 'ready', age_s: 1, last_error: null } },
    { ...structuredClone(base), id: 'remote', listener: { state: 'not_observable', quality: 'ready', age_s: 1, last_error: null } },
    { ...structuredClone(base), id: 'stale-remote', listener: { state: 'not_observable', quality: 'stale', age_s: 60, last_error: null } },
  ];
  await open(page, { state: 'online', status, console_version: '1.1.0' });
  const cards = page.locator('#upstreams article');
  await expect(cards.nth(0)).toContainText('Loopback only');
  await expect(cards.nth(1)).toContainText('Listens on network');
  await expect(cards.nth(1)).toContainText('Direct requests to this target bypass owner protection.');
  await expect(cards.nth(1)).not.toContainText(/internet/i);
  await expect(cards.nth(2)).toContainText('Unknown');
  await expect(cards.nth(2)).toContainText('listener probe unavailable');
  await expect(cards.nth(3)).toContainText('Unknown');
  await expect(cards.nth(3)).not.toContainText('Listens on network');
  await expect(cards.nth(4)).toContainText('Not observable here');
  await expect(cards.nth(4)).not.toContainText('Loopback only');
  await expect(cards.nth(5)).toContainText('Unknown');
  await expect(cards.nth(5)).not.toContainText('Not observable here');
});

test('GPU utilization is visibly separate from available VRAM', async ({ page }) => {
  const status = await fixture('lending');
  await open(page, { state: 'online', status, console_version: '1.1.0' });
  await page.getByRole('link', { name: 'Hardware', exact: true }).click();
  const card = page.getByRole('article', { name: 'Example GPU (gpu0) resource' });
  await expect(card).toContainText('Available of 15.6 GiB');
  await expect(card).toContainText('GPU UTILIZATION');
  expect(await card.getByRole('progressbar', { name: 'Example GPU (gpu0) GPU utilization' }).evaluate(meter => meter.value)).toBe(0);
});

test('keyboard, reduced motion and narrow layout retain visible content', async ({ page }) => {
  await page.emulateMedia({ reducedMotion: 'reduce' });
  await page.setViewportSize({ width: 640, height: 800 });
  await open(page, { state: 'online', status: await fixture('lending'), console_version: '0.1.0' });
  await page.keyboard.press('Tab');
  await expect(page.getByRole('link', { name: 'Skip to machine status' })).toBeFocused();
  await page.keyboard.press('Enter');
  await expect(page.locator('#overview')).toBeFocused();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  await page.getByRole('link', { name: 'Installation', exact: true }).click();
  await expect(page.getByRole('heading', { name: 'Installation', exact: true })).toBeVisible();
  // The native window can shrink this far on a scaled Windows desktop.
  await page.setViewportSize({ width: 480, height: 320 });
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  await page.getByRole('link', { name: 'Installation', exact: true }).click();
  await page.locator('footer').scrollIntoViewIfNeeded();
  await expect(page.locator('footer')).toBeInViewport();
});

test('retain a rendered fixture for visual review', async ({ page }) => {
  const status = await fixture('lending');
  for (const [id, kind, label, free, total, utilization] of [['memory', 'system-memory', 'System memory', 24576, 32768, 25], ['cpu', 'system-cpu', 'Processor', null, null, 12]]) {
    status.resources.push({ ...structuredClone(status.resources[0]), id, kind, label, free_mib: free, total_mib: total, utilization_pct: utilization, pressure: null, adapter_mapping: null });
  }
  await open(page, { state: 'online', status, console_version: '0.1.0' });
  await expect(page.locator('#decision-title')).toHaveText('Room for borrowed work');
  await page.evaluate(() => document.fonts.ready);
  await page.screenshot({ path: 'test-results/console-lending.png', fullPage: true });
  await page.emulateMedia({ forcedColors: 'active' });
  await page.screenshot({ path: 'test-results/console-lending-contrast.png', fullPage: true });
});

for (const [quality, http_status, text] of [
  ['ready', 200, 'Target responded · HTTP 200'],
  ['ready', 401, 'Target requires authentication · HTTP 401'],
  ['ready', 503, 'Target check needs attention · HTTP 503'],
  ['stale', 200, 'Target health unavailable; waiting for a fresh check.'],
]) {
  test(`target check ${quality}/${http_status} stays separate from vacancy`, async ({ page }) => {
    const status = await fixture('lending');
    status.upstreams[0].health = { quality, http_status, age_s: 40, last_error: null };
    await open(page, { state: 'online', status, console_version: '0.1.0' });
    await expect(page.locator('#upstreams')).toContainText(text);
    await expect(page.locator('#decision-title')).toHaveText('Room for borrowed work');
    if (quality === 'stale') await expect(page.locator('#upstreams')).not.toContainText('HTTP 200');
  });
}


test('held work explains its lifetime and remains read-only', async ({ page }) => {
  const status = await fixture('upstream-refusal');
  status.work = [{ id: 7, upstream: status.upstreams[0].id, model: '<script>unsafe</script>', resources: [status.upstreams[0].resource], age_s: 92, completion: 'tracked', phase: 'held', tracking_id: 'job-123', model_state: 'unknown' }];
  await open(page, { state: 'online', status, console_version: '0.1.0' });
  await expect(page.locator('#upstreams')).toContainText('WORK 7');
  await expect(page.locator('#upstreams')).toContainText('held for 92s');
  await expect(page.locator('#upstreams')).toContainText('Stop the job upstream');
  await expect(page.locator('#upstreams script')).toHaveCount(0);
  await expect(page.locator('#upstreams button')).toHaveCount(0);
});


test('sidebar selects one view and overview fits a normal desktop', async ({ page }) => {
  await page.setViewportSize({ width: 1180, height: 740 });
  await open(page, { state: 'online', status: await fixture('lending'), console_version: '1.0.0' });
  await expect(page.locator('#overview')).toBeVisible();
  await expect(page.locator('#hardware')).toBeHidden();
  expect(await page.locator('main').evaluate(e => e.scrollHeight <= e.clientHeight)).toBe(true);
  for (const [name, id] of [['Hardware', 'hardware'], ['Workloads', 'workloads'], ['Installation', 'installation'], ['Settings', 'settings'], ['Overview', 'overview']]) {
    await page.getByRole('link', { name, exact: true }).click();
    await expect(page.locator(`#${id}`)).toBeVisible();
    await expect(page.locator('main > section:not([hidden])')).toHaveCount(1);
    await expect(page.getByRole('link', { name, exact: true })).toHaveAttribute('aria-current', 'page');
    await expect(page.locator(`#${id}`)).toBeFocused();
  }
});

function settingsSnapshot() {
  return structuredClone(defaultSettings);
}

async function settingsFixture(page, saveFailure, settings = settingsSnapshot(), restartRequired = true) {
  const status = await fixture('lending');
  await page.addInitScript(({ status, settings, saveFailure, restartRequired }) => {
    window.settingsChanges = [];
    window.__TAURI__ = { core: { invoke: async (command, args) => {
      if (command === 'read_status') return { state: 'online', status, console_version: '1.2.0' };
      if (command === 'read_settings') return { state: 'online', settings };
      if (command === 'save_settings') {
        window.settingsChanges.push(args.request);
        if (saveFailure) return { state: 'failed', reason: saveFailure };
        Object.assign(settings, structuredClone(args.request), { revision: 'b'.repeat(64), application: { capacity_revision: 'b'.repeat(64), restart_required: restartRequired } });
        return { state: 'saved', settings, restart_required: restartRequired };
      }
      throw new Error(`Unexpected command ${command}`);
    } } };
  }, { status, settings, saveFailure, restartRequired });
  await page.goto('/');
  await page.getByRole('link', { name: 'Settings', exact: true }).click();
  await expect(page.locator('#settings-form')).toBeVisible();
}

test('settings show common policy first and disclose advanced controls', async ({ page }) => {
  await page.setViewportSize({ width: 1180, height: 740 });
  await settingsFixture(page);
  await expect(page.getByRole('checkbox', { name: 'Treat an unlocked Windows session as owner presence' })).toBeChecked();
  await expect(page.getByLabel('Idle time before an unlocked session may lend')).toHaveValue('900');
  await expect(page.locator('#settings-resources').getByText('gpu0 (nvidia-gpu)')).toBeHidden();
  await expect(page.getByLabel('Processes that reserve the host')).toBeHidden();
  await page.screenshot({ path: 'test-results/console-settings-layout.png' });
  const layout = await page.locator('main').evaluate(element => ({ scroll: element.scrollHeight, height: element.clientHeight }));
  expect(layout.scroll).toBeLessThanOrEqual(layout.height);
  expect(await page.locator('main').evaluate(element => element.scrollWidth <= element.clientWidth)).toBe(true);
  await page.evaluate(() => document.fonts.ready);
  await page.screenshot({ path: 'test-results/console-settings-common.png' });
  await page.getByText('Advanced owner protection settings', { exact: true }).click();
  await expect(page.getByLabel('Processes that reserve the host')).toHaveValue('game.exe');
  await expect(page.getByRole('checkbox', { name: 'Treat the Windows sign-in screen as owner presence' })).not.toBeChecked();
  await page.screenshot({ path: 'test-results/console-settings-advanced.png' });
  expect(await page.evaluate(() => document.body.scrollHeight <= document.body.clientHeight)).toBe(true);
  expect(await page.locator('body').textContent()).not.toMatch(/[–—]/);
  await page.getByRole('tab', { name: 'Hardware', exact: true }).click();
  await expect(page.locator('#settings-resources').getByText('gpu0 (nvidia-gpu)')).toBeVisible();
  await page.getByRole('tab', { name: 'Workloads', exact: true }).click();
  await expect(page.getByRole('group', { name: 'Owner return by workload' })).toContainText('ollama');
});

test('workload safety permissions are advanced, warned and saved without changing running status', async ({ page }) => {
  await settingsFixture(page);
  await page.getByRole('tab', { name: 'Workloads', exact: true }).click();
  const scope = page.getByLabel('Owner activity protection', { exact: true });
  const management = page.getByLabel('Borrower management permissions', { exact: true });
  await expect(scope).toBeHidden();
  await page.getByText('Advanced permissions for ollama', { exact: true }).click();
  await expect(scope).toHaveValue('host');
  await expect(management).toHaveValue('deny');
  await expect(scope).toHaveAccessibleDescription(/Only GPU pressure on its watched hardware can reclaim it/);
  await expect(management).toHaveAccessibleDescription(/change upstream state and access owner data/);
  await scope.selectOption('resource');
  await management.selectOption('allow');
  await page.getByText('Advanced permissions for ollama', { exact: true }).scrollIntoViewIfNeeded();
  await page.screenshot({ path: 'test-results/console-settings-permissions.png' });
  await page.getByRole('button', { name: 'Save settings' }).click();
  await expect(page.locator('#settings-state')).toContainText('Restart the vacancyd service');
  const changes = await page.evaluate(() => window.settingsChanges);
  expect(changes[0].upstreams[0]).toMatchObject({ owner_scope: 'resource', management: 'allow' });
  await expect(page.locator('.upstream-exempt')).toBeHidden();
  await page.getByText('Advanced permissions for ollama', { exact: true }).click();
  await expect(scope).toHaveValue('resource');
  await expect(management).toHaveValue('allow');
});

test('search reveals nested advanced permissions and preserves them after declined elevation', async ({ page }) => {
  const declined = 'Administrator approval was declined. Nothing was changed.';
  await settingsFixture(page, declined);
  await page.getByRole('searchbox', { name: 'Search settings' }).fill('owner_scope');
  const result = page.locator('#settings-search-results button').filter({ hasText: /^Owner activity protection/ });
  await expect(result).toHaveCount(1);
  await expect(result).toContainText('Advanced');
  await result.click();
  const scope = page.getByLabel('Owner activity protection', { exact: true });
  await expect(scope).toBeFocused();
  await scope.selectOption('resource');
  await page.getByRole('button', { name: 'Save settings' }).click();
  await expect(page.locator('#settings-state')).toHaveText(declined);
  await expect(scope).toHaveValue('resource');
  await expect(page.locator('.upstream-exempt')).toBeHidden();
});

test('similar workload identifiers keep distinct labels and permission warnings', async ({ page }) => {
  const settings = settingsSnapshot();
  settings.upstreams = ['model.a', 'model-a', 'model_a'].map(id => ({ ...settings.upstreams[0], id }));
  await settingsFixture(page, undefined, settings);
  await page.getByRole('tab', { name: 'Workloads', exact: true }).click();
  for (const id of ['model.a', 'model-a', 'model_a']) {
    await page.getByText(`Advanced permissions for ${id}`, { exact: true }).click();
    const group = page.getByRole('group', { name: `${id} (ollama)`, exact: true });
    await expect(group.getByLabel('Owner activity protection', { exact: true })).toHaveCount(1);
    await group.getByLabel('Owner activity protection', { exact: true }).selectOption(id === 'model-a' ? 'resource' : 'host');
  }
  const ids = await page.locator('#settings-upstreams [id]').evaluateAll(nodes => nodes.map(node => node.id));
  expect(new Set(ids).size).toBe(ids.length);
  await page.getByRole('button', { name: 'Save settings' }).click();
  await expect(page.locator('#settings-state')).toContainText('Settings saved.');
  const changes = await page.evaluate(() => window.settingsChanges);
  expect(changes[0].upstreams.map(upstream => upstream.owner_scope)).toEqual(['host', 'resource', 'host']);
});

test('settings tabs support keyboard navigation without losing edits', async ({ page }) => {
  await settingsFixture(page);
  await page.getByLabel('Owner return hold time').fill('65');
  const owner = page.getByRole('tab', { name: 'Owner protection', exact: true });
  await owner.focus();
  await page.keyboard.press('ArrowRight');
  await expect(page.getByRole('tab', { name: 'Hardware', exact: true })).toBeFocused();
  await expect(page.getByRole('tabpanel', { name: 'Hardware', exact: true })).toBeVisible();
  await page.keyboard.press('End');
  await expect(page.getByRole('tab', { name: 'Service', exact: true })).toBeFocused();
  await page.keyboard.press('Home');
  await expect(owner).toBeFocused();
  await expect(page.getByLabel('Owner return hold time')).toHaveValue('65');
  await expect(page.locator('[role="tab"][tabindex="0"]')).toHaveCount(1);
});

test('search reveals advanced settings and never submits a save', async ({ page }) => {
  await settingsFixture(page);
  const search = page.getByRole('searchbox', { name: 'Search settings' });
  await search.fill('processes');
  await expect(page.locator('#settings-search-status')).toContainText('matching setting');
  await search.press('Enter');
  const result = page.locator('#settings-search-results button').filter({ hasText: 'Processes that reserve the host' });
  await expect(result).toBeFocused();
  await expect(result).toContainText('Advanced');
  await result.press('Enter');
  await expect(page.getByLabel('Processes that reserve the host')).toBeFocused();
  await page.getByLabel('Processes that reserve the host').fill('blender');
  await search.fill('ollama drain');
  await page.locator('#settings-search-results button').click();
  await expect(page.getByLabel('Drain time in seconds')).toBeFocused();
  await search.fill('<img src=x> no such setting');
  await expect(page.locator('#settings-search-status')).toContainText('No matching settings');
  await expect(page.locator('#settings-search-results img')).toHaveCount(0);
  await search.press('Escape');
  await expect(search).toHaveValue('');
  await expect(page.locator('#settings-search-results')).toBeHidden();
  await page.getByRole('tab', { name: 'Owner protection', exact: true }).click();
  await expect(page.getByLabel('Processes that reserve the host')).toHaveValue('blender');
  expect(await page.evaluate(() => window.settingsChanges)).toHaveLength(0);
});

test('search alone does not mark settings dirty', async ({ page }) => {
  await settingsFixture(page);
  await page.getByRole('searchbox', { name: 'Search settings' }).fill('idle');
  page.on('dialog', () => { throw new Error('Search must not cause a discard confirmation'); });
  await page.getByRole('button', { name: 'Discard changes and reload' }).click();
  await expect(page.locator('#settings-state')).toContainText('Settings loaded.');
});

test('search explains conditional settings without enabling them', async ({ page }) => {
  await settingsFixture(page);
  await page.getByRole('searchbox', { name: 'Search settings' }).fill('fixed threshold');
  await page.locator('#settings-search-results button').filter({ hasText: /^Fixed threshold/ }).click();
  await expect(page.getByRole('group', { name: 'GPU pressure', exact: true })).toBeFocused();
  await expect(page.getByLabel('Fixed threshold')).toBeDisabled();
  await expect(page.locator('#settings-search-status')).toContainText('currently unavailable');
  expect(await page.evaluate(() => window.settingsChanges)).toHaveLength(0);
});

test('invalid fields are revealed before browser validation focuses them', async ({ page }) => {
  await settingsFixture(page);
  await page.getByLabel('Owner return hold time').fill('-1');
  await page.getByRole('tab', { name: 'GPU pressure', exact: true }).click();
  await page.getByLabel('Sustained load before protection engages').fill('-1');
  await page.getByRole('tab', { name: 'Hardware', exact: true }).click();
  await page.getByRole('button', { name: 'Save settings' }).click();
  await expect(page.getByRole('tabpanel', { name: 'Owner protection', exact: true })).toBeVisible();
  await expect(page.getByLabel('Owner return hold time')).toBeFocused();
  expect(await page.evaluate(() => window.settingsChanges)).toHaveLength(0);
});

test('settings tabs and search reflow in a narrow window', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await settingsFixture(page);
  await page.getByRole('searchbox', { name: 'Search settings' }).fill('pressure');
  await expect(page.locator('#settings-search-results')).toBeVisible();
  for (const name of ['Owner protection', 'Hardware', 'GPU pressure', 'Workloads']) {
    await page.getByRole('tab', { name, exact: true }).click();
    expect(await page.locator('main').evaluate(element => element.scrollWidth <= element.clientWidth)).toBe(true);
  }
  await page.screenshot({ path: 'test-results/console-settings-narrow.png' });
});

test('settings save a complete whitelist and explain restart requirement', async ({ page }) => {
  await settingsFixture(page);
  await page.getByLabel('Idle time before an unlocked session may lend').fill('600');
  await page.getByRole('tab', { name: 'GPU pressure', exact: true }).click();
  await page.getByLabel('Threshold mode').selectOption('fixed');
  await page.getByLabel('Fixed threshold').fill('48');
  await page.getByRole('tab', { name: 'Owner protection', exact: true }).click();
  await page.getByText('Advanced owner protection settings', { exact: true }).click();
  await page.getByLabel('Processes that reserve the host').fill('game.exe\nblender.exe');
  await page.getByRole('checkbox', { name: 'Treat the Windows sign-in screen as owner presence' }).check();
  await page.getByRole('button', { name: 'Save settings' }).click();
  await expect(page.locator('#settings-state')).toHaveText('Settings saved. Capacity budgets are active for new admissions. Restart the vacancyd service to apply the remaining policy.');
  await expect(page.getByRole('button', { name: 'Save settings' })).toBeFocused();
  const changes = await page.evaluate(() => window.settingsChanges);
  expect(changes).toHaveLength(1);
  expect(changes[0].revision).toBe('a'.repeat(64));
  expect(changes[0].owner.idle_lends_after_s).toBe(600);
  expect(changes[0].owner.gpu_pressure).toMatchObject({ mode: 'fixed', threshold_pct: 48 });
  expect(changes[0].owner.processes).toEqual(['game.exe', 'blender.exe']);
  expect(changes[0].owner.sign_in_screen_is_presence).toBe(true);
  expect(changes[0].resources).toEqual(settingsSnapshot().resources);
  expect(changes[0].upstreams).toEqual(settingsSnapshot().upstreams);
});

test('failed settings save retains the edited form and reports the service result', async ({ page }) => {
  await settingsFixture(page, 'Settings changed elsewhere. Read them again before saving.');
  await page.getByLabel('Owner return hold time').fill('60');
  await page.getByRole('button', { name: 'Save settings' }).click();
  await expect(page.locator('#settings-state')).toHaveText('Settings changed elsewhere. Read them again before saving.');
  await expect(page.getByLabel('Owner return hold time')).toHaveValue('60');
  await expect(page.getByRole('button', { name: 'Save settings' })).toBeEnabled();
  // The edited field is unsaved, so discarding it asks first.
  page.once('dialog', dialog => dialog.accept());
  await page.getByRole('button', { name: 'Discard changes and reload' }).click();
  await expect(page.locator('#settings-state')).toHaveText('Settings loaded. Changes are saved only when you choose Save settings.');
  await expect(page.getByLabel('Owner return hold time')).toHaveValue('120');
});

test('declined administrator approval keeps the edited settings and says nothing was saved', async ({ page }) => {
  const declined = 'Administrator approval was declined. Nothing was changed.';
  await settingsFixture(page, declined);
  await expect(page.locator('#settings-save-hint')).toContainText('administrator approval');
  await page.getByLabel('Owner return hold time').fill('60');
  await page.getByRole('button', { name: 'Save settings' }).click();
  await expect(page.locator('#settings-state')).toHaveText(declined);
  await expect(page.getByLabel('Owner return hold time')).toHaveValue('60');
  await expect(page.getByRole('button', { name: 'Save settings' })).toBeEnabled();
  // Saving again resubmits the same edited document.
  await page.getByRole('button', { name: 'Save settings' }).click();
  await expect(page.locator('#settings-state')).toHaveText(declined);
  const changes = await page.evaluate(() => window.settingsChanges);
  expect(changes).toHaveLength(2);
  expect(changes[1].owner.release_after_s).toBe(60);
});

test('reload with no unsaved edits does not ask for confirmation', async ({ page }) => {
  await settingsFixture(page);
  let asked = false;
  page.on('dialog', () => { asked = true; });
  await page.getByRole('button', { name: 'Discard changes and reload' }).click();
  await expect(page.locator('#settings-state')).toHaveText('Settings loaded. Changes are saved only when you choose Save settings.');
  expect(asked).toBe(false);
});

test('a failed settings read shows a retry outside the hidden form', async ({ page }) => {
  const status = await fixture('lending');
  await page.addInitScript(({ status, settings }) => {
    let attempt = 0;
    window.__TAURI__ = { core: { invoke: async (command) => {
      if (command === 'read_status') return { state: 'online', status, console_version: '1.2.0' };
      if (command === 'read_settings') {
        attempt += 1;
        if (attempt === 1) return { state: 'unavailable', reason: 'The local service could not read the saved settings. Check the service log.' };
        return { state: 'online', settings };
      }
      throw new Error(`Unexpected command ${command}`);
    } } };
  }, { status, settings: settingsSnapshot() });
  await page.goto('/');
  await page.getByRole('link', { name: 'Settings', exact: true }).click();
  await expect(page.locator('#settings-state')).toHaveText('The local service could not read the saved settings. Check the service log.');
  await expect(page.locator('#settings-form')).toBeHidden();
  const retry = page.getByRole('button', { name: 'Retry' });
  await expect(retry).toBeVisible();
  await retry.click();
  await expect(page.locator('#settings-form')).toBeVisible();
  await expect(retry).toBeHidden();
});

async function controlsFixture(page, fail = false) {
  const status = await fixture('lending');
  await page.addInitScript(({ status, fail }) => {
    window.changes = [];
    window.__TAURI__ = { core: { invoke: async (command, args) => {
      if (command === 'read_status') return { state: 'online', status, console_version: '1.0.0' };
      window.changes.push(args.request);
      if (fail) return { state: 'failed', reason: 'Local controls are denied by the service.' };
      if (args.request.resource === null) status.lending_enabled = args.request.enabled;
      else status.resources.find(r => r.id === args.request.resource).enabled = args.request.enabled;
      return { state: 'applied', enabled: args.request.enabled };
    } } };
  }, { status, fail });
  await page.goto('/');
}

test('pause requires confirmation, cancel changes nothing, resume reflects service', async ({ page }) => {
  await controlsFixture(page);
  await page.getByRole('button', { name: 'Pause lending', exact: true }).click();
  await expect(page.getByRole('dialog')).toContainText('does not guarantee immediate cancellation');
  await page.getByRole('button', { name: 'Cancel', exact: true }).click();
  expect(await page.evaluate(() => window.changes)).toEqual([]);
  await expect(page.getByRole('button', { name: 'Pause lending', exact: true })).toBeFocused();
  await page.getByRole('button', { name: 'Pause lending', exact: true }).click();
  await page.getByRole('button', { name: 'Confirm', exact: true }).click();
  await expect(page.getByRole('button', { name: 'Resume lending', exact: true })).toBeEnabled();
  expect(await page.evaluate(() => window.changes)).toEqual([{ enabled: false, resource: null }]);
  await page.getByRole('button', { name: 'Resume lending', exact: true }).click();
  await expect(page.getByRole('dialog')).toContainText('does not force lending');
  // Resuming someone else's pause is the case that needs an administrator.
  await expect(page.getByRole('dialog')).toContainText('If another account paused lending');
  await page.keyboard.press('Escape');
  expect(await page.evaluate(() => window.changes)).toHaveLength(1);
});

test('denied control shows error without optimistic success', async ({ page }) => {
  await controlsFixture(page, true);
  await page.getByRole('button', { name: 'Pause lending', exact: true }).click();
  await page.getByRole('button', { name: 'Confirm', exact: true }).click();
  await expect(page.locator('#control-result')).toContainText('denied');
  await expect(page.getByRole('button', { name: 'Pause lending', exact: true })).toBeEnabled();
});

test('resource control targets only selected resource and polling preserves focus', async ({ page }) => {
  await controlsFixture(page);
  await page.getByRole('link', { name: 'Hardware', exact: true }).click();
  const button = page.getByRole('button', { name: /Disable sharing:/ });
  await button.focus();
  await expect.poll(async () => page.evaluate(() => document.activeElement?.getAttribute('data-resource'))).not.toBeNull();
  await button.click();
  await expect(page.getByRole('dialog')).toContainText('Changing a resource needs administrator approval');
  await page.getByRole('button', { name: 'Confirm', exact: true }).click();
  await expect(page.getByRole('button', { name: /Enable sharing:/ })).toBeEnabled();
  const changes = await page.evaluate(() => window.changes);
  expect(changes).toHaveLength(1);
  expect(changes[0].resource).not.toBeNull();
  expect(changes[0].enabled).toBe(false);
  await expect(page.locator('#hardware')).toBeVisible();
});

test('unavailable state disables owner controls', async ({ page }) => {
  await open(page, { state: 'unavailable', reason: 'Service stopped.' });
  await expect(page.getByRole('button', { name: 'Lending state unavailable' })).toBeDisabled();
  // A control that cannot act is not drawn in the lit-sign amber.
  await expect(page.locator('#lending-control')).toHaveCSS('background-color', 'rgb(21, 27, 39)');
  await expect(page.locator('#sign')).toHaveAttribute('src', 'assets/mark-idle.svg');
});

test('four-resource hardware view fits desktop and remains reachable when small', async ({ page }) => {
  const status = await fixture('lending');
  const gpu = status.resources[0];
  status.resources = ['RTX 5080', 'RTX 3050', 'System memory', '32 logical CPUs'].map((label, index) => ({ ...structuredClone(gpu), id: `resource-${index}`, label, kind: index < 2 ? 'nvidia-gpu' : index === 2 ? 'system-memory' : 'system-cpu', pressure: index < 2 ? structuredClone(gpu.pressure) : null, utilization_pct: index === 3 ? 12 : 5 }));
  await page.setViewportSize({ width: 1180, height: 740 });
  await open(page, { state: 'online', status, console_version: '1.0.0' });
  await page.getByRole('link', { name: 'Hardware', exact: true }).click();
  await expect(page.locator('.resource-control')).toHaveCount(4);
  expect(await page.locator('main').evaluate(e => e.scrollHeight <= e.clientHeight)).toBe(true);
  await page.evaluate(() => document.fonts.ready);
  await page.screenshot({ path: 'test-results/console-hardware.png' });
  await page.setViewportSize({ width: 480, height: 320 });
  const last = page.locator('.resource-control').last();
  await last.scrollIntoViewIfNeeded();
  await expect(last).toBeInViewport();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
});

test('a pending mutation cannot be submitted twice', async ({ page }) => {
  await controlsFixture(page);
  await page.evaluate(() => {
    const invoke = window.__TAURI__.core.invoke;
    window.__TAURI__.core.invoke = (command, args) => command === 'set_lending'
      ? new Promise(resolve => { window.finishChange = () => { void invoke(command, args).then(resolve); }; })
      : invoke(command, args);
  });
  await page.getByRole('button', { name: 'Pause lending', exact: true }).click();
  await page.getByRole('button', { name: 'Confirm', exact: true }).click();
  await expect(page.getByRole('button', { name: 'Applying change…' })).toBeDisabled();
  await page.evaluate(() => window.finishChange());
  await expect(page.getByRole('button', { name: 'Resume lending', exact: true })).toBeEnabled();
  expect(await page.evaluate(() => window.changes)).toHaveLength(1);
});

test('per-upstream reclaim controls have distinct accessible names', async ({ page }) => {
  await settingsFixture(page);
  await page.getByRole('tab', { name: 'Workloads', exact: true }).click();
  await expect(page.getByLabel('Existing work on owner return')).toHaveValue('drain');
  await expect(page.getByLabel('Drain time in seconds')).toHaveValue('30');
  await expect(page.getByRole('group', { name: 'ollama (ollama)' })).toBeVisible();
});

test('owner return settings of an exempt route say they apply only to pressure reclaim', async ({ page }) => {
  const status = await fixture('lending');
  status.upstreams[0].id = 'ollama';
  status.upstreams[0].owner_scope = 'resource';
  status.owner_veto.exempt_upstreams = ['ollama'];
  const settings = settingsSnapshot();
  settings.upstreams.push({ ...structuredClone(settings.upstreams[0]), id: 'comfyui', kind: 'comfyui', yield_on_veto: true, on_owner_return: 'cancel', drain_timeout_s: 30, owner_scope: 'host', management: 'deny' });
  await page.addInitScript(({ status, settings }) => {
    window.__TAURI__ = { core: { invoke: async command => {
      if (command === 'read_status') return { state: 'online', status, console_version: '1.2.0' };
      if (command === 'read_settings') return { state: 'online', settings };
      throw new Error(`Unexpected command ${command}`);
    } } };
  }, { status, settings });
  await page.goto('/');
  await page.getByRole('link', { name: 'Settings', exact: true }).click();
  await page.getByRole('tab', { name: 'Workloads', exact: true }).click();
  const exempt = page.getByRole('group', { name: 'ollama (ollama)' });
  await expect(exempt.locator('.upstream-exempt')).toBeVisible();
  await expect(exempt).toContainText('Owner presence does not pause this route (owner_scope = "resource").');
  await expect(page.getByRole('group', { name: 'comfyui (comfyui)' }).locator('.upstream-exempt')).toBeHidden();
});

test('switching pressure mode away from Fixed and back keeps the typed threshold', async ({ page }) => {
  await settingsFixture(page);
  await page.getByRole('tab', { name: 'GPU pressure', exact: true }).click();
  await page.getByLabel('Threshold mode').selectOption('fixed');
  await page.getByLabel('Fixed threshold').fill('48');
  await page.getByLabel('Threshold mode').selectOption('calibrated');
  await page.getByLabel('Threshold mode').selectOption('fixed');
  await expect(page.getByLabel('Fixed threshold')).toHaveValue('48');
  await page.getByRole('button', { name: 'Save settings' }).click();
  await expect(page.locator('#settings-state')).toHaveText('Settings saved. Capacity budgets are active for new admissions. Restart the vacancyd service to apply the remaining policy.');
  const changes = await page.evaluate(() => window.settingsChanges);
  expect(changes.at(-1).owner.gpu_pressure).toMatchObject({ mode: 'fixed', threshold_pct: 48 });
});

test('a saved settings change shows a pending restart banner until the service catches up', async ({ page }) => {
  const status = await fixture('lending');
  status.config_revision = 'a'.repeat(64);
  const settings = settingsSnapshot();
  settings.application.restart_required = false;
  await page.addInitScript(({ status, settings }) => {
    window.restartStatus = status;
    window.__TAURI__ = { core: { invoke: async (command, args) => {
      if (command === 'read_status') return { state: 'online', status, console_version: '1.2.0' };
      if (command === 'read_settings') return { state: 'online', settings };
      if (command === 'save_settings') {
        Object.assign(settings, structuredClone(args.request), { revision: 'b'.repeat(64) });
        settings.application.restart_required = true;
        return { state: 'saved', settings, restart_required: true };
      }
      throw new Error(`Unexpected command ${command}`);
    } } };
  }, { status, settings });
  await page.goto('/');
  await page.getByRole('link', { name: 'Settings', exact: true }).click();
  await expect(page.locator('#settings-form')).toBeVisible();
  await expect(page.locator('#settings-restart-pending')).toBeHidden();
  await page.getByLabel('Owner return hold time').fill('60');
  await page.getByRole('button', { name: 'Save settings' }).click();
  await expect(page.locator('#settings-state')).toHaveText('Settings saved. Capacity budgets are active for new admissions. Restart the vacancyd service to apply the remaining policy.');
  await expect(page.locator('#settings-restart-pending')).toBeVisible();
  await expect(page.locator('#settings-restart-pending')).toContainText('waiting for a service restart');
  const hold = page.getByLabel('Owner return hold time');
  await hold.fill('75');
  await page.evaluate(() => { window.restartStatus.config_revision = 'b'.repeat(64); });
  await expect(page.locator('#settings-restart-pending')).toBeHidden();
  await expect(page.locator('#settings-state')).toHaveText('Saved settings are active. Changes are saved only when you choose Save settings.');
  await expect(hold).toHaveValue('75');
  await expect(hold).toBeFocused();
});

test('withheld detail summarizes when routes disagree on the reason', async ({ page }) => {
  const status = await fixture('upstream-refusal');
  const second = structuredClone(status.upstreams[0]);
  second.id = 'second-route';
  second.refusal = { reason: 'owner_active', detail: 'lending is paused by owner policy (session unlocked)', retry_after_s: 30 };
  status.upstreams.push(second);
  await open(page, { state: 'online', status, console_version: '0.1.0' });
  await expect(page.locator('#decision-title')).toHaveText('Holding spare capacity');
  await expect(page.locator('#decision-detail')).toContainText('2 routes are withheld for different reasons');
  await expect(page.locator('#decision-detail')).toContainText('See Workloads');
});

test('withheld detail names the shared reason when every route agrees', async ({ page }) => {
  const status = await fixture('upstream-refusal');
  const second = structuredClone(status.upstreams[0]);
  second.id = 'second-route';
  status.upstreams.push(second);
  await open(page, { state: 'online', status, console_version: '0.1.0' });
  await expect(page.locator('#decision-detail')).toHaveText('model requires more free memory');
});

test('a resource control keeps its DOM node across a poll that changes nothing', async ({ page }) => {
  await open(page, { state: 'online', status: await fixture('lending'), console_version: '0.1.0' });
  await page.getByRole('link', { name: 'Hardware', exact: true }).click();
  const button = page.getByRole('button', { name: /Disable sharing:/ });
  await button.evaluate(el => { el.dataset.identityProbe = 'kept'; });
  // main.ts polls every 2s; wait for at least one more cycle with unchanged
  // data and confirm the same DOM node (carrying the probe attribute) is
  // still there, rather than a lookalike replacement.
  await page.waitForTimeout(2200);
  await expect(button).toHaveAttribute('data-identity-probe', 'kept');
});

test('the live region announces a title or tone change but not a changing detail', async ({ page }) => {
  const status = await fixture('upstream-refusal');
  await page.addInitScript(({ status }) => {
    let poll = 0;
    window.__TAURI__ = { core: { invoke: async () => {
      poll += 1;
      const clone = structuredClone(status);
      clone.upstreams[0].refusal.detail = `attempt ${poll}`;
      return { state: 'online', status: clone, console_version: '0.1.0' };
    } } };
  }, { status });
  await page.goto('/');
  await expect(page.locator('#decision-detail')).toHaveText('attempt 1');
  await expect(page.locator('#announcement')).toContainText('attempt 1');
  await page.waitForTimeout(2200);
  await expect(page.locator('#decision-detail')).toHaveText('attempt 2');
  await expect(page.locator('#announcement')).toContainText('attempt 1');
  await expect(page.locator('#announcement')).not.toContainText('attempt 2');
});

test('no visible text renders below a 12px floor', async ({ page }) => {
  await open(page, { state: 'online', status: await fixture('lending'), console_version: '0.1.0' });
  await page.getByRole('link', { name: 'Hardware', exact: true }).click();
  const sizes = await page.evaluate(() => [...document.querySelectorAll('body *')]
    .filter(el => el.children.length === 0 && el.textContent?.trim())
    .map(el => parseFloat(getComputedStyle(el).fontSize)));
  expect(sizes.length).toBeGreaterThan(0);
  expect(Math.min(...sizes)).toBeGreaterThanOrEqual(12);
});


test('capacity budgets preserve wide decimal values across search, tabs and save', async ({ page }) => {
  await settingsFixture(page, undefined, settingsSnapshot(), false);
  await page.getByRole('tab', { name: 'Workloads', exact: true }).click();
  const budget = page.getByLabel('Default free capacity (MiB)', { exact: true });
  await budget.fill('9007199254740993');
  await page.getByLabel('Search settings', { exact: true }).fill('default_min_free_mib ollama');
  await page.locator('#settings-search-results button').click();
  await expect(budget).toBeFocused();
  await page.getByLabel('Search settings', { exact: true }).fill('');
  await page.getByRole('tab', { name: 'Service', exact: true }).click();
  await expect(page.locator('#settings-capabilities')).toContainText('0 access token(s) configured');
  await page.getByRole('tab', { name: 'Workloads', exact: true }).click();
  await expect(budget).toHaveValue('9007199254740993');
  await page.getByRole('button', { name: 'Save settings' }).click();
  const request = await page.evaluate(() => window.settingsChanges[0]);
  expect(request.upstreams[0].default_min_free_mib).toBe('9007199254740993');
  expect(request).not.toHaveProperty('capabilities');
  expect(request).not.toHaveProperty('application');
  await expect(page.locator('#settings-restart-pending')).toBeHidden();
  await expect(page.locator('#settings-state')).toContainText('Capacity budgets are active');
});

test('model mode changes retain drafts and submit only the selected requirement shape', async ({ page }) => {
  await settingsFixture(page);
  await page.getByRole('tab', { name: 'Workloads', exact: true }).click();
  const model = page.locator('.model-setting');
  await model.getByLabel('Model load requirement (MiB)', { exact: true }).fill('14000');
  await model.getByText('Advanced model requirements', { exact: true }).click();
  const mode = model.getByLabel('Model requirement mode', { exact: true });
  await mode.selectOption('per_resource');
  await model.getByText('Per-resource model budgets', { exact: true }).click();
  await model.getByRole('button', { name: 'Add resource budget', exact: true }).click();
  await model.getByLabel('Resource load requirement (MiB)', { exact: true }).fill('15000');
  await mode.selectOption('scalar');
  await expect(model.getByLabel('Model load requirement (MiB)', { exact: true })).toHaveValue('14000');
  await mode.selectOption('per_resource');
  await expect(model.getByLabel('Resource load requirement (MiB)', { exact: true })).toHaveValue('15000');
  await page.getByRole('button', { name: 'Save settings' }).click();
  expect(await page.evaluate(() => window.settingsChanges[0].upstreams[0].models[0])).toMatchObject({
    mode: 'per_resource', min_free_mib: '0', resources: [{ id: 'gpu0', min_free_mib: '15000' }],
  });
  await page.getByRole('button', { name: 'Add model requirement', exact: true }).click();
  await page.locator('.model-setting').last().getByLabel('Model name', { exact: true }).fill('other:latest');
  await page.locator('.model-setting').first().getByRole('button', { name: 'Remove model requirement', exact: true }).click();
  await page.getByRole('button', { name: 'Save settings' }).click();
  expect(await page.evaluate(() => window.settingsChanges[1].upstreams[0].models.map(model => model.name))).toEqual(['other:latest']);
});

test('calibration timing and service limits save with their native units and inherited values', async ({ page }) => {
  await settingsFixture(page);
  const search = page.getByLabel('Search settings', { exact: true });
  await search.fill('media_poll_ms');
  await page.locator('#settings-search-results button').click();
  await page.getByLabel('Media polling interval (ms)', { exact: true }).fill('7000');
  await search.fill('learn_quantile');
  await page.locator('#settings-search-results button').click();
  await page.getByLabel('Learning quantile (between 0 and 1)', { exact: true }).fill('0.9');
  await page.getByRole('tab', { name: 'GPU pressure', exact: true }).click();
  await page.getByLabel('Threshold mode', { exact: true }).selectOption('fixed');
  await page.getByLabel('Fixed threshold', { exact: true }).fill('60');
  await page.getByLabel('Threshold mode', { exact: true }).selectOption('calibrated');
  await expect(page.getByLabel('Learning quantile (between 0 and 1)', { exact: true })).toHaveValue('0.9');
  await search.fill('max_buffered_body_mib');
  await page.locator('#settings-search-results button').click();
  await expect(page.getByLabel('Buffered request budget (MiB, blank inherits)', { exact: true })).toBeFocused();
  await page.getByLabel('Buffered request budget (MiB, blank inherits)', { exact: true }).fill('512');
  await page.getByLabel('Header and keep-alive timeout (seconds)', { exact: true }).fill('50');
  await page.getByRole('button', { name: 'Save settings' }).click();
  const request = await page.evaluate(() => window.settingsChanges[0]);
  expect(request.owner.media_poll_ms).toBe(7000);
  expect(request.owner.gpu_pressure.learn_quantile).toBe(0.9);
  expect(request.owner.gpu_pressure).not.toHaveProperty('baseline_quantile');
  expect(request.server_limits).toMatchObject({ max_buffered_body_mib: 512, header_read_timeout_s: 50 });
});

test('model budget validation rejects overflow, resident overcommit and duplicate names before native save', async ({ page }) => {
  await settingsFixture(page);
  await page.getByRole('tab', { name: 'Workloads', exact: true }).click();
  const load = page.getByLabel('Model load requirement (MiB)', { exact: true });
  await load.fill('9223372036854775808');
  await page.getByRole('button', { name: 'Save settings' }).click();
  await expect(page.locator('#settings-state')).toContainText('9223372036854775807');
  await load.fill('13000');
  await page.getByLabel('Resident requirement (MiB)', { exact: true }).fill('13001');
  await page.getByRole('tab', { name: 'Service', exact: true }).click();
  await page.getByRole('button', { name: 'Save settings' }).click();
  await expect(page.locator('#settings-state')).toContainText('Resident capacity must not exceed');
  await expect(page.getByLabel('Resident requirement (MiB)', { exact: true })).toBeFocused();
  await page.getByLabel('Resident requirement (MiB)', { exact: true }).fill('0');
  await page.getByRole('button', { name: 'Add model requirement', exact: true }).click();
  await page.locator('.model-setting').last().getByLabel('Model name', { exact: true }).fill('gemma3:12b');
  await page.getByRole('button', { name: 'Save settings' }).click();
  await expect(page.locator('#settings-state')).toContainText('Model names must be unique');
  await expect(page.locator('.model-setting').last().getByLabel('Model name', { exact: true })).toBeFocused();
  expect(await page.evaluate(() => window.settingsChanges)).toEqual([]);
});

test('memory budget errors reveal their tab and related edits clear stale validation', async ({ page }) => {
  const settings = settingsSnapshot();
  settings.resources.push({ id: 'cpu', kind: 'system-cpu', enabled: true, reserve: '75', selector: null, adapter: null });
  await settingsFixture(page, undefined, settings);
  await page.getByRole('tab', { name: 'Workloads', exact: true }).click();
  await page.getByText('Advanced model requirements', { exact: true }).click();
  await page.getByLabel('Model requirement mode', { exact: true }).selectOption('per_resource');
  await page.getByText('Per-resource model budgets', { exact: true }).click();
  await page.getByRole('button', { name: 'Add resource budget', exact: true }).click();
  const resource = page.getByLabel('Budgeted resource', { exact: true });
  const load = page.getByLabel('Resource load requirement (MiB)', { exact: true });
  await resource.selectOption('cpu');
  await load.fill('13000');
  await page.getByRole('tab', { name: 'Service', exact: true }).click();
  await page.getByRole('button', { name: 'Save settings' }).click();
  await expect(page.locator('#settings-state')).toContainText('MiB budgets require a memory or GPU resource');
  await expect(resource).toBeFocused();
  expect(await page.evaluate(() => window.settingsChanges)).toEqual([]);
  await resource.selectOption('gpu0');
  const resident = page.getByLabel('Resident requirement (MiB)', { exact: true });
  await resident.fill('13001');
  await page.getByRole('button', { name: 'Save settings' }).click();
  await expect(resident).toBeFocused();
  await load.fill('13002');
  await page.getByRole('button', { name: 'Save settings' }).click();
  await expect(page.locator('#settings-state')).toContainText('Settings saved');
  const changes = await page.evaluate(() => window.settingsChanges);
  expect(changes).toHaveLength(1);
  expect(changes[0].upstreams[0].models[0]).toMatchObject({ mode: 'per_resource', resident_min_free_mib: '13001', resources: [{ id: 'gpu0', min_free_mib: '13002' }] });
});

test('percentage limits reject invalid units and keep the pressure floor below the cap', async ({ page }) => {
  const settings = settingsSnapshot();
  settings.resources.push({ id: 'cpu', kind: 'system-cpu', enabled: true, reserve: '75', selector: null, adapter: null });
  await settingsFixture(page, undefined, settings);
  await page.getByRole('tab', { name: 'Hardware', exact: true }).click();
  const ceiling = page.getByLabel('CPU utilization ceiling (%, blank uses 100)', { exact: true });
  await ceiling.fill('101');
  await page.getByRole('tab', { name: 'Service', exact: true }).click();
  await page.getByRole('button', { name: 'Save settings' }).click();
  await expect(page.locator('#settings-state')).toContainText('CPU utilization ceiling must be between 0 and 100 percent');
  await expect(ceiling).toBeFocused();
  expect(await page.evaluate(() => window.settingsChanges)).toEqual([]);
  await ceiling.fill('95');
  await page.getByRole('tab', { name: 'GPU pressure', exact: true }).click();
  await page.getByText('Advanced gpu pressure settings', { exact: true }).click();
  const cap = page.getByLabel('Pressure cap (%)', { exact: true });
  await cap.fill('0');
  const floor = page.getByLabel('Pressure floor (%)', { exact: true });
  await floor.fill('0');
  await page.getByRole('tab', { name: 'Service', exact: true }).click();
  await page.getByRole('button', { name: 'Save settings' }).click();
  await expect(page.locator('#settings-state')).toContainText('Pressure floor must be greater than 0');
  await expect(floor).toBeFocused();
  await floor.fill('25');
  await page.getByRole('button', { name: 'Save settings' }).click();
  // The learned threshold is clamped between them, so a new save keeps the
  // floor at most the cap, and the cap at most 90, as the service does.
  await expect(page.locator('#settings-state')).toContainText('Pressure floor must be at most the pressure cap');
  await expect(floor).toBeFocused();
  expect(await page.evaluate(() => window.settingsChanges)).toEqual([]);
  await cap.fill('25');
  await page.getByRole('button', { name: 'Save settings' }).click();
  await expect(page.locator('#settings-state')).toContainText('Settings saved');
  await cap.fill('90.5');
  await page.getByRole('button', { name: 'Save settings' }).click();
  await expect(page.locator('#settings-state')).toContainText('Pressure cap must be at most 90 percent');
  await expect(cap).toBeFocused();
  await cap.fill('90');
  await page.getByRole('button', { name: 'Save settings' }).click();
  await expect(page.locator('#settings-state')).toContainText('Settings saved');
  const changes = await page.evaluate(() => window.settingsChanges);
  expect(changes).toHaveLength(2);
  expect(changes[0].resources.find(resource => resource.id === 'cpu').reserve).toBe('95');
  expect(changes[0].owner.gpu_pressure).toMatchObject({ cap_pct: 25, floor_pct: 25 });
  expect(changes[1].owner.gpu_pressure).toMatchObject({ cap_pct: 90, floor_pct: 25 });
});

test('polling and calibration bounds reveal errors and let related edits repair feasibility', async ({ page }) => {
  await settingsFixture(page);
  await page.getByText('Advanced owner protection settings', { exact: true }).click();
  const media = page.getByLabel('Media polling interval (ms)', { exact: true });
  await media.fill('999');
  await page.getByRole('tab', { name: 'Service', exact: true }).click();
  await page.getByRole('button', { name: 'Save settings' }).click();
  await expect(media).toBeFocused();
  await media.fill('1000');
  await page.getByRole('tab', { name: 'GPU pressure', exact: true }).click();
  await page.getByText('Advanced gpu pressure settings', { exact: true }).click();
  const poll = page.getByLabel('Pressure polling interval (ms)', { exact: true });
  await poll.fill('999');
  await page.getByRole('tab', { name: 'Service', exact: true }).click();
  await page.getByRole('button', { name: 'Save settings' }).click();
  await expect(poll).toBeFocused();
  await poll.fill('1000');
  const halfLife = page.getByLabel('Learning half-life (hours)', { exact: true });
  await halfLife.fill('721');
  await page.getByRole('tab', { name: 'Service', exact: true }).click();
  await page.getByRole('button', { name: 'Save settings' }).click();
  await expect(halfLife).toBeFocused();
  await halfLife.fill('24');
  const quantile = page.getByLabel('Learning quantile (between 0 and 1)', { exact: true });
  await quantile.fill('1');
  await page.getByRole('button', { name: 'Save settings' }).click();
  await expect(page.locator('#settings-state')).toContainText('Learning quantile must be between 0 and 1');
  await expect(quantile).toBeFocused();
  const raise = page.getByLabel('Threshold rise limit (% per hour)', { exact: true });
  await quantile.fill('0.8');
  await raise.fill('0');
  await page.getByRole('button', { name: 'Save settings' }).click();
  await expect(page.locator('#settings-state')).toContainText('Threshold rise limit must be greater than 0');
  await expect(raise).toBeFocused();
  expect(await page.evaluate(() => window.settingsChanges)).toEqual([]);
  await raise.fill('0.5');
  await page.getByRole('button', { name: 'Save settings' }).click();
  await expect(page.locator('#settings-state')).toContainText('Settings saved');
  const changes = await page.evaluate(() => window.settingsChanges);
  expect(changes).toHaveLength(1);
  expect(changes[0].owner.media_poll_ms).toBe(1000);
  expect(changes[0].owner.gpu_pressure).toMatchObject({ learn_half_life_h: 24, learn_quantile: 0.8, raise_pct_per_h: 0.5, poll_ms: 1000 });
});

test('multiple GPU resources require selectors even for a disabled saved default', async ({ page }) => {
  const settings = settingsSnapshot();
  const firstGpu = settings.resources.find(resource => resource.kind === 'nvidia-gpu');
  firstGpu.selector = '0';
  settings.resources.push({ ...firstGpu, id: 'gpu1', selector: '1', enabled: false });
  await settingsFixture(page, undefined, settings);
  await page.getByRole('tab', { name: 'Hardware', exact: true }).click();
  const group = page.getByRole('group', { name: 'gpu1 (nvidia-gpu)', exact: true });
  await group.getByText('Advanced GPU binding', { exact: true }).click();
  const selector = group.getByLabel('GPU selector (required for each GPU)', { exact: true });
  await selector.fill('');
  await page.getByRole('tab', { name: 'Service', exact: true }).click();
  await page.getByRole('button', { name: 'Save settings' }).click();
  await expect(selector).toBeFocused();
  expect(await page.evaluate(() => window.settingsChanges)).toEqual([]);
  await selector.fill('1');
  await page.getByRole('button', { name: 'Save settings' }).click();
  await expect(page.locator('#settings-state')).toContainText('Settings saved');
  expect((await page.evaluate(() => window.settingsChanges))[0].resources.find(resource => resource.id === 'gpu1')).toMatchObject({ selector: '1', enabled: false });
});

test('tuning remains usable at narrow width and doubled text size', async ({ page }) => {
  await page.setViewportSize({ width: 540, height: 900 });
  await page.emulateMedia({ reducedMotion: 'reduce' });
  await settingsFixture(page);
  await page.getByRole('tab', { name: 'Workloads', exact: true }).click();
  await page.locator('body').evaluate(node => { node.style.zoom = '2'; });
  await page.getByLabel('Default free capacity (MiB)', { exact: true }).fill('8500');
  await expect(page.getByLabel('Default free capacity (MiB)', { exact: true })).toHaveValue('8500');
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  await page.screenshot({ path: 'test-results/console-settings-models-zoom.png', fullPage: true });
  await page.locator('body').evaluate(node => { node.style.zoom = ''; });
  await page.screenshot({ path: 'test-results/console-settings-models-narrow.png', fullPage: true });
  await page.setViewportSize({ width: 1280, height: 960 });
  await page.getByRole('tab', { name: 'Service', exact: true }).click();
  await page.getByText('Advanced service settings', { exact: true }).click();
  await page.screenshot({ path: 'test-results/console-settings-service.png', fullPage: true });
});

async function troubleshooting(page, { logging, exportResult = { state: 'saved', path: 'C:\\Users\\owner\\vacancyd-diagnostics.json' }, debugResult } = {}) {
  const status = await fixture('lending');
  if (logging !== undefined) status.logging = logging;
  await page.addInitScript(({ status, exportResult, debugResult }) => {
    window.nativeCalls = [];
    window.__TAURI__ = { core: { invoke: async (command, args) => {
      window.nativeCalls.push({ command, args });
      if (command === 'read_status') return { state: 'online', status, console_version: '1.0.0' };
      if (command === 'set_debug_logging') {
        if (debugResult) return debugResult;
        status.logging = args.request.enabled
          ? { ...status.logging, level: 'debug', source: 'debug_window', debug_until: '2026-10-07T05:00:00Z', debug_remaining_s: 1800 }
          : { ...status.logging, level: status.logging.configured_level, source: 'config', debug_until: null, debug_remaining_s: null };
        return { state: 'applied', enabled: args.request.enabled };
      }
      if (command === 'export_diagnostics') return exportResult;
      throw new Error(`Unexpected command ${command}`);
    } } };
  }, { status, exportResult, debugResult });
  await page.goto('/');
  await page.getByRole('link', { name: 'Installation', exact: true }).click();
}

const configuredInfo = { level: 'info', source: 'config', configured_level: 'info', environment_override: false };

test('debug logging opens for 30 minutes, shows the time left and can end early', async ({ page }) => {
  await troubleshooting(page, { logging: configuredInfo });
  const debug = page.getByRole('button', { name: 'Debug logging for 30 minutes' });
  await expect(page.locator('#logging-state')).toHaveText('The service log records info and above, from the configuration.');
  await expect(debug).toBeEnabled();
  await debug.click();
  await expect(page.locator('#diagnostics-result')).toHaveText('The temporary debug logging window is open until the time shown.');
  await expect(page.locator('#logging-state')).toHaveText('Debug logging is on for 30 more min, then returns to info.');
  const end = page.getByRole('button', { name: 'End debug logging' });
  await expect(end).toBeEnabled();
  await page.screenshot({ path: 'test-results/console-troubleshooting.png' });
  await end.click();
  await expect(page.locator('#diagnostics-result')).toHaveText('The temporary debug logging window has ended.');
  await expect(page.getByRole('button', { name: 'Debug logging for 30 minutes' })).toBeVisible();
  const changes = await page.evaluate(() => window.nativeCalls.filter(call => call.command === 'set_debug_logging').map(call => call.args));
  // The renderer chooses on or off; the host fixes the length.
  expect(changes).toEqual([{ request: { enabled: true } }, { request: { enabled: false } }]);
  expect(await page.locator('body').textContent()).not.toMatch(/[–—]/);
});

test('a debug change made right after the console opens shows the new state without waiting for a poll', async ({ page }) => {
  await troubleshooting(page, { logging: configuredInfo });
  const debug = page.getByRole('button', { name: 'Debug logging for 30 minutes' });
  await expect(debug).toBeEnabled();
  await debug.click();
  // The next scheduled poll is two seconds after load; the change must not wait for it.
  await expect(page.locator('#logging-state')).toHaveText('Debug logging is on for 30 more min, then returns to info.', { timeout: 1000 });
  const calls = await page.evaluate(() => window.nativeCalls.map(call => call.command));
  expect(calls[calls.indexOf('set_debug_logging') + 1]).toBe('read_status');
  await expect(page.locator('#export-hint')).toContainText('names of the programs using each GPU most');
});

test('a refused debug change shows the service reason and keeps the observed state', async ({ page }) => {
  await troubleshooting(page, { logging: configuredInfo, debugResult: { state: 'failed', reason: 'Administrator approval was declined. Nothing was changed.' } });
  await page.getByRole('button', { name: 'Debug logging for 30 minutes' }).click();
  await expect(page.locator('#diagnostics-result')).toHaveText('Administrator approval was declined. Nothing was changed.');
  await expect(page.getByRole('button', { name: 'Debug logging for 30 minutes' })).toBeEnabled();
});

test('a RUST_LOG filter in force is named instead of the configured level', async ({ page }) => {
  await troubleshooting(page, { logging: { level: 'custom', source: 'environment', configured_level: 'info', environment_override: true } });
  await expect(page.locator('#logging-state')).toHaveText('The service log follows the RUST_LOG filter set for the service.');
  await expect(page.getByRole('button', { name: 'Debug logging for 30 minutes' })).toBeEnabled();
});

test('a service that does not report logging keeps export but not the debug switch', async ({ page }) => {
  await troubleshooting(page);
  await expect(page.locator('#logging-state')).toHaveText('This service version does not report its log level.');
  await expect(page.locator('#debug-control')).toBeDisabled();
  await expect(page.getByRole('button', { name: 'Export diagnostics' })).toBeEnabled();
});

for (const [result, message] of [
  [{ state: 'saved', path: 'C:\\Users\\owner\\vacancyd-diagnostics.json' }, 'Diagnostics saved to C:\\Users\\owner\\vacancyd-diagnostics.json.'],
  [{ state: 'cancelled' }, 'Export cancelled. Nothing was saved.'],
  [{ state: 'failed', reason: 'The service refused the diagnostics read. Its authorization policy denies this connection.' }, 'The service refused the diagnostics read. Its authorization policy denies this connection.'],
]) {
  test(`export diagnostics reports ${result.state}`, async ({ page }) => {
    await troubleshooting(page, { logging: configuredInfo, exportResult: result });
    await page.getByRole('button', { name: 'Export diagnostics' }).click();
    await expect(page.locator('#diagnostics-result')).toHaveText(message);
    expect(await page.evaluate(() => window.nativeCalls.filter(call => call.command === 'export_diagnostics').length)).toBe(1);
  });
}

test('troubleshooting controls are disabled while the service is unreachable', async ({ page }) => {
  await open(page, { state: 'unavailable', reason: 'Cannot reach the local service.' });
  await page.getByRole('link', { name: 'Installation', exact: true }).click();
  await expect(page.locator('#logging-state')).toHaveText('Log level unavailable while the service cannot be reached.');
  await expect(page.locator('#debug-control')).toBeDisabled();
  await expect(page.locator('#export-diagnostics')).toBeDisabled();
});

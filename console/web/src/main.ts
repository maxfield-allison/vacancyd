import { render, type Poll } from './view.js';
import { loadSettings, noteConfigRevision, noteVetoExemptions, noteCapacityObservations } from './settings.js';
import { installDiagnostics, noteLogging } from './diagnostics.js';
declare global { interface Window { __TAURI__?: { core: { invoke<T>(command: string, args?: unknown): Promise<T> } } } }
type Change = { enabled: boolean; resource: string | null };
type Result = { state: 'applied'; enabled: boolean } | { state: 'failed'; reason: string };
const globalControl = document.querySelector<HTMLButtonElement>('#lending-control')!;
const dialog = document.querySelector<HTMLDialogElement>('#control-dialog')!;
const message = document.querySelector<HTMLElement>('#control-result')!;
let latest: Poll | undefined;
let pending: Change | undefined;
let busy = false;
let restoreFocus: HTMLElement | null = null;
const links = [...document.querySelectorAll<HTMLAnchorElement>('nav a')];
function select(id: string, focus = true): void {
  if (!links.some(link => link.hash === `#${id}`)) return;
  for (const section of document.querySelectorAll<HTMLElement>('main > section')) section.hidden = section.id !== id;
  for (const link of links) {
    if (link.hash === `#${id}`) link.setAttribute('aria-current', 'page');
    else link.removeAttribute('aria-current');
  }
  document.querySelector('main')!.scrollTop = 0;
  if (focus) document.getElementById(id)!.focus({ preventScroll: true });
  if (id === 'settings') void loadSettings();
}
for (const link of document.querySelectorAll<HTMLAnchorElement>('nav a, .skip, .wordmark')) {
  link.addEventListener('click', event => { event.preventDefault(); select(link.hash.slice(1)); });
}
function controls(): void {
  const online = latest?.state === 'online';
  globalControl.disabled = !online || busy;
  globalControl.textContent = busy ? 'Applying change…' : latest?.state === 'online' ? (latest.status.lending_enabled ? 'Pause lending' : 'Resume lending') : 'Lending state unavailable';
  for (const button of document.querySelectorAll<HTMLButtonElement>('.resource-control')) button.disabled = !online || busy;
}
function present(value: Poll): void {
  // Cards keep their DOM node across a poll when their id is unchanged (see
  // view.ts's syncCards), so focus ordinarily never moves. This restores it
  // only for the rarer case a card was actually replaced, and never steals
  // focus from an open dialog or moves the selected page.
  const focused = document.activeElement instanceof HTMLButtonElement ? document.activeElement.dataset.resource : undefined;
  latest = value;
  render(value);
  controls();
  noteConfigRevision(value.state === 'online' ? value.status.config_revision : undefined);
  noteCapacityObservations(value.state === 'online' ? value.status : undefined);
  noteLogging(value.state === 'online' ? value.status : undefined);
  // Exemption is configuration, not an observation, so an outage keeps the
  // last known answer rather than relabelling exempt routes as host-scoped.
  if (value.state === 'online') noteVetoExemptions(value.status.owner_veto.exempt_upstreams ?? []);
  if (focused && !dialog.open) {
    const replacement = [...document.querySelectorAll<HTMLButtonElement>('.resource-control')].find(b => b.dataset.resource === focused);
    (replacement ?? document.querySelector<HTMLElement>('main > section:not([hidden])'))?.focus({ preventScroll: true });
  }
}
function request(change: Change, source: HTMLElement): void {
  if (busy || latest?.state !== 'online' || dialog.open) return;
  pending = change;
  restoreFocus = source;
  const resource = change.resource === null ? undefined : latest.status.resources.find(r => r.id === change.resource);
  if (change.resource !== null && !resource) return;
  document.getElementById('control-title')!.textContent = resource
    ? `${change.enabled ? 'Enable' : 'Disable'} sharing for ${resource.label ?? resource.id}?`
    : `${change.enabled ? 'Resume' : 'Pause'} lending?`;
  const approval = resource
    ? ' Changing a resource needs administrator approval, which Windows asks for.'
    : change.enabled ? ' If another account paused lending, resuming needs administrator approval, which Windows asks for.' : '';
  document.getElementById('control-detail')!.textContent = (change.enabled
    ? 'Allow new work only when owner protection, resource limits and readiness permit it. This does not force lending while you use the machine.'
    : 'Refuse new work for this selection. Existing work follows the service’s reclaim policy; this does not guarantee immediate cancellation or freed GPU memory.') + approval;
  dialog.returnValue = 'cancel';
  dialog.showModal();
}
globalControl.addEventListener('click', () => {
  if (latest?.state === 'online') request({ enabled: !latest.status.lending_enabled, resource: null }, globalControl);
});
document.getElementById('resources')!.addEventListener('click', event => {
  const button = (event.target as Element).closest<HTMLButtonElement>('button[data-resource]');
  if (button) request({ enabled: button.dataset.enabled === 'true', resource: button.dataset.resource! }, button);
});
dialog.addEventListener('close', () => {
  const change = pending;
  pending = undefined;
  const target = restoreFocus;
  if (target?.isConnected) target.focus();
  else if (target?.dataset.resource) [...document.querySelectorAll<HTMLButtonElement>('.resource-control')].find(b => b.dataset.resource === target.dataset.resource)?.focus();
  if (dialog.returnValue === 'confirm' && change) void apply(change);
});
async function apply(change: Change): Promise<void> {
  if (busy || latest?.state !== 'online') return;
  busy = true;
  controls();
  message.textContent = 'Applying change…';
  try {
    if (!window.__TAURI__) throw new Error('No native host');
    const result = await window.__TAURI__.core.invoke<Result>('set_lending', { request: change });
    message.textContent = result.state === 'applied'
      ? 'Change acknowledged. Refreshing current service state…'
      : result.reason;
    // Status is authoritative even after failed/ambiguous acknowledgement.
    present(await window.__TAURI__.core.invoke<Poll>('read_status'));
    if (result.state === 'applied') message.textContent = latest?.state === 'online'
      ? 'The service acknowledged the change.'
      : 'Change acknowledged, but current service status is unavailable. Check again before making another change.';
  } catch {
    message.textContent = 'Could not confirm the change. Check current service state before trying again.';
    present({ state: 'unavailable', reason: 'Control response unavailable. Reconnecting to the service…' });
  } finally {
    busy = false; controls();
    if (document.activeElement === document.body) {
      const resourceButton = [...document.querySelectorAll<HTMLButtonElement>('.resource-control')].find(b => b.dataset.resource === change.resource);
      if (resourceButton && !document.getElementById('hardware')!.hidden) resourceButton.focus();
      else if (change.resource === null) globalControl.focus();
    }
  }
}
async function poll(): Promise<void> {
  let deadline: ReturnType<typeof setTimeout> | undefined;
  try {
    if (busy) return;
    if (!window.__TAURI__) throw new Error('native host unavailable');
    const response = await Promise.race([
      window.__TAURI__.core.invoke<Poll>('read_status'),
      new Promise<never>((_, reject) => { deadline = setTimeout(() => reject(new Error('timeout')), 3000); }),
    ]);
    if (!busy) present(response);
  } catch {
    if (!busy) present({ state: 'unavailable', reason: 'Cannot read local status. Check that the service is running and its version matches the console.' });
  } finally { clearTimeout(deadline); setTimeout(() => { void poll(); }, 2000); }
}
installDiagnostics(async () => {
  if (window.__TAURI__) present(await window.__TAURI__.core.invoke<Poll>('read_status'));
});
void poll();

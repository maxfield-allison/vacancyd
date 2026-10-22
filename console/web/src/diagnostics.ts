// Troubleshooting controls on the Installation page: a temporary debug
// logging window and the diagnostics export. The service decides both; this
// module only shows what it reports and asks the native host to act.
import type { Logging } from './view.js';

type Change = { state: 'applied'; enabled: boolean } | { state: 'failed'; reason: string };
type Export = { state: 'saved'; path: string } | { state: 'cancelled' } | { state: 'failed'; reason: string };

const debugControl = document.querySelector<HTMLButtonElement>('#debug-control')!;
const exportControl = document.querySelector<HTMLButtonElement>('#export-diagnostics')!;
const level = document.querySelector<HTMLElement>('#logging-state')!;
const result = document.querySelector<HTMLElement>('#diagnostics-result')!;
let logging: Logging | null | undefined;
let online = false;
let busy = false;
let refresh: () => Promise<void> = async () => {};

const minutes = (seconds: number) => Math.max(1, Math.ceil(seconds / 60));

function describe(value: Logging | null | undefined): string {
  if (!online) return 'Log level unavailable while the service cannot be reached.';
  if (!value) return 'This service version does not report its log level.';
  if (value.debug_until && value.debug_remaining_s != null) return `Debug logging is on for ${minutes(value.debug_remaining_s)} more min, then returns to ${value.environment_override ? 'the RUST_LOG filter' : value.configured_level}.`;
  if (value.environment_override) return 'The service log follows the RUST_LOG filter set for the service.';
  return `The service log records ${value.level} and above, from the configuration.`;
}

function controls(): void {
  const open = Boolean(logging?.debug_until);
  // A service without the switch reports no logging state at all.
  debugControl.disabled = !online || !logging || busy;
  debugControl.textContent = open ? 'End debug logging' : 'Debug logging for 30 minutes';
  debugControl.dataset.enabled = String(!open);
  exportControl.disabled = !online || busy;
  level.textContent = describe(logging);
}

/** Note the latest status poll; `undefined` when the service is unreachable. */
export function noteLogging(status: { logging?: Logging | null } | undefined): void {
  online = status !== undefined;
  logging = status?.logging;
  controls();
}

async function run(work: () => Promise<string>): Promise<void> {
  if (busy) return;
  busy = true;
  controls();
  try {
    if (!window.__TAURI__) throw new Error('native host unavailable');
    result.textContent = await work();
  } catch {
    result.textContent = 'Could not confirm the result. Check current service state before trying again.';
  } finally {
    busy = false;
    controls();
  }
}

debugControl.addEventListener('click', () => {
  const enabled = debugControl.dataset.enabled === 'true';
  void run(async () => {
    result.textContent = enabled ? 'Turning on debug logging…' : 'Ending debug logging…';
    const change = await window.__TAURI__!.core.invoke<Change>('set_debug_logging', { request: { enabled } });
    // Status is authoritative, including after a failed or ambiguous answer.
    await refresh();
    if (change.state === 'failed') return change.reason;
    return enabled ? 'The temporary debug logging window is open until the time shown.' : 'The temporary debug logging window has ended.';
  });
});

exportControl.addEventListener('click', () => {
  void run(async () => {
    result.textContent = 'Collecting diagnostics…';
    const saved = await window.__TAURI__!.core.invoke<Export>('export_diagnostics');
    if (saved.state === 'saved') return `Diagnostics saved to ${saved.path}.`;
    if (saved.state === 'cancelled') return 'Export cancelled. Nothing was saved.';
    return saved.reason;
  });
});

/** `reread` fetches and presents current status after a change. */
export function installDiagnostics(reread: () => Promise<void>): void {
  refresh = reread;
  controls();
}

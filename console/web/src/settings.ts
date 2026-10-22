import { settingsNavigation } from './settings-navigation.js';

type PressureMode = 'calibrated' | 'fixed';
type ReclaimMode = 'cancel' | 'drain' | 'finish';
type OwnerScope = 'host' | 'resource';
type ManagementPolicy = 'deny' | 'allow';

import type { SettingsSnapshot, SettingsUpdate, ResourceSetting, UpstreamSetting } from './settings-wire.js';
import { installTuningFields, renderTuning, collectTuning, updateCalibrationMode, setTuningNavigation, restoreModelModes } from './settings-tuning.js';
export { noteCapacityObservations } from './settings-tuning.js';
export type { SettingsSnapshot } from './settings-wire.js';

type SettingsPoll =
  | { state: 'online'; settings: SettingsSnapshot }
  | { state: 'unavailable'; reason: string };
type SettingsSaveResult =
  | { state: 'saved'; settings: SettingsSnapshot; restart_required: boolean }
  | { state: 'failed'; reason: string };

const form = document.querySelector<HTMLFormElement>('#settings-form')!;
const state = document.querySelector<HTMLElement>('#settings-state')!;
const save = document.querySelector<HTMLButtonElement>('#save-settings')!;
const reload = document.querySelector<HTMLButtonElement>('#reload-settings')!;
const retry = document.querySelector<HTMLButtonElement>('#settings-retry')!;
const restartPending = document.querySelector<HTMLElement>('#settings-restart-pending')!;
const resources = document.querySelector<HTMLElement>('#settings-resources')!;
const upstreams = document.querySelector<HTMLElement>('#settings-upstreams')!;
const pressureMode = document.querySelector<HTMLSelectElement>('#setting-pressure-mode')!;
const pressureThreshold = document.querySelector<HTMLInputElement>('#setting-pressure-threshold')!;
const savedRestartMessage = 'Settings saved. Capacity budgets are active for new admissions. Restart the vacancyd service to apply the remaining policy.';
installTuningFields(form);
const refreshNavigation = settingsNavigation(form);
setTuningNavigation(refreshNavigation);
let snapshot: SettingsSnapshot | undefined;
let loading = false;
let saving = false;
// Whether the form holds edits the owner has not saved. Reload discards
// them, so it asks first instead of losing typed values silently.
let dirty = false;
// The config revision the running service actually loaded, from the most
// recent /status poll (see main.ts). Compared against `snapshot.revision`
// to show a saved-but-not-yet-active banner.
let activeConfigRevision: string | undefined;
// Upstreams the running service exempts from the owner veto. Their "owner
// return" choices apply only when pressure on their own hardware reclaims
// them, so each such row says so.
let vetoExempt = new Set<string>();

function input<T extends HTMLInputElement | HTMLTextAreaElement | HTMLSelectElement>(id: string): T {
  return document.querySelector<T>(`#${id}`)!;
}

function setControlsDisabled(value: boolean): void {
  for (const control of form.querySelectorAll<HTMLInputElement | HTMLTextAreaElement | HTMLSelectElement | HTMLButtonElement>('input, textarea, select, button')) {
    control.disabled = value || (control === pressureThreshold && pressureMode.value !== 'fixed');
  }
  for (const row of upstreams.querySelectorAll<HTMLElement>('.upstream-setting')) {
    const mode = row.querySelector<HTMLSelectElement>('.upstream-reclaim')!;
    row.querySelector<HTMLInputElement>('.upstream-drain')!.disabled = value || mode.value !== 'drain';
  }
  updateCalibrationMode(value);
  restoreModelModes(value);
}

function setBusy(value: boolean): void {
  saving = value;
  setControlsDisabled(value);
  save.textContent = value ? 'Saving settings…' : 'Save settings';
}

function updatePressureMode(): void {
  // Disable but do not clear: the field only matters in fixed mode, and
  // `collect()` sends 0 for every other mode regardless of what is shown
  // here. Zeroing it on every mode change used to erase a typed threshold
  // when the owner switched away from Fixed and back.
  pressureThreshold.disabled = saving || pressureMode.value !== 'fixed';
  updateCalibrationMode(saving);
}

function addText(parent: HTMLElement, tag: 'strong' | 'span' | 'p', text: string, className?: string): HTMLElement {
  const child = document.createElement(tag);
  child.textContent = text;
  if (className) child.className = className;
  parent.append(child);
  return child;
}

function renderResources(values: ResourceSetting[]): void {
  resources.replaceChildren();
  for (const value of values) {
    const label = document.createElement('label');
    label.className = 'check-row resource-default';
    label.dataset.id = value.id;
    label.dataset.kind = value.kind;
    const checkbox = document.createElement('input');
    checkbox.type = 'checkbox';
    checkbox.checked = value.enabled;
    checkbox.className = 'resource-default-control';
    label.append(checkbox);
    addText(label, 'span', `${value.id} (${value.kind})`);
    resources.append(label);
  }
}

// Preserve identity: replacing punctuation would make valid upstream names
// such as "model.a" and "model-a" share labels and warning descriptions.
function elementId(prefix: string, id: string): string {
  return `${prefix}-${[...id].map(character => character.codePointAt(0)!.toString(16)).join('-')}`;
}

function renderUpstreams(values: UpstreamSetting[]): void {
  upstreams.replaceChildren();
  for (const value of values) {
    const card = document.createElement('div');
    card.className = 'upstream-setting';
    card.dataset.id = value.id;
    card.dataset.kind = value.kind;
    // Groups the controls below under this upstream's identity, so an
    // assistive-technology user does not hear the same generic checkbox
    // name ("Ask the upstream to unload…") for every route with no way to
    // tell them apart.
    card.setAttribute('role', 'group');
    card.setAttribute('aria-label', `${value.id} (${value.kind})`);
    addText(card, 'strong', value.id);
    addText(card, 'span', value.kind, 'muted');
    const exemption = addText(card, 'p', 'Owner presence does not pause this route (owner_scope = "resource"). The choices below apply when pressure on its own hardware reclaims it.', 'hint upstream-exempt');
    exemption.hidden = !vetoExempt.has(value.id);

    const unloadLabel = document.createElement('label');
    unloadLabel.className = 'check-row';
    const unload = document.createElement('input');
    unload.type = 'checkbox';
    unload.checked = value.yield_on_veto;
    unload.className = 'upstream-yield';
    unloadLabel.append(unload);
    addText(unloadLabel, 'span', 'Ask the upstream to unload on owner return');
    card.append(unloadLabel);

    const modeId = elementId('upstream-reclaim', value.id);
    const modeLabel = document.createElement('label');
    modeLabel.textContent = 'Existing work on owner return';
    modeLabel.htmlFor = modeId;
    const mode = document.createElement('select');
    mode.id = modeId;
    mode.className = 'upstream-reclaim';
    for (const [key, label] of [['cancel', 'Cancel now'], ['drain', 'Drain, then cancel'], ['finish', 'Allow work to finish']] as const) {
      const option = document.createElement('option');
      option.value = key;
      option.textContent = label;
      option.selected = key === value.on_owner_return;
      mode.append(option);
    }
    card.append(modeLabel, mode);

    const timeoutId = elementId('upstream-drain', value.id);
    const timeoutLabel = document.createElement('label');
    timeoutLabel.textContent = 'Drain time in seconds';
    timeoutLabel.htmlFor = timeoutId;
    const timeout = document.createElement('input');
    timeout.id = timeoutId;
    timeout.type = 'number';
    timeout.min = '0';
    timeout.max = '86400';
    timeout.step = '1';
    timeout.required = true;
    timeout.value = String(value.drain_timeout_s);
    timeout.className = 'upstream-drain';
    timeout.disabled = value.on_owner_return !== 'drain';
    mode.addEventListener('change', () => { timeout.disabled = saving || mode.value !== 'drain'; });
    card.append(timeoutLabel, timeout);
    const safety = document.createElement('details');
    safety.className = 'upstream-safety advanced-settings';
    const summary = document.createElement('summary');
    summary.textContent = `Advanced permissions for ${value.id}`;
    safety.append(summary);
    for (const [key, title, choices, warning] of [
      ['owner_scope', 'Owner activity protection', [['host', 'Protect owner activity'], ['resource', 'Allow lending during owner activity']], 'Allowing lending during owner activity exempts this workload from the owner veto. Only GPU pressure on its watched hardware can reclaim it; memory and CPU checks do not stop running work.'],
      ['management', 'Borrower management permissions', [['deny', 'Deny management and private data access'], ['allow', 'Allow management and private data access']], 'Allowing management lets borrowers change upstream state and access owner data through this route. Keep it denied unless those borrowers are trusted to manage the upstream.'],
    ] as const) {
      const id = elementId(`upstream-${key}`, value.id);
      const label = document.createElement('label');
      label.htmlFor = id;
      label.textContent = title;
      const select = document.createElement('select');
      select.id = id;
      select.className = `upstream-${key}`;
      select.setAttribute('aria-describedby', `${id}-warning`);
      for (const [optionValue, text] of choices) {
        const option = document.createElement('option');
        option.value = optionValue;
        option.textContent = text;
        option.selected = value[key] === optionValue;
        select.append(option);
      }
      safety.append(label, select);
      const explanation = addText(safety, 'p', warning, 'hint');
      explanation.id = `${id}-warning`;
    }
    addText(safety, 'p', 'These permissions require administrator approval to save and a service restart to take effect. The Workloads status page describes the running policy.', 'hint');
    card.append(safety);
    upstreams.append(card);
  }
}

function render(value: SettingsSnapshot): void {
  snapshot = value;
  input<HTMLInputElement>('setting-unlocked-presence').checked = value.owner.unlocked_session_is_presence;
  input<HTMLInputElement>('setting-idle-seconds').value = String(value.owner.idle_lends_after_s);
  input<HTMLInputElement>('setting-media-presence').checked = value.owner.media_is_presence;
  input<HTMLInputElement>('setting-release-seconds').value = String(value.owner.release_after_s);
  input<HTMLTextAreaElement>('setting-processes').value = value.owner.processes.join('\n');
  input<HTMLTextAreaElement>('setting-media-ignore').value = value.owner.media_ignore.join('\n');
  input<HTMLInputElement>('setting-sign-in-presence').checked = value.owner.sign_in_screen_is_presence;
  input<HTMLInputElement>('setting-pressure-enabled').checked = value.owner.gpu_pressure.enabled;
  pressureMode.value = value.owner.gpu_pressure.mode;
  pressureThreshold.value = String(value.owner.gpu_pressure.threshold_pct);
  input<HTMLInputElement>('setting-pressure-sustain').value = String(value.owner.gpu_pressure.sustain_s);
  input<HTMLTextAreaElement>('setting-pressure-ignore').value = value.owner.gpu_pressure.ignore.join('\n');
  renderResources(value.resources);
  renderUpstreams(value.upstreams);
  renderTuning(value);
  refreshNavigation();
  form.hidden = false;
  updatePressureMode();
  dirty = false;
  updateRestartBanner();
}

// Capacity budgets apply live; remaining policy requires a restart. Clear
// that policy notice when /status confirms the saved revision was loaded,
// without re-rendering the form or discarding unsaved edits.
function updateRestartBanner(): void {
  const restarted = snapshot?.revision === activeConfigRevision && activeConfigRevision !== undefined;
  restartPending.hidden = !snapshot || restarted || !snapshot.application.restart_required;
  if (restarted && state.textContent === savedRestartMessage) {
    state.textContent = 'Saved settings are active. Changes are saved only when you choose Save settings.';
  }
}

export function noteVetoExemptions(ids: string[]): void {
  vetoExempt = new Set(ids);
  for (const row of upstreams.querySelectorAll<HTMLElement>('.upstream-setting')) {
    row.querySelector<HTMLElement>('.upstream-exempt')!.hidden = !vetoExempt.has(row.dataset.id ?? '');
  }
}

export function noteConfigRevision(revision: string | null | undefined): void {
  activeConfigRevision = revision ?? undefined;
  updateRestartBanner();
}

function boundedInteger(id: string, name: string): number {
  const control = input<HTMLInputElement>(id);
  const value = Number(control.value);
  if (!Number.isSafeInteger(value) || !control.checkValidity()) throw new Error(`${name} must be a whole number in the displayed range.`);
  return value;
}

function lines(id: string, name: string): string[] {
  const values = input<HTMLTextAreaElement>(id).value.split(/\r?\n/).map(value => value.trim()).filter(Boolean);
  if (values.length > 64) throw new Error(`${name} accepts at most 64 entries.`);
  if (values.some(value => [...value].length > 128)) throw new Error(`${name} entries must be 128 characters or fewer.`);
  return values;
}

function collect(): SettingsUpdate {
  if (!snapshot) throw new Error('Settings are not available.');
  const mode = pressureMode.value as PressureMode;
  // Only fixed mode's threshold is meaningful; every other mode always
  // sends 0, regardless of what the disabled field last showed.
  const threshold = mode === 'fixed' ? Number(pressureThreshold.value) : 0;
  if (mode === 'fixed' && (!Number.isFinite(threshold) || threshold <= 0 || threshold > 100)) {
    throw new Error('Fixed GPU pressure needs a threshold greater than 0 and at most 100.');
  }
  const upstreamValues = [...upstreams.querySelectorAll<HTMLElement>('.upstream-setting')].map(row => {
    const drain = row.querySelector<HTMLInputElement>('.upstream-drain')!;
    const drainTimeout = Number(drain.value);
    if (!Number.isSafeInteger(drainTimeout) || drainTimeout < 0 || drainTimeout > 86400) {
      throw new Error(`Drain time for ${row.dataset.id!} must be a whole number from 0 through 86400.`);
    }
    return {
      ...snapshot!.upstreams.find(value => value.id === row.dataset.id)!,
      id: row.dataset.id!,
      kind: row.dataset.kind!,
      yield_on_veto: row.querySelector<HTMLInputElement>('.upstream-yield')!.checked,
      on_owner_return: row.querySelector<HTMLSelectElement>('.upstream-reclaim')!.value as ReclaimMode,
      drain_timeout_s: drainTimeout,
      owner_scope: row.querySelector<HTMLSelectElement>('.upstream-owner_scope')!.value as OwnerScope,
      management: row.querySelector<HTMLSelectElement>('.upstream-management')!.value as ManagementPolicy,
    };
  });
  const { application: _application, capabilities: _capabilities, ...original } = snapshot;
  return collectTuning({
    ...original,
    revision: snapshot.revision,
    owner: {
      ...snapshot.owner,
      unlocked_session_is_presence: input<HTMLInputElement>('setting-unlocked-presence').checked,
      idle_lends_after_s: boundedInteger('setting-idle-seconds', 'Idle time'),
      media_is_presence: input<HTMLInputElement>('setting-media-presence').checked,
      release_after_s: boundedInteger('setting-release-seconds', 'Owner return hold time'),
      processes: lines('setting-processes', 'Owner activity'),
      media_ignore: lines('setting-media-ignore', 'Media exceptions'),
      sign_in_screen_is_presence: input<HTMLInputElement>('setting-sign-in-presence').checked,
      gpu_pressure: {
        ...snapshot.owner.gpu_pressure,
        enabled: input<HTMLInputElement>('setting-pressure-enabled').checked,
        mode,
        threshold_pct: threshold,
        sustain_s: boundedInteger('setting-pressure-sustain', 'GPU pressure sustain time'),
        ignore: lines('setting-pressure-ignore', 'GPU pressure exceptions'),
      },
    },
    resources: [...resources.querySelectorAll<HTMLElement>('.resource-default')].map(row => ({
      ...snapshot!.resources.find(value => value.id === row.dataset.id)!,
      id: row.dataset.id!,
      kind: row.dataset.kind!,
      enabled: row.querySelector<HTMLInputElement>('input')!.checked,
    })),
    upstreams: upstreamValues,
  }, snapshot);
}

export async function loadSettings(force = false): Promise<void> {
  if ((!force && snapshot) || loading || saving) return;
  loading = true;
  setControlsDisabled(true);
  // A failed read leaves the form hidden with nothing to click; a visible
  // Retry (outside the form, so it is reachable regardless) replaces
  // clicking the Settings nav link again as the only way back in.
  retry.hidden = true;
  state.textContent = 'Reading protected settings…';
  try {
    if (!window.__TAURI__) throw new Error('Native host unavailable.');
    const result = await window.__TAURI__.core.invoke<SettingsPoll>('read_settings');
    if (result.state === 'online') {
      render(result.settings);
      state.textContent = 'Settings loaded. Changes are saved only when you choose Save settings.';
    } else {
      state.textContent = result.reason;
      retry.hidden = false;
    }
  } catch {
    state.textContent = 'Settings are unavailable. Check that the service and console versions match.';
    retry.hidden = false;
  } finally {
    loading = false;
    setControlsDisabled(false);
  }
}

pressureMode.addEventListener('change', updatePressureMode);
function markDirty(event: Event): void {
  if (!(event.target instanceof Element) || !event.target.closest('[data-settings-navigation]')) {
    dirty = true;
    // Cross-field errors are recomputed on Save. Changing another field
    // can fix them, so stale custom validity must not block that next save.
    for (const control of form.querySelectorAll<HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement>('input, select, textarea')) control.setCustomValidity('');
  }
}
form.addEventListener('input', markDirty);
form.addEventListener('change', markDirty);
reload.addEventListener('click', () => {
  if (dirty && !window.confirm('Discard your unsaved settings changes and reload the last saved values?')) return;
  void loadSettings(true);
});
retry.addEventListener('click', () => { void loadSettings(true); });
form.addEventListener('submit', event => {
  event.preventDefault();
  if (saving) return;
  void (async () => {
    try {
      const request = collect();
      setBusy(true);
      state.textContent = 'Validating and saving protected settings. Approve the Windows administrator prompt to continue…';
      if (!window.__TAURI__) throw new Error('Native host unavailable.');
      const result = await window.__TAURI__.core.invoke<SettingsSaveResult>('save_settings', { request });
      if (result.state === 'saved') {
        render(result.settings);
        state.textContent = result.restart_required
          ? savedRestartMessage
          : 'Settings saved. Capacity budgets are active for new admissions.';
      } else {
        state.textContent = result.reason;
      }
    } catch (error) {
      state.textContent = error instanceof Error ? error.message : 'Settings could not be saved.';
    } finally {
      if (saving) {
        setBusy(false);
        save.focus();
      }
    }
  })();
});

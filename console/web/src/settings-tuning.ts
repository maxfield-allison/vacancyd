import type { SettingsSnapshot, SettingsUpdate, ModelSetting, UpstreamSetting } from './settings-wire.js';
import type { Status } from './view.js';

type NumericField = readonly [string, string, string, string, string];
const ownerNumbers: NumericField[] = [ ['media_poll_ms', 'Media polling interval (ms)', '1000', '9007199254740991', '1'] ];
// The service's highest calibration cap (`MAX_CAP_PCT` in src/plugin/pressure.rs).
const MAX_CAP_PCT = 90;
const pressureNumbers: NumericField[] = [
  ['cap_pct', 'Pressure cap (%)', '0', '100', 'any'],
  ['margin_pct', 'Pressure margin (%)', '0', '100', 'any'],
  ['floor_pct', 'Pressure floor (%)', '0', '100', 'any'],
  ['learn_quantile', 'Learning quantile (between 0 and 1)', '0', '1', 'any'],
  ['learn_half_life_h', 'Learning half-life (hours)', '0', '720', 'any'],
  ['raise_pct_per_h', 'Threshold rise limit (% per hour)', '0', '100', 'any'],
  ['warmup_samples', 'Calibration warmup samples', '1', '9007199254740991', '1'],
  ['poll_ms', 'Pressure polling interval (ms)', '1000', '9007199254740991', '1'],
];
const limits: NumericField[] = [
  ['header_read_timeout_s', 'Header and keep-alive timeout (seconds)', '1', '300', '1'],
  ['body_read_timeout_s', 'Whole request body timeout (seconds)', '1', '3600', '1'],
  ['body_idle_timeout_s', 'Request body idle timeout (seconds)', '1', '300', '1'],
  ['write_stall_timeout_s', 'Response write stall timeout (seconds)', '1', '3600', '1'],
  ['max_connections', 'Connections per listener', '1', '4096', '1'],
  ['max_request_body_mib', 'Maximum request body (MiB)', '1', '1024', '1'],
  ['max_buffered_body_mib', 'Buffered request budget (MiB, blank inherits)', '1', '65536', '1'],
  ['tunnel_idle_timeout_s', 'WebSocket idle timeout (seconds)', '1', '9007199254740991', '1'],
];
let refreshSearch = (): void => {};
let observed: Status | undefined;
let resourceChoices: SettingsSnapshot['resources'] = [];

function text(parent: HTMLElement, value: string): HTMLParagraphElement {
  const node = document.createElement('p'); node.className = 'hint'; node.textContent = value; parent.append(node); return node;
}
function field(parent: HTMLElement, key: string, title: string, value: string, optional = false): HTMLInputElement {
  const label = document.createElement('label'); label.textContent = title;
  const control = document.createElement('input'); control.id = `tuning-${key}`; control.type = 'text'; control.maxLength = 2048; control.dataset.configKey = key;
  label.htmlFor = control.id; control.value = value; control.required = !optional;
  control.addEventListener('input', () => control.setCustomValidity(''));
  parent.append(label, control); return control;
}
function numberField(parent: HTMLElement, prefix: string, spec: NumericField, value = ''): HTMLInputElement {
  const [key, title, min, max, step] = spec;
  const control = field(parent, `${prefix}-${key}`, title, value, key === 'max_buffered_body_mib');
  Object.assign(control, { type: 'number', min, max, step }); return control;
}
function select(parent: HTMLElement, key: string, title: string, choices: readonly (readonly [string, string])[], value: string): HTMLSelectElement {
  const label = document.createElement('label'); label.textContent = title;
  const control = document.createElement('select'); control.id = `tuning-${key}`; control.dataset.configKey = key; label.htmlFor = control.id;
  for (const [key, title] of choices) { const option = document.createElement('option'); option.value = key; option.textContent = title; control.append(option); }
  control.value = value; parent.append(label, control); return control;
}
function details(parent: HTMLElement, title: string): HTMLDetailsElement {
  const node = document.createElement('details'); node.className = 'advanced-settings';
  const summary = document.createElement('summary'); summary.textContent = title; node.append(summary); parent.append(node); return node;
}
function card(form: HTMLElement, category: string, title: string, advanced = true): HTMLFieldSetElement {
  const node = document.createElement('fieldset'); node.className = 'settings-card'; node.dataset.settingsCategory = category;
  const legend = document.createElement('legend'); legend.textContent = title; node.append(legend);
  form.querySelector(advanced ? '.advanced-grid' : '.settings-grid')!.append(node); return node;
}
function id(value: string): string { return [...value].map(character => character.codePointAt(0)!.toString(16)).join('-'); }
function control(key: string): HTMLInputElement | HTMLSelectElement { return document.getElementById(`tuning-${key}`) as HTMLInputElement | HTMLSelectElement; }
function integer(key: string): number {
  const node = control(key); const value = Number(node.value);
  if (node.value.trim() === '' || !Number.isSafeInteger(value) || !node.checkValidity()) throw new Error(`${node.labels?.[0]?.textContent} needs a whole number in the displayed range.`);
  return value;
}
function invalid(node: HTMLInputElement | HTMLSelectElement, message: string): never {
  node.setCustomValidity(message); node.reportValidity(); throw new Error(message);
}
function decimal(node: HTMLInputElement, optional = false): string | null {
  if (optional && node.value === '') return null;
  if (!/^(0|[1-9][0-9]*)$/.test(node.value) || BigInt(node.value) > 9223372036854775807n) {
    const message = `${node.labels?.[0]?.textContent} needs whole decimal digits from 0 through 9223372036854775807.`;
    invalid(node, message);
  }
  return node.value;
}
function capacity(parent: HTMLElement, key: string, title: string, value: string): HTMLInputElement {
  const node = field(parent, key, title, value); node.maxLength = 19; node.inputMode = 'numeric'; node.pattern = '(0|[1-9][0-9]*)'; return node;
}

export function installTuningFields(form: HTMLFormElement): void {
  const status = card(form, 'service', 'Authentication and local administration', false);
  status.dataset.settingsSearch = 'auth.token auth.trust_loopback auth.allow_unauthenticated_remote upstream.credential server.admin_group server.admin_socket server.admission_state_dir owner.gpu_pressure.state_file';
  const configured = text(status, 'Reading configured capabilities…'); configured.id = 'settings-capabilities';
  text(status, 'Configured status does not prove that a credential resolves. Credential replacement and installation migration use separate privileged workflows. Generic Save does not change them.');
  const owner = card(form, 'owner', 'Presence source and timing');
  select(owner, 'presence', 'Presence source', [['auto', 'Automatic desktop and session evidence'], ['headless', 'Headless: disable desktop and session evidence']], 'auto');
  text(owner, 'Headless disables desktop/session evidence. Named activity processes remain configured; incompatible combinations are refused. Changes require a service restart.');
  ownerNumbers.forEach(spec => numberField(owner, 'owner', spec));
  const pressure = card(form, 'pressure', 'Pressure calibration and sampling');
  text(pressure, 'Calibration learns each GPU\'s ordinary load. The threshold is the learning quantile plus the margin, never below the floor or above the cap; it falls at once and rises at most the rise limit per observed hour. Older readings count half after each half-life. Fixed mode uses its threshold. Polling still applies in every mode. Changes require restart.');
  pressureNumbers.forEach(spec => numberField(pressure, 'pressure', spec));
  const hardware = card(form, 'hardware', 'Hardware sampling');
  numberField(hardware, 'hardware', ['poll_interval_ms', 'Hardware polling interval (ms)', '1', '9007199254740991', '1']);
  const service = card(form, 'service', 'Request handling and network topology');
  text(service, 'These settings require a service restart. Changing a listener can disconnect the console or expose a service to a different network. Header/body limits never cut off a progressing generation; write stall limits apply when the client stops reading.');
  field(service, 'server-listen', 'Status API listener (IP:port)', '');
  limits.forEach(spec => numberField(service, 'limits', spec));
  text(service, 'A blank buffered budget inherits the larger of 256 MiB and the request-body limit. WebSocket tunnels have their own connection limit and idle timeout.');
}

function annotateTuning(): void {
  const keys: Record<string, string> = {
    'resource-reserve': 'resource.reserve', 'resource-selector': 'resource.selector',
    'resource-adapter': 'resource.adapter', 'upstream-default-mib': 'upstream.default_min_free_mib',
    'model-name': 'upstream.models.name', 'model-mode': 'upstream.models.mode',
    'model-load': 'upstream.models.min_free_mib', 'model-resident': 'upstream.models.resident_min_free_mib',
    'model-resource-id': 'upstream.models.resources.id', 'model-resource-mib': 'upstream.models.resources.min_free_mib',
    'upstream-completion-timeout': 'upstream.completion_timeout_s',
    'upstream-forward-authorization': 'upstream.forward_authorization',
    'upstream-listen': 'upstream.listen', 'upstream-target': 'upstream.target',
    'upstream-primary': 'upstream.resource', 'upstream-also': 'upstream.also_requires',
  };
  for (const node of document.querySelectorAll<HTMLInputElement | HTMLSelectElement>('[id^="tuning-"]')) {
    const key = node.id.slice('tuning-'.length);
    node.dataset.configKey = keys[node.className] ?? (key.startsWith('pressure-') ? `owner.gpu_pressure.${key.slice(9)}` : key.startsWith('limits-') ? `server.limits.${key.slice(7)}` : key === 'presence' ? 'owner.presence' : key === 'server-listen' ? 'server.listen' : key.replace(/^(owner|hardware)-/, '$1.'));
    const hints = [...(node.parentElement?.children ?? [])].filter((item): item is HTMLElement => item instanceof HTMLElement && item.classList.contains('hint'));
    hints.forEach((hint, index) => { hint.id ||= `${node.id}-hint-${index}`; });
    node.setAttribute('aria-describedby', hints.map(hint => hint.id).join(' '));
  }
}

export function setTuningNavigation(refresh: () => void): void { refreshSearch = refresh; }
export function updateCalibrationMode(saving = false): void {
  const fixed = (document.getElementById('setting-pressure-mode') as HTMLSelectElement).value === 'fixed';
  for (const [key] of pressureNumbers) {
    const node = control(`pressure-${key}`); if (!node) continue;
    node.disabled = saving || (fixed && key !== 'poll_ms');
  }
}

function renderModels(parent: HTMLElement, upstream: UpstreamSetting): void {
  const container = document.createElement('div'); container.className = 'model-settings'; parent.append(container);
  function add(model: ModelSetting): void {
    const row = document.createElement('div'); row.className = 'model-setting settings-policy-group'; row.setAttribute('role', 'group');
    row.setAttribute('aria-label', `${upstream.id} model ${model.name || 'new model'}`);
    const prefix = `${id(upstream.id)}-model-${container.children.length}-${crypto.randomUUID()}`;
    const name = field(row, `${prefix}-name`, 'Model name', model.name); name.className = 'model-name';
    name.addEventListener('input', () => { row.setAttribute('aria-label', `${upstream.id} model ${name.value || 'new model'}`); refreshSearch(); });
    const advanced = details(row, 'Advanced model requirements');
    const mode = select(advanced, `${prefix}-mode`, 'Model requirement mode', [['scalar', 'Same load requirement on applicable GPUs or primary RAM'], ['per_resource', 'Per-resource load requirements']], model.mode); mode.className = 'model-mode';
    const scalar = capacity(row, `${prefix}-load`, 'Model load requirement (MiB)', model.min_free_mib); scalar.className = 'model-load';
    const resident = capacity(row, `${prefix}-resident`, 'Resident requirement (MiB)', model.resident_min_free_mib); resident.className = 'model-resident';
    text(row, 'Resident credit applies only when the gate proves the loaded model can serve this request. A different context or stale observation needs the load budget.');
    const deeper = details(advanced, 'Per-resource model budgets'); deeper.classList.add('model-resource-settings');
    const rows = document.createElement('div'); rows.className = 'model-resource-rows'; deeper.append(rows);
    function addResource(value: { id: string; min_free_mib: string }): void {
      const item = document.createElement('div'); item.className = 'model-resource-row';
      const key = `${prefix}-resource-${crypto.randomUUID()}`;
      select(item, `${key}-id`, 'Budgeted resource', resourceChoices.map(resource => [resource.id, `${resource.id} (${resource.kind})`] as const), value.id).className = 'model-resource-id';
      capacity(item, `${key}-mib`, 'Resource load requirement (MiB)', value.min_free_mib).className = 'model-resource-mib';
      const remove = document.createElement('button'); remove.type = 'button'; remove.textContent = 'Remove resource budget'; remove.addEventListener('click', () => { item.remove(); changed(); }); item.append(remove); rows.append(item);
      annotateTuning(); refreshSearch();
    }
    for (const resource of model.resources) addResource(resource);
    const addButton = document.createElement('button'); addButton.type = 'button'; addButton.textContent = 'Add resource budget';
    addButton.addEventListener('click', () => { addResource({ id: upstream.resource, min_free_mib: '0' }); changed(); updateMode(); }); deeper.append(addButton);
    function updateMode(): void {
      scalar.disabled = mode.value !== 'scalar'; deeper.hidden = mode.value !== 'per_resource';
      for (const node of deeper.querySelectorAll<HTMLInputElement | HTMLSelectElement | HTMLButtonElement>('input, select, button')) node.disabled = mode.value !== 'per_resource';
    }
    mode.addEventListener('change', updateMode); updateMode();
    const remove = document.createElement('button'); remove.type = 'button'; remove.textContent = 'Remove model requirement'; remove.addEventListener('click', () => { row.remove(); changed(); }); row.append(remove);
    container.append(row); annotateTuning(); refreshSearch();
  }
  function changed(): void { parent.dispatchEvent(new Event('input', { bubbles: true })); refreshSearch(); }
  upstream.models.forEach(add);
  const button = document.createElement('button'); button.type = 'button'; button.textContent = 'Add model requirement';
  button.addEventListener('click', () => { add({ name: '', mode: 'scalar', min_free_mib: '0', resident_min_free_mib: '0', resources: [] }); changed(); container.lastElementChild?.querySelector<HTMLInputElement>('.model-name')?.focus(); }); parent.append(button);
}

export function renderTuning(snapshot: SettingsSnapshot): void {
  resourceChoices = snapshot.resources;
  const capability = snapshot.capabilities;
  document.getElementById('settings-capabilities')!.textContent = `${capability.auth_tokens_configured} access token(s) configured; ${capability.upstream_credentials_configured} upstream credential(s) configured. Loopback trust ${capability.trust_loopback ? 'enabled' : 'disabled'}; unauthenticated remote access ${capability.allow_unauthenticated_remote ? 'enabled' : 'disabled'}. Local administration: ${capability.local_admin_channel}; group access ${capability.local_admin_group_configured ? 'configured' : 'not configured'}. Admission storage ${capability.custom_admission_storage ? 'custom' : 'default'}; pressure cache ${capability.custom_pressure_cache ? 'custom' : 'default'}; local socket ${capability.custom_admin_socket ? 'custom' : 'default'}.`;
  control('presence').value = snapshot.owner.presence;
  ownerNumbers.forEach(([key]) => { control(`owner-${key}`).value = String(snapshot.owner[key as 'media_poll_ms']); });
  pressureNumbers.forEach(([key]) => { control(`pressure-${key}`).value = String(snapshot.owner.gpu_pressure[key as keyof typeof snapshot.owner.gpu_pressure]); });
  control('hardware-poll_interval_ms').value = String(snapshot.poll_interval_ms);
  control('server-listen').value = snapshot.server_listen;
  limits.forEach(([key]) => { control(`limits-${key}`).value = String(snapshot.server_limits[key as keyof typeof snapshot.server_limits] ?? ''); });
  const multipleGpus = snapshot.resources.filter(resource => resource.kind === 'nvidia-gpu').length > 1;
  for (const resource of snapshot.resources) {
    const row = [...document.querySelectorAll<HTMLElement>('.resource-default')].find(node => node.dataset.id === resource.id)!;
    // Inputs must sit outside the checkbox's label so each has one native label.
    const parent = document.createElement('div'); parent.className = 'resource-tuning'; parent.dataset.id = resource.id; row.after(parent);
    parent.setAttribute('role', 'group'); parent.setAttribute('aria-label', `${resource.id} (${resource.kind})`);
    if (resource.kind === 'system-memory' || resource.kind === 'system-cpu') {
      const reserve = field(parent, `${id(resource.id)}-reserve`, resource.kind === 'system-memory' ? 'Owner RAM reserve (bytes, blank uses 0)' : 'CPU utilization ceiling (%, blank uses 100)', resource.reserve ?? '', true); reserve.className = 'resource-reserve'; reserve.inputMode = 'numeric'; reserve.pattern = '(0|[1-9][0-9]*)';
    }
    if (resource.kind === 'nvidia-gpu') {
      const deeper = details(parent, 'Advanced GPU binding');
      const selector = field(deeper, `${id(resource.id)}-selector`, multipleGpus ? 'GPU selector (required for each GPU)' : 'GPU selector (blank uses automatic selection)', resource.selector ?? '', !multipleGpus); selector.className = 'resource-selector'; selector.maxLength = 256;
      const adapter = field(deeper, `${id(resource.id)}-adapter`, 'PCI adapter address (blank uses discovery)', resource.adapter ?? '', true); adapter.className = 'resource-adapter'; adapter.maxLength = 32;
      text(deeper, 'Changing hardware identity requires restart. Confirm the selected GPU and PCI adapter before saving.');
    }
    text(parent, 'Saved defaults and reserves take effect after restart. Hardware runtime switches remain separate.');
  }
  for (const upstream of snapshot.upstreams) {
    const row = [...document.querySelectorAll<HTMLElement>('.upstream-setting')].find(node => node.dataset.id === upstream.id)!;
    const prefix = id(upstream.id);
    capacity(row, `${prefix}-default`, 'Default free capacity (MiB)', upstream.default_min_free_mib).className = 'upstream-default-mib';
    text(row, 'Default and model budgets apply to subsequent admissions after a successful save. Existing active or held work keeps its lease.');
    const observation = text(row, 'Resident observation unavailable.'); observation.className = 'resident-observation';
    renderModels(row, upstream);
    const deeper = details(row, `Advanced service and topology for ${upstream.id}`);
    numberField(deeper, prefix, ['completion_timeout_s', 'Completion tracking timeout (seconds)', '0', '4294967295', '1'], String(upstream.completion_timeout_s)).className = 'upstream-completion-timeout';
    text(deeper, 'A completion timeout retains the reservation for operator recovery. Zero expires tracking immediately. It does not prove work stopped. These fields require restart.');
    const forwarding = select(deeper, `${prefix}-forward`, 'Forward borrower authorization', [['false', 'Strip borrower token'], ['true', 'Forward borrower token to upstream']], String(upstream.forward_authorization)); forwarding.className = 'upstream-forward-authorization';
    text(deeper, 'Forwarding exposes the borrower token to the upstream target. Enable only for a trusted target; stored upstream credentials cannot be combined with forwarding.');
    field(deeper, `${prefix}-listen`, 'Proxy listener (IP:port)', upstream.listen).className = 'upstream-listen';
    field(deeper, `${prefix}-target`, 'Upstream target URL', upstream.target).className = 'upstream-target';
    select(deeper, `${prefix}-primary`, 'Primary dependency', snapshot.resources.map(resource => [resource.id, `${resource.id} (${resource.kind})`] as const), upstream.resource).className = 'upstream-primary';
    field(deeper, `${prefix}-also`, 'Additional dependencies (comma separated)', upstream.also_requires.join(', '), true).className = 'upstream-also';
    text(deeper, 'Changing listeners, targets or dependencies requires restart and may disconnect the console or change which hardware and services borrowers can reach.');
  }
  annotateTuning(); updateCalibrationMode(); noteCapacityObservations(observed);
}

export function noteCapacityObservations(status: Status | undefined): void {
  observed = status;
  for (const row of document.querySelectorAll<HTMLElement>('.upstream-setting')) {
    const upstream = status?.upstreams.find(item => item.id === row.dataset.id);
    const node = row.querySelector<HTMLElement>('.resident-observation'); if (!node) continue;
    const quality = upstream?.model_observation?.quality;
    node.textContent = quality === 'ready' ? `Observed resident state: ${(upstream?.models ?? []).map(model => `${model.name ?? 'unnamed model'}: ${model.state}, ${model.resident_bytes == null ? 'size unavailable' : `${Math.floor(model.resident_bytes / 1048576)} MiB`}`).join('; ') || 'no resident models'}.` : `Resident observation ${quality ?? 'unavailable'}. Saved budgets remain separate from observed residency.`;
  }
}

export function collectTuning(request: SettingsUpdate, snapshot: SettingsSnapshot): SettingsUpdate {
  request.owner.presence = control('presence').value;
  request.owner.media_poll_ms = integer('owner-media_poll_ms');
  for (const [key, , , , step] of pressureNumbers) {
    const node = control(`pressure-${key}`);
    const value = node.disabled ? snapshot.owner.gpu_pressure[key as keyof typeof snapshot.owner.gpu_pressure] : step === '1' ? integer(`pressure-${key}`) : Number(node.value);
    if (typeof value !== 'number' || !Number.isFinite(value) || (!node.disabled && !node.checkValidity())) throw new Error(`${node.labels?.[0]?.textContent} needs a number in the displayed range.`);
    Object.assign(request.owner.gpu_pressure, { [key]: value });
  }
  const pressure = request.owner.gpu_pressure;
  if (pressure.floor_pct <= 0 || pressure.floor_pct >= 100) invalid(control('pressure-floor_pct'), 'Pressure floor must be greater than 0 and below 100 percent.');
  // Fixed mode does not use the floor or cap and cannot edit them here, so an
  // older file's combination is left as it is; the service runs it adjusted.
  const calibrated = (document.getElementById('setting-pressure-mode') as HTMLSelectElement).value !== 'fixed';
  if (calibrated && pressure.floor_pct > pressure.cap_pct) invalid(control('pressure-floor_pct'), 'Pressure floor must be at most the pressure cap.');
  if (calibrated && pressure.cap_pct > MAX_CAP_PCT) invalid(control('pressure-cap_pct'), `Pressure cap must be at most ${MAX_CAP_PCT} percent.`);
  if (pressure.learn_quantile <= 0 || pressure.learn_quantile >= 1) invalid(control('pressure-learn_quantile'), 'Learning quantile must be between 0 and 1, not either end.');
  if (pressure.learn_half_life_h <= 0) invalid(control('pressure-learn_half_life_h'), 'Learning half-life must be greater than 0 hours.');
  if (pressure.raise_pct_per_h <= 0) invalid(control('pressure-raise_pct_per_h'), 'Threshold rise limit must be greater than 0 percent per hour.');
  request.poll_interval_ms = integer('hardware-poll_interval_ms'); request.server_listen = control('server-listen').value;
  for (const [key] of limits) Object.assign(request.server_limits, { [key]: key === 'max_buffered_body_mib' && control(`limits-${key}`).value === '' ? null : integer(`limits-${key}`) });
  for (const resource of request.resources) {
    const row = [...document.querySelectorAll<HTMLElement>('.resource-tuning')].find(node => node.dataset.id === resource.id)!;
    const reserve = row.querySelector<HTMLInputElement>('.resource-reserve'); if (reserve) resource.reserve = decimal(reserve, true);
    if (reserve && resource.kind === 'system-cpu' && resource.reserve !== null && BigInt(resource.reserve!) > 100n) invalid(reserve, 'CPU utilization ceiling must be between 0 and 100 percent.');
    for (const key of ['selector', 'adapter'] as const) {
      const node = row.querySelector<HTMLInputElement>(`.resource-${key}`); if (node) resource[key] = node.value || null;
    }
  }
  for (const upstream of request.upstreams) {
    const row = [...document.querySelectorAll<HTMLElement>('.upstream-setting')].find(node => node.dataset.id === upstream.id)!;
    upstream.default_min_free_mib = decimal(row.querySelector<HTMLInputElement>('.upstream-default-mib')!)!;
    upstream.completion_timeout_s = integer(`${id(upstream.id)}-completion_timeout_s`);
    upstream.forward_authorization = row.querySelector<HTMLSelectElement>('.upstream-forward-authorization')!.value === 'true';
    upstream.listen = row.querySelector<HTMLInputElement>('.upstream-listen')!.value;
    upstream.target = row.querySelector<HTMLInputElement>('.upstream-target')!.value;
    upstream.resource = row.querySelector<HTMLSelectElement>('.upstream-primary')!.value;
    upstream.also_requires = row.querySelector<HTMLInputElement>('.upstream-also')!.value.split(',').map(value => value.trim()).filter(Boolean);
    const modelRows = [...row.querySelectorAll<HTMLElement>('.model-setting')];
    upstream.models = modelRows.map(model => {
      const mode = model.querySelector<HTMLSelectElement>('.model-mode')!.value;
      return { name: model.querySelector<HTMLInputElement>('.model-name')!.value, mode,
        min_free_mib: mode === 'scalar' ? decimal(model.querySelector<HTMLInputElement>('.model-load')!)! : '0',
        resident_min_free_mib: decimal(model.querySelector<HTMLInputElement>('.model-resident')!)!,
        resources: mode === 'scalar' ? [] : [...model.querySelectorAll<HTMLElement>('.model-resource-row')].map(resource => ({ id: resource.querySelector<HTMLSelectElement>('.model-resource-id')!.value, min_free_mib: decimal(resource.querySelector<HTMLInputElement>('.model-resource-mib')!)! })) };
    });
    const names = new Set<string>();
    for (const [index, model] of upstream.models.entries()) {
      const modelRow = modelRows[index];
      if (names.has(model.name)) invalid(modelRow.querySelector<HTMLInputElement>('.model-name')!, 'Model names must be unique within each workload.');
      names.add(model.name);
      const budgetRows = [...modelRow.querySelectorAll<HTMLElement>('.model-resource-row')];
      const ids = new Set<string>();
      if (model.mode === 'per_resource' && !model.resources.length) invalid(modelRow.querySelector<HTMLSelectElement>('.model-mode')!, 'Add at least one per-resource load budget.');
      for (const [resourceIndex, resource] of model.resources.entries()) {
        const resourceField = budgetRows[resourceIndex].querySelector<HTMLSelectElement>('.model-resource-id')!;
        if (ids.has(resource.id)) invalid(resourceField, 'Per-resource budgets must name each resource once.');
        ids.add(resource.id);
        if (resourceChoices.find(choice => choice.id === resource.id)?.kind === 'system-cpu') invalid(resourceField, 'MiB budgets require a memory or GPU resource. Set CPU utilization limits in Hardware.');
      }
      const loads = model.mode === 'scalar' ? [model.min_free_mib] : model.resources.map(resource => resource.min_free_mib);
      if (loads.some(load => BigInt(model.resident_min_free_mib) > BigInt(load))) invalid(modelRow.querySelector<HTMLInputElement>('.model-resident')!, 'Resident capacity must not exceed the load requirement.');
    }
  }
  return request;
}

export function restoreModelModes(saving: boolean): void {
  for (const row of document.querySelectorAll<HTMLElement>('.model-setting')) {
    const mode = row.querySelector<HTMLSelectElement>('.model-mode')!.value;
    row.querySelector<HTMLInputElement>('.model-load')!.disabled = saving || mode !== 'scalar';
    for (const node of row.querySelectorAll<HTMLInputElement | HTMLSelectElement | HTMLButtonElement>('.model-resource-settings input, .model-resource-settings select, .model-resource-settings button')) node.disabled = saving || mode !== 'per_resource';
  }
}

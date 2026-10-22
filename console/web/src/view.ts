type Quality = 'ready' | 'unknown' | 'stale';
interface Observation { quality: Quality; age_s: number | null; last_error: string | null }
interface AdapterMapping extends Observation { pci: string | null; luid: string | null; source: string | null }
interface Resource {
  id: string; kind: string; label: string | null; enabled: boolean; lendable: boolean;
  required: boolean; required_by: string[];
  free_mib: number | null; total_mib: number | null; utilization_pct: number | null;
  capacity: Observation;
  adapter_mapping: AdapterMapping | null;
  pressure: (Observation & { non_lender_pct: number | null; threshold_pct: number | null;
    target_pct?: number | null; learned_s?: number | null;
    mode: string; calibrated: boolean | null; calibration_samples: number | null;
    calibration_required: number; calibration_hint: string | null }) | null;
}
interface Work { model_state?: string; id: number; upstream: string; model: string | null; resources: string[]; age_s: number; completion: string; phase: string; tracking_id: string | null }
interface Model { name: string | null; state: string; resident_bytes: number | null }
interface Upstream { models?: Model[]; model_observation?: Observation; id: string; adapter: string; target: string; resource: string; vacancy: boolean;
  owner_scope?: string;
  health?: Observation & { http_status: number | null }; listener: Observation & { state: string };
  can_unload: boolean; refusal: { reason: string; detail: string } | null }
export interface Logging {
  level: string; source: string; configured_level: string; environment_override: boolean;
  debug_until?: string | null; debug_remaining_s?: number | null;
}
export interface Status {
  daemon: { name: string; version: string }; ready: boolean; readiness_failures: string[];
  lending_enabled: boolean; owner_veto: { active: boolean; cause: string | null; exempt_upstreams?: string[] };
  presence: { session: string; idle_seconds: number | null; foreground: string | null; basis?: string | null };
  agent_channel: { state: string; protocol_version: number; report_ttl_s: number };
  session_authority: { state: string; applicable_count: number; usable_count: number;
    sessions: { session_id: number; report_state: string; agent_version: string | null; age_s: number | null }[] };
  resources: Resource[]; upstreams: Upstream[]; uptime_s: number; work?: Work[];
  config_revision?: string | null;
  logging?: Logging | null;
}
export type Poll = { state: 'online'; status: Status; console_version: string }
  | { state: 'unavailable'; reason: string } | { state: 'incompatible'; schema_version: number };

function element<K extends keyof HTMLElementTagNameMap>(tag: K, text = '', className = ''): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  node.textContent = text;
  node.className = className;
  return node;
}
function at(id: string): HTMLElement { return document.getElementById(id)!; }
function vacancySign(vacancy: boolean): HTMLElement {
  const sign = element('span', '', 'vacancy-sign');
  sign.dataset.vacancy = String(vacancy);
  sign.setAttribute('role', 'img');
  sign.setAttribute('aria-label', vacancy ? 'Vacancy' : 'No vacancy');
  const lettering = element('span');
  lettering.setAttribute('aria-hidden', 'true');
  lettering.append(element('span', 'NO', 'sign-no'), document.createTextNode(' VACANCY'));
  sign.append(lettering);
  return sign;
}
function fact(label: string, value: string, detail = ''): HTMLElement {
  const item = element('div', '', 'fact');
  item.append(element('span', label, 'eyebrow'), element('strong', value));
  if (detail) item.append(element('span', detail, 'muted'));
  return item;
}
const readable = (s: string) => s.replaceAll('_', ' ');
const percent = (value: number | null) => value == null ? 'Unknown' : `${value.toFixed(0)}%`;
const memory = (mib: number | null) => mib == null ? 'Unknown' : `${(mib / 1024).toFixed(1)} GiB`;
const learnedSpan = (seconds: number) => seconds < 3600 ? `${Math.round(seconds / 60)} min` : `${Math.round(seconds / 3600)} h`;
const age = (seconds: number | null) => seconds == null ? 'No observation' : `${seconds}s old`;
const bounded = (value: string, limit = 180) => value.length <= limit ? value : `${value.slice(0, limit - 1)}…`;

function resourceName(r: Resource): string {
  return r.label && r.label !== r.id ? `${r.label} (${r.id})` : r.id;
}

// Reuse one DOM node per item id across polls instead of tearing every
// card down and rebuilding it. A screen-reader user resting on a card's
// control (for example "Disable sharing") previously had that button
// replaced by a lookalike new element every 2s poll, which assistive
// technology announces as a real focus change even though nothing the
// owner cares about moved. `build` receives the existing node (if any) so
// it can patch content in place and leave an unrelated focused control
// untouched.
function syncCards<T extends { id: string }>(
  container: HTMLElement,
  items: T[],
  build: (item: T, existing?: HTMLElement) => HTMLElement,
): void {
  const existingByKey = new Map<string, HTMLElement>();
  for (const child of [...container.children] as HTMLElement[]) {
    // A placeholder ("No current workload state.") has no key and must not
    // stay beside the cards that replace it.
    if (child.dataset.key) existingByKey.set(child.dataset.key, child);
    else child.remove();
  }
  let anchor: HTMLElement | null = null;
  for (const item of items) {
    const previous = existingByKey.get(item.id);
    existingByKey.delete(item.id);
    const node = build(item, previous);
    node.dataset.key = item.id;
    if (anchor) {
      if (node.previousElementSibling !== anchor) anchor.after(node);
    } else if (container.firstElementChild !== node) {
      container.prepend(node);
    }
    anchor = node;
  }
  for (const stale of existingByKey.values()) stale.remove();
}

function observationError(label: string, observation: Observation | null | undefined): HTMLElement | null {
  return observation?.quality !== 'ready' && observation?.last_error
    ? element('p', `${label}: ${bounded(observation.last_error)}`, 'hint')
    : null;
}

function resourceCard(r: Resource, existing?: HTMLElement): HTMLElement {
  const card = existing ?? element('article', '', 'resource card');
  // The control button (when this resource has one) keeps its DOM node
  // across renders; everything else is rebuilt fresh each poll into `parts`
  // and inserted before it, so a focused button is never removed from the
  // document and never re-announced to a screen reader.
  const control = card.querySelector<HTMLButtonElement>('.resource-control') ?? undefined;
  for (const child of [...card.children]) if (child !== control) child.remove();
  const parts: HTMLElement[] = [];
  const known = r.capacity.quality === 'ready';
  const pressureKnown = r.pressure?.quality === 'ready';
  const tone = !r.required ? 'withheld' : !known || (r.pressure && !pressureKnown) ? 'fault' : r.enabled && r.lendable ? 'lending' : 'withheld';
  card.dataset.tone = tone;
  card.setAttribute('aria-label', `${resourceName(r)} resource`);
  const top = element('div', '', 'card-heading');
  const badge = !r.required ? 'NOT USED' : !known ? r.capacity.quality.toUpperCase() : !r.enabled ? 'DISABLED' : r.lendable ? 'CAPACITY READY' : 'WITHHELD';
  top.append(element('span', r.kind === 'system-memory' ? 'MEMORY' : r.kind === 'system-cpu' ? 'CPU' : r.kind === 'nvidia-gpu' ? 'GPU' : readable(r.kind).toUpperCase(), 'eyebrow'), element('span', badge, 'badge'));
  parts.push(top, element('h3', resourceName(r)));
  const mapping = r.adapter_mapping;
  const identity = [`Resource ${r.id}`];
  if (mapping?.quality === 'ready' && mapping.pci) identity.push(`PCI ${mapping.pci}`);
  if (mapping?.quality === 'ready' && mapping.source) identity.push(readable(mapping.source));
  parts.push(element('p', identity.join(' · '), 'metric-note'));
  parts.push(element('p', r.required
    ? `Used by ${r.required_by.length ? r.required_by.join(', ') : 'a configured borrowing route'}`
    : 'No borrowing route uses this resource.', 'metric-note'));
  const cpu = r.kind === 'system-cpu';
  parts.push(element('p', known ? cpu ? percent(r.utilization_pct) : memory(r.free_mib) : 'Unknown', 'measurement'));
  parts.push(element('p', cpu ? 'Current utilization' : `Available${known ? ' of ' + memory(r.total_mib) : ''}`, 'muted'));
  if (known && r.utilization_pct != null) {
    const utilizationLabel = r.kind === 'nvidia-gpu' ? 'GPU UTILIZATION' : r.kind === 'system-cpu' ? 'CPU UTILIZATION' : 'MEMORY UTILIZATION';
    const meter = element('progress');
    meter.max = 100; meter.value = Math.min(100, Math.max(0, r.utilization_pct));
    meter.setAttribute('aria-label', `${resourceName(r)} ${readable(utilizationLabel.toLowerCase())}`);
    parts.push(element('p', utilizationLabel, 'eyebrow'), meter, element('p', `${percent(r.utilization_pct)} utilized · ${age(r.capacity.age_s)}`, 'metric-note'));
  } else if (known && r.kind === 'nvidia-gpu') parts.push(element('p', `GPU utilization unavailable · ${age(r.capacity.age_s)}`, 'metric-note'));
  else parts.push(element('p', `${readable(r.capacity.quality)} · ${age(r.capacity.age_s)}`, 'metric-note'));
  if (r.pressure) {
    const p = r.pressure;
    parts.push(element('div', '', 'divider'), fact('OWNER PRESSURE', pressureKnown ? percent(p.non_lender_pct) : 'Unknown', pressureKnown ? `${percent(p.threshold_pct)} threshold · ${readable(p.mode)}` : `${p.quality} observation`));
    if (pressureKnown && p.calibrated === false) parts.push(element('p', `Calibrating: ${p.calibration_samples ?? 0}/${p.calibration_required} samples. ${p.calibration_hint ?? ''}`, 'hint'));
    else if (pressureKnown && p.threshold_pct != null && p.target_pct != null && p.target_pct > p.threshold_pct) {
      // Whole percentages can round a nearly finished rise to "40%, rising toward 40%".
      const [threshold, target] = [p.threshold_pct, p.target_pct];
      const shown = (value: number) => percent(threshold) === percent(target) ? `${value.toFixed(1)}%` : percent(value);
      parts.push(element('p', `Threshold ${shown(threshold)}, rising toward ${shown(target)} (learned from ${learnedSpan(p.learned_s ?? 0)}).`, 'hint'));
    }
    if (pressureKnown && p.calibrated === true && p.calibration_hint) parts.push(element('p', p.calibration_hint, 'hint'));
  }
  for (const error of [
    observationError('Capacity', r.capacity),
    observationError('Adapter mapping', mapping),
    observationError('Owner pressure', r.pressure),
  ]) if (error) parts.push(error);
  if (!r.required) {
    control?.remove();
    parts.push(element('p', 'Sharing controls are unavailable because this resource is outside every borrowing route.', 'hint'));
    card.append(...parts);
    return card;
  }
  const button = control ?? document.createElement('button');
  button.className = 'resource-control';
  button.textContent = r.enabled ? 'Disable sharing' : 'Enable sharing';
  button.dataset.resource = r.id;
  button.dataset.enabled = String(!r.enabled);
  button.setAttribute('aria-label', `${r.enabled ? 'Disable' : 'Enable'} sharing: ${resourceName(r)}`);
  if (control) {
    control.before(...parts);
  } else {
    card.append(...parts, button);
  }
  return card;
}

// An upstream with owner_scope = "resource" is exempt from the owner veto:
// it keeps lending while the owner is present, and only its own resources
// refuse or reclaim it. Any other value, or none, is host-scoped.
const vetoExempt = (u: Upstream) => u.owner_scope === 'resource';
const names = (ids: string[]) => ids.length <= 1 ? ids.join('') : `${ids.slice(0, -1).join(', ')} and ${ids[ids.length - 1]}`;
// Serving means admitting new work or still holding accepted work.
function exemptServing(s: Status): Upstream[] {
  return s.upstreams.filter(u => vetoExempt(u) && (u.vacancy || (s.work ?? []).some(w => w.upstream === u.id)));
}

// Why an exempt route keeps lending, and what can still stop it. GPU
// pressure is the only measurement that reclaims accepted work; a
// dependency without one is limited only by its free capacity.
function exemptionFact(u: Upstream, resources: Resource[]): HTMLElement {
  const dependencies = resources.filter(r => r.required_by.includes(u.id));
  const watched = dependencies.filter(r => r.pressure).map(r => r.id);
  const unwatched = dependencies.filter(r => !r.pressure).map(r => r.id);
  const parts = ['Keeps lending while you use this machine (owner_scope = "resource").'];
  if (watched.length) parts.push(`Owner pressure on ${names(watched)} still refuses and reclaims it.`);
  if (unwatched.length) parts.push(`Nothing measures your use of ${names(unwatched)}, so only free capacity limits it there.`);
  return fact('OWNER PRESENCE', 'Exempt', parts.join(' '));
}

function listenerFact(listener: Upstream['listener']): HTMLElement {
  if (listener.quality === 'ready' && listener.state === 'loopback_only') {
    return fact('DIRECT LISTENER', 'Loopback only', 'Only loopback listeners were observed.');
  }
  if (listener.quality === 'ready' && listener.state === 'listens_on_network') {
    return fact('DIRECT LISTENER', 'Listens on network', 'Direct requests to this target bypass owner protection.');
  }
  if (listener.quality === 'ready' && listener.state === 'not_observable') {
    return fact('DIRECT LISTENER', 'Not observable here', 'The target address cannot be matched to a local listener here. Check its bind where the upstream runs.');
  }
  const error = listener.last_error ? ` ${bounded(listener.last_error)}` : '';
  return fact('DIRECT LISTENER', 'Unknown', `Confirm the upstream bind before relying on owner protection.${error}`);
}

function upstreamCard(u: Upstream, work: Work[], resources: Resource[], existing?: HTMLElement): HTMLElement {
  const card = existing ?? element('article', '', 'upstream card');
  card.replaceChildren();
  card.dataset.tone = u.vacancy ? 'lending' : 'withheld';
  const title = element('div', '', 'card-heading');
  const badge = element('span', '', 'badge');
  badge.append(vacancySign(u.vacancy));
  title.append(element('h3', u.id), badge);
  card.append(title, element('p', `${u.adapter} · ${u.resource}`, 'muted'), element('p', u.target, 'endpoint'));
  const healthReady = u.health?.quality === 'ready' && u.health.http_status != null
    && u.health.http_status >= 200 && u.health.http_status < 300;
  const admission = u.vacancy
    ? `The default budget fits. ${healthReady ? 'The requested model may need more capacity.' : 'Target health is unconfirmed; requests may fail.'}`
    : u.refusal?.detail ?? 'Borrowing withheld.';
  card.append(element('p', admission, 'route-decision'));
  if (vetoExempt(u)) card.append(exemptionFact(u, resources));
  card.append(listenerFact(u.listener));
  card.append(element('p', u.can_unload ? 'Adapter supports model unloading' : 'Adapter cannot unload models', 'metric-note'));
  for (const entry of work.filter(w => w.upstream === u.id)) {
    card.append(fact(`WORK ${entry.id}`, readable(entry.phase), `${entry.model ?? 'Model unknown'} · ${readable(entry.model_state || 'unknown')} · held for ${entry.age_s}s · ${entry.resources.join(', ')}`));
    card.append(element('p', entry.phase === 'held' ? 'Completion is unconfirmed. Stop the job upstream before an administrator releases this hold.' : entry.completion === 'tracked' ? 'Reservation stays until upstream completion is confirmed.' : 'Reservation stays until the upstream response finishes.', 'hint'));
  }
  if (u.model_observation?.quality === 'ready') {
    for (const model of u.models ?? []) card.append(fact(model.name ?? 'UPSTREAM WORK', readable(model.state), model.resident_bytes == null ? 'Model memory unavailable' : `${memory(model.resident_bytes / (1024 * 1024))} resident`));
  } else card.append(element('p', `Model lifecycle ${u.model_observation?.quality ?? 'unknown'}`, 'metric-note'));
  const health = u.health;
  const code = health?.quality === 'ready' ? health.http_status : null;
  const detail = code !== null && code !== undefined
    ? (code >= 200 && code < 300 ? `Target responded · HTTP ${code}`
      : code === 401 || code === 403 ? `Target requires authentication · HTTP ${code}`
      : `Target check needs attention · HTTP ${code}`)
    : health?.last_error ?? 'Target health unavailable; waiting for a fresh check.';
  card.append(element('p', detail, 'metric-note'));
  if (health?.age_s !== null && health?.age_s !== undefined) card.append(element('p', `Target check ${health.age_s}s ago · ${health.quality}`, 'metric-note'));
  return card;
}

// Announce a decision change to the polite live region only when the tone
// or title actually changed, not on every 2s poll. Gate detail carries live
// numbers ("12s old", calibration counts), so announcing on every detail
// change re-read the whole decision to a screen-reader user each poll even
// though nothing the owner needed to hear had changed. The visible detail
// text still updates every time; only the announcement is debounced.
let lastAnnounced: { tone: string; title: string } | undefined;
function decision(tone: string, title: string, detail: string, label: string) {
  at('decision').dataset.tone = tone;
  at('decision-title').textContent = title;
  at('decision-detail').textContent = detail;
  const hasVacancyState = label === 'VACANCY' || label === 'NO VACANCY';
  at('decision-label').classList.toggle('vacancy-pill', hasVacancyState);
  at('decision-label').replaceChildren(hasVacancyState ? vacancySign(label === 'VACANCY') : document.createTextNode(label));
  if (lastAnnounced?.tone === tone && lastAnnounced?.title === title) return;
  lastAnnounced = { tone, title };
  at('announcement').textContent = `${title}. ${detail}`;
}

// With several borrowing routes withheld for different reasons, showing
// only the first route's detail could name a reason that is not why the
// others are withheld. Name the shared reason when every route agrees, and
// point to Workloads for the per-route detail otherwise.
function withheldDetail(upstreams: Upstream[]): string {
  if (!upstreams.length) return 'No borrowing routes are configured.';
  const reasons = new Set(upstreams.map(u => u.refusal?.reason));
  if (reasons.size === 1) return upstreams[0].refusal?.detail ?? 'Borrowing is withheld.';
  return `${upstreams.length} routes are withheld for different reasons. See Workloads.`;
}

// An unknown session is a backend without a trustworthy answer yet (not yet
// confirmed, unreadable, inconsistent); its basis says which. Otherwise the
// idle reading is the useful detail.
function presenceDetail(p: Status['presence']): string {
  if (p.session === 'unknown' && p.basis) return bounded(p.basis);
  return p.idle_seconds == null ? 'Idle time unavailable' : `Idle for ${p.idle_seconds}s`;
}

export function render(poll: Poll): void {
  // The sign is dark only while the service gives no readable answer. Owner
  // presence and refusals keep it lit and switch on NO (docs/brand.md).
  const sign = poll.state === 'online' ? 'assets/mark-lit.svg' : 'assets/mark-idle.svg';
  if (at('sign').getAttribute('src') !== sign) at('sign').setAttribute('src', sign);
  at('freshness').textContent = poll.state === 'online' ? 'Updated just now · refreshes every 2s' : 'No current observations';
  if (poll.state !== 'online') {
    const detail = poll.state === 'incompatible' ? `Service schema ${poll.schema_version} is incompatible. Install matching service and console versions.` : poll.reason;
    decision('fault', 'Status unavailable', detail, 'UNKNOWN');
    at('protection').replaceChildren(fact('BORROWING', 'Unknown'), fact('OWNER PRESENCE', 'Unknown'), fact('LENDING SWITCH', 'Unknown'));
    at('veto-exemptions').hidden = true;
    for (const id of ['resources', 'upstreams', 'health']) at(id).replaceChildren(element('p', 'No current information. Previous observations have been cleared.', 'empty'));
    return;
  }
  const s = poll.status;
  const lending = s.upstreams.filter(u => u.vacancy).map(u => u.id);
  const exempt = s.upstreams.filter(vetoExempt).map(u => u.id);
  const serving = exemptServing(s);
  const badEvidence = !s.ready && s.resources.some(r => r.required && r.enabled && (r.capacity.quality !== 'ready' || (r.pressure && r.pressure.quality !== 'ready')));
  const calibrationOnly = !s.ready && s.readiness_failures.length > 0 && s.readiness_failures.every(failure => s.resources.some(r => r.required && r.pressure?.quality === 'ready' && r.pressure.calibrated === false && failure === `resource '${r.id}' pressure is not ready`));
  if (calibrationOnly) decision('withheld', 'Preparing safe lending', 'Pressure calibration is in progress. Borrowing waits for a trustworthy baseline.', 'CALIBRATING');
  else if (badEvidence) decision('fault', 'Waiting for trustworthy readings', 'Stale or unknown observations withhold the resources they protect.', 'NOT READY');
  else if (!s.ready) decision('fault', 'Service needs attention', s.readiness_failures.map(readable).join(' · '), 'NOT READY');
  else if (s.owner_veto.active && serving.length) {
    // The owner veto pauses host-scoped routes only. Saying the machine is
    // simply "yours" while an exempt route admits or runs borrowed work
    // would hide that route from the owner.
    const admitting = serving.some(u => u.vacancy);
    const paused = s.upstreams.length - exempt.length;
    decision(admitting ? 'lending' : 'withheld', 'Only exempt routes are lending',
      `Owner policy pauses ${paused} ${paused === 1 ? 'route' : 'routes'}${s.owner_veto.cause ? ` (${readable(s.owner_veto.cause)})` : ''}. ${names(serving.map(u => u.id))} ${serving.length === 1 ? 'is' : 'are'} exempt and ${serving.length === 1 ? 'keeps' : 'keep'} lending on ${serving.length === 1 ? 'its' : 'their'} own hardware. See Workloads.`,
      admitting ? 'VACANCY' : 'NO VACANCY');
  }
  else if (s.owner_veto.active) decision('withheld', 'Owner policy pauses new work', exempt.length
    ? `${s.owner_veto.cause ? `${readable(s.owner_veto.cause)}. ` : ''}${names(exempt)} ${exempt.length === 1 ? 'is' : 'are'} exempt from owner presence but not lending now.`
    : s.owner_veto.cause ? readable(s.owner_veto.cause) : 'Owner policy refuses new borrowed work.', 'NO VACANCY');
  else if (!s.lending_enabled) decision('withheld', 'Lending is paused', 'The lending switch is off. New borrowed work is refused.', 'NO VACANCY');
  else if (lending.length) decision('lending', 'Room for borrowed work', `The default budget fits for ${names(lending)} (${lending.length} of ${s.upstreams.length} routes). Model requirements or target health can still prevent work. See Workloads. ${exempt.length ? `Owner protection covers every route except ${names(exempt)}.` : 'Owner protection remains active.'}`, 'VACANCY');
  else decision('withheld', 'Holding spare capacity', withheldDetail(s.upstreams), 'NO VACANCY');
  const exemptions = at('veto-exemptions');
  exemptions.hidden = !exempt.length;
  exemptions.textContent = exempt.length
    ? `Owner presence never pauses ${names(exempt)}. ${exempt.length === 1 ? 'It is' : 'They are'} set to owner_scope = "resource" and ${exempt.length === 1 ? 'yields' : 'yield'} only to ${exempt.length === 1 ? 'its' : 'their'} own hardware. See Workloads.`
    : '';
  at('protection').replaceChildren(
    fact('OWNER PRESENCE', readable(s.presence.session), presenceDetail(s.presence)),
    fact('LENDING SWITCH', s.lending_enabled ? 'Enabled' : 'Paused', 'The service controls admission'),
    fact('SESSION EVIDENCE', `${s.session_authority.usable_count} / ${s.session_authority.applicable_count}`, readable(s.session_authority.state)),
  );
  if (s.resources.length) syncCards(at('resources'), s.resources, resourceCard);
  else at('resources').replaceChildren(element('p', 'No resources configured.', 'empty'));
  if (s.upstreams.length) syncCards(at('upstreams'), s.upstreams, (u, existing) => upstreamCard(u, s.work ?? [], s.resources, existing));
  else at('upstreams').replaceChildren(element('p', 'No borrowing routes configured.', 'empty'));
  const health = at('health');
  health.replaceChildren(fact('SERVICE', s.ready ? 'Ready' : 'Not ready', `vacancyd ${s.daemon.version}`), fact('CONSOLE', poll.console_version, poll.console_version === s.daemon.version ? 'Version matches service' : 'Version differs from service'), fact('REPORTER CHANNEL', readable(s.agent_channel.state), `Protocol ${s.agent_channel.protocol_version} · reports expire after ${s.agent_channel.report_ttl_s}s`), fact('UPTIME', `${Math.floor(s.uptime_s / 60)} min`, 'Since service start'));
  for (const session of s.session_authority.sessions) health.append(fact(`SESSION ${session.session_id}`, readable(session.report_state), `${session.agent_version ?? 'Version unknown'} · ${age(session.age_s)}`));
}

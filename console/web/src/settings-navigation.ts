// Navigation changes visibility, never the settings values or save document.
const categories = [
  ['owner', 'Owner protection'],
  ['hardware', 'Hardware'],
  ['pressure', 'GPU pressure'],
  ['workloads', 'Workloads'],
  ['service', 'Service'],
] as const;

const configKeys: Record<string, string> = {
  'setting-unlocked-presence': 'owner.unlocked_session_is_presence',
  'setting-idle-seconds': 'owner.idle_lends_after_s',
  'setting-media-presence': 'owner.media_is_presence',
  'setting-release-seconds': 'owner.release_after_s',
  'setting-processes': 'owner.processes', 'setting-media-ignore': 'owner.media_ignore',
  'setting-sign-in-presence': 'owner.sign_in_screen_is_presence',
  'setting-pressure-enabled': 'owner.gpu_pressure.enabled',
  'setting-pressure-mode': 'owner.gpu_pressure.mode',
  'setting-pressure-threshold': 'owner.gpu_pressure.threshold_pct',
  'setting-pressure-sustain': 'owner.gpu_pressure.sustain_s',
  'setting-pressure-ignore': 'owner.gpu_pressure.ignore',
};

export function settingsNavigation(form: HTMLFormElement): () => void {
  const tabs = form.querySelector<HTMLElement>('#settings-tabs')!;
  const search = form.querySelector<HTMLInputElement>('#settings-search')!;
  const results = form.querySelector<HTMLUListElement>('#settings-search-results')!;
  const status = form.querySelector<HTMLElement>('#settings-search-status')!;
  const cards = [...form.querySelectorAll<HTMLFieldSetElement>('fieldset[data-settings-category]')];
  const advanced = new Set(cards.filter(card => card.closest('details') && card.dataset.settingsLevel !== 'standard'));
  const oldAdvanced = form.querySelector('#settings-advanced')!;
  const oldGrid = form.querySelector('.settings-grid')!;
  const panels = new Map<string, HTMLElement>();
  const buttons = new Map<string, HTMLButtonElement>();

  function select(id: string, focus = false): void {
    for (const [key, panel] of panels) {
      panel.hidden = key !== id;
      const button = buttons.get(key)!;
      button.setAttribute('aria-selected', String(key === id));
      button.tabIndex = key === id ? 0 : -1;
    }
    if (focus) buttons.get(id)!.focus();
  }

  for (const [id, name] of categories) {
    const button = document.createElement('button');
    button.type = 'button';
    button.id = `settings-tab-${id}`;
    button.textContent = name;
    button.setAttribute('role', 'tab');
    button.setAttribute('aria-controls', `settings-panel-${id}`);
    button.addEventListener('click', () => select(id));
    tabs.append(button);
    buttons.set(id, button);

    const panel = document.createElement('div');
    panel.id = `settings-panel-${id}`;
    panel.setAttribute('role', 'tabpanel');
    panel.setAttribute('aria-labelledby', button.id);
    panel.tabIndex = 0;
    panels.set(id, panel);
    const common = document.createElement('div');
    common.className = 'settings-grid';
    const details = document.createElement('details');
    details.className = 'advanced-settings';
    const summary = document.createElement('summary');
    summary.textContent = `Advanced ${name.toLowerCase()} settings`;
    details.append(summary);
    const deeper = document.createElement('div');
    deeper.className = 'settings-grid';
    details.append(deeper);
    for (const card of cards.filter(card => card.dataset.settingsCategory === id)) {
      (advanced.has(card) ? deeper : common).append(card);
    }
    panel.append(common);
    if (deeper.childElementCount) panel.append(details);
    form.insertBefore(panel, form.querySelector('.settings-actions'));
  }
  oldGrid.remove();
  oldAdvanced.remove();
  select('owner');

  tabs.addEventListener('keydown', event => {
    const current = [...buttons.values()].indexOf(event.target as HTMLButtonElement);
    if (current < 0) return;
    let next: number;
    if (event.key === 'ArrowRight') next = (current + 1) % categories.length;
    else if (event.key === 'ArrowLeft') next = (current + categories.length - 1) % categories.length;
    else if (event.key === 'Home') next = 0;
    else if (event.key === 'End') next = categories.length - 1;
    else return;
    event.preventDefault();
    select(categories[next][0], true);
  });

  function reveal(control: HTMLElement): void {
    const card = control.closest<HTMLElement>('[data-settings-category]');
    if (!card) return;
    select(card.dataset.settingsCategory!);
    for (let ancestor = control.parentElement; ancestor; ancestor = ancestor.parentElement) {
      if (ancestor instanceof HTMLDetailsElement) ancestor.open = true;
      if (ancestor === form) break;
    }
  }

  // Native validation must be able to focus a field in an inactive tab or
  // closed disclosure. Do this during capture, before the browser focuses it.
  form.addEventListener('invalid', event => {
    const first = [...form.elements].find(control =>
      (control instanceof HTMLInputElement || control instanceof HTMLSelectElement || control instanceof HTMLTextAreaElement)
      && control.willValidate && !control.validity.valid);
    if (event.target === first && first instanceof HTMLElement) reveal(first);
    // Only the first error gets native focus. Other invalid controls may
    // be in hidden panels, but must not switch away from the first error.
    else event.preventDefault();
  }, true);

  function refresh(): void {
    results.replaceChildren();
    const words = search.value.trim().toLocaleLowerCase().split(/\s+/).filter(Boolean);
    results.hidden = words.length === 0;
    if (!words.length) { status.textContent = ''; return; }
    let count = 0;
    for (const card of cards) {
      const category = categories.find(([id]) => id === card.dataset.settingsCategory)![1];
      if (card.dataset.settingsSearch && words.every(word => `${card.textContent} ${card.dataset.settingsSearch} ${category}`.toLocaleLowerCase().includes(word))) {
        const item = document.createElement('li');
        const result = document.createElement('button'); result.type = 'button';
        result.textContent = `${card.querySelector('legend')!.textContent} · ${category} · Status`;
        result.addEventListener('click', () => { reveal(card); card.tabIndex = -1; card.focus(); card.scrollIntoView({ block: 'nearest' }); });
        item.append(result); results.append(item); count += 1;
      }
      for (const control of card.querySelectorAll<HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement>('input, select, textarea')) {
        const label = [...(control.labels ?? [])].map(item => item.textContent?.trim() ?? '').join(' ');
        if (!label) continue;
        const group = control.closest('[role="group"]')?.getAttribute('aria-label') ?? '';
        const descriptions = (control.getAttribute('aria-describedby') ?? '').split(/\s+/).map(id => document.getElementById(id)?.textContent ?? '').join(' ');
        const haystack = `${label} ${group} ${category} ${control.id.replaceAll('-', ' ')} ${control.dataset.configKey ?? configKeys[control.id] ?? (control.closest('.resource-default') ? 'resource.enabled' : '')} ${descriptions}`.toLocaleLowerCase();
        if (!words.every(word => haystack.includes(word))) continue;
        const item = document.createElement('li');
        const result = document.createElement('button');
        result.type = 'button';
        result.textContent = `${label}${group ? ` (${group})` : ''} · ${category}${control.closest('details') ? ' · Advanced' : ''}`;
        result.addEventListener('click', () => {
          reveal(control);
          // A conditional setting is still discoverable. Do not enable it
          // just to focus it or silently leave focus in the results list.
          const target = control.disabled ? card : control;
          if (control.disabled) {
            card.tabIndex = -1;
            status.textContent = `${label} is currently unavailable. Review the related options in this group.`;
          }
          target.focus();
          target.scrollIntoView({ block: 'nearest' });
        });
        item.append(result);
        results.append(item);
        count += 1;
      }
    }
    status.textContent = count ? `${count} matching ${count === 1 ? 'setting' : 'settings'}.` : 'No matching settings. Clear or change your search.';
  }
  search.addEventListener('input', refresh);
  search.addEventListener('keydown', event => {
    if (event.key === 'Enter') {
      // Searching must never submit an administrator settings change.
      event.preventDefault();
      results.querySelector<HTMLButtonElement>('button')?.focus();
    } else if (event.key === 'Escape') {
      event.preventDefault();
      search.value = '';
      refresh();
    }
  });
  return refresh;
}

# Brand

The canonical source for vacancyd's visual identity. Tokens defined here are the only ones permitted anywhere in the project, including the docs site, the tray app, the README and release artwork. If something needs a colour that is not here, that is a brand decision, not a styling one.

Status: **console visual direction settled**. Its scope is dark surfaces, amber vacancy, slate owner vetoes, Archivo/IBM Plex fonts, hardware sharing controls and workload status cards. Native accessibility and resource-use qualification remain separate. Other applications of this identity remain draft.

---

## The idea

The name is doing the work already, so the identity should follow it rather than invent something alongside it.

**vacancyd is a vacancy sign.** VACANCY means there's room for a new temporary resident: a borrowed workload. NO VACANCY means the machine isn't accepting that work under the current rules. The sign matches the interface: `/status` reports `vacancy: true|false` per upstream, and a refusal returns the error code `no_vacancy`. Use it to show the lending state.

The sign stays lit. **VACANCY** is always illuminated; only **NO** switches on when borrowing is withheld. When borrowing is available, the unlit NO lettering remains in place. The sign's shape and position stay stable.

As on the motel sign the product is named after, **the lettering is the light source**. The plate, its frame and the bar it hangs from are unlit metal. Every asset and every screen follows that: amber marks something lit, so a lit plate behind dark letters, or an amber control that cannot act, reads as the sign wired backwards.

### The principle that governs everything else

> **A refusal is not a failure.**

A `503 no_vacancy` refusal means admission is withheld by policy or a required resource check. It can be a normal result when the owner returns, rather than a service outage. Accepted work still follows its cancel, drain or finish policy; the sign does not promise that computation has stopped or memory has been recovered.

The identity has to carry that, because the obvious visual choice would betray it. Green-for-available and red-for-busy is the reflex, and it is wrong here: it codes the correct behaviour as an alarm and trains people to treat the daemon working properly as something to investigate.

Instead:

| State | Reads as | Colour |
| ----- | -------- | ------ |
| Lending | VACANCY lit, NO unlit: come in | amber sign and status |
| Withheld | NO VACANCY lit: the room is occupied, all is well | amber sign, slate status |
| Fault | something is actually broken | ember, and only then |
| No answer | the sign is dark: nothing is answering | unlit sign, ember status |

Amber and slate carry every normal state. Ember is held in reserve and is deliberately *outside* the core palette, so that reaching for it is a decision rather than a habit.

The sign goes dark only when there is no answer at all: the owner console shows the switched-off mark until the service returns a readable status. The owner returning never darkens it; that is the sign working.

---

## Colour

The core palette has six tokens.

| Token | Hex | Role |
| ----- | --- | ---- |
| `--vac-night-950` | `#0B0F17` | Deepest ground. The night the sign hangs in. |
| `--vac-night-900` | `#151B27` | Raised surface on dark: cards, code blocks, nav. |
| `--vac-amber-400` | `#FFB454` | The lit sign. **Dark grounds only.** |
| `--vac-amber-700` | `#A85F06` | The lit sign on light grounds. Text-safe. |
| `--vac-slate-400` | `#94A3B8` | Withheld, muted, secondary. Dark grounds. |
| `--vac-slate-600` | `#55637A` | Withheld, muted, secondary. Light grounds. |

Reserved, outside the six:

| Token | Hex | Role |
| ----- | --- | ---- |
| `--vac-ember-500` | `#E2603F` | Genuine faults, dark grounds. |
| `--vac-ember-700` | `#A83C22` | Genuine faults, light grounds. |

Derived tints, one job each. They are computed from the tokens above, not chosen alongside them, and nothing else may use them:

| Name | Hex | Derivation | Job |
| ---- | --- | ---------- | --- |
| `--amber-unlit` | `#5B4934` | `amber-400` at 30% over `night-900` | The unlit NO: the same tube switched off. 2.01:1 by design; the sign's text label carries the state. |
| `--input-border` | `#6B7A94` | between `slate-600` and `slate-400` | Form-control edges. `slate-600` is 2.83:1 on `night-900`, short of WCAG 1.4.11; this is 3.97:1. |

### Measured contrast

Calculated WCAG 2.1 contrast ratios:

| Pair | Ratio | Verdict |
| ---- | ----- | ------- |
| `amber-400` on `night-950` | **10.88** | Body text, UI, anything |
| `amber-400` on white | 1.76 | **Never.** Not even large text |
| `amber-700` on white | **4.88** | Body text and links (AA) |
| `amber-700` on `night-950` | 3.93 | Large text and graphics only |
| `slate-400` on `night-950` | **7.48** | Secondary text on dark |
| `slate-600` on white | **6.08** | Secondary text on light |
| `ember-700` on white | **6.30** | Fault text on light |
| `ember-500` on `night-950` | **5.46** | Fault text on dark |
| white on `night-950` | 19.18 | Primary text on dark |
| `slate-600` on `night-950` | 3.15 | Sign frame and rail on dark (non-text) |
| `slate-600` on `night-900` | 2.83 | Decorative dividers only |

The single rule most likely to be broken: **`amber-400` is a dark-ground colour.** It is the glow of a lit sign, and a lit sign on a white page is just orange text at 1.76:1. Use `amber-700` there.

---

## Type

Two families, both SIL OFL 1.1 and both bundled locally. Use the bundled fonts rather than a third-party font CDN.

| Role | Family | Weights |
| ---- | ------ | ------- |
| Wordmark, headings, UI | **Archivo** | 400, 600, 700 |
| Code, config, terminal output | **IBM Plex Mono** | 400, 600 |

Archivo is a grotesque with roots in signage and high-legibility print, which is the right ancestry for a project named after a sign. Its use in the console is checked with the actual text and window sizes; the font choice is not a comparative legibility claim.

The wordmark is set in **Archivo 700, tracked tight (-0.02em), all lowercase**: `vacancyd`. Lowercase because it is a daemon name as typed, and the trailing `d` should read as the Unix convention rather than as a stylised flourish. Never title-case it, never `VacancyD`.

---

## The mark

A dark sign hanging from a bar, with a lit **V**.

It is drawn the way the real sign is built: the V is the light source, and the plate, its frame and the bar are unlit metal. The bar is wider than the plate and the sign hangs from its ends, so it reads as a hanging sign rather than a handle.

Four files, in `docs/assets/brand/`:

| File | Plate | V | Use |
| ---- | ----- | - | --- |
| `mark-lit.svg` | `night-950`, `slate-600` frame | `amber-400`, lit | Primary mark. Dark grounds, including the console and the Windows icon; also holds on light grounds. |
| `mark-lit-deep.svg` | `night-950`, no frame | `amber-400`, lit | The light-ground cut. A white page already outlines the plate. Not for dark grounds, where the plate edge disappears. |
| `mark-idle.svg` | `night-950`, `slate-600` frame | `slate-600`, unlit | The sign switched off. Only where nothing is answering; never an admission state. |
| `mark-mono.svg` | `currentColor` outline | `currentColor` | Single-colour contexts and print. |

The V is lit in every admission state. Admission is carried by the separate NO lettering, as on the sign itself, so the mark names the project and never a state; its title is `vacancyd`. Explicit status text accompanies the sign, and screen readers receive "Vacancy" or "No vacancy" without the unlit lettering. In Windows high contrast, the unlit NO is hidden while its space is retained.

Every lit cut puts `amber-400` on a `night-950` plate: 10.88:1, the same pair as lit text in the console. The sign brings its own night, so `amber-400`'s dark-ground rule holds on any page and no light-ground recolour of the V is needed. On a dark ground the `slate-600` frame separates the plate at 3.15:1; on white the plate itself stands at 19.18:1 and the rail at 6.08:1. The unlit V is `slate-600` on `night-950`, 3.15:1: visible as a tube, deliberately quiet.

The `idle` file name predates this design and is kept because the Windows installer ships the brand files as one component keyed on it. In vacancyd's own vocabulary an idle owner means vacancy; the file means a sign switched off.

### Rules

- Clear space on all sides: the height of the hanging rail.
- Minimum size 16px. Below 24px, drop the rail and use the plate alone. `console/web/build-icon.mjs` does this for the Windows icon's 16px frame.
- Light only the lettering. Never fill the plate, frame or bar with amber.
- Do not recolour a supplied mark outside its documented palette.
- Do not add a glow, bloom, or neon outline. The V is lit by being amber; a literal neon effect ages badly and destroys the 16px reading.
- Do not rotate, skew, or place the mark on a busy photograph.

### The sign in the console

The owner console draws the vacancy sign the same way wherever it appears: Archivo 600 lettering, VACANCY in `amber-400`, NO in `amber-400` when borrowing is withheld and `--amber-unlit` when it is not, inside a `slate-600` frame. Status words that are not the sign (CALIBRATING, NOT READY, UNKNOWN) are readouts, set in IBM Plex Mono in their state colour. An amber fill marks the primary action; a disabled control drops to `night-900` and `slate-400` rather than a dimmed amber.

---

## Voice

Lead with the reason to use it: **keep your workstation, and let your homelab borrow its spare capacity for local AI.** The workstation can keep its operating system and everyday applications; it doesn't have to become a cluster node or virtualization host. The [README introduction](../README.md) is the full product explanation, also used as the documentation homepage.

The short brand line remains **Spare compute. On your terms.** Pair it with the concrete workstation benefit, then explain the vacancy sign and the owner's house rules. Supported AI services define the lending scope; this is not a promise to run arbitrary cluster jobs. A resource can keep lending while the owner is present if its policy permits, so avoid describing all lending as idle-only.

Use natural contractions in product copy: "what's lending", "doesn't" and "you're". Keep the documentation direct and technically specific. Explain limitations alongside the behavior they qualify.

**State the honest limitation next to the claim.** Admission and upstream cancellation are different checks. A closed proxy connection does not prove that the upstream stopped computing. See [admission and recovery](admission.md#owner-return).

**Name what it does not do.** The [related approaches](prior-art.md) page distinguishes admission from model lifetime and gateway routing. Describe those differences without claiming the policy is unique.

Avoid: "effortlessly", "seamlessly", "blazingly fast", "just works". The project is for people who will read the measurements.

---

## Current assets and future applications

Use plain headings for feature cards. Don't add decorative numbered labels or arrow flourishes to links.

The repository ships four SVG marks under `docs/assets/brand/`, the Windows icon under `console/icons/`, and the console's embedded Archivo and IBM Plex Mono fonts with their licenses. The app website is [vacancyd.com](https://vacancyd.com/); native Zensical documentation is [docs.vacancyd.com](https://docs.vacancyd.com/), built from this repository's Markdown. Website assets include console screenshots rendered with example data, the SVG favicon, and a 1200 × 630 social card. Their source and provenance live under `website/assets/` and `console/web/site-tests/`. Horizontal and stacked lockups and a broader favicon set remain future applications.

Any new artwork should follow these tokens and sign geometry and be reviewed before use. Keep release availability separate from the product description: source, installers and container images have distinct publication states.

### Deliberately not copying dnsweaver

dnsweaver is the quality bar and the structural model: a fixed token set, ratios documented rather than guessed, fonts self-hosted with licences, a full asset inventory and one canonical brand file.

dnsweaver uses cool navy and cyan, Sora and JetBrains Mono, and a weave. vacancyd uses warm amber on night, Archivo and IBM Plex Mono, and a sign. Keep the same care in the design while giving each project its own identity.

---

## Future refinements

The v1 visual direction is approved. These are optional refinements for later identity work rather than release gates:

1. **Is amber legible enough as the sole accent?** It carries both "lending" and every link and heading. A second structural hue may be needed for navigation so that "lit" keeps its meaning instead of becoming the page's default colour.
2. **Wordmark treatment.** Plain Archivo 700 may be too plain. Worth testing a version where the `V` of the wordmark and the `V` of the mark are the same drawing.
3. **Distinctive applications.** Amber vacancy signs are a common visual space; registered marks or printed merchandise should receive a separate collision review.

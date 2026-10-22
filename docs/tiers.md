# Availability tiers

Available capacity changes with the owner's activity, often by enough to determine which models fit. Measure each state instead of publishing one free-VRAM figure.

## The three tiers

| Tier | Owner state | vacancyd sees | Lending |
| ---- | ----------- | ------------- | ------- |
| **1: gaming** | Playing; a game owns the GPU | `session: active` + owner process, or GPU pressure | closed |
| **2: idle** | Logged in, unlocked, ordinary apps; **or locked** | `session: active` / `locked` | closed when unlocked and recently touched, open when locked (after the release hold) |
| **3: logged-out** | Nobody logged in, including a PC waiting at the sign-in screen and a Linux server with only SSH sessions | `session: logged_out` | open (after the release hold) |

The lending column describes host-scoped upstreams, the default. An upstream set to `owner_scope = "resource"` is exempt from owner presence: it lends in every tier, limited only by its own resources, and `/status`, `vacancyd status` and the console name it. See [resource-scoped upstreams](admission.md#resource-scoped-upstreams).

**Lending opens after a hold, not at once.** The owner veto engages on the first read that counts as presence and releases only after `owner.release_after_s` (120 seconds by default) of reads that do not. Locking the screen, signing out, switching user or going idle therefore lends once that hold has passed since the owner was last seen, and returning refuses again immediately.

**Locked belongs with idle, not with logged-out.** A lock leaves every application running and every framebuffer allocated. The desktop's VRAM cost does not go away when the screen does. What changes at lock is *permission*, not capacity: the machine becomes lendable while its free VRAM stays roughly what it was. Tier 2 therefore describes one capacity with two lending answers, which is exactly why capacity and permission are separate signals in the daemon.

Tier 3 is the only tier that actually returns memory, because logging out tears down the session's processes and their allocations.

**Unlocked is not unconditionally closed.** `owner.idle_lends_after_s` (0 by default, which disables it) reopens tier 2 once an unlocked session has gone untouched by keyboard or mouse for that many seconds, and `/status` then names idle lending in `presence.basis`. On Windows this needs the [session agent](split-agent.md) running, since a service alone cannot read idle time. On Linux it uses the idle hint the desktop reports to systemd-logind; a desktop that never sets that hint never lends on idle. Without a reading, or before the threshold is reached, unlocked stays closed exactly as the table says. This is a relaxation *within* tier 2, not a fourth tier: measured capacity stays whatever tier 2 measured, only the lending permission changes again.

On Windows, a PC at the sign-in screen is in tier 3. That covers a restart that nobody has signed in after, a sign-out, and the Switch user screen. The sign-in screen is a connected session with no user, and vacancyd lends only after two consecutive session reads show the same view, so a brief state during sign-in or sign-out cannot open lending. If another user's session is still signed in, its state decides instead: an unlocked session refuses, a locked or disconnected one lends. A session read that fails or returns a state vacancyd does not recognise refuses. To keep the sign-in screen refusing, set `owner.sign_in_screen_is_presence = true` or turn on **Keep refusing while Windows shows the sign-in screen** under Advanced settings in the owner console.

On Linux the same tiers come from systemd-logind. A session counts as someone at the machine when it belongs to a user and is graphical (X11, Wayland or Mir, local or remote) or is a local console login on a seat:

| logind shows | vacancyd sees | Lending |
| ------------ | ------------- | ------- |
| A counted session in the foreground, unlocked | `session: active` | closed, unless idle lending applies |
| No unlocked counted session in the foreground, and at least one counted session locked | `session: locked` | open after the release hold |
| Counted sessions, none in the foreground or locked (switched to another user or terminal) | `session: disconnected` | open after two identical reads and the release hold |
| No counted session: only SSH logins, the display-manager greeter, or nothing | `session: logged_out` | open after two identical reads and the release hold |
| The first of those two reads, including the first read after logind returns | `session: unknown` | closed |
| logind cannot be read | `session: unknown` | closed |

SSH and other remote text logins, the per-user service manager, background sessions and sessions that are closing do not count. The greeter is the Linux sign-in screen; `owner.sign_in_screen_is_presence = true` keeps a greeter in the foreground refusing. A host without systemd-logind can set `owner.presence = "headless"` to lend as logged out without reading sessions at all. `/status` shows every session logind reported under `presence.sessions`, with whether it counted and why.

## Measure each state

An idle desktop, a locked desktop and a logged-out machine can have different free memory. Existing models and background applications also change the budget. Measure the states your owner policy permits on the machine you will lend; do not copy another machine's capacity figures. A locked screen changes presence, while the applications it leaves running can still consume resources.

## Measuring

`scripts/measure-tiers.ps1` samples every NVIDIA GPU, system memory, CPU load, and vacancyd's own session view, tagging each row with its tier. It is read-only and allocates nothing on any GPU.

```powershell
.\scripts\measure-tiers.ps1 -Tier idle       -Minutes 10
.\scripts\measure-tiers.ps1 -Tier gaming     -Minutes 15   # then launch the game
.\scripts\measure-tiers.ps1 -Tier locked     -Minutes 10   # then lock
.\scripts\measure-tiers.ps1 -Tier logged-out -Minutes 10   # needs a service/task
```

Tier 3 cannot be measured from an interactive shell because the shell dies at logout. Run it from the vacancyd service itself (its metrics are sampled continuously and survive logout), or as a scheduled task with "run whether user is logged on or not".

> ⚠️ **These transitions are the operator's to make.** The script never locks
> the machine, never logs anyone out, and never touches power or display
> policy. Automating those is a separate decision; on this workstation they
> are deliberately manual today.

## Turning tiers into thresholds

A per-model threshold in `vacancyd.toml` is the free memory a request needs when it may have to **load** the model: **measured footprint + ~500 MiB headroom**. Once the model is resident and a request reuses it as loaded, admission charges the model's `resident_min_free_mib` (default 0) instead, so the threshold is not a second copy of memory the model already holds (see [the gate](architecture.md#gate-srcgaters)). Sanity-check the load threshold against each tier:

| If the threshold is… | …then the model |
| -------------------- | --------------- |
| below tier 3 free | fits the measured logged-out capacity; it may also fit in other states |
| below tier 2 free | fits the measured idle or locked capacity; admission still depends on owner policy and current observations |
| below tier 1 free | fits the measured gaming-state capacity; admission still depends on owner policy and current observations |

The gate does not need to know about tiers: it compares live free capacity against the threshold, and the tiers are simply the recurring shapes that live capacity takes. The tiers are for *choosing* thresholds and for explaining the machine to whoever borrows from it.

## Recording results

Publish the tier tables for borrowers and keep the raw CSVs with the measurements behind each threshold.

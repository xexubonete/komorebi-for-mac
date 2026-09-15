# komorebi for Mac — `mac-tweaks`

Tiling window management for macOS. **This is a personal fork**, not the original.

> **Upstream:** [LGUG2Z/komorebi-for-mac](https://github.com/LGUG2Z/komorebi-for-mac)
> **This fork:** [xexubonete/komorebi-for-mac](https://github.com/xexubonete/komorebi-for-mac), branch `mac-tweaks`
>
> All the credit for komorebi belongs to [LGUG2Z](https://github.com/LGUG2Z). What follows
> documents only what this branch adds on top. For the project itself — what it is, how
> tiling works, the community, and **the licensing terms, which are unchanged and still
> apply** — read the [upstream README](https://github.com/LGUG2Z/komorebi-for-mac#readme)
> and [`LICENSE.md`](LICENSE.md).

`mac-tweaks` sits about 50 commits ahead of `upstream/master`, across roughly 4,700 lines.
It is a daily driver, not a proposal: everything here exists because something misbehaved
on a real desk with a laptop screen and an external monitor.

---

## What this branch adds

### Windows stay where you put them

- **Session persistence.** The window→workspace map is written to disk, so restarting
  komorebi while the apps stay alive (`rset`) puts every window back on its own workspace.
  Windows are matched by Accessibility id first, then by app name plus title — the second
  is what survives a logout or a reboot with *"reopen windows when logging back in"*.
- **The session is keyed to the boot UUID**, so after a real reboot — where macOS reassigns
  every window id — komorebi doesn't restore a map that no longer means anything.
- **Screen lock no longer destroys the layout.** A locked screen reports no windows; the
  original read that as "every window closed" and reaped them all. The session file is now
  held through a lock and the latch lifts only once the model is whole again.
- **Startup pulls windows out of macOS full screen** before enumerating them, instead of
  leaving them in a space komorebi can't reach.

### Windows that refuse to be resized

Some applications will not shrink below a size of their own choosing. The Accessibility
call succeeds, the window keeps its size, and it overlaps whatever sits next to it.

- **Minimum sizes are learned once and remembered on disk**, per application and in both
  dimensions. Measured here: WhatsApp and Música refuse to go below ~600 points tall and
  hold widths near 1000; Mail refuses widths in the 750s.
- **A window that cannot fit its cell is relocated** to a workspace with room, rather than
  left overlapping its neighbours.
- What is stored is the *cause* (this app will not go below N points), not the situation
  it broke in — so nothing is relearned when a different monitor is plugged in.

### Focus follows mouse

Moving the cursor **onto** a window focuses it.

The distinction matters: an earlier version focused whatever the cursor was *over*, every
tenth of a second, which overruled everything else — typing into a launcher stopped
working, because the cursor was resting over another window and focus was pulled away
mid-word. A cursor that has not moved is not asking for anything, so Cmd+Tab, launchers
and workspace changes keep what they chose.

It is implemented by polling the cursor position, deliberately, rather than by adding
`MouseMoved` to the existing `CGEventTap`: that tap is an active one, mouse movement
arrives up to 120 times a second, and a callback that takes too long gets the tap disabled
by macOS — which would take window dragging, resizing and reaping down with it.

**This is a runtime toggle, not a config key:**

```sh
komorebic focus-follows-mouse enable
komorebic focus-follows-mouse disable
komorebic toggle-focus-follows-mouse
```

Bind it in `~/.skhdrc` if you want it on a key:

```
ctrl + shift - m : komorebic toggle-focus-follows-mouse
```

### Borders

- **Only the focused window draws a border.** Unfocused windows, and windows komorebi
  doesn't manage, draw nothing.
- **The border flashes when a window takes focus**, with configurable width, duration and
  timing curve (see below).
- **Per-application corner radius.** Windows are not all rounded the same: Apple's own apps
  use the system frame, while anything built on Electron, or a terminal with custom chrome,
  picks its own radius. macOS exposes no way to ask a window what its radius is, so the
  exceptions are listed in config.
- **The border is redrawn when something other than komorebi moves a window**, so it no
  longer trails behind after a manual drag.

### Speed

The whole point of these was a workspace change that felt slow.

- **Windows are placed in parallel**, one thread per application. One slow app no longer
  defines the cost of the whole workspace.
- **Thread quality of service is declared**, so the threads placing windows are scheduled
  as user-interactive. macOS propagates QoS across synchronous IPC, so the Accessibility
  call inherits that priority *inside the other application's process* too.
- **Window positions, titles and application names are cached** instead of being asked for
  on every placement.
- **A window already in the right place is skipped**, and refusals are reported rather than
  silently swallowed.
- **The screen is held still across a whole workspace change**, and borders come down as it
  starts rather than after it.
- **Reconciliation the user has already navigated past is dropped**, instead of being
  applied late.
- **The state snapshot is only built when something is subscribed to it.**

### Correctness fixes

- The global work area offset is applied on **every** layout path. Two paths disagreeing by
  the offset meant 98% of placements resized every window, and the next pass resized it
  back — for two points of difference.
- Workspaces are capped to the configured count **without losing the windows on them**.
- Closing a window no longer drags focus to another workspace.
- Windows with no title yet are managed (if they are standard windows), instead of being
  dropped before the checks run.
- Windows that declare no subrole are left out of the grid.
- Hidden windows are parked *beside* the screen rather than below it.
- Windows are never reaped while the screen is covered.

---

## New configuration options

These go in `~/.config/komorebi/komorebi.json` alongside the upstream keys.

| Key | Type | Default | What it does |
|---|---|---|---|
| `border_flash_style` | `"width"` \| `"none"` | `"width"` | Animation played when a window takes focus. `width` flares the border wide and settles it back. |
| `border_flash_factor` | number | `3.5` | How far the border flares, as a multiple of its settled width. |
| `border_flash_duration_ms` | number | `220` | How long the flash lasts. |
| `border_flash_easing` | string | `"easeOut"` | Timing curve: `easeOut`, `easeIn`, `easeInEaseOut` or `linear`. |
| `border_radius_rules` | object | — | Per-application corner radius, keyed by application name. |

> **On `border_flash_style`:** only `width` and `none` reach the screen. Opacity, scale,
> colour and pulse variants were implemented and removed — the border layer sits inside a
> transparent window with implicit actions disabled, so only its border width actually
> redraws. They animated correctly and were invisible, which is worse than not offering
> them at all.

Example:

```jsonc
{
  "border": true,
  "border_width": 1,
  "border_offset": -7,
  "border_radius": 18,

  "border_flash_style": "width",
  "border_flash_factor": 9.0,
  "border_flash_duration_ms": 110,
  "border_flash_easing": "easeOut",

  // macOS cannot report a window's radius, so apps that draw their own go here.
  "border_radius_rules": {
    "Discord": 14,
    "WhatsApp": 14
  }
}
```

**Focus follows mouse is not configured here** — it is a runtime toggle, see above.

---

## Installation

komorebi is **not** installed from Homebrew here: the three binaries (`komorebi`,
`komorebic`, `komorebi-bar`) all come from this fork, so the daemon and the CLI are
guaranteed to speak the same protocol.

### 1. Build

```sh
git clone --branch mac-tweaks https://github.com/xexubonete/komorebi-for-mac.git ~/dev/komorebi-for-mac
cd ~/dev/komorebi-for-mac
cargo build --release
```

### 2. Code-sign

macOS ties Accessibility permission to a stable code signature. Without one, every rebuild
asks for permission again. A self-signed certificate is enough:

```sh
sh ~/dev/dotfiles/komorebi/setup-codesign.sh ~/dev/komorebi-for-mac/target/release/komorebi
```

### 3. Put the binaries on PATH

```sh
mkdir -p ~/.local/bin
for bin in komorebi komorebic komorebi-bar; do
  ln -sfn ~/dev/komorebi-for-mac/target/release/$bin ~/.local/bin/$bin
done
```

Make sure `~/.local/bin` comes **first** on your PATH, and that it is set in `.zshenv`
rather than `.zshrc` — skhd runs shortcuts through a non-interactive `zsh -c`, which never
reads `.zshrc`. Without that, a shortcut can resolve to a different `komorebic`.

### 4. Grant permissions

System Settings → Privacy & Security, and add the `komorebi` binary to **both**:

- **Accessibility** — required. komorebi refuses to start without it.
- **Screen Recording** — needed to read window titles, and therefore by every rule that
  matches on one.

**komorebi asks for both.** Upstream only prompts for Screen Recording; this branch prompts
for Accessibility too. What it does with the answer differs per permission: **Accessibility
is fatal**, since without it komorebi cannot move a single window, while a missing **Screen
Recording** only logs a warning — a tiling manager without window titles still tiles, and
refusing to start over it would trade a degraded desktop for no desktop at all.

Before prompting it retries for twenty seconds, because a LaunchAgent can start before the
WindowServer is ready and the permission APIs report `false` even when the answer is yes.

`komorebi --check-permissions` reports both and exits non-zero if either is missing, which
is what lets an installer verify instead of assume. Read it with one caveat: macOS credits a
permission request to the *responsible* process, which for anything started from a terminal
is the terminal — so the same binary can report Screen Recording missing from a shell and
work fine under `launchd`. To check the daemon, check what it can do: if it is alive it has
Accessibility, and if it reads window titles it has Screen Recording.

The dialogs only appear once per machine, as long as the binary keeps a **stable code
signature** (step 2). This is why it must be rebuilt with `kbuild`, which signs, and never
with a bare `cargo build`, whose ad-hoc signature changes on every build: macOS then reads
it as a different program and silently stops applying both permissions. The row in System
Settings still looks ticked, because that list goes by path — so the symptom is a komorebi
that is denied permissions it appears to have been granted.

---

## My configuration (dotfiles)

Everything I run is in [xexubonete/dotfiles](https://github.com/xexubonete/dotfiles). If you
want to reuse it, **copy the files to these paths** — or run that repo's `install.sh`, which
symlinks them, builds this fork and sets up the launch agents for you.

**Copy (or symlink) these as they are:**

| File in `dotfiles/` | Destination |
|---|---|
| `komorebi/komorebi.json` | `~/.config/komorebi/komorebi.json` |
| `komorebi/applications.json` | `~/.config/komorebi/applications.json` |
| `komorebi/komorebi.bar.json` | `~/.config/komorebi/komorebi.bar.json` |
| `skhd/skhdrc` | `~/.skhdrc` |
| `komorebi/wakeup.sh` | `~/.wakeup` |

Symlinking beats copying if you plan to change anything: edits stay versioned.

**The launch agents are templates, not files to copy.** Each contains a placeholder that
has to be replaced with an absolute path before it will load — `launchd` does not expand
`~` or `$HOME`:

| Template | Placeholder to replace |
|---|---|
| `komorebi/com.lgug2z.komorebi.plist` | `KOMOREBI_BINARY_PLACEHOLDER`, `KOMOREBI_CONFIG_PLACEHOLDER` |
| `komorebi/com.xexu.startup.plist` | `STARTUP_SCRIPT_PLACEHOLDER` |
| `komorebi/com.xexu.lockwatch.plist` | `DOTFILES_PLACEHOLDER` |
| `komorebi/com.user.sleepwatcher.plist` | `DOTFILES_HOME` |

Substitute, write the result to `~/Library/LaunchAgents/`, then `launchctl bootstrap` it.
`install.sh` does all of this for you, which is the reason to prefer it over copying by
hand.

```sh
git clone https://github.com/xexubonete/dotfiles.git ~/dev/dotfiles
~/dev/dotfiles/install.sh
```

### What the helper scripts are for

| Script | Why it exists |
|---|---|
| `komorebi/restart.sh` | `rset` — restarts komorebi with the apps still running. Session persistence is what makes this survivable. |
| `komorebi/startup.sh` | Brings komorebi up at login, in the right order. |
| `komorebi/wakeup.sh` | Run by sleepwatcher on wake. Waking is when displays reshuffle. |
| `komorebi/lockwatch.sh` | Watches for screen lock/unlock, which is what the session latch depends on. |
| `komorebi/setup-codesign.sh` | Self-signed certificate so Accessibility permission survives a rebuild. |
| `komorebi/ensure-permissions.sh` | Verifies both permissions were actually granted, and keeps asking until they are. |
| `komorebi/tab-to-workspace.sh` | Sends the focused window to a workspace by name. |

### Keybindings

Shortcuts live in `skhd/skhdrc`, vim-style: `ctrl + h/j/k/l` to focus, `ctrl + shift +
h/j/k/l` to move, `ctrl + t` to float, `ctrl + f` for monocle. The file is commented with
**why** certain bindings were removed, which is usually more useful than the binding itself.

---

## Staying in sync with upstream

```sh
git remote add upstream https://github.com/LGUG2Z/komorebi-for-mac.git
git fetch upstream
git log --oneline upstream/master..HEAD   # what this branch adds
git rebase upstream/master                # expect conflicts; these are deep changes
```

## Licensing

Unchanged from upstream. komorebi is released under the **Komorebi License 2.0.0** — see
[`LICENSE.md`](LICENSE.md). Commercial use requires a licence from the original author, and
forking does not alter that. If you use komorebi, [sponsor
LGUG2Z](https://github.com/sponsors/LGUG2Z).

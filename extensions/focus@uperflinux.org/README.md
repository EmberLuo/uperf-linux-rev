# uperf-linux compositor reporter

Reports the focused window's PID, compositor render lifecycle, and Mutter's
physical display power state to `org.uperflinux.Daemon2`.

The extension is only a reporter. It decides nothing: the daemon authorizes
every report (same UID, active local logind session, not a protected process),
holds an expiring compositor-reporter lease, and releases the workload when the
focus lease is not renewed, when this extension's bus peer disappears, or when
the reported process exits.

## Install

The Debian packages already ship these files to
`/usr/share/gnome-shell/extensions/focus@uperflinux.org/`, so with a package
install you only enable it:

```sh
gnome-extensions enable focus@uperflinux.org
```

From a source tree, install it for your user first:

```sh
install -d ~/.local/share/gnome-shell/extensions
cp -a extensions/focus@uperflinux.org ~/.local/share/gnome-shell/extensions/
gnome-extensions enable focus@uperflinux.org
```

Either way the daemon ignores every report unless `scheduler.focus.enabled` is
true in `policy.json`.

On Wayland, log out and back in for the first install. Then confirm:

```sh
uperfctl status          # workload: ... (source focus)
uperfctl health          # a rejected PID appears here as focus.rejected
```

## Behaviour

| Event | Action |
| --- | --- |
| Focus changes | one idle later, re-read focus, resolve the top-level PID, debounce 120 ms, `SetForegroundProcess` |
| Focus remains unchanged | renew the same lease every 5 seconds |
| Focus becomes null | `ClearForegroundProcess` |
| Modal dialog focused | reports the transient parent, so the lease does not thrash |
| First compositor paint after ≥50 ms quiet | `render-started` |
| Compositor becomes quiet for 50 ms | `render-idle`; the daemon adds its own 200 ms slack |
| Mutter DisplayConfig enters standby/suspend/off | `display-blanked`, renewed every 5 seconds while blank |
| Mutter DisplayConfig returns to on | `display-unblanked` |
| Daemon restarts | name-owner watch clears the dedup cache and re-reports |
| D-Bus error | bounded exponential backoff, 500 ms to 30 s; never permanently disabled |
| Screen locks | immediately clears focused workload; only physical-display observation remains active |
| `disable()` | best-effort clears focus, disconnects signals, and cancels pending work; reporter authorization then expires or is revoked with its peer |

`session-modes` includes `unlock-dialog` solely so a compositor-trusted source
can distinguish a real display blank from logind's `LockedHint`. The extension
does not install keyboard or pointer listeners. It clears application focus as
soon as the shell leaves user mode, suppresses frame hints while locked, and
keeps only Mutter's `PowerSaveMode` observer alive.

`render-started` and `render-idle` are deliberately best-effort signals. The
daemon ignores them unless the same D-Bus peer owns the current reporter lease
and an interaction is active. Mutter's `ClutterStage::presented` carries a raw
frame-info pointer that GJS cannot safely marshal, so this extension does not
subscribe to it. Display state uses a bounded retry and an idempotent keepalive
so a long lock interval does not lose the authenticated reporter lease.

The bundled reporter assumes `scheduler.focus.lease_ttl_ms` is at least
15 seconds, matching the packaged policy. Its 5-second renewal interval leaves
room for one delayed or timed-out D-Bus call. Configurations with a shorter TTL
must use a correspondingly faster reporter and are not supported by this
extension.

Reporting requires no root and no polkit action, but the daemon requires the
caller to be in an active local session and ties frame/display hints to the
compositor reporter's D-Bus peer, UID, and expiring lease. A remote or inactive
session cannot steer scheduling.

The reporter state tests use Node only as a deterministic GLib/D-Bus mock; the
production extension still runs exclusively inside GNOME Shell:

```sh
node extensions/focus@uperflinux.org/extension.test.mjs
```

Derived in part from pop-shell's `src/scheduler.ts` (GPL-3.0); see the header
comment in `extension.js` for the list of corrected defects.

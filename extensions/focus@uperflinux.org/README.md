# uperf-linux focus reporter

Reports the focused window's PID to `org.uperflinux.Daemon1` so the daemon can
treat the application you are actually using as the active workload.

The extension is only a reporter. It decides nothing: the daemon authorizes
every report (same UID, active local logind session, not a protected process),
holds an expiring lease, and releases the boost when the lease is not renewed,
when this extension's bus peer disappears, or when the reported process exits.

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
| Daemon restarts | name-owner watch clears the dedup cache and re-reports |
| D-Bus error | bounded exponential backoff, 500 ms to 30 s; never permanently disabled |
| Screen locks | `session-modes` excludes the lock screen, so `disable()` runs and the lease is released |
| `disable()` | releases the lease, disconnects signals, cancels pending sources |

`session-modes` is deliberately `["user"]` only. Locking the screen should
release the boost, and that only happens if the extension is disabled there.

The bundled reporter assumes `scheduler.focus.lease_ttl_ms` is at least
15 seconds, matching the packaged policy. Its 5-second renewal interval leaves
room for one delayed or timed-out D-Bus call. Configurations with a shorter TTL
must use a correspondingly faster reporter and are not supported by this
extension.

Reporting requires no root and no polkit action, but the daemon does require the
caller to be in an active local session, so a remote or inactive session cannot
steer scheduling.

The reporter state tests use Node only as a deterministic GLib/D-Bus mock; the
production extension still runs exclusively inside GNOME Shell:

```sh
node extensions/focus@uperflinux.org/extension.test.mjs
```

Derived in part from pop-shell's `src/scheduler.ts` (GPL-3.0); see the header
comment in `extension.js` for the list of corrected defects.

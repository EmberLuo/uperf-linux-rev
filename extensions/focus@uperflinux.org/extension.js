// uperf-linux focus reporter for GNOME Shell.
//
// SPDX-License-Identifier: GPL-3.0-or-later
//
// The signal shape (focus notification -> one idle -> re-read the focused
// window -> resolve its PID -> single asynchronous D-Bus call) is adapted from
// pop-shell's `src/scheduler.ts` and `src/extension.ts`, GPL-3.0. Every defect
// of that implementation is deliberately corrected here; see NOTES below.
//
// NOTES on the differences from pop-shell:
//   * the proxy is not constructed at module load, and no proxy is constructed
//     at all: a plain `Gio.DBus.system.call()` avoids an introspection round
//     trip and cannot throw during `enable()`;
//   * a D-Bus failure never disables reporting permanently. It backs off
//     exponentially with a bound and resumes;
//   * a stable focus is renewed before the daemon's lease expires, while every
//     callback and timer is guarded by a monotonically increasing generation;
//   * `Gio.bus_watch_name` resets the deduplication cache, so a daemon restart
//     is re-reported instead of being suppressed forever;
//   * PIDs are validated as strictly positive (`-1` is truthy in JS);
//   * `disable()` releases the lease, disconnects every signal, and cancels
//     every pending source;
//   * nothing is reported speculatively: only a focus window the compositor
//     has already committed to is sent;
//   * a `null` focus window explicitly releases the lease instead of being
//     silently ignored;
//   * modal dialogs resolve to their top-level parent rather than dropping the
//     event, so opening a dialog does not thrash the lease.

import Gio from 'gi://Gio';
import GLib from 'gi://GLib';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';

const SERVICE_NAME = 'org.uperflinux.Daemon2';
const OBJECT_PATH = '/org/uperflinux/Daemon2';
const INTERFACE_NAME = 'org.uperflinux.Daemon2';
const DISPLAY_CONFIG_NAME = 'org.gnome.Mutter.DisplayConfig';
const DISPLAY_CONFIG_PATH = '/org/gnome/Mutter/DisplayConfig';
const PROPERTIES_INTERFACE = 'org.freedesktop.DBus.Properties';

// The daemon debounces as well. This only collapses a burst of alt-tab
// notifications into one bus message.
const DEBOUNCE_MS = 120;
const CALL_TIMEOUT_MS = 5000;
// The packaged policy uses a 15-second lease. Renewing every five seconds
// leaves room for one timeout or delayed shell iteration without dropping a
// stable focus lease.
const RENEWAL_MS = 5000;
const RETRY_MINIMUM_MS = 500;
const RETRY_MAXIMUM_MS = 30000;
// A 50 ms quiet window is longer than one frame at 30 Hz. The daemon applies
// its own 200 ms render-idle slack after receiving this observation.
const RENDER_IDLE_MS = 50;
const DEADLINE_MISS_RATIO = 1.5;
const DISPLAY_RENEWAL_MS = 5000;

export default class FocusReporterExtension extends Extension {
    enable() {
        this._enabled = true;
        this._signals = [];
        this._idleId = 0;
        this._debounceId = 0;
        this._retryId = 0;
        this._renewalId = 0;
        this._retryDelay = RETRY_MINIMUM_MS;
        this._daemonPresent = false;
        this._userSession = this._isUserSession();
        this._desiredKnown = false;
        this._desired = null;
        this._forceUpdate = false;
        this._generation = 0;
        this._call = null;
        this._frameCalls = new Set();
        this._renderIdleId = 0;
        this._rendering = false;
        this._presentations = new Map();
        this._displayState = null;
        this._reportedDisplayState = null;
        this._displayQuery = null;
        this._displayRetryId = 0;
        this._displayRetryDelay = RETRY_MINIMUM_MS;
        this._displayRenewalId = 0;
        // The PID from the most recent successful D-Bus receipt. A receipt
        // acknowledges asynchronous identity resolution, so periodic renewal
        // also retries a transient post-ack rejection.
        this._reported = null;

        this._watchId = Gio.bus_watch_name(
            Gio.BusType.SYSTEM,
            SERVICE_NAME,
            Gio.BusNameWatcherFlags.NONE,
            () => this._onDaemonAppeared(),
            () => this._onDaemonVanished(),
        );

        // A window closing while focused also emits this with a null focus
        // window, which releases the lease instead of boosting a dead PID.
        this._connect(global.display, 'notify::focus-window', () => this._queueUpdate());
        this._connect(Main.sessionMode, 'updated', () => this._onSessionModeChanged());

        // These are compositor lifecycle signals, not input-event hooks. They
        // remain connected in unlock-dialog mode so physical display state can
        // still be reported, while their handlers suppress application frame
        // hints outside a user session.
        this._connect(global.stage, 'before-paint', () => this._onBeforePaint());
        this._connect(global.stage, 'after-paint', () => this._onAfterPaint());
        this._connect(global.stage, 'presented', (...args) => this._onPresented(...args));

        this._monitorManager = global.backend?.get_monitor_manager?.() ?? null;
        if (this._monitorManager) {
            this._connect(
                this._monitorManager,
                'power-save-mode-changed',
                () => this._queryDisplayPowerState(),
            );
        }
        this._queueUpdate();
    }

    disable() {
        const shouldClear = this._daemonPresent &&
            (this._desiredKnown || this._reported !== null || this._desired !== null ||
             this._call !== null);
        this._enabled = false;
        this._generation += 1;
        this._clearSources();
        this._cancelCall();
        this._cancelDisplayQuery();
        this._cancelFrameCalls();
        this._resetRenderState();
        for (const [object, id] of this._signals ?? [])
            object.disconnect(id);
        this._signals = [];

        if (this._watchId) {
            Gio.bus_unwatch_name(this._watchId);
            this._watchId = 0;
        }

        this._daemonPresent = false;
        this._reported = null;
        this._desired = null;
        this._desiredKnown = false;
        this._monitorManager = null;
        this._displayState = null;
        this._reportedDisplayState = null;
        // `unlock-dialog` is intentionally supported so the extension can
        // observe Mutter's physical-display state while locked. No keyboard
        // or pointer event signal is installed, and focus is cleared as soon
        // as the session leaves user mode.
        // A Set may already have reached the daemon even when its callback has
        // not run. Send an uncancelled Clear after cancelling local callbacks.
        // The daemon TTL is the final backstop if the peer cannot complete it.
        if (shouldClear)
            this._bestEffortClear();
    }

    _connect(object, signal, handler) {
        this._signals.push([object, object.connect(signal, handler)]);
    }

    _clearSources() {
        for (const name of [
            '_idleId',
            '_debounceId',
            '_retryId',
            '_renewalId',
            '_renderIdleId',
            '_displayRetryId',
            '_displayRenewalId',
        ])
            this._cancelSource(name);
    }

    _cancelSource(name) {
        if (!this[name])
            return;
        GLib.Source.remove(this[name]);
        this[name] = 0;
    }

    _cancelCall() {
        this._call?.cancellable.cancel();
        this._call = null;
    }

    _cancelFrameCalls() {
        for (const call of this._frameCalls ?? [])
            call.cancellable.cancel();
        this._frameCalls?.clear();
    }

    _cancelDisplayQuery() {
        this._displayQuery?.cancellable.cancel();
        this._displayQuery = null;
    }

    _advanceGeneration() {
        this._generation += 1;
        this._cancelSource('_debounceId');
        this._cancelSource('_retryId');
        this._cancelSource('_renewalId');
        this._cancelCall();
    }

    _onDaemonAppeared() {
        if (!this._enabled)
            return;
        this._daemonPresent = true;
        this._retryDelay = RETRY_MINIMUM_MS;
        this._reported = null;
        this._reportedDisplayState = null;
        this._displayRetryDelay = RETRY_MINIMUM_MS;
        // A restarted daemon holds no lease. Advance the generation so a late
        // callback from the previous owner cannot overwrite the new state.
        this._advanceGeneration();
        this._queueUpdate(true);
    }

    _onDaemonVanished() {
        if (!this._enabled)
            return;
        this._daemonPresent = false;
        this._reported = null;
        this._reportedDisplayState = null;
        this._cancelDisplayQuery();
        this._cancelSource('_displayRetryId');
        this._cancelSource('_displayRenewalId');
        this._cancelFrameCalls();
        this._advanceGeneration();
    }

    _isUserSession() {
        return Main.sessionMode.currentMode === 'user' ||
            Main.sessionMode.parentMode === 'user';
    }

    _onSessionModeChanged() {
        const userSession = this._isUserSession();
        if (userSession === this._userSession)
            return;
        this._userSession = userSession;
        this._resetRenderState();
        if (userSession)
            this._queueUpdate(true);
        else
            this._setDesired(null, true);
        this._queryDisplayPowerState();
    }

    // The focused window at notification time is not always the final one, so
    // the value is re-read after one idle. This is pop-shell's observation.
    _queueUpdate(force = false) {
        this._forceUpdate ||= force;
        if (this._idleId)
            return;
        this._idleId = GLib.idle_add(GLib.PRIORITY_DEFAULT_IDLE, () => {
            this._idleId = 0;
            const forceUpdate = this._forceUpdate;
            this._forceUpdate = false;
            this._setDesired(this._focusedPid(), forceUpdate);
            return GLib.SOURCE_REMOVE;
        });
    }

    _focusedPid() {
        if (!this._userSession)
            return null;
        const focused = global.display.get_focus_window();
        if (!focused)
            return null;
        // Walk to the top level so a modal dialog reports its parent.
        let toplevel = focused;
        for (let depth = 0; depth < 8; depth += 1) {
            const parent = toplevel.get_transient_for?.();
            if (!parent)
                break;
            toplevel = parent;
        }
        const pid = toplevel.get_pid?.();
        // `-1` means the PID is unknown, and it is truthy.
        if (typeof pid !== 'number' || pid <= 0)
            return null;
        return pid;
    }

    _setDesired(pid, force = false) {
        if (this._desiredKnown && pid === this._desired && !force)
            return;
        this._desiredKnown = true;
        this._desired = pid;
        this._advanceGeneration();
        if (!this._daemonPresent)
            return;
        if (pid === null) {
            // Clear even if Set is only in flight. The generation guard keeps a
            // late Set callback from restoring stale local state.
            this._sendClear();
            return;
        }
        this._debounceId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, DEBOUNCE_MS, () => {
            this._debounceId = 0;
            this._sendDesired();
            return GLib.SOURCE_REMOVE;
        });
    }

    _sendDesired() {
        if (!this._enabled || !this._daemonPresent || !this._desiredKnown)
            return;
        if (this._desired === null) {
            this._sendClear();
            return;
        }
        this._sendSet(this._desired);
    }

    _sendSet(pid) {
        if (this._call)
            return;
        this._cancelSource('_renewalId');
        const generation = this._generation;
        this._startCall(
            'SetForegroundProcess',
            new GLib.Variant('(us)', [pid, 'gnome-shell focus']),
            generation,
            () => {
                if (this._desired !== pid)
                    return;
                this._reported = pid;
                this._scheduleRenewal(generation, pid);
                this._queryDisplayPowerState();
            },
        );
    }

    _sendClear() {
        if (this._call)
            return;
        this._cancelSource('_renewalId');
        const generation = this._generation;
        this._startCall(
            'ClearForegroundProcess',
            null,
            generation,
            () => {
                if (this._desired === null)
                    this._reported = null;
                this._queryDisplayPowerState();
            },
        );
    }

    _startCall(method, parameters, generation, onSuccess) {
        const cancellable = new Gio.Cancellable();
        const call = {cancellable, generation};
        this._call = call;
        Gio.DBus.system.call(
            SERVICE_NAME,
            OBJECT_PATH,
            INTERFACE_NAME,
            method,
            parameters,
            null,
            Gio.DBusCallFlags.NONE,
            CALL_TIMEOUT_MS,
            cancellable,
            (connection, result) => {
                let error = null;
                try {
                    connection.call_finish(result);
                } catch (caught) {
                    error = caught;
                }
                const current = this._call === call;
                if (current)
                    this._call = null;
                if (error?.matches?.(Gio.IOErrorEnum, Gio.IOErrorEnum.CANCELLED))
                    return;
                if (!current || !this._enabled || generation !== this._generation)
                    return;
                if (error) {
                    console.warn(`uperf-linux focus: ${method} failed: ${error}`);
                    this._retryLater();
                    return;
                }
                this._cancelSource('_retryId');
                this._retryDelay = RETRY_MINIMUM_MS;
                onSuccess();
            },
        );
    }

    _scheduleRenewal(generation, pid) {
        this._cancelSource('_renewalId');
        if (!this._enabled || !this._daemonPresent || this._desired !== pid)
            return;
        this._renewalId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, RENEWAL_MS, () => {
            this._renewalId = 0;
            if (generation === this._generation && this._desired === pid)
                this._sendSet(pid);
            return GLib.SOURCE_REMOVE;
        });
    }

    // Bounded exponential backoff. Reporting is never disabled permanently.
    _retryLater() {
        if (this._retryId || !this._daemonPresent)
            return;
        this._cancelSource('_renewalId');
        const delay = this._retryDelay;
        this._retryDelay = Math.min(this._retryDelay * 2, RETRY_MAXIMUM_MS);
        const generation = this._generation;
        this._retryId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, delay, () => {
            this._retryId = 0;
            if (generation === this._generation)
                this._sendDesired();
            return GLib.SOURCE_REMOVE;
        });
    }

    _onBeforePaint() {
        if (!this._enabled || !this._daemonPresent || !this._userSession ||
            this._displayState === true)
            return;
        if (!this._rendering) {
            this._rendering = true;
            this._presentations.clear();
            this._reportFrameHint('render-started');
        }
    }

    _onAfterPaint() {
        if (!this._rendering)
            return;
        this._cancelSource('_renderIdleId');
        this._renderIdleId = GLib.timeout_add(
            GLib.PRIORITY_DEFAULT,
            RENDER_IDLE_MS,
            () => {
                this._renderIdleId = 0;
                if (!this._rendering)
                    return GLib.SOURCE_REMOVE;
                this._rendering = false;
                this._presentations.clear();
                this._reportFrameHint('render-idle');
                return GLib.SOURCE_REMOVE;
            },
        );
    }

    _onPresented(...args) {
        if (!this._rendering || !this._userSession || this._displayState === true)
            return;
        const infoIndex = args.findIndex(
            value => typeof value?.get_presentation_time_us === 'function' ||
                typeof value?.get_presentation_time === 'function' ||
                value?.presentation_time !== undefined,
        );
        if (infoIndex < 0)
            return;
        const frameInfo = args[infoIndex];
        const view = infoIndex > 0 ? args[infoIndex - 1] : global.stage;
        let presentationUs;
        if (typeof frameInfo.get_presentation_time_us === 'function')
            presentationUs = Number(frameInfo.get_presentation_time_us());
        else if (frameInfo.presentation_time !== undefined)
            presentationUs = Number(frameInfo.presentation_time);
        else
            presentationUs = Number(frameInfo.get_presentation_time()) / 1000;
        const refreshRate = Number(
            typeof frameInfo.get_refresh_rate === 'function'
                ? frameInfo.get_refresh_rate()
                : frameInfo.refresh_rate,
        );
        if (!Number.isFinite(presentationUs) || presentationUs <= 0 ||
            !Number.isFinite(refreshRate) || refreshRate <= 0)
            return;

        const previous = this._presentations.get(view);
        this._presentations.set(view, presentationUs);
        if (previous === undefined || presentationUs <= previous)
            return;
        const expectedUs = 1_000_000 / refreshRate;
        if (presentationUs - previous > expectedUs * DEADLINE_MISS_RATIO)
            this._reportFrameHint('deadline-missed');
    }

    _resetRenderState() {
        this._cancelSource('_renderIdleId');
        this._rendering = false;
        this._presentations?.clear();
    }

    _reportFrameHint(event, completed = null) {
        if (!this._enabled || !this._daemonPresent) {
            completed?.(false);
            return;
        }
        const cancellable = new Gio.Cancellable();
        const call = {cancellable};
        this._frameCalls.add(call);
        Gio.DBus.system.call(
            SERVICE_NAME,
            OBJECT_PATH,
            INTERFACE_NAME,
            'ReportFrameHint',
            new GLib.Variant('(s)', [event]),
            null,
            Gio.DBusCallFlags.NONE,
            CALL_TIMEOUT_MS,
            cancellable,
            (connection, result) => {
                let error = null;
                try {
                    connection.call_finish(result);
                } catch (caught) {
                    error = caught;
                }
                const current = this._frameCalls.delete(call);
                if (error?.matches?.(Gio.IOErrorEnum, Gio.IOErrorEnum.CANCELLED))
                    return;
                if (!current || !this._enabled)
                    return;
                // Render events are best effort; display state has its own
                // bounded retry below.
                completed?.(error === null);
            },
        );
    }

    _queryDisplayPowerState() {
        if (!this._enabled || !this._daemonPresent || !this._monitorManager)
            return;
        this._cancelDisplayQuery();
        const cancellable = new Gio.Cancellable();
        const query = {cancellable};
        this._displayQuery = query;
        Gio.DBus.session.call(
            DISPLAY_CONFIG_NAME,
            DISPLAY_CONFIG_PATH,
            PROPERTIES_INTERFACE,
            'Get',
            new GLib.Variant(
                '(ss)',
                [DISPLAY_CONFIG_NAME, 'PowerSaveMode'],
            ),
            null,
            Gio.DBusCallFlags.NONE,
            CALL_TIMEOUT_MS,
            cancellable,
            (connection, result) => {
                let reply = null;
                let error = null;
                try {
                    reply = connection.call_finish(result);
                } catch (caught) {
                    error = caught;
                }
                const current = this._displayQuery === query;
                if (current)
                    this._displayQuery = null;
                if (error?.matches?.(Gio.IOErrorEnum, Gio.IOErrorEnum.CANCELLED))
                    return;
                if (!current || !this._enabled || error)
                    return;
                const mode = this._unpackInteger(reply);
                // Mutter's DisplayConfig ABI uses 0 for on, 1..3 for
                // standby/suspend/off, and -1 when power saving is unsupported.
                if (mode === 0)
                    this._setDisplayState(false);
                else if (mode !== null && mode >= 1 && mode <= 3)
                    this._setDisplayState(true);
            },
        );
    }

    _unpackInteger(reply) {
        let value = reply;
        for (let depth = 0; depth < 6; depth += 1) {
            if (Array.isArray(value) && value.length === 1) {
                [value] = value;
                continue;
            }
            if (typeof value?.deep_unpack !== 'function')
                break;
            const unpacked = value.deep_unpack();
            if (unpacked === value)
                break;
            value = unpacked;
        }
        return Number.isInteger(value) ? value : null;
    }

    _setDisplayState(blanked) {
        if (this._displayState === blanked &&
            this._reportedDisplayState === blanked)
            return;
        this._displayState = blanked;
        this._cancelSource('_displayRetryId');
        this._displayRetryDelay = RETRY_MINIMUM_MS;
        if (blanked)
            this._resetRenderState();
        else
            this._cancelSource('_displayRenewalId');
        this._reportDisplayState();
    }

    _reportDisplayState(force = false) {
        const blanked = this._displayState;
        if (blanked === null ||
            (!force && this._reportedDisplayState === blanked))
            return;
        this._reportFrameHint(
            blanked ? 'display-blanked' : 'display-unblanked',
            success => {
                if (this._displayState !== blanked) {
                    this._reportDisplayState();
                    return;
                }
                if (!success) {
                    this._retryDisplayState();
                    return;
                }
                this._reportedDisplayState = blanked;
                this._displayRetryDelay = RETRY_MINIMUM_MS;
                this._cancelSource('_displayRetryId');
                if (blanked)
                    this._renewBlankedDisplayState();
                else
                    this._cancelSource('_displayRenewalId');
            },
        );
    }

    _retryDisplayState() {
        if (this._displayRetryId || !this._daemonPresent)
            return;
        const delay = this._displayRetryDelay;
        this._displayRetryDelay = Math.min(
            this._displayRetryDelay * 2,
            RETRY_MAXIMUM_MS,
        );
        this._displayRetryId = GLib.timeout_add(
            GLib.PRIORITY_DEFAULT,
            delay,
            () => {
                this._displayRetryId = 0;
                this._reportDisplayState(true);
                return GLib.SOURCE_REMOVE;
            },
        );
    }

    _renewBlankedDisplayState() {
        this._cancelSource('_displayRenewalId');
        if (!this._daemonPresent || this._displayState !== true)
            return;
        // Repeating an idempotent blanked event keeps the authenticated
        // compositor-reporter lease alive through a long lock interval.
        this._displayRenewalId = GLib.timeout_add(
            GLib.PRIORITY_DEFAULT,
            DISPLAY_RENEWAL_MS,
            () => {
                this._displayRenewalId = 0;
                if (this._displayState === true)
                    this._reportDisplayState(true);
                return GLib.SOURCE_REMOVE;
            },
        );
    }

    _bestEffortClear() {
        Gio.DBus.system.call(
            SERVICE_NAME,
            OBJECT_PATH,
            INTERFACE_NAME,
            'ClearForegroundProcess',
            null,
            null,
            Gio.DBusCallFlags.NONE,
            CALL_TIMEOUT_MS,
            null,
            (connection, result) => {
                try {
                    connection.call_finish(result);
                } catch (error) {
                    console.warn(`uperf-linux focus: final clear failed: ${error}`);
                }
            },
        );
    }
}

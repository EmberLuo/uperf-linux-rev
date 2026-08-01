// SPDX-License-Identifier: GPL-3.0-or-later

import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';

const extensionSource = readFileSync(
    new URL('./extension.js', import.meta.url),
    'utf8',
)
    .replace(/^import .*;\n/gm, '')
    .replace(
        'export default class FocusReporterExtension',
        'class FocusReporterExtension',
    )
    .concat('\nglobalThis.FocusReporterExtension = FocusReporterExtension;\n');

function harness(initialPid = null) {
    let nextSourceId = 1;
    let nextSignalId = 1;
    let focusedPid = initialPid;
    let watcher = null;
    const sources = new Map();
    const calls = [];
    const propertyCalls = [];
    const signals = new Map();

    class Cancellable {
        cancel() {
            this.cancelled = true;
        }
    }

    class Variant {
        constructor(signature, value) {
            this.signature = signature;
            this.value = value;
        }
    }

    const systemConnection = {
        call_finish(result) {
            if (result.error)
                throw result.error;
            return result.reply ?? null;
        },
    };
    const sessionConnection = {
        call_finish(result) {
            if (result.error)
                throw result.error;
            return result.reply ?? null;
        },
    };
    const recordCall = (collection, connection) => (
        _service,
        _path,
        _interface,
        method,
        parameters,
        _replyType,
        _flags,
        _timeout,
        cancellable,
        callback,
    ) => {
        collection.push({
            method,
            parameters,
            cancellable,
            complete(error = null, reply = null) {
                callback(connection, {error, reply});
            },
        });
    };
    const Gio = {
        BusType: {SYSTEM: 0},
        BusNameWatcherFlags: {NONE: 0},
        DBusCallFlags: {NONE: 0},
        IOErrorEnum: {CANCELLED: 'cancelled'},
        Cancellable,
        Variant,
        DBus: {
            system: {
                call: recordCall(calls, systemConnection),
            },
            session: {
                call: recordCall(propertyCalls, sessionConnection),
            },
        },
        bus_watch_name(_type, _name, _flags, appeared, vanished) {
            watcher = {appeared, vanished};
            return 1;
        },
        bus_unwatch_name() {},
    };
    const addSource = (kind, delay, callback) => {
        const id = nextSourceId++;
        sources.set(id, {kind, delay, callback});
        return id;
    };
    const GLib = {
        PRIORITY_DEFAULT: 0,
        PRIORITY_DEFAULT_IDLE: 0,
        SOURCE_REMOVE: false,
        Variant,
        Source: {
            remove(id) {
                sources.delete(id);
            },
        },
        idle_add(_priority, callback) {
            return addSource('idle', 0, callback);
        },
        timeout_add(_priority, delay, callback) {
            return addSource('timeout', delay, callback);
        },
    };
    const signalObject = methods => {
        const object = {...methods};
        object.connect = (signal, callback) => {
            const id = nextSignalId++;
            signals.set(id, {object, signal, callback});
            return id;
        };
        object.disconnect = id => {
            signals.delete(id);
        };
        return object;
    };
    const display = signalObject({
        get_focus_window() {
            if (focusedPid === null)
                return null;
            return {
                get_pid: () => focusedPid,
                get_transient_for: () => null,
            };
        },
    });
    const stage = signalObject({});
    const monitorManager = signalObject({});
    const sessionMode = signalObject({
        currentMode: 'user',
        parentMode: null,
    });
    const context = vm.createContext({
        console: {warn() {}},
        Gio,
        GLib,
        Extension: class {},
        Main: {sessionMode},
        global: {
            display,
            stage,
            backend: {
                get_monitor_manager: () => monitorManager,
            },
        },
    });
    vm.runInContext(extensionSource, context, {filename: 'extension.js'});
    const reporter = new context.FocusReporterExtension();

    const runSource = predicate => {
        const match = [...sources].find(([, source]) => predicate(source));
        assert.ok(match, 'expected GLib source was not scheduled');
        const [id, source] = match;
        sources.delete(id);
        source.callback();
    };
    const emit = (object, signal, ...args) => {
        for (const entry of signals.values()) {
            if (entry.object === object && entry.signal === signal)
                entry.callback(object, ...args);
        }
    };
    return {
        reporter,
        calls,
        propertyCalls,
        sources,
        appear() {
            assert.ok(watcher);
            watcher.appeared();
        },
        vanish() {
            assert.ok(watcher);
            watcher.vanished();
        },
        setFocusedPid(pid) {
            focusedPid = pid;
        },
        emitStage(signal, ...args) {
            emit(stage, signal, ...args);
        },
        setSessionMode(currentMode, parentMode = null) {
            sessionMode.currentMode = currentMode;
            sessionMode.parentMode = parentMode;
            emit(sessionMode, 'updated');
        },
        powerSaveChanged() {
            emit(monitorManager, 'power-save-mode-changed', {});
        },
        completePowerSaveMode(mode) {
            const query = [...propertyCalls]
                .reverse()
                .find(call => call.method === 'Get' && !call.completed);
            assert.ok(query, 'expected a DisplayConfig PowerSaveMode query');
            query.completed = true;
            query.complete(null, {deep_unpack: () => [mode]});
        },
        runIdle() {
            runSource(source => source.kind === 'idle');
        },
        runAfter(delay) {
            runSource(source => source.kind === 'timeout' && source.delay === delay);
        },
        callsNamed(method) {
            return calls.filter(call => call.method === method);
        },
        connectedSignalNames() {
            return [...signals.values()].map(entry => entry.signal);
        },
    };
}

function startInitialSet(test, pid) {
    test.reporter.enable();
    test.appear();
    test.runIdle();
    test.runAfter(120);
    const sets = test.callsNamed('SetForegroundProcess');
    assert.equal(sets.length, 1);
    assert.deepEqual(Array.from(sets[0].parameters.value), [pid, 'gnome-shell focus']);
    return sets[0];
}

{
    const test = harness(41);
    const first = startInitialSet(test, 41);
    first.complete();
    test.runAfter(5000);
    const sets = test.callsNamed('SetForegroundProcess');
    assert.equal(sets.length, 2, 'stable focus must be renewed');
    assert.deepEqual(Array.from(sets[1].parameters.value), [41, 'gnome-shell focus']);
    sets[1].complete();
    test.reporter.disable();
    assert.equal(test.callsNamed('ClearForegroundProcess').length, 1);
    assert.equal(test.sources.size, 0, 'disable must remove renewal sources');
}

{
    const test = harness(41);
    const stale = startInitialSet(test, 41);
    test.reporter._setDesired(42, true);
    test.runAfter(120);
    const current = test.callsNamed('SetForegroundProcess')[1];
    current.complete();
    stale.complete();
    assert.equal(
        test.reporter._reported,
        42,
        'an old callback must not overwrite a newer PID',
    );
    test.reporter.disable();
}

{
    const test = harness(null);
    test.reporter.enable();
    test.appear();
    test.runIdle();
    const stale = test.callsNamed('ClearForegroundProcess')[0];
    test.reporter._setDesired(42, true);
    test.runAfter(120);
    test.callsNamed('SetForegroundProcess')[0].complete();
    stale.complete();
    assert.equal(
        test.reporter._reported,
        42,
        'an old Clear callback must not erase a newer Set receipt',
    );
    test.reporter.disable();
}

{
    const test = harness(41);
    const stale = startInitialSet(test, 41);
    test.reporter._setDesired(null, true);
    assert.equal(
        test.callsNamed('ClearForegroundProcess').length,
        1,
        'null focus must clear even while Set is in flight',
    );
    stale.complete();
    test.callsNamed('ClearForegroundProcess')[0].complete();
    assert.equal(test.reporter._reported, null);
    test.reporter.disable();
}

{
    const test = harness(41);
    startInitialSet(test, 41);
    test.reporter.disable();
    assert.equal(
        test.callsNamed('ClearForegroundProcess').length,
        1,
        'disable must clear an in-flight Set',
    );
    assert.equal(test.sources.size, 0);
}

{
    const test = harness(41);
    startInitialSet(test, 41);
    test.reporter._setDesired(null, true);
    test.callsNamed('ClearForegroundProcess')[0].complete({
        matches: () => false,
        toString: () => 'simulated clear failure',
    });
    test.reporter.disable();
    assert.equal(
        test.callsNamed('ClearForegroundProcess').length,
        2,
        'disable must retry a failed in-flight clear without a timer',
    );
    assert.equal(test.sources.size, 0);
}

{
    const test = harness(41);
    const first = startInitialSet(test, 41);
    first.complete({
        matches: () => false,
        toString: () => 'simulated failure',
    });
    assert.ok([...test.sources.values()].some(source => source.delay === 500));
    test.vanish();
    assert.ok(
        ![...test.sources.values()].some(source => source.delay === 500),
        'daemon disappearance must cancel retry',
    );
    test.appear();
    test.runIdle();
    test.runAfter(120);
    assert.equal(
        test.callsNamed('SetForegroundProcess').length,
        2,
        'daemon reappearance must re-report current focus',
    );
    test.reporter.disable();
}

{
    const test = harness(41);
    const first = startInitialSet(test, 41);
    first.complete();
    assert.ok(
        !test.connectedSignalNames().includes('presented'),
        'GJS cannot marshal the raw frame-info pointer from Stage::presented',
    );
    test.emitStage('before-paint', {});
    const started = test.callsNamed('ReportFrameHint');
    assert.equal(started.length, 1);
    assert.deepEqual(Array.from(started[0].parameters.value), ['render-started']);
    started[0].complete();
    test.emitStage('after-paint', {});
    test.runAfter(50);
    const hints = test.callsNamed('ReportFrameHint');
    assert.equal(hints.length, 2);
    assert.deepEqual(Array.from(hints[1].parameters.value), ['render-idle']);
    hints[1].complete();
    test.reporter.disable();
}

{
    const test = harness(41);
    const first = startInitialSet(test, 41);
    first.complete();
    test.completePowerSaveMode(3);
    const blanked = test.callsNamed('ReportFrameHint').at(-1);
    assert.deepEqual(Array.from(blanked.parameters.value), ['display-blanked']);
    blanked.complete();
    assert.ok(
        [...test.sources.values()].some(source => source.delay === 5000),
        'a blank display must schedule an authenticated keepalive',
    );

    test.powerSaveChanged();
    test.completePowerSaveMode(0);
    const unblanked = test.callsNamed('ReportFrameHint').at(-1);
    assert.deepEqual(Array.from(unblanked.parameters.value), ['display-unblanked']);
    unblanked.complete();
    test.reporter.disable();
}

{
    const test = harness(41);
    const first = startInitialSet(test, 41);
    first.complete();
    test.setSessionMode('unlock-dialog');
    const clears = test.callsNamed('ClearForegroundProcess');
    assert.equal(clears.length, 1, 'locking must immediately clear focused workload');
    clears[0].complete();

    const before = test.callsNamed('ReportFrameHint').length;
    test.emitStage('before-paint', {});
    assert.equal(
        test.callsNamed('ReportFrameHint').length,
        before,
        'lock-screen paints must not produce application frame hints',
    );

    test.setSessionMode('user');
    test.runIdle();
    test.runAfter(120);
    assert.equal(
        test.callsNamed('SetForegroundProcess').length,
        2,
        'unlocking must re-establish the focused workload lease',
    );
    test.reporter.disable();
}

console.log('compositor reporter state tests passed');

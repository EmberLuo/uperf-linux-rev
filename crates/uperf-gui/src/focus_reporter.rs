//! Session-bus probe for the bundled GNOME focus reporter.
//!
//! The daemon deliberately cannot see whether a compositor reporter exists: it
//! only ever receives reports. Without this probe the GUI could not tell
//! "nobody is focused" apart from "nothing is reporting", which is the failure
//! users actually hit. Everything here is best effort and read-only apart from
//! the explicit enable request; any absent interface leaves the state
//! [`ReporterState::Unknown`] so non-GNOME desktops show no misleading advice.

use std::collections::HashMap;

use zbus::zvariant::OwnedValue;

use crate::view_model::ReporterState;

/// UUID of the reporter shipped in `extensions/`.
pub const EXTENSION_UUID: &str = "focus@uperflinux.org";

/// GNOME ships the extension surface as a separate activatable service that
/// exits when idle, so every call here activates it on demand instead of
/// holding a proxy open.
const SHELL_SERVICE: &str = "org.gnome.Shell.Extensions";
const SHELL_PATH: &str = "/org/gnome/Shell/Extensions";
const SHELL_INTERFACE: &str = "org.gnome.Shell.Extensions";

/// GNOME Shell `ExtensionState` values that mean the reporter is running or is
/// on its way to running. Everything else installed counts as switched off.
const STATE_ENABLED: i64 = 1;
const STATE_ACTIVATING: i64 = 8;

/// Read the reporter state once.
pub async fn probe(connection: &zbus::Connection) -> ReporterState {
    let Ok(proxy) = extensions_proxy(connection).await else {
        return ReporterState::Unknown;
    };
    let info: Result<HashMap<String, OwnedValue>, _> =
        proxy.call("GetExtensionInfo", &(EXTENSION_UUID)).await;
    match info {
        Ok(info) => classify(&info),
        // A shell that is present but rejects the call tells us nothing useful.
        Err(_) => ReporterState::Unknown,
    }
}

/// Ask GNOME Shell to enable the reporter for the current user.
///
/// # Errors
///
/// Returns the D-Bus failure text when no shell is listening or the shell
/// refuses to enable the extension.
pub async fn enable(connection: &zbus::Connection) -> Result<(), String> {
    let proxy = extensions_proxy(connection)
        .await
        .map_err(|error| error.to_string())?;
    let enabled: bool = proxy
        .call("EnableExtension", &(EXTENSION_UUID))
        .await
        .map_err(|error| error.to_string())?;
    if enabled {
        Ok(())
    } else {
        Err(format!("GNOME Shell refused to enable {EXTENSION_UUID}"))
    }
}

/// Subscribe to state changes for the reporter alone.
///
/// The match rule filters on the UUID argument, so the bus never forwards the
/// churn of every other extension. Subscribing succeeds even while the
/// activatable shell service is idle and unowned: the stream tracks the name and
/// starts delivering once it is activated again.
pub async fn watch(connection: &zbus::Connection) -> Option<zbus::proxy::SignalStream<'static>> {
    let proxy = extensions_proxy(connection).await.ok()?;
    proxy
        .receive_signal_with_args("ExtensionStateChanged", &[(0, EXTENSION_UUID)])
        .await
        .ok()
}

async fn extensions_proxy(connection: &zbus::Connection) -> zbus::Result<zbus::Proxy<'_>> {
    zbus::Proxy::new(connection, SHELL_SERVICE, SHELL_PATH, SHELL_INTERFACE).await
}

/// Map one `GetExtensionInfo` reply onto a reporter state.
///
/// An empty dictionary is GNOME Shell's answer for an unknown UUID, so it means
/// the reporter is not installed for this user rather than that the probe failed.
fn classify(info: &HashMap<String, OwnedValue>) -> ReporterState {
    if info.is_empty() {
        return ReporterState::Missing;
    }
    match info.get("state").and_then(state_number) {
        Some(STATE_ENABLED | STATE_ACTIVATING) => ReporterState::Enabled,
        // Installed but stopped, errored, or out of date: all are "not
        // reporting", and all are resolved by enabling it again.
        Some(_) => ReporterState::Disabled,
        None => ReporterState::Unknown,
    }
}

/// GNOME Shell publishes `state` as a double; older and third-party shells have
/// used integers, so accept every numeric shape rather than guessing one.
fn state_number(value: &OwnedValue) -> Option<i64> {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "extension states are small integers carried in a double"
    )]
    if let Ok(state) = f64::try_from(value) {
        return Some(state as i64);
    }
    if let Ok(state) = i64::try_from(value) {
        return Some(state);
    }
    if let Ok(state) = i32::try_from(value) {
        return Some(i64::from(state));
    }
    u32::try_from(value).ok().map(i64::from)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use zbus::zvariant::{OwnedValue, Str};

    use super::classify;
    use crate::view_model::ReporterState;

    fn info(state: OwnedValue) -> HashMap<String, OwnedValue> {
        HashMap::from([("state".to_owned(), state)])
    }

    #[test]
    fn an_empty_reply_means_the_reporter_is_not_installed() {
        assert_eq!(classify(&HashMap::new()), ReporterState::Missing);
    }

    #[test]
    fn shell_reports_the_state_as_a_double() {
        assert_eq!(
            classify(&info(OwnedValue::from(1.0_f64))),
            ReporterState::Enabled
        );
        assert_eq!(
            classify(&info(OwnedValue::from(2.0_f64))),
            ReporterState::Disabled
        );
    }

    #[test]
    fn integer_states_are_accepted_too() {
        assert_eq!(
            classify(&info(OwnedValue::from(8_u32))),
            ReporterState::Enabled
        );
        assert_eq!(
            classify(&info(OwnedValue::from(3_i32))),
            ReporterState::Disabled,
            "an errored extension is installed but not reporting"
        );
    }

    #[test]
    fn an_installed_extension_without_a_state_field_stays_unknown() {
        let info = HashMap::from([(
            "uuid".to_owned(),
            OwnedValue::from(Str::from_static(super::EXTENSION_UUID)),
        )]);
        assert_eq!(classify(&info), ReporterState::Unknown);
    }
}

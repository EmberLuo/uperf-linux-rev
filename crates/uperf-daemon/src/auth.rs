//! D-Bus peer identity and `PolicyKit` authorization.

use std::collections::HashMap;

use uperf_api::ServiceError;
use zbus::{
    Connection, Proxy,
    fdo::DBusProxy,
    message::Header,
    names::BusName,
    zvariant::{OwnedObjectPath, Str, Value},
};

pub const CONTROL_ACTION: &str = "org.uperflinux.control";
pub const ADMIN_ACTION: &str = "org.uperflinux.admin";
type LogindSession = (String, u32, String, String, OwnedObjectPath);

/// Production authorization uses `PolicyKit`; session-bus development can
/// explicitly trust the local peer while still enforcing workload ownership.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthorizationMode {
    PolicyKit,
    DevelopmentSession,
}

#[derive(Clone, Copy, Debug)]
pub struct Authorizer {
    mode: AuthorizationMode,
}

impl Authorizer {
    #[must_use]
    pub const fn new(mode: AuthorizationMode) -> Self {
        Self { mode }
    }

    /// Resolve the Unix user ID associated with the calling D-Bus peer.
    ///
    /// # Errors
    ///
    /// Returns an authorization error when the message has no sender or when
    /// the bus cannot resolve that sender to a Unix user.
    pub async fn caller_uid(
        &self,
        connection: &Connection,
        header: &Header<'_>,
    ) -> Result<u32, ServiceError> {
        let sender = header
            .sender()
            .ok_or_else(|| ServiceError::NotAuthorized("D-Bus sender is missing".to_owned()))?;
        let proxy = DBusProxy::new(connection)
            .await
            .map_err(|error| ServiceError::Internal(error.to_string()))?;
        proxy
            .get_connection_unix_user(BusName::from(sender.clone()))
            .await
            .map_err(|error| ServiceError::NotAuthorized(error.to_string()))
    }

    /// Require the caller to sit in a local, currently active logind session.
    ///
    /// Focus reporting is not gated by `PolicyKit` because it changes no global
    /// state, so this is what keeps a background or remote session from
    /// steering the boost of whoever is physically at the machine. Root and
    /// development-session peers bypass it so headless verification works.
    ///
    /// # Errors
    ///
    /// Returns an error when the caller cannot be resolved, logind is
    /// unavailable, or the session is not local and active.
    pub async fn require_active_local_session(
        &self,
        connection: &Connection,
        header: &Header<'_>,
    ) -> Result<u32, ServiceError> {
        let uid = self.caller_uid(connection, header).await?;
        if uid == 0 || self.mode == AuthorizationMode::DevelopmentSession {
            return Ok(uid);
        }
        let sender = header
            .sender()
            .ok_or_else(|| ServiceError::NotAuthorized("D-Bus sender is missing".to_owned()))?;
        let bus = DBusProxy::new(connection)
            .await
            .map_err(|error| ServiceError::Internal(error.to_string()))?;
        let caller_pid = bus
            .get_connection_unix_process_id(BusName::from(sender.clone()))
            .await
            .map_err(|error| ServiceError::NotAuthorized(error.to_string()))?;
        let manager = Proxy::new(
            connection,
            "org.freedesktop.login1",
            "/org/freedesktop/login1",
            "org.freedesktop.login1.Manager",
        )
        .await
        .map_err(|error| ServiceError::Unavailable(error.to_string()))?;
        let direct_session: Result<OwnedObjectPath, zbus::Error> =
            manager.call("GetSessionByPID", &(caller_pid,)).await;
        if let Ok(path) = direct_session {
            return if session_is_active_local(connection, path).await? {
                Ok(uid)
            } else {
                Err(ServiceError::NotAuthorized(
                    "only a local, currently active session may report focus".to_owned(),
                ))
            };
        }

        // Modern GNOME Shell runs as a systemd user service under
        // user@UID.service rather than inside session-N.scope, so logind cannot
        // map its PID back with GetSessionByPID. In that case only, accept the
        // peer when the same UID owns a genuinely active, non-remote session.
        // A caller that did map to an inactive or remote session was rejected
        // above and never receives this fallback.
        let sessions: Vec<LogindSession> = manager
            .call("ListSessions", &())
            .await
            .map_err(|error| ServiceError::Unavailable(error.to_string()))?;
        for (_, session_uid, _, _, path) in sessions {
            if session_uid == uid && session_is_active_local(connection, path).await? {
                return Ok(uid);
            }
        }
        Err(ServiceError::NotAuthorized(
            "caller has no local, currently active logind session".to_owned(),
        ))
    }

    /// Require the caller to hold a `PolicyKit` action.
    ///
    /// Root and explicitly configured development-session peers are accepted
    /// without contacting `PolicyKit`.
    ///
    /// # Errors
    ///
    /// Returns an error when the caller identity cannot be resolved, the
    /// authorization service is unavailable, or the requested action is
    /// denied.
    pub async fn require_action(
        &self,
        connection: &Connection,
        header: &Header<'_>,
        action: &str,
    ) -> Result<u32, ServiceError> {
        let uid = self.caller_uid(connection, header).await?;
        if uid == 0 || self.mode == AuthorizationMode::DevelopmentSession {
            return Ok(uid);
        }
        let sender = header
            .sender()
            .ok_or_else(|| ServiceError::NotAuthorized("D-Bus sender is missing".to_owned()))?;
        let proxy = Proxy::new(
            connection,
            "org.freedesktop.PolicyKit1",
            "/org/freedesktop/PolicyKit1/Authority",
            "org.freedesktop.PolicyKit1.Authority",
        )
        .await
        .map_err(|error| ServiceError::Unavailable(error.to_string()))?;
        let subject_details = HashMap::from([("name", Value::from(Str::from(sender.as_str())))]);
        let subject = ("system-bus-name", subject_details);
        let details = HashMap::<&str, &str>::new();
        let (authorized, _challenge, _details): (bool, bool, HashMap<String, String>) = proxy
            .call("CheckAuthorization", &(subject, action, details, 1_u32, ""))
            .await
            .map_err(|error| ServiceError::Unavailable(error.to_string()))?;
        if authorized {
            Ok(uid)
        } else {
            Err(ServiceError::NotAuthorized(format!(
                "PolicyKit denied {action}"
            )))
        }
    }
}

async fn session_is_active_local(
    connection: &Connection,
    path: OwnedObjectPath,
) -> Result<bool, ServiceError> {
    let session = Proxy::new(
        connection,
        "org.freedesktop.login1",
        path.into_inner(),
        "org.freedesktop.login1.Session",
    )
    .await
    .map_err(|error| ServiceError::Unavailable(error.to_string()))?;
    let active: bool = session
        .get_property("Active")
        .await
        .map_err(|error| ServiceError::Unavailable(error.to_string()))?;
    let remote: bool = session
        .get_property("Remote")
        .await
        .map_err(|error| ServiceError::Unavailable(error.to_string()))?;
    Ok(active && !remote)
}

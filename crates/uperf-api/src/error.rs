use thiserror::Error;
use zbus::DBusError;

use crate::ApiVersion;

/// Stable errors emitted by the daemon instead of ambiguous boolean results.
#[derive(Debug, DBusError)]
#[zbus(prefix = "org.uperflinux.Daemon2.Error")]
pub enum ServiceError {
    /// An argument is malformed or outside the advertised capability range.
    InvalidArgument(String),
    /// Client and daemon use different API versions.
    IncompatibleVersion(String),
    /// A target, process, rule, or mode does not exist.
    NotFound(String),
    /// Observed state changed since the request was prepared.
    Conflict(String),
    /// `PolicyKit` or ownership checks denied the operation.
    NotAuthorized(String),
    /// A required kernel or service capability is unavailable.
    Unavailable(String),
    /// The daemon is deliberately read-only after a safety or recovery failure.
    Degraded(String),
    /// Configuration validation failed.
    ValidationFailed(String),
    /// An unexpected internal failure occurred.
    Internal(String),
    /// Transport error used by zbus interface implementations.
    #[zbus(error)]
    ZBus(zbus::Error),
}

/// Errors returned by [`crate::DaemonClient`].
#[derive(Debug, Error)]
pub enum ClientError {
    /// The request failed before reaching a service method.
    #[error("D-Bus transport error: {0}")]
    Transport(#[source] zbus::Error),
    /// The daemon returned a stable named D-Bus error.
    #[error("daemon rejected the request ({name}): {message}")]
    Remote {
        /// Fully qualified D-Bus error name.
        name: String,
        /// Human-readable daemon detail.
        message: String,
    },
    /// The daemon does not speak this client's exact API version.
    #[error("incompatible D-Bus API: client {client}, daemon {server}")]
    IncompatibleApi {
        /// Version implemented by this client.
        client: ApiVersion,
        /// Version returned by the daemon.
        server: ApiVersion,
    },
    /// A client-side invariant rejected the request before sending it.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
}

impl ClientError {
    /// Return the remote D-Bus error name, when the daemon rejected the call.
    #[must_use]
    pub fn remote_name(&self) -> Option<&str> {
        match self {
            Self::Remote { name, .. } => Some(name),
            _ => None,
        }
    }
}

impl From<zbus::Error> for ClientError {
    fn from(error: zbus::Error) -> Self {
        match error {
            zbus::Error::MethodError(name, detail, _) => Self::Remote {
                name: name.to_string(),
                message: detail.unwrap_or_else(|| "no details supplied".into()),
            },
            other => Self::Transport(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use zbus::DBusError as _;

    use super::ServiceError;

    #[test]
    fn service_error_names_are_stable() {
        let error = ServiceError::NotFound("missing".into());
        assert_eq!(
            error.name().as_str(),
            "org.uperflinux.Daemon2.Error.NotFound"
        );
    }
}

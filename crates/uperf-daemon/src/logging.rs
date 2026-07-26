//! Process-wide structured logging.
//!
//! systemd captures the JSON lines written to stderr and adds its own trusted
//! journal metadata. `UPERF_LOG` (or the conventional `RUST_LOG`) controls the
//! filter without changing the daemon's configuration schema.

use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

/// Install the daemon's process-wide JSON subscriber.
///
/// Repeated initialization is harmless, which keeps binary and integration
/// test harnesses composable.
pub fn init() {
    let filter = EnvFilter::try_from_env("UPERF_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("uperf_daemon=info,uperf_linux=info"));
    let subscriber = tracing_subscriber::registry().with(filter).with(
        fmt::layer()
            .json()
            .flatten_event(true)
            .with_ansi(false)
            .with_current_span(false)
            .with_span_list(false)
            .with_target(true),
    );
    let _ = subscriber.try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialization_is_idempotent() {
        init();
        init();
    }
}

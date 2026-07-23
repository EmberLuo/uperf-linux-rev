//! Linux and Ubuntu implementations of the platform ports.
//!
//! Hardware discovery is read-only.  The only writable adapter,
//! [`RootedSysfs`], denies every target unless the actuator explicitly builds
//! it with an exact allowlist derived from [`LinuxDiscovery`].

mod discovery;
mod environment;
mod input;
mod procfs;
mod scheduler;
mod sysfs;
mod systemd;

pub use discovery::{FrequencyTargetPaths, LinuxDiscovery};
pub use environment::{LinuxEnvironment, ProbeReport, SystemInfo, SystemRoots};
pub use input::{
    AxisRange, EvdevInputSource, GestureConfig, RawTouchEvent, TouchAxes, TouchConfigurationError,
    TouchStateMachine,
};
pub use procfs::{LinuxClock, LinuxProc};
pub use scheduler::LinuxProcessController;
pub use sysfs::RootedSysfs;
pub use systemd::SystemdDbusClient;

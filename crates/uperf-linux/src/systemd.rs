//! Typed systemd D-Bus resource-control adapter.
//!
//! systemd remains the only cgroup writer. This module never opens cgroupfs and
//! never migrates a PID. `SystemdClient` is synchronous, so this implementation
//! owns one asynchronous zbus connection on a dedicated current-thread Tokio
//! runtime. Synchronous callers use a bounded request queue; async callers must
//! still invoke the facade from `spawn_blocking` rather than a Tokio core worker.

use std::{
    fmt, io,
    path::{Path, PathBuf},
    sync::mpsc::{self, SyncSender},
    time::Duration,
};

use tokio::sync::mpsc as tokio_mpsc;
use uperf_core::{CpuId, CpuSet, ProcessId};
use uperf_platform::{
    PlatformError, PlatformResult, SystemdClient, SystemdUnitInstanceIdentity,
    SystemdUnitInstanceKey, SystemdUnitProperties,
};
use zbus::{
    Address, Connection, Error as ZbusError, Proxy,
    connection::Builder as ConnectionBuilder,
    zvariant::{OwnedObjectPath, Value},
};

use crate::scheduler::parse_cpu_list;

const SYSTEMD_SERVICE: &str = "org.freedesktop.systemd1";
const SYSTEMD_PATH: &str = "/org/freedesktop/systemd1";
const MANAGER_INTERFACE: &str = "org.freedesktop.systemd1.Manager";
const UNIT_INTERFACE: &str = "org.freedesktop.systemd1.Unit";
const ONLINE_CPUS: &str = "/sys/devices/system/cpu/online";
const MIN_CPU_WEIGHT: u64 = 1;
const MAX_CPU_WEIGHT: u64 = 10_000;
const UNSET_CPU_WEIGHT: u64 = u64::MAX;
const METHOD_TIMEOUT: Duration = Duration::from_millis(50);
const OWNER_QUEUE_CAPACITY: usize = 16;
const OWNER_STACK_SIZE: usize = 512 * 1024;

/// Synchronous facade for a connection owned by one dedicated D-Bus thread.
///
/// The implementation sends only `CPUWeight` (`t`) and `AllowedCPUs` (`ay`)
/// through `SetUnitProperties`. It does not expose an untyped property name or
/// arbitrary variant surface. Every facade clone shares the same bounded queue,
/// so a write, readback, and any required rollback remain one serialized owner
/// request.
#[derive(Clone)]
pub struct SystemdDbusClient {
    requests: tokio_mpsc::Sender<OwnerRequest>,
    online_path: PathBuf,
}

impl fmt::Debug for SystemdDbusClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SystemdDbusClient")
            .field("online_path", &self.online_path)
            .finish_non_exhaustive()
    }
}

impl SystemdDbusClient {
    /// Connect once to the system bus.
    ///
    /// # Errors
    ///
    /// Returns an error if the system bus cannot be reached or the online CPU
    /// list cannot be parsed.
    pub fn connect_system() -> PlatformResult<Self> {
        let online_path = PathBuf::from(ONLINE_CPUS);
        read_online(&online_path)?;
        Self::start_owner(ConnectionSource::System, online_path)
    }

    /// Connect to an explicit bus address.
    ///
    /// This is useful for integration tests on an isolated bus. The connection
    /// is still constructed and exclusively owned by the dedicated owner
    /// thread, so it cannot inherit another Tokio runtime.
    ///
    /// # Errors
    ///
    /// Returns an error if the bus cannot be reached or the host online CPU
    /// list cannot be parsed.
    pub fn from_address(address: Address) -> PlatformResult<Self> {
        let online_path = PathBuf::from(ONLINE_CPUS);
        read_online(&online_path)?;
        Self::start_owner(ConnectionSource::Address(address), online_path)
    }

    fn start_owner(source: ConnectionSource, online_path: PathBuf) -> PlatformResult<Self> {
        let (request_sender, request_receiver) = tokio_mpsc::channel(OWNER_QUEUE_CAPACITY);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let owner_online_path = online_path.clone();
        std::thread::Builder::new()
            .name("uperf-systemd".to_owned())
            .stack_size(OWNER_STACK_SIZE)
            .spawn(move || {
                systemd_owner(source, owner_online_path, request_receiver, ready_sender);
            })
            .map_err(|error| {
                PlatformError::io(
                    "start systemd D-Bus owner",
                    dbus_resource(SYSTEMD_SERVICE),
                    error,
                )
            })?;

        ready_receiver
            .recv()
            .map_err(|_| owner_unavailable("initialize systemd D-Bus owner"))??;
        Ok(Self {
            requests: request_sender,
            online_path,
        })
    }

    fn request<T>(
        &self,
        operation: &'static str,
        request: impl FnOnce(SyncSender<PlatformResult<T>>) -> OwnerRequest,
    ) -> PlatformResult<T> {
        let (reply_sender, reply_receiver) = mpsc::sync_channel(1);
        match self.requests.try_send(request(reply_sender)) {
            Ok(()) => {}
            Err(tokio_mpsc::error::TrySendError::Full(_)) => {
                return Err(owner_queue_full(operation));
            }
            Err(tokio_mpsc::error::TrySendError::Closed(_)) => {
                return Err(owner_unavailable(operation));
            }
        }
        reply_receiver
            .recv()
            .map_err(|_| owner_unavailable(operation))?
    }
}

enum ConnectionSource {
    System,
    Address(Address),
}

enum OwnerRequest {
    UnitInstanceForProcess {
        process: ProcessId,
        reply: SyncSender<PlatformResult<Option<SystemdUnitInstanceIdentity>>>,
    },
    UnitInstanceIdentity {
        unit: String,
        reply: SyncSender<PlatformResult<SystemdUnitInstanceIdentity>>,
    },
    UnitProcesses {
        unit: String,
        reply: SyncSender<PlatformResult<Vec<ProcessId>>>,
    },
    ReadUnitProperties {
        unit: String,
        reply: SyncSender<PlatformResult<SystemdUnitProperties>>,
    },
    WriteUnitProperties {
        unit: String,
        desired: SystemdUnitProperties,
        reply: SyncSender<PlatformResult<SystemdUnitProperties>>,
    },
}

struct SystemdOwner {
    connection: Connection,
    online_path: PathBuf,
}

fn systemd_owner(
    source: ConnectionSource,
    online_path: PathBuf,
    mut requests: tokio_mpsc::Receiver<OwnerRequest>,
    ready: SyncSender<PlatformResult<()>>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = ready.send(Err(PlatformError::io(
                "build systemd D-Bus runtime",
                dbus_resource(SYSTEMD_SERVICE),
                error,
            )));
            return;
        }
    };
    runtime.block_on(async move {
        let builder = match source {
            ConnectionSource::System => ConnectionBuilder::system()
                .map_err(|error| map_dbus_error("configure system bus", error)),
            ConnectionSource::Address(address) => ConnectionBuilder::address(address)
                .map_err(|error| map_dbus_error("configure D-Bus address", error)),
        };
        let builder = match builder {
            Ok(builder) => builder,
            Err(error) => {
                let _ = ready.send(Err(error));
                return;
            }
        };
        let connection = builder
            .method_timeout(METHOD_TIMEOUT)
            .build()
            .await
            .map_err(|error| map_dbus_error("connect D-Bus", error));
        let connection = match connection {
            Ok(connection) => connection,
            Err(error) => {
                let _ = ready.send(Err(error));
                return;
            }
        };
        let owner = SystemdOwner {
            connection,
            online_path,
        };
        if ready.send(Ok(())).is_err() {
            return;
        }
        while let Some(request) = requests.recv().await {
            owner.handle(request).await;
        }
    });
}

impl SystemdOwner {
    async fn handle(&self, request: OwnerRequest) {
        match request {
            OwnerRequest::UnitInstanceForProcess { process, reply } => {
                let result = self.unit_instance_for_process(process).await;
                let _ = reply.send(result);
            }
            OwnerRequest::UnitInstanceIdentity { unit, reply } => {
                let result = self.unit_instance_identity(&unit).await;
                let _ = reply.send(result);
            }
            OwnerRequest::UnitProcesses { unit, reply } => {
                let result = self.unit_processes(&unit).await;
                let _ = reply.send(result);
            }
            OwnerRequest::ReadUnitProperties { unit, reply } => {
                let result = self.read_unit_properties(&unit).await;
                let _ = reply.send(result);
            }
            OwnerRequest::WriteUnitProperties {
                unit,
                desired,
                reply,
            } => {
                let result = self.write_unit_properties(&unit, &desired).await;
                let _ = reply.send(result);
            }
        }
    }

    async fn manager(&self) -> PlatformResult<Proxy<'_>> {
        Proxy::new(
            &self.connection,
            SYSTEMD_SERVICE,
            SYSTEMD_PATH,
            MANAGER_INTERFACE,
        )
        .await
        .map_err(|error| map_dbus_error("create systemd manager proxy", error))
    }

    async fn resolve_unit(&self, unit: &str) -> PlatformResult<ResolvedUnit> {
        let type_interface = validate_unit_name(unit)?;
        let manager = self.manager().await?;
        let path: OwnedObjectPath = manager
            .call("GetUnit", &unit)
            .await
            .map_err(|error| map_dbus_error("resolve systemd unit", error))?;
        let identity = Proxy::new(
            &self.connection,
            SYSTEMD_SERVICE,
            path.as_str(),
            UNIT_INTERFACE,
        )
        .await
        .map_err(|error| map_dbus_error("create systemd unit proxy", error))?;
        let actual: String = identity
            .get_property("Id")
            .await
            .map_err(|error| map_dbus_error("read systemd unit identity", error))?;
        if actual != unit {
            return Err(PlatformError::invalid(
                dbus_resource(unit),
                format!("resolved unit identity `{actual}` differs from `{unit}`"),
            ));
        }
        drop(identity);
        Ok(ResolvedUnit {
            path,
            type_interface,
        })
    }

    async fn instance_identity(
        &self,
        unit: &str,
        path: &OwnedObjectPath,
    ) -> PlatformResult<SystemdUnitInstanceIdentity> {
        let proxy = Proxy::new(
            &self.connection,
            SYSTEMD_SERVICE,
            path.as_str(),
            UNIT_INTERFACE,
        )
        .await
        .map_err(|error| map_dbus_error("create systemd unit identity proxy", error))?;
        read_instance_identity(unit, &proxy).await
    }

    async fn read_raw(&self, unit: &str) -> PlatformResult<RawUnitProperties> {
        let resolved = self.resolve_unit(unit).await?;
        let proxy = Proxy::new(
            &self.connection,
            SYSTEMD_SERVICE,
            resolved.path.as_str(),
            resolved.type_interface,
        )
        .await
        .map_err(|error| map_dbus_error("create typed systemd unit proxy", error))?;
        let cpu_weight = proxy
            .get_property::<u64>("CPUWeight")
            .await
            .map_err(|error| map_dbus_error("read systemd CPUWeight", error))?;
        let allowed_cpus = proxy
            .get_property::<Vec<u8>>("AllowedCPUs")
            .await
            .map_err(|error| map_dbus_error("read systemd AllowedCPUs", error))?;
        Ok(RawUnitProperties {
            cpu_weight,
            allowed_cpus,
        })
    }

    async fn set_raw(
        &self,
        unit: &str,
        cpu_weight: u64,
        allowed_cpus: Vec<u8>,
    ) -> PlatformResult<()> {
        let properties: Vec<(&str, Value<'static>)> = vec![
            ("CPUWeight", Value::from(cpu_weight)),
            ("AllowedCPUs", Value::from(allowed_cpus)),
        ];
        let manager = self.manager().await?;
        manager
            .call::<_, _, ()>("SetUnitProperties", &(unit, true, properties))
            .await
            .map_err(|error| map_dbus_error("set typed systemd unit properties", error))
    }

    fn validate_desired(&self, desired: &SystemdUnitProperties) -> PlatformResult<()> {
        validate_desired_properties(desired, &self.online_path)
    }

    async fn unit_instance_for_process(
        &self,
        process: ProcessId,
    ) -> PlatformResult<Option<SystemdUnitInstanceIdentity>> {
        validate_pid(process)?;
        let manager = self.manager().await?;
        let path: OwnedObjectPath = match manager.call("GetUnitByPID", &process.0).await {
            Ok(path) => path,
            Err(error) if is_no_unit_for_pid(&error) => return Ok(None),
            Err(error) => return Err(map_dbus_error("resolve unit for PID", error)),
        };
        let identity = Proxy::new(
            &self.connection,
            SYSTEMD_SERVICE,
            path.as_str(),
            UNIT_INTERFACE,
        )
        .await
        .map_err(|error| map_dbus_error("create systemd unit identity proxy", error))?;
        let unit: String = identity
            .get_property("Id")
            .await
            .map_err(|error| map_dbus_error("read systemd unit identity", error))?;
        validate_unit_name(&unit)?;
        Ok(Some(read_instance_identity(&unit, &identity).await?))
    }

    async fn unit_instance_identity(
        &self,
        unit: &str,
    ) -> PlatformResult<SystemdUnitInstanceIdentity> {
        let resolved = self.resolve_unit(unit).await?;
        self.instance_identity(unit, &resolved.path).await
    }

    async fn unit_processes(&self, unit: &str) -> PlatformResult<Vec<ProcessId>> {
        validate_unit_name(unit)?;
        let manager = self.manager().await?;
        let processes: Vec<(String, u32, String)> =
            manager
                .call("GetUnitProcesses", &unit)
                .await
                .map_err(|error| map_dbus_error("enumerate systemd unit processes", error))?;
        let mut ids = processes
            .into_iter()
            .map(|(_, pid, _)| ProcessId(pid))
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        Ok(ids)
    }

    async fn read_unit_properties(&self, unit: &str) -> PlatformResult<SystemdUnitProperties> {
        raw_to_domain(unit, &self.read_raw(unit).await?)
    }

    async fn write_unit_properties(
        &self,
        unit: &str,
        desired: &SystemdUnitProperties,
    ) -> PlatformResult<SystemdUnitProperties> {
        validate_unit_name(unit)?;
        self.validate_desired(desired)?;
        let original_raw = self.read_raw(unit).await?;
        let desired_raw = domain_to_raw(desired);
        self.set_raw(
            unit,
            desired_raw.cpu_weight,
            desired_raw.allowed_cpus.clone(),
        )
        .await?;

        let readback = match self.read_unit_properties(unit).await {
            Ok(readback) => readback,
            Err(error) => {
                let rollback = self
                    .set_raw(
                        unit,
                        original_raw.cpu_weight,
                        original_raw.allowed_cpus.clone(),
                    )
                    .await;
                return Err(with_systemd_rollback(unit, error, rollback));
            }
        };
        if !unit_matches_desired(&readback, desired) {
            let failure = PlatformError::invalid(
                dbus_resource(unit),
                format!(
                    "systemd readback differs from request: requested {desired:?}, got {readback:?}"
                ),
            );
            let rollback = self
                .set_raw(unit, original_raw.cpu_weight, original_raw.allowed_cpus)
                .await;
            return Err(with_systemd_rollback(unit, failure, rollback));
        }
        Ok(readback)
    }
}

impl SystemdClient for SystemdDbusClient {
    fn unit_for_process(&self, process: ProcessId) -> PlatformResult<Option<String>> {
        Ok(self
            .unit_instance_for_process(process)?
            .map(|identity| identity.unit))
    }

    fn unit_instance_for_process(
        &self,
        process: ProcessId,
    ) -> PlatformResult<Option<SystemdUnitInstanceIdentity>> {
        self.request("resolve unit for PID", |reply| {
            OwnerRequest::UnitInstanceForProcess { process, reply }
        })
    }

    fn unit_instance_identity(&self, unit: &str) -> PlatformResult<SystemdUnitInstanceIdentity> {
        self.request("read systemd unit identity", |reply| {
            OwnerRequest::UnitInstanceIdentity {
                unit: unit.to_owned(),
                reply,
            }
        })
    }

    fn unit_processes(&self, unit: &str) -> PlatformResult<Vec<ProcessId>> {
        self.request("enumerate systemd unit processes", |reply| {
            OwnerRequest::UnitProcesses {
                unit: unit.to_owned(),
                reply,
            }
        })
    }

    fn read_unit_properties(&self, unit: &str) -> PlatformResult<SystemdUnitProperties> {
        self.request("read systemd unit properties", |reply| {
            OwnerRequest::ReadUnitProperties {
                unit: unit.to_owned(),
                reply,
            }
        })
    }

    fn write_unit_properties(
        &self,
        unit: &str,
        desired: &SystemdUnitProperties,
    ) -> PlatformResult<SystemdUnitProperties> {
        self.request("write systemd unit properties", |reply| {
            OwnerRequest::WriteUnitProperties {
                unit: unit.to_owned(),
                desired: desired.clone(),
                reply,
            }
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RawUnitProperties {
    cpu_weight: u64,
    allowed_cpus: Vec<u8>,
}

#[derive(Debug)]
struct ResolvedUnit {
    path: OwnedObjectPath,
    type_interface: &'static str,
}

async fn read_instance_identity(
    unit: &str,
    proxy: &Proxy<'_>,
) -> PlatformResult<SystemdUnitInstanceIdentity> {
    let invocation_id = match proxy.get_property::<Vec<u8>>("InvocationID").await {
        Ok(value) => Some(value),
        Err(error) if is_unknown_property(&error) => None,
        Err(error) => {
            return Err(map_dbus_error("read systemd unit InvocationID", error));
        }
    };
    let control_group = if invocation_id
        .as_deref()
        .is_none_or(|value| value.iter().all(|byte| *byte == 0))
    {
        Some(
            proxy
                .get_property::<String>("ControlGroup")
                .await
                .map_err(|error| map_dbus_error("read systemd unit ControlGroup", error))?,
        )
    } else {
        None
    };
    select_instance_identity(unit, invocation_id.as_deref(), control_group.as_deref())
}

fn select_instance_identity(
    unit: &str,
    invocation_id: Option<&[u8]>,
    control_group: Option<&str>,
) -> PlatformResult<SystemdUnitInstanceIdentity> {
    validate_unit_name(unit)?;
    let key = match invocation_id {
        Some(bytes) if bytes.iter().any(|byte| *byte != 0) => {
            let invocation_id: [u8; 16] = bytes.try_into().map_err(|_| {
                PlatformError::invalid(
                    dbus_resource(unit),
                    format!(
                        "systemd InvocationID must contain exactly 16 bytes, got {}",
                        bytes.len()
                    ),
                )
            })?;
            SystemdUnitInstanceKey::InvocationId(invocation_id)
        }
        _ => {
            let control_group = control_group.ok_or_else(|| {
                PlatformError::invalid(
                    dbus_resource(unit),
                    "systemd exposes neither a nonzero InvocationID nor ControlGroup",
                )
            })?;
            validate_control_group(unit, control_group)?;
            SystemdUnitInstanceKey::ControlGroup(control_group.to_owned())
        }
    };
    Ok(SystemdUnitInstanceIdentity {
        unit: unit.to_owned(),
        key,
    })
}

fn raw_to_domain(unit: &str, raw: &RawUnitProperties) -> PlatformResult<SystemdUnitProperties> {
    let cpu_weight = match raw.cpu_weight {
        0 | u64::MAX => None,
        value if (MIN_CPU_WEIGHT..=MAX_CPU_WEIGHT).contains(&value) => Some(value),
        value => {
            return Err(PlatformError::invalid(
                dbus_resource(unit),
                format!("systemd returned invalid CPUWeight {value}"),
            ));
        }
    };
    let allowed_cpus = if raw.allowed_cpus.is_empty() {
        None
    } else {
        Some(decode_cpu_mask(unit, &raw.allowed_cpus)?)
    };
    Ok(SystemdUnitProperties {
        cpu_weight,
        allowed_cpus,
    })
}

fn domain_to_raw(properties: &SystemdUnitProperties) -> RawUnitProperties {
    RawUnitProperties {
        cpu_weight: properties.cpu_weight.unwrap_or(UNSET_CPU_WEIGHT),
        allowed_cpus: properties
            .allowed_cpus
            .as_ref()
            .map_or_else(Vec::new, encode_cpu_mask),
    }
}

fn unit_matches_desired(actual: &SystemdUnitProperties, desired: &SystemdUnitProperties) -> bool {
    actual == desired
}

fn validate_desired_properties(
    desired: &SystemdUnitProperties,
    online_path: &Path,
) -> PlatformResult<()> {
    if let Some(weight) = desired.cpu_weight
        && !(MIN_CPU_WEIGHT..=MAX_CPU_WEIGHT).contains(&weight)
    {
        return Err(PlatformError::invalid(
            "dbus/systemd/CPUWeight",
            format!("CPUWeight {weight} is outside {MIN_CPU_WEIGHT}..={MAX_CPU_WEIGHT}"),
        ));
    }
    if let Some(cpus) = &desired.allowed_cpus {
        if cpus.is_empty() {
            return Err(PlatformError::invalid(
                "dbus/systemd/AllowedCPUs",
                "AllowedCPUs must not be empty",
            ));
        }
        let online = read_online(online_path)?;
        if !cpus.is_subset(&online) {
            return Err(PlatformError::invalid(
                online_path,
                "AllowedCPUs contains an offline or nonexistent CPU",
            ));
        }
    }
    Ok(())
}

fn encode_cpu_mask(cpus: &CpuSet) -> Vec<u8> {
    let Some(maximum) = cpus.iter().next_back() else {
        return Vec::new();
    };
    let byte_count = usize::try_from(maximum.0 / 8 + 1).unwrap_or(usize::MAX);
    let mut bytes = vec![0; byte_count];
    for cpu in cpus {
        let byte = usize::try_from(cpu.0 / 8).expect("u32 CPU byte index fits usize on Linux");
        bytes[byte] |= 1_u8 << (cpu.0 % 8);
    }
    bytes
}

fn decode_cpu_mask(unit: &str, bytes: &[u8]) -> PlatformResult<CpuSet> {
    let mut cpus = CpuSet::new();
    for (byte_index, byte) in bytes.iter().copied().enumerate() {
        for bit in 0_u32..8 {
            if byte & (1_u8 << bit) == 0 {
                continue;
            }
            let byte_index = u32::try_from(byte_index).map_err(|error| {
                PlatformError::invalid(
                    dbus_resource(unit),
                    format!("AllowedCPUs mask is too large: {error}"),
                )
            })?;
            let cpu = byte_index
                .checked_mul(8)
                .and_then(|base| base.checked_add(bit))
                .ok_or_else(|| {
                    PlatformError::invalid(dbus_resource(unit), "AllowedCPUs CPU index overflow")
                })?;
            cpus.insert(CpuId(cpu));
        }
    }
    if cpus.is_empty() {
        return Err(PlatformError::invalid(
            dbus_resource(unit),
            "nonempty AllowedCPUs byte array contains no CPUs",
        ));
    }
    Ok(cpus)
}

#[allow(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "systemd unit type suffixes are protocol identifiers and are case-sensitive"
)]
fn validate_unit_name(unit: &str) -> PlatformResult<&'static str> {
    if unit.is_empty() || unit.len() > 256 {
        return Err(PlatformError::invalid(
            dbus_resource(unit),
            "systemd unit name must contain 1..=256 bytes",
        ));
    }
    if !unit.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'_' | b'-' | b'.' | b'@' | b'\\')
    }) {
        return Err(PlatformError::invalid(
            dbus_resource(unit),
            "systemd unit name contains a forbidden character",
        ));
    }
    if unit.ends_with(".service") {
        Ok("org.freedesktop.systemd1.Service")
    } else if unit.ends_with(".scope") {
        Ok("org.freedesktop.systemd1.Scope")
    } else if unit.ends_with(".slice") {
        Ok("org.freedesktop.systemd1.Slice")
    } else {
        Err(PlatformError::Unsupported(
            "only systemd service, scope, and slice units support workload controls",
        ))
    }
}

fn validate_pid(process: ProcessId) -> PlatformResult<()> {
    if process.0 == 0 || process.0 > i32::MAX as u32 {
        Err(PlatformError::invalid(
            dbus_resource("GetUnitByPID"),
            format!(
                "PID {} is outside the positive Linux pid_t range",
                process.0
            ),
        ))
    } else {
        Ok(())
    }
}

fn validate_control_group(unit: &str, control_group: &str) -> PlatformResult<()> {
    let invalid_component = control_group.strip_prefix('/').is_none_or(|relative| {
        relative.is_empty()
            || relative.len() > 4095
            || relative
                .split('/')
                .any(|component| component.is_empty() || matches!(component, "." | ".."))
    });
    if invalid_component || control_group.contains('\0') {
        return Err(PlatformError::invalid(
            dbus_resource(unit),
            format!("systemd returned invalid ControlGroup `{control_group}`"),
        ));
    }
    Ok(())
}

fn read_online(path: &Path) -> PlatformResult<CpuSet> {
    let contents = std::fs::read_to_string(path)
        .map_err(|error| PlatformError::io("read online CPUs", path, error))?;
    parse_cpu_list(path, &contents)
}

fn is_no_unit_for_pid(error: &ZbusError) -> bool {
    matches!(
        error,
        ZbusError::MethodError(name, _, _)
            if name.as_str() == "org.freedesktop.systemd1.NoUnitForPID"
    )
}

fn is_unknown_property(error: &ZbusError) -> bool {
    matches!(
        error,
        ZbusError::FDO(error) if matches!(error.as_ref(), zbus::fdo::Error::UnknownProperty(_))
    ) || matches!(
        error,
        ZbusError::MethodError(name, _, _)
            if name.as_str() == "org.freedesktop.DBus.Error.UnknownProperty"
    )
}

fn with_systemd_rollback(
    unit: &str,
    failure: PlatformError,
    rollback: PlatformResult<()>,
) -> PlatformError {
    match rollback {
        Ok(()) => failure,
        Err(rollback) => PlatformError::invalid(
            dbus_resource(unit),
            format!(
                "systemd property transaction failed ({failure}); rollback also failed: {rollback}"
            ),
        ),
    }
}

fn map_dbus_error(operation: &'static str, error: ZbusError) -> PlatformError {
    PlatformError::io(
        operation,
        dbus_resource(SYSTEMD_SERVICE),
        io::Error::other(error),
    )
}

fn owner_unavailable(operation: &'static str) -> PlatformError {
    PlatformError::io(
        operation,
        dbus_resource(SYSTEMD_SERVICE),
        io::Error::new(
            io::ErrorKind::BrokenPipe,
            "systemd D-Bus owner thread is unavailable",
        ),
    )
}

fn owner_queue_full(operation: &'static str) -> PlatformError {
    PlatformError::io(
        operation,
        dbus_resource(SYSTEMD_SERVICE),
        io::Error::new(
            io::ErrorKind::WouldBlock,
            "systemd D-Bus owner request queue is full",
        ),
    )
}

fn dbus_resource(name: &str) -> PathBuf {
    PathBuf::from("dbus").join(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synchronous_facade_is_send_and_sync() {
        const fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SystemdDbusClient>();
    }

    #[test]
    fn owner_reports_connection_failure_during_startup() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let address = format!(
            "unix:path={}",
            directory.path().join("missing-system-bus").display()
        )
        .parse()
        .expect("test bus address");

        let error =
            SystemdDbusClient::from_address(address).expect_err("missing bus socket must fail");
        assert!(matches!(error, PlatformError::Io { .. }));
    }

    #[test]
    fn disconnected_owner_fails_without_falling_back_to_another_runtime() {
        let (requests, receiver) = tokio_mpsc::channel(1);
        drop(receiver);
        let client = SystemdDbusClient {
            requests,
            online_path: PathBuf::from(ONLINE_CPUS),
        };

        let error = client
            .unit_processes("game.scope")
            .expect_err("closed owner queue must fail");
        assert!(matches!(
            error,
            PlatformError::Io { source, .. } if source.kind() == io::ErrorKind::BrokenPipe
        ));
    }

    #[test]
    fn full_owner_queue_fails_without_blocking_send() {
        let (requests, _receiver) = tokio_mpsc::channel(1);
        let (reply, _reply_receiver) = mpsc::sync_channel(1);
        assert!(
            requests
                .try_send(OwnerRequest::UnitProcesses {
                    unit: "queued.scope".to_owned(),
                    reply,
                })
                .is_ok()
        );
        let client = SystemdDbusClient {
            requests,
            online_path: PathBuf::from(ONLINE_CPUS),
        };

        let error = client
            .unit_processes("game.scope")
            .expect_err("full owner queue must fail");
        assert!(matches!(
            error,
            PlatformError::Io { source, .. } if source.kind() == io::ErrorKind::WouldBlock
        ));
    }

    #[test]
    fn cpu_mask_round_trip_supports_sparse_high_ids() {
        let expected: CpuSet = [CpuId(0), CpuId(7), CpuId(8), CpuId(65), CpuId(511)]
            .into_iter()
            .collect();
        let bytes = encode_cpu_mask(&expected);
        assert_eq!(bytes.len(), 64);
        assert_eq!(decode_cpu_mask("game.scope", &bytes).unwrap(), expected);
    }

    #[test]
    fn empty_mask_is_the_systemd_unset_representation() {
        let properties = raw_to_domain(
            "game.scope",
            &RawUnitProperties {
                cpu_weight: 100,
                allowed_cpus: Vec::new(),
            },
        )
        .unwrap();
        assert_eq!(properties.cpu_weight, Some(100));
        assert_eq!(properties.allowed_cpus, None);
    }

    #[test]
    fn none_encodes_an_explicit_property_reset() {
        let raw = domain_to_raw(&SystemdUnitProperties {
            cpu_weight: None,
            allowed_cpus: None,
        });
        assert_eq!(
            raw,
            RawUnitProperties {
                cpu_weight: UNSET_CPU_WEIGHT,
                allowed_cpus: Vec::new(),
            }
        );
        assert_eq!(
            raw_to_domain("game.scope", &raw).expect("decode reset"),
            SystemdUnitProperties {
                cpu_weight: None,
                allowed_cpus: None,
            }
        );
    }

    #[test]
    fn desired_none_does_not_match_an_existing_property() {
        assert!(!unit_matches_desired(
            &SystemdUnitProperties {
                cpu_weight: Some(100),
                allowed_cpus: Some([CpuId(0)].into_iter().collect()),
            },
            &SystemdUnitProperties {
                cpu_weight: None,
                allowed_cpus: None,
            },
        ));
    }

    #[test]
    fn rejects_invalid_or_unsupported_unit_names() {
        assert!(validate_unit_name("../../evil.service").is_err());
        assert!(validate_unit_name("game.mount").is_err());
        assert!(validate_unit_name("game scope.scope").is_err());
        assert_eq!(
            validate_unit_name("app-flatpak-game.scope").unwrap(),
            "org.freedesktop.systemd1.Scope"
        );
    }

    #[test]
    fn invocation_id_is_preferred_over_control_group() {
        let first =
            select_instance_identity("game.scope", Some(&[1; 16]), Some("/user.slice/game.scope"))
                .unwrap();
        let second =
            select_instance_identity("game.scope", Some(&[2; 16]), Some("/user.slice/game.scope"))
                .unwrap();
        assert_eq!(first.key, SystemdUnitInstanceKey::InvocationId([1; 16]));
        assert_ne!(first, second, "same-name activations must be distinct");
    }

    #[test]
    fn control_group_is_used_when_invocation_id_is_unavailable() {
        let missing =
            select_instance_identity("game.scope", None, Some("/user.slice/game.scope")).unwrap();
        let zero =
            select_instance_identity("game.scope", Some(&[0; 16]), Some("/user.slice/game.scope"))
                .unwrap();
        assert_eq!(
            missing.key,
            SystemdUnitInstanceKey::ControlGroup("/user.slice/game.scope".to_owned())
        );
        assert_eq!(missing, zero);
    }

    #[test]
    fn rejects_malformed_instance_identity() {
        assert!(
            select_instance_identity("game.scope", Some(&[1; 15]), Some("/user.slice/game.scope"))
                .is_err()
        );
        assert!(select_instance_identity("game.scope", None, None).is_err());
        for control_group in [
            "",
            "/",
            "user.slice/game.scope",
            "/user.slice//game.scope",
            "/user.slice/../game.scope",
        ] {
            assert!(
                select_instance_identity("game.scope", None, Some(control_group)).is_err(),
                "{control_group:?} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_invalid_weight_and_empty_cpuset_without_dbus() {
        let directory = tempfile::tempdir().unwrap();
        let online = directory.path().join("online");
        std::fs::write(&online, "0-7\n").unwrap();
        assert!(
            validate_desired_properties(
                &SystemdUnitProperties {
                    cpu_weight: Some(0),
                    allowed_cpus: None,
                },
                &online,
            )
            .is_err()
        );
        assert!(
            validate_desired_properties(
                &SystemdUnitProperties {
                    cpu_weight: None,
                    allowed_cpus: Some(CpuSet::new()),
                },
                &online,
            )
            .is_err()
        );
        assert!(
            validate_desired_properties(
                &SystemdUnitProperties {
                    cpu_weight: Some(100),
                    allowed_cpus: Some([CpuId(0), CpuId(9)].into_iter().collect()),
                },
                &online,
            )
            .is_err()
        );
    }
}

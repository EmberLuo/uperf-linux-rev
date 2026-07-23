//! Typed systemd D-Bus resource-control adapter.
//!
//! systemd remains the only cgroup writer. This module never opens cgroupfs and
//! never migrates a PID. `SystemdClient` is synchronous, so this implementation
//! deliberately uses one reusable blocking zbus connection; async callers must
//! invoke it from `spawn_blocking` rather than a Tokio core worker.

use std::{
    fmt, io,
    path::{Path, PathBuf},
    time::Duration,
};

use uperf_core::{CpuId, CpuSet, ProcessId};
use uperf_platform::{
    PlatformError, PlatformResult, SystemdClient, SystemdUnitInstanceIdentity,
    SystemdUnitInstanceKey, SystemdUnitProperties,
};
use zbus::{
    Error as ZbusError,
    blocking::{Connection, Proxy, connection::Builder as ConnectionBuilder},
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

/// Reusable blocking connection to systemd's system-bus manager.
///
/// The implementation sends only `CPUWeight` (`t`) and `AllowedCPUs` (`ay`)
/// through `SetUnitProperties`. It does not expose an untyped property name or
/// arbitrary variant surface.
#[derive(Clone)]
pub struct SystemdDbusClient {
    connection: Connection,
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
        let connection = ConnectionBuilder::system()
            .and_then(|builder| builder.method_timeout(METHOD_TIMEOUT).build())
            .map_err(|error| map_dbus_error("connect system bus", error))?;
        Ok(Self {
            connection,
            online_path,
        })
    }

    /// Construct from an already-open connection.
    ///
    /// This is useful for integration tests on an isolated bus. The connection
    /// is cloned cheaply and retained for every call.
    ///
    /// # Errors
    ///
    /// Returns an error if the host online CPU list cannot be parsed.
    pub fn from_connection(connection: &Connection) -> PlatformResult<Self> {
        let online_path = PathBuf::from(ONLINE_CPUS);
        read_online(&online_path)?;
        Ok(Self {
            connection: connection.clone(),
            online_path,
        })
    }

    fn manager(&self) -> PlatformResult<Proxy<'_>> {
        Proxy::new(
            &self.connection,
            SYSTEMD_SERVICE,
            SYSTEMD_PATH,
            MANAGER_INTERFACE,
        )
        .map_err(|error| map_dbus_error("create systemd manager proxy", error))
    }

    fn resolve_unit(&self, unit: &str) -> PlatformResult<ResolvedUnit> {
        let type_interface = validate_unit_name(unit)?;
        let manager = self.manager()?;
        let path: OwnedObjectPath = manager
            .call("GetUnit", &unit)
            .map_err(|error| map_dbus_error("resolve systemd unit", error))?;
        let identity = Proxy::new(
            &self.connection,
            SYSTEMD_SERVICE,
            path.as_str(),
            UNIT_INTERFACE,
        )
        .map_err(|error| map_dbus_error("create systemd unit proxy", error))?;
        let actual: String = identity
            .get_property("Id")
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

    fn instance_identity(
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
        .map_err(|error| map_dbus_error("create systemd unit identity proxy", error))?;
        read_instance_identity(unit, &proxy)
    }

    fn read_raw(&self, unit: &str) -> PlatformResult<RawUnitProperties> {
        let resolved = self.resolve_unit(unit)?;
        let proxy = Proxy::new(
            &self.connection,
            SYSTEMD_SERVICE,
            resolved.path.as_str(),
            resolved.type_interface,
        )
        .map_err(|error| map_dbus_error("create typed systemd unit proxy", error))?;
        let cpu_weight = proxy
            .get_property::<u64>("CPUWeight")
            .map_err(|error| map_dbus_error("read systemd CPUWeight", error))?;
        let allowed_cpus = proxy
            .get_property::<Vec<u8>>("AllowedCPUs")
            .map_err(|error| map_dbus_error("read systemd AllowedCPUs", error))?;
        Ok(RawUnitProperties {
            cpu_weight,
            allowed_cpus,
        })
    }

    fn set_raw(&self, unit: &str, cpu_weight: u64, allowed_cpus: Vec<u8>) -> PlatformResult<()> {
        let properties: Vec<(&str, Value<'static>)> = vec![
            ("CPUWeight", Value::from(cpu_weight)),
            ("AllowedCPUs", Value::from(allowed_cpus)),
        ];
        let manager = self.manager()?;
        manager
            .call::<_, _, ()>("SetUnitProperties", &(unit, true, properties))
            .map_err(|error| map_dbus_error("set typed systemd unit properties", error))
    }

    fn validate_desired(&self, desired: &SystemdUnitProperties) -> PlatformResult<()> {
        validate_desired_properties(desired, &self.online_path)
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
        validate_pid(process)?;
        let manager = self.manager()?;
        let path: OwnedObjectPath = match manager.call("GetUnitByPID", &process.0) {
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
        .map_err(|error| map_dbus_error("create systemd unit identity proxy", error))?;
        let unit: String = identity
            .get_property("Id")
            .map_err(|error| map_dbus_error("read systemd unit identity", error))?;
        validate_unit_name(&unit)?;
        Ok(Some(read_instance_identity(&unit, &identity)?))
    }

    fn unit_instance_identity(&self, unit: &str) -> PlatformResult<SystemdUnitInstanceIdentity> {
        let resolved = self.resolve_unit(unit)?;
        self.instance_identity(unit, &resolved.path)
    }

    fn unit_processes(&self, unit: &str) -> PlatformResult<Vec<ProcessId>> {
        validate_unit_name(unit)?;
        let manager = self.manager()?;
        let processes: Vec<(String, u32, String)> = manager
            .call("GetUnitProcesses", &unit)
            .map_err(|error| map_dbus_error("enumerate systemd unit processes", error))?;
        let mut ids = processes
            .into_iter()
            .map(|(_, pid, _)| ProcessId(pid))
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        Ok(ids)
    }

    fn read_unit_properties(&self, unit: &str) -> PlatformResult<SystemdUnitProperties> {
        raw_to_domain(unit, &self.read_raw(unit)?)
    }

    fn write_unit_properties(
        &self,
        unit: &str,
        desired: &SystemdUnitProperties,
    ) -> PlatformResult<SystemdUnitProperties> {
        validate_unit_name(unit)?;
        self.validate_desired(desired)?;
        let original_raw = self.read_raw(unit)?;
        let desired_raw = domain_to_raw(desired);
        self.set_raw(
            unit,
            desired_raw.cpu_weight,
            desired_raw.allowed_cpus.clone(),
        )?;

        let readback = match self.read_unit_properties(unit) {
            Ok(readback) => readback,
            Err(error) => {
                let rollback = self.set_raw(
                    unit,
                    original_raw.cpu_weight,
                    original_raw.allowed_cpus.clone(),
                );
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
            let rollback = self.set_raw(unit, original_raw.cpu_weight, original_raw.allowed_cpus);
            return Err(with_systemd_rollback(unit, failure, rollback));
        }
        Ok(readback)
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

fn read_instance_identity(
    unit: &str,
    proxy: &Proxy<'_>,
) -> PlatformResult<SystemdUnitInstanceIdentity> {
    let invocation_id = match proxy.get_property::<Vec<u8>>("InvocationID") {
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

fn dbus_resource(name: &str) -> PathBuf {
    PathBuf::from("dbus").join(name)
}

#[cfg(test)]
mod tests {
    use super::*;

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

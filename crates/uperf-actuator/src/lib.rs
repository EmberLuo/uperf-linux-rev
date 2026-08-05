//! Transactional mutations, durable journaling and crash recovery.
//!
//! This crate is the only layer allowed to turn desired frequency ranges and
//! typed scalar values into machine mutations. It deliberately accepts
//! discovered logical targets instead of arbitrary paths from API clients.
//! While a frequency target is journaled, its user-facing min/max request
//! nodes are exclusively owned by this actuator; effective sysfs values cannot
//! distinguish direct writes from constraints owned by other kernel clients.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    thread,
    time::{Duration, Instant},
};

use crc32fast::hash as crc32;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uperf_core::{
    CpuId, CpuSet, FrequencyLimits, Hertz, ProcessId, ProcessIdentity, SchedulingClass, TargetId,
};
use uperf_platform::{
    PlatformError, ProcReader, ProcessController, ProcessSchedulingState, StateStore, SysfsIo,
    SystemdClient, SystemdUnitInstanceIdentity, SystemdUnitInstanceKey, SystemdUnitProperties,
};

// v6 is the only accepted recovery-journal contract.
const JOURNAL_SCHEMA_VERSION: u32 = 6;
const MAX_JOURNAL_PAYLOAD_BYTES: usize = 1024 * 1024;
const MAX_JOURNAL_ENVELOPE_BYTES: usize = 5 * 1024 * 1024;
const MAX_SCALAR_STRING_BYTES: usize = 256;
const MAX_SCALAR_CPU_ID: u32 = 4095;
const FREQUENCY_SETTLE_TIMEOUT: Duration = Duration::from_millis(50);
const FREQUENCY_SETTLE_POLL_INTERVAL: Duration = Duration::from_millis(2);

/// Atomic, durable journal file store.
///
/// The temporary file is created beside the destination, flushed, renamed,
/// and followed by a parent-directory `fsync`.  Consequently a successful
/// return means the pre-mutation journal survives both daemon and host crashes.
#[derive(Clone, Debug)]
pub struct FileStateStore {
    path: PathBuf,
}

impl FileStateStore {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn parent(&self) -> Result<&Path, PlatformError> {
        self.path.parent().ok_or_else(|| {
            PlatformError::invalid(&self.path, "journal path does not have a parent directory")
        })
    }

    fn sync_parent(&self) -> Result<(), PlatformError> {
        let parent = self.parent()?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| PlatformError::io("fsync journal directory", parent, source))
    }
}

impl StateStore for FileStateStore {
    fn load(&self) -> Result<Option<Vec<u8>>, PlatformError> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(PlatformError::io("open journal", &self.path, source)),
        };
        let mut bytes = Vec::new();
        file.take((MAX_JOURNAL_ENVELOPE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|source| PlatformError::io("read journal", &self.path, source))?;
        if bytes.len() > MAX_JOURNAL_ENVELOPE_BYTES {
            return Err(PlatformError::invalid(
                &self.path,
                format!("journal exceeds {MAX_JOURNAL_ENVELOPE_BYTES} bytes"),
            ));
        }
        Ok(Some(bytes))
    }

    fn store_durable(&self, bytes: &[u8]) -> Result<(), PlatformError> {
        let parent = self.parent()?;
        fs::create_dir_all(parent)
            .map_err(|source| PlatformError::io("create journal directory", parent, source))?;
        let temporary = self.path.with_extension("json.new");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)
            .map_err(|source| PlatformError::io("open journal temporary", &temporary, source))?;
        file.write_all(bytes)
            .map_err(|source| PlatformError::io("write journal temporary", &temporary, source))?;
        file.sync_all()
            .map_err(|source| PlatformError::io("fsync journal temporary", &temporary, source))?;
        fs::rename(&temporary, &self.path)
            .map_err(|source| PlatformError::io("replace journal", &self.path, source))?;
        self.sync_parent()
    }

    fn remove_durable(&self) -> Result<(), PlatformError> {
        match fs::remove_file(&self.path) {
            Ok(()) => self.sync_parent(),
            Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(PlatformError::io("remove journal", &self.path, source)),
        }
    }
}

/// A frequency target resolved from root-owned device configuration and live
/// hardware discovery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrequencyTarget {
    /// Stable API/configuration identifier.
    pub id: TargetId,
    /// Kernel minimum-frequency attribute.
    pub min_path: PathBuf,
    /// Kernel maximum-frequency attribute.
    pub max_path: PathBuf,
    /// Hardware-supported lower bound.
    pub hardware_min: Hertz,
    /// Hardware-supported upper bound.
    pub hardware_max: Hertz,
    /// Sorted unique operating points.
    pub opps: Vec<Hertz>,
    /// Number of hertz represented by one integer in the kernel attribute.
    ///
    /// cpufreq normally uses kHz (`1000`), while devfreq normally uses Hz (`1`).
    pub hertz_per_unit: u64,
}

impl FrequencyTarget {
    /// Construct and validate a discovered frequency target.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe sysfs paths, reversed hardware bounds, or
    /// operating points outside those bounds.
    pub fn new(
        id: TargetId,
        min_path: impl Into<PathBuf>,
        max_path: impl Into<PathBuf>,
        hardware_min: Hertz,
        hardware_max: Hertz,
        mut opps: Vec<Hertz>,
    ) -> Result<Self, ActuatorError> {
        let min_path = min_path.into();
        let max_path = max_path.into();
        validate_sysfs_path(&min_path)?;
        validate_sysfs_path(&max_path)?;
        if min_path == max_path {
            return Err(ActuatorError::InvalidTarget(format!(
                "{id}: minimum and maximum attributes must be distinct"
            )));
        }
        if hardware_min > hardware_max {
            return Err(ActuatorError::InvalidLimits {
                target: id.to_string(),
                minimum: hardware_min.get(),
                maximum: hardware_max.get(),
            });
        }
        opps.sort_unstable();
        opps.dedup();
        if opps
            .iter()
            .any(|opp| *opp < hardware_min || *opp > hardware_max)
        {
            return Err(ActuatorError::InvalidTarget(format!(
                "{id}: OPP lies outside the hardware range"
            )));
        }
        Ok(Self {
            id,
            min_path,
            max_path,
            hardware_min,
            hardware_max,
            opps,
            hertz_per_unit: 1,
        })
    }

    /// Set the unit used by this target's kernel attributes.
    ///
    /// # Errors
    ///
    /// Returns an error when the unit is zero or any configured frequency
    /// cannot be represented exactly in that unit.
    pub fn with_hertz_per_unit(mut self, hertz_per_unit: u64) -> Result<Self, ActuatorError> {
        if hertz_per_unit == 0 {
            return Err(ActuatorError::InvalidTarget(format!(
                "{}: hertz_per_unit must be non-zero",
                self.id
            )));
        }
        let all_representable = self.hardware_min.get().is_multiple_of(hertz_per_unit)
            && self.hardware_max.get().is_multiple_of(hertz_per_unit)
            && self
                .opps
                .iter()
                .all(|frequency| frequency.get().is_multiple_of(hertz_per_unit));
        if !all_representable {
            return Err(ActuatorError::InvalidTarget(format!(
                "{}: frequency is not representable in kernel units",
                self.id
            )));
        }
        self.hertz_per_unit = hertz_per_unit;
        Ok(self)
    }

    /// Snap a frequency window inward to real operating points.
    ///
    /// # Errors
    ///
    /// Returns an error when the requested limits are outside hardware bounds
    /// or are not exactly representable in the target's kernel unit.
    pub fn snap_limits(
        &self,
        requested: FrequencyLimits,
    ) -> Result<FrequencyLimits, ActuatorError> {
        self.validate_limits(requested)?;
        if self.opps.is_empty() {
            return Ok(requested);
        }
        let minimum = self
            .opps
            .iter()
            .copied()
            .find(|opp| *opp >= requested.min)
            .unwrap_or(self.hardware_max);
        let maximum = self
            .opps
            .iter()
            .rev()
            .copied()
            .find(|opp| *opp <= requested.max)
            .unwrap_or(self.hardware_min);
        FrequencyLimits::new(minimum.min(maximum), maximum).map_err(|_| {
            ActuatorError::InvalidLimits {
                target: self.id.to_string(),
                minimum: minimum.get(),
                maximum: maximum.get(),
            }
        })
    }

    fn validate_limits(&self, limits: FrequencyLimits) -> Result<(), ActuatorError> {
        if limits.min < self.hardware_min
            || limits.max > self.hardware_max
            || limits.min > limits.max
            || self.hertz_per_unit == 0
            || !limits.min.get().is_multiple_of(self.hertz_per_unit)
            || !limits.max.get().is_multiple_of(self.hertz_per_unit)
        {
            return Err(ActuatorError::InvalidLimits {
                target: self.id.to_string(),
                minimum: limits.min.get(),
                maximum: limits.max.get(),
            });
        }
        Ok(())
    }

    fn recovery_manifest(&self) -> RecoveryFrequencyTargetManifest {
        RecoveryFrequencyTargetManifest {
            id: self.id.clone(),
            min_path: self.min_path.clone(),
            max_path: self.max_path.clone(),
            hardware_min: self.hardware_min,
            hardware_max: self.hardware_max,
            opps: self.opps.clone(),
            hertz_per_unit: self.hertz_per_unit,
        }
    }
}

/// A closed, typed value domain for one scalar sysfs attribute.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ScalarDomain {
    IntegerRange {
        minimum: i64,
        maximum: i64,
    },
    IntegerEnum {
        values: Vec<i64>,
    },
    StringEnum {
        values: Vec<String>,
    },
    CpuList {
        allowed_cpus: CpuSet,
        #[serde(default)]
        allow_empty: bool,
    },
}

impl ScalarDomain {
    fn canonicalize(mut self, id: &TargetId) -> Result<Self, ActuatorError> {
        match &mut self {
            Self::IntegerRange { minimum, maximum } => {
                if *minimum > *maximum {
                    return Err(ActuatorError::InvalidTarget(format!(
                        "{id}: scalar integer range is reversed"
                    )));
                }
            }
            Self::IntegerEnum { values } => {
                values.sort_unstable();
                values.dedup();
                if values.is_empty() {
                    return Err(ActuatorError::InvalidTarget(format!(
                        "{id}: scalar integer enum is empty"
                    )));
                }
            }
            Self::StringEnum { values } => {
                values.sort();
                values.dedup();
                if values.is_empty() {
                    return Err(ActuatorError::InvalidTarget(format!(
                        "{id}: scalar string enum is empty"
                    )));
                }
                for value in values.iter() {
                    validate_scalar_string(id, value)?;
                }
            }
            Self::CpuList { allowed_cpus, .. } => {
                if allowed_cpus.is_empty() {
                    return Err(ActuatorError::InvalidTarget(format!(
                        "{id}: scalar CPU-list domain has no allowed CPUs"
                    )));
                }
                if allowed_cpus.iter().any(|cpu| cpu.get() > MAX_SCALAR_CPU_ID) {
                    return Err(ActuatorError::InvalidTarget(format!(
                        "{id}: scalar CPU-list domain exceeds CPU {MAX_SCALAR_CPU_ID}"
                    )));
                }
            }
        }
        Ok(self)
    }

    fn validate_value(&self, id: &TargetId, value: &ScalarValue) -> Result<(), ActuatorError> {
        let valid = match (self, value) {
            (Self::IntegerRange { minimum, maximum }, ScalarValue::Integer(candidate)) => {
                candidate >= minimum && candidate <= maximum
            }
            (Self::IntegerEnum { values }, ScalarValue::Integer(candidate)) => {
                values.binary_search(candidate).is_ok()
            }
            (Self::StringEnum { values }, ScalarValue::String(candidate)) => {
                values.binary_search(candidate).is_ok()
            }
            (
                Self::CpuList {
                    allowed_cpus,
                    allow_empty,
                },
                ScalarValue::CpuList(candidate),
            ) => (*allow_empty || !candidate.is_empty()) && candidate.is_subset(allowed_cpus),
            _ => false,
        };
        if valid {
            Ok(())
        } else {
            Err(ActuatorError::InvalidScalarValue {
                target: id.to_string(),
                value: format!("{value:?}"),
            })
        }
    }
}

/// One typed value accepted by a [`ScalarTarget`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
pub enum ScalarValue {
    Integer(i64),
    String(String),
    CpuList(CpuSet),
}

/// A single sysfs attribute resolved from root-owned device configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScalarTarget {
    pub id: TargetId,
    pub path: PathBuf,
    pub domain: ScalarDomain,
}

impl ScalarTarget {
    /// Construct and validate a typed scalar target.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe paths or an empty/invalid value domain.
    pub fn new(
        id: TargetId,
        path: impl Into<PathBuf>,
        domain: ScalarDomain,
    ) -> Result<Self, ActuatorError> {
        let path = path.into();
        validate_sysfs_path(&path)?;
        let domain = domain.canonicalize(&id)?;
        Ok(Self { id, path, domain })
    }

    fn validate_value(&self, value: &ScalarValue) -> Result<(), ActuatorError> {
        self.domain.validate_value(&self.id, value)
    }

    fn recovery_manifest(&self) -> RecoveryScalarTargetManifest {
        RecoveryScalarTargetManifest {
            id: self.id.clone(),
            path: self.path.clone(),
            domain: self.domain.clone(),
        }
    }
}

/// Immutable allowlist of discovered mutation targets.
#[derive(Clone, Debug, Default)]
pub struct TargetRegistry {
    targets: BTreeMap<TargetId, FrequencyTarget>,
    scalar_targets: BTreeMap<TargetId, ScalarTarget>,
}

impl TargetRegistry {
    /// Build a registry, rejecting duplicate logical IDs.
    ///
    /// # Errors
    ///
    /// Returns an error when multiple targets use the same stable identifier
    /// or claim any of the same kernel attributes.
    pub fn new(targets: impl IntoIterator<Item = FrequencyTarget>) -> Result<Self, ActuatorError> {
        let mut registry = Self::default();
        let mut claimed_paths = BTreeMap::<PathBuf, TargetId>::new();
        for target in targets {
            let id = target.id.clone();
            if registry.targets.contains_key(&id) {
                return Err(ActuatorError::DuplicateTarget(id.to_string()));
            }
            for path in [&target.min_path, &target.max_path] {
                if let Some(owner) = claimed_paths.insert(path.clone(), id.clone()) {
                    return Err(ActuatorError::InvalidTarget(format!(
                        "{id} and {owner} both claim {}",
                        path.display()
                    )));
                }
            }
            registry.targets.insert(id, target);
        }
        Ok(registry)
    }

    /// Resolve a logical target.
    #[must_use]
    pub fn get(&self, id: &TargetId) -> Option<&FrequencyTarget> {
        self.targets.get(id)
    }

    /// Add typed scalar targets to an existing frequency registry.
    ///
    /// # Errors
    ///
    /// Returns an error when a logical ID or sysfs path is already claimed.
    pub fn with_scalar_targets(
        mut self,
        targets: impl IntoIterator<Item = ScalarTarget>,
    ) -> Result<Self, ActuatorError> {
        let mut claimed_paths = BTreeMap::<PathBuf, TargetId>::new();
        for target in self.targets.values() {
            claimed_paths.insert(target.min_path.clone(), target.id.clone());
            claimed_paths.insert(target.max_path.clone(), target.id.clone());
        }
        for target in targets {
            let canonical = ScalarTarget::new(
                target.id.clone(),
                target.path.clone(),
                target.domain.clone(),
            )?;
            if canonical != target {
                return Err(ActuatorError::InvalidTarget(format!(
                    "{}: scalar target is not canonical",
                    target.id
                )));
            }
            let id = target.id.clone();
            if self.targets.contains_key(&id) || self.scalar_targets.contains_key(&id) {
                return Err(ActuatorError::DuplicateTarget(id.to_string()));
            }
            if let Some(owner) = claimed_paths.insert(target.path.clone(), id.clone()) {
                return Err(ActuatorError::InvalidTarget(format!(
                    "{id} and {owner} both claim {}",
                    target.path.display()
                )));
            }
            self.scalar_targets.insert(id, target);
        }
        Ok(self)
    }

    /// Resolve a logical scalar target.
    #[must_use]
    pub fn get_scalar(&self, id: &TargetId) -> Option<&ScalarTarget> {
        self.scalar_targets.get(id)
    }
}

/// One desired range in an atomic batch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrequencyRequest {
    pub target: TargetId,
    pub limits: FrequencyLimits,
}

/// One desired typed scalar value in an atomic batch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScalarRequest {
    pub target: TargetId,
    pub value: ScalarValue,
}

/// One stable process scheduling mutation in a batch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskRequest {
    pub identity: ProcessIdentity,
    pub desired: ProcessSchedulingState,
}

/// One systemd-owned unit property mutation in a batch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnitRequest {
    pub unit: String,
    pub desired: SystemdUnitProperties,
}

/// Complete frequency-target identity persisted for configuration-independent
/// crash recovery.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryFrequencyTargetManifest {
    pub id: TargetId,
    pub min_path: PathBuf,
    pub max_path: PathBuf,
    pub hardware_min: Hertz,
    pub hardware_max: Hertz,
    pub opps: Vec<Hertz>,
    pub hertz_per_unit: u64,
}

impl RecoveryFrequencyTargetManifest {
    /// Rebuild the exact target used when the journal was created.
    ///
    /// # Errors
    ///
    /// Returns an error when paths, bounds, operating points, or units in the
    /// manifest are invalid.
    pub fn to_frequency_target(&self) -> Result<FrequencyTarget, ActuatorError> {
        let target = FrequencyTarget::new(
            self.id.clone(),
            self.min_path.clone(),
            self.max_path.clone(),
            self.hardware_min,
            self.hardware_max,
            self.opps.clone(),
        )?
        .with_hertz_per_unit(self.hertz_per_unit)?;
        if target.recovery_manifest() != *self {
            return Err(ActuatorError::InvalidTarget(format!(
                "{}: recovery manifest is not canonical",
                self.id
            )));
        }
        Ok(target)
    }
}

/// Complete scalar-target identity persisted for configuration-independent
/// crash recovery.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryScalarTargetManifest {
    pub id: TargetId,
    pub path: PathBuf,
    pub domain: ScalarDomain,
}

impl RecoveryScalarTargetManifest {
    /// Rebuild the exact scalar target used when the journal was created.
    ///
    /// # Errors
    ///
    /// Returns an error when the path or typed value domain is invalid or
    /// non-canonical.
    pub fn to_scalar_target(&self) -> Result<ScalarTarget, ActuatorError> {
        let target = ScalarTarget::new(self.id.clone(), self.path.clone(), self.domain.clone())?;
        if target.recovery_manifest() != *self {
            return Err(ActuatorError::InvalidTarget(format!(
                "{}: scalar recovery manifest is not canonical",
                self.id
            )));
        }
        Ok(target)
    }
}

/// Tagged identity for every journal-owned sysfs resource.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "target", rename_all = "kebab-case")]
pub enum RecoveryResourceManifest {
    FrequencyPair(RecoveryFrequencyTargetManifest),
    Scalar(RecoveryScalarTargetManifest),
}

/// One resource discovered while inspecting a durable journal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecoveryResourceTarget {
    FrequencyPair(RecoveryFrequencyTargetManifest),
    Scalar(RecoveryScalarTargetManifest),
}

/// Read-only recovery metadata obtained without loading device configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryManifest {
    pub schema_version: u32,
    pub boot_id: String,
    pub device_fingerprint: String,
    pub resource_targets: Vec<RecoveryResourceTarget>,
    pub frequency_targets: Vec<RecoveryFrequencyTargetManifest>,
    pub has_tasks: bool,
    pub has_systemd_units: bool,
}

impl RecoveryManifest {
    /// Build a registry from the journal's self-describing resources.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid or duplicate target.
    pub fn self_describing_registry(&self) -> Result<TargetRegistry, ActuatorError> {
        let mut frequencies = Vec::new();
        let mut scalars = Vec::new();
        for target in &self.resource_targets {
            match target {
                RecoveryResourceTarget::FrequencyPair(target) => {
                    frequencies.push(target.to_frequency_target()?);
                }
                RecoveryResourceTarget::Scalar(target) => {
                    scalars.push(target.to_scalar_target()?);
                }
            }
        }
        TargetRegistry::new(frequencies)?.with_scalar_targets(scalars)
    }

    /// Exact logical sysfs paths that recovery may need to mutate.
    #[must_use]
    pub fn frequency_write_paths(&self) -> Vec<PathBuf> {
        self.frequency_targets
            .iter()
            .flat_map(|target| [target.min_path.clone(), target.max_path.clone()])
            .collect()
    }

    /// Exact scalar sysfs paths that recovery may need to mutate.
    #[must_use]
    pub fn scalar_write_paths(&self) -> Vec<PathBuf> {
        self.resource_targets
            .iter()
            .filter_map(|target| match target {
                RecoveryResourceTarget::Scalar(target) => Some(target.path.clone()),
                RecoveryResourceTarget::FrequencyPair(_) => None,
            })
            .collect()
    }

    /// Exact sysfs paths required for complete frequency and scalar recovery.
    #[must_use]
    pub fn resource_write_paths(&self) -> Vec<PathBuf> {
        let mut paths = self.frequency_write_paths();
        paths.extend(self.scalar_write_paths());
        paths
    }
}

/// Inspect durable recovery state without constructing an actuator or loading
/// device/policy configuration.
///
/// # Errors
///
/// Returns an error when storage cannot be read or the journal envelope,
/// checksum, schema, manifest, or resource identities are invalid.
pub fn inspect_recovery_journal(
    store: &dyn StateStore,
) -> Result<Option<RecoveryManifest>, ActuatorError> {
    let Some(bytes) = store.load()? else {
        return Ok(None);
    };
    let journal = decode_journal(&bytes)?;
    if journal.is_empty() {
        return Ok(None);
    }
    let frequency_targets = journal
        .entries
        .values()
        .map(|entry| entry.recovery_frequency_manifest().clone())
        .collect::<Vec<_>>();
    let mut resource_targets = frequency_targets
        .iter()
        .cloned()
        .map(RecoveryResourceTarget::FrequencyPair)
        .collect::<Vec<_>>();
    resource_targets.extend(
        journal
            .scalars
            .values()
            .map(|entry| RecoveryResourceTarget::Scalar(entry.recovery_scalar_manifest().clone())),
    );
    Ok(Some(RecoveryManifest {
        schema_version: journal.schema_version,
        boot_id: journal.boot_id,
        device_fingerprint: journal.device_fingerprint,
        resource_targets,
        frequency_targets,
        has_tasks: !journal.tasks.is_empty(),
        has_systemd_units: !journal.units.is_empty(),
    }))
}

/// Whether mutation is currently safe.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActuatorMode {
    ReadWrite,
    ReadOnlyDegraded { reason: String },
}

/// Outcome of applying a complete frequency batch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchOutcome {
    pub applied: BTreeMap<TargetId, FrequencyLimits>,
}

/// Outcome of applying a complete typed scalar batch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScalarBatchOutcome {
    pub applied: BTreeMap<TargetId, ScalarValue>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskBatchOutcome {
    pub applied: BTreeMap<ProcessIdentity, ProcessSchedulingState>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnitBatchOutcome {
    pub applied: BTreeMap<String, SystemdUnitProperties>,
}

/// Errors from validation, persistence, mutation or recovery.
#[derive(Debug, Error)]
pub enum ActuatorError {
    #[error("invalid mutation target: {0}")]
    InvalidTarget(String),
    #[error("duplicate target ID: {0}")]
    DuplicateTarget(String),
    #[error("unknown target ID: {0}")]
    UnknownTarget(String),
    #[error("frequency target is not durably owned: {0}")]
    OwnershipRequired(String),
    #[error("duplicate request for target ID: {0}")]
    DuplicateRequest(String),
    #[error("invalid frequency range for {target}: {minimum}..{maximum} Hz")]
    InvalidLimits {
        target: String,
        minimum: u64,
        maximum: u64,
    },
    #[error("invalid scalar value for {target}: {value}")]
    InvalidScalarValue { target: String, value: String },
    #[error("invalid value read from {path}: {value:?}")]
    InvalidReadback { path: PathBuf, value: String },
    #[error("scalar ownership was changed externally: {0}")]
    ScalarOwnershipLost(String),
    #[error("platform operation failed: {0}")]
    Platform(#[from] PlatformError),
    #[error("journal is invalid: {0}")]
    InvalidJournal(String),
    #[error("actuator is read-only degraded: {0}")]
    Degraded(String),
    #[error("a durable journal must be recovered before accepting mutations")]
    RecoveryRequired,
    #[error("frequency transaction failed for {target}: {reason}")]
    Transaction { target: String, reason: String },
    #[error("transaction rollback failed: {0}")]
    Rollback(String),
    #[error("task mutation backends are unavailable")]
    TaskBackendUnavailable,
    #[error("systemd mutation backend is unavailable")]
    SystemdBackendUnavailable,
    #[error("process identity changed or disappeared: {0:?}")]
    ProcessIdentityChanged(ProcessIdentity),
    #[error("systemd unit instance changed or disappeared: {0}")]
    UnitInstanceChanged(String),
    #[error("actuator state lock was poisoned")]
    LockPoisoned,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalEntry {
    target: TargetId,
    min_path: PathBuf,
    max_path: PathBuf,
    resource_manifest: RecoveryResourceManifest,
    original: FrequencyLimits,
    desired: FrequencyLimits,
    applied: FrequencyLimits,
    legal_pairs: Vec<FrequencyLimits>,
}

impl JournalEntry {
    fn recovery_frequency_manifest(&self) -> &RecoveryFrequencyTargetManifest {
        match &self.resource_manifest {
            RecoveryResourceManifest::FrequencyPair(manifest) => manifest,
            RecoveryResourceManifest::Scalar(_) => {
                unreachable!("validated frequency journal manifest")
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScalarJournalEntry {
    target: TargetId,
    path: PathBuf,
    manifest: RecoveryResourceManifest,
    original: ScalarValue,
    desired: ScalarValue,
    applied: ScalarValue,
}

impl ScalarJournalEntry {
    fn recovery_scalar_manifest(&self) -> &RecoveryScalarTargetManifest {
        match &self.manifest {
            RecoveryResourceManifest::Scalar(manifest) => manifest,
            RecoveryResourceManifest::FrequencyPair(_) => {
                unreachable!("validated scalar journal manifest")
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "explicit named journal fields are safer and clearer than bit positions"
)]
struct TaskFieldMask {
    affinity: bool,
    nice: bool,
    policy: bool,
    uclamp_min: bool,
    uclamp_max: bool,
}

impl TaskFieldMask {
    fn changed(before: &ProcessSchedulingState, after: &ProcessSchedulingState) -> Self {
        Self {
            affinity: before.affinity != after.affinity,
            nice: before.nice != after.nice,
            // Linux changes scheduler class and fixed real-time priority in
            // one sched_setscheduler operation. Journal ownership therefore
            // treats them as one indivisible tuple.
            policy: before.policy != after.policy || before.rt_priority != after.rt_priority,
            uclamp_min: before.uclamp_min != after.uclamp_min,
            uclamp_max: before.uclamp_max != after.uclamp_max,
        }
    }

    fn union(self, other: Self) -> Self {
        Self {
            affinity: self.affinity || other.affinity,
            nice: self.nice || other.nice,
            policy: self.policy || other.policy,
            uclamp_min: self.uclamp_min || other.uclamp_min,
            uclamp_max: self.uclamp_max || other.uclamp_max,
        }
    }

    fn without(self, other: Self) -> Self {
        Self {
            affinity: self.affinity && !other.affinity,
            nice: self.nice && !other.nice,
            policy: self.policy && !other.policy,
            uclamp_min: self.uclamp_min && !other.uclamp_min,
            uclamp_max: self.uclamp_max && !other.uclamp_max,
        }
    }

    fn is_empty(self) -> bool {
        self == Self::default()
    }

    fn intersects(self, other: Self) -> bool {
        (self.affinity && other.affinity)
            || (self.nice && other.nice)
            || (self.policy && other.policy)
            || (self.uclamp_min && other.uclamp_min)
            || (self.uclamp_max && other.uclamp_max)
    }

    fn fields_matching_either(
        self,
        current: &ProcessSchedulingState,
        first: &ProcessSchedulingState,
        second: &ProcessSchedulingState,
    ) -> Self {
        Self {
            affinity: self.affinity
                && (current.affinity == first.affinity || current.affinity == second.affinity),
            nice: self.nice && (current.nice == first.nice || current.nice == second.nice),
            policy: self.policy
                && (same_policy_priority(current, first) || same_policy_priority(current, second)),
            uclamp_min: self.uclamp_min
                && (current.uclamp_min == first.uclamp_min
                    || current.uclamp_min == second.uclamp_min),
            uclamp_max: self.uclamp_max
                && (current.uclamp_max == first.uclamp_max
                    || current.uclamp_max == second.uclamp_max),
        }
    }

    fn copy(self, destination: &mut ProcessSchedulingState, source: &ProcessSchedulingState) {
        if self.affinity {
            destination.affinity.clone_from(&source.affinity);
        }
        if self.nice {
            destination.nice = source.nice;
        }
        if self.policy {
            destination.policy = source.policy;
            destination.rt_priority = source.rt_priority;
        }
        if self.uclamp_min {
            destination.uclamp_min = source.uclamp_min;
        }
        if self.uclamp_max {
            destination.uclamp_max = source.uclamp_max;
        }
    }

    fn values_equal(self, left: &ProcessSchedulingState, right: &ProcessSchedulingState) -> bool {
        (!self.affinity || left.affinity == right.affinity)
            && (!self.nice || left.nice == right.nice)
            && (!self.policy || same_policy_priority(left, right))
            && (!self.uclamp_min || left.uclamp_min == right.uclamp_min)
            && (!self.uclamp_max || left.uclamp_max == right.uclamp_max)
    }
}

fn same_policy_priority(left: &ProcessSchedulingState, right: &ProcessSchedulingState) -> bool {
    left.policy == right.policy && left.rt_priority == right.rt_priority
}

fn validate_journal_scheduling_state(state: &ProcessSchedulingState) -> Result<(), String> {
    match (state.policy, state.rt_priority) {
        (SchedulingClass::Fifo, Some(priority)) if (1..=99).contains(&priority) => Ok(()),
        (SchedulingClass::Fifo, Some(priority)) => Err(format!(
            "FIFO priority {priority} is outside Linux range 1..=99"
        )),
        (SchedulingClass::Fifo, None) => {
            Err("FIFO policy is missing its fixed priority".to_owned())
        }
        (SchedulingClass::Other | SchedulingClass::Batch | SchedulingClass::Idle, None) => Ok(()),
        (
            SchedulingClass::Other | SchedulingClass::Batch | SchedulingClass::Idle,
            Some(priority),
        ) => Err(format!(
            "non-real-time policy carries unexpected priority {priority}"
        )),
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UnitFieldMask {
    cpu_weight: bool,
    allowed_cpus: bool,
}

impl UnitFieldMask {
    fn changed(before: &SystemdUnitProperties, after: &SystemdUnitProperties) -> Self {
        Self {
            cpu_weight: before.cpu_weight != after.cpu_weight,
            allowed_cpus: before.allowed_cpus != after.allowed_cpus,
        }
    }

    fn union(self, other: Self) -> Self {
        Self {
            cpu_weight: self.cpu_weight || other.cpu_weight,
            allowed_cpus: self.allowed_cpus || other.allowed_cpus,
        }
    }

    fn without(self, other: Self) -> Self {
        Self {
            cpu_weight: self.cpu_weight && !other.cpu_weight,
            allowed_cpus: self.allowed_cpus && !other.allowed_cpus,
        }
    }

    fn is_empty(self) -> bool {
        self == Self::default()
    }

    fn intersects(self, other: Self) -> bool {
        (self.cpu_weight && other.cpu_weight) || (self.allowed_cpus && other.allowed_cpus)
    }

    fn fields_matching_either(
        self,
        current: &SystemdUnitProperties,
        first: &SystemdUnitProperties,
        second: &SystemdUnitProperties,
    ) -> Self {
        Self {
            cpu_weight: self.cpu_weight
                && (current.cpu_weight == first.cpu_weight
                    || current.cpu_weight == second.cpu_weight),
            allowed_cpus: self.allowed_cpus
                && (current.allowed_cpus == first.allowed_cpus
                    || current.allowed_cpus == second.allowed_cpus),
        }
    }

    fn copy(self, destination: &mut SystemdUnitProperties, source: &SystemdUnitProperties) {
        if self.cpu_weight {
            destination.cpu_weight = source.cpu_weight;
        }
        if self.allowed_cpus {
            destination.allowed_cpus.clone_from(&source.allowed_cpus);
        }
    }

    fn values_equal(self, left: &SystemdUnitProperties, right: &SystemdUnitProperties) -> bool {
        (!self.cpu_weight || left.cpu_weight == right.cpu_weight)
            && (!self.allowed_cpus || left.allowed_cpus == right.allowed_cpus)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskJournalEntry {
    identity: ProcessIdentity,
    original: ProcessSchedulingState,
    desired: ProcessSchedulingState,
    applied: ProcessSchedulingState,
    owned_fields: TaskFieldMask,
    relinquished_fields: TaskFieldMask,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UnitJournalEntry {
    unit: String,
    instance: SystemdUnitInstanceIdentity,
    original: SystemdUnitProperties,
    desired: SystemdUnitProperties,
    applied: SystemdUnitProperties,
    owned_fields: UnitFieldMask,
    relinquished_fields: UnitFieldMask,
}

#[derive(Clone, Debug)]
struct PreparedTaskMutation {
    before: ProcessSchedulingState,
    desired: ProcessSchedulingState,
    changed_fields: TaskFieldMask,
    rollback_entry: Option<TaskJournalEntry>,
}

#[derive(Clone, Debug)]
struct PreparedFrequencyMutation {
    target: FrequencyTarget,
    before_effective: FrequencyLimits,
    previous_request: FrequencyLimits,
    desired_request: FrequencyLimits,
    needs_write: bool,
}

#[derive(Clone, Debug)]
struct PreparedScalarMutation {
    target: ScalarTarget,
    before: ScalarValue,
    desired: ScalarValue,
    needs_write: bool,
    needs_journal: bool,
}

#[derive(Clone, Debug)]
struct PreparedUnitMutation {
    instance: SystemdUnitInstanceIdentity,
    before: SystemdUnitProperties,
    desired: SystemdUnitProperties,
    changed_fields: UnitFieldMask,
    rollback_entry: Option<UnitJournalEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    schema_version: u32,
    boot_id: String,
    device_fingerprint: String,
    generation: u64,
    entries: BTreeMap<TargetId, JournalEntry>,
    scalars: BTreeMap<TargetId, ScalarJournalEntry>,
    tasks: BTreeMap<String, TaskJournalEntry>,
    units: BTreeMap<String, UnitJournalEntry>,
}

impl Journal {
    fn is_empty(&self) -> bool {
        self.entries.is_empty()
            && self.scalars.is_empty()
            && self.tasks.is_empty()
            && self.units.is_empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalEnvelope {
    payload: Vec<u8>,
    checksum: u32,
}

#[derive(Debug)]
struct RuntimeState {
    mode: ActuatorMode,
    journal: Journal,
    recovery_required: bool,
}

/// Transactional frequency, scalar, task, and systemd actuator.
pub struct FrequencyActuator {
    io: Arc<dyn SysfsIo>,
    store: Arc<dyn StateStore>,
    registry: TargetRegistry,
    boot_id: String,
    device_fingerprint: String,
    proc_reader: Option<Arc<dyn ProcReader>>,
    process_controller: Option<Arc<dyn ProcessController>>,
    systemd: Option<Arc<dyn SystemdClient>>,
    state: Mutex<RuntimeState>,
}

impl FrequencyActuator {
    /// Construct an actuator and load any existing journal.
    ///
    /// Invalid journal data does not get ignored: the returned actuator is
    /// placed in read-only degraded mode until an administrator resolves it.
    #[must_use]
    pub fn new(
        io: Arc<dyn SysfsIo>,
        store: Arc<dyn StateStore>,
        registry: TargetRegistry,
        boot_id: impl Into<String>,
        device_fingerprint: impl Into<String>,
    ) -> Self {
        let boot_id = boot_id.into();
        let device_fingerprint = device_fingerprint.into();
        let empty = Journal {
            schema_version: JOURNAL_SCHEMA_VERSION,
            boot_id: boot_id.clone(),
            device_fingerprint: device_fingerprint.clone(),
            generation: 0,
            entries: BTreeMap::new(),
            scalars: BTreeMap::new(),
            tasks: BTreeMap::new(),
            units: BTreeMap::new(),
        };
        let (mode, journal, recovery_required) = match store.load() {
            Ok(Some(bytes)) => match decode_journal(&bytes) {
                Ok(mut journal) => {
                    let recovery_required = !journal.is_empty();
                    if !recovery_required {
                        journal.schema_version = JOURNAL_SCHEMA_VERSION;
                        journal.boot_id.clone_from(&boot_id);
                        journal.device_fingerprint.clone_from(&device_fingerprint);
                    }
                    (ActuatorMode::ReadWrite, journal, recovery_required)
                }
                Err(error) => (
                    ActuatorMode::ReadOnlyDegraded {
                        reason: error.to_string(),
                    },
                    empty,
                    true,
                ),
            },
            Ok(None) => (ActuatorMode::ReadWrite, empty, false),
            Err(error) => (
                ActuatorMode::ReadOnlyDegraded {
                    reason: error.to_string(),
                },
                empty,
                true,
            ),
        };
        Self {
            io,
            store,
            registry,
            boot_id,
            device_fingerprint,
            proc_reader: None,
            process_controller: None,
            systemd: None,
            state: Mutex::new(RuntimeState {
                mode,
                journal,
                recovery_required,
            }),
        }
    }

    /// Attach typed process mutation backends.
    ///
    /// The actuator still validates stable process identity before every task
    /// mutation.
    #[must_use]
    pub fn with_process_backend(
        mut self,
        proc_reader: Arc<dyn ProcReader>,
        process_controller: Arc<dyn ProcessController>,
    ) -> Self {
        self.proc_reader = Some(proc_reader);
        self.process_controller = Some(process_controller);
        self
    }

    /// Attach the typed systemd mutation backend.
    ///
    /// Unit ownership must be established by the caller; ambiguous units must
    /// not be submitted.
    #[must_use]
    pub fn with_systemd_backend(mut self, systemd: Arc<dyn SystemdClient>) -> Self {
        self.systemd = Some(systemd);
        self
    }

    /// Whether typed process scheduling mutations are available.
    #[must_use]
    pub fn has_process_backend(&self) -> bool {
        self.proc_reader.is_some() && self.process_controller.is_some()
    }

    /// Whether typed systemd unit-property mutations are available.
    #[must_use]
    pub fn has_systemd_backend(&self) -> bool {
        self.systemd.is_some()
    }

    /// Current safety mode.
    ///
    /// # Errors
    ///
    /// Returns an error if the internal state lock is poisoned.
    pub fn mode(&self) -> Result<ActuatorMode, ActuatorError> {
        Ok(self.lock_state()?.mode.clone())
    }

    /// Report whether a journal loaded at startup still requires recovery.
    ///
    /// Normal resources actively owned by the current daemon do not make this
    /// true. Invalid/unreadable startup journals do, so health reporting stays
    /// fail-closed.
    ///
    /// # Errors
    ///
    /// Returns an error if the internal state lock is poisoned.
    pub fn startup_recovery_required(&self) -> Result<bool, ActuatorError> {
        Ok(self.lock_state()?.recovery_required)
    }

    /// Report whether startup recovery is both incomplete and degraded.
    ///
    /// # Errors
    ///
    /// Returns an error if the internal state lock is poisoned.
    pub fn startup_recovery_failed(&self) -> Result<bool, ActuatorError> {
        let state = self.lock_state()?;
        Ok(state.recovery_required && matches!(&state.mode, ActuatorMode::ReadOnlyDegraded { .. }))
    }

    /// Report whether the current or previous daemon owns journaled resources.
    ///
    /// # Errors
    ///
    /// Returns an error if the internal state lock is poisoned.
    pub fn has_owned_resources(&self) -> Result<bool, ActuatorError> {
        let state = self.lock_state()?;
        let tasks_owned = state
            .journal
            .tasks
            .values()
            .any(|entry| !entry.owned_fields.is_empty());
        let units_owned = state
            .journal
            .units
            .values()
            .any(|entry| !entry.owned_fields.is_empty());
        Ok(!state.journal.entries.is_empty()
            || !state.journal.scalars.is_empty()
            || tasks_owned
            || units_owned)
    }

    /// Carry a failed pre-configuration recovery into a newly constructed
    /// configuration-backed actuator.
    ///
    /// # Errors
    ///
    /// Returns an error if the internal state lock is poisoned.
    pub fn mark_startup_recovery_failed(
        &self,
        reason: impl Into<String>,
    ) -> Result<(), ActuatorError> {
        let mut state = self.lock_state()?;
        state.recovery_required = true;
        state.mode = ActuatorMode::ReadOnlyDegraded {
            reason: reason.into(),
        };
        Ok(())
    }

    /// Read a target's actual current limits.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown target, unreadable sysfs attributes, or
    /// malformed/reversed kernel values.
    pub fn read_limits(&self, id: &TargetId) -> Result<FrequencyLimits, ActuatorError> {
        let target = self
            .registry
            .get(id)
            .ok_or_else(|| ActuatorError::UnknownTarget(id.to_string()))?;
        read_limits(self.io.as_ref(), target)
    }

    /// Read and type-check a scalar target's current value.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown target, unavailable I/O, malformed
    /// readback, or a value outside the target's configured domain.
    pub fn read_scalar(&self, id: &TargetId) -> Result<ScalarValue, ActuatorError> {
        let target = self
            .registry
            .get_scalar(id)
            .ok_or_else(|| ActuatorError::UnknownTarget(id.to_string()))?;
        read_scalar(self.io.as_ref(), target)
    }

    /// Read scheduler state while verifying that the PID still denotes the
    /// expected stable process identity.
    ///
    /// # Errors
    ///
    /// Returns an error when process backends are unavailable, the identity
    /// changes during the read, or the scheduler state cannot be observed.
    pub fn read_task_state(
        &self,
        identity: ProcessIdentity,
    ) -> Result<ProcessSchedulingState, ActuatorError> {
        let proc_reader = self
            .proc_reader
            .as_ref()
            .ok_or(ActuatorError::TaskBackendUnavailable)?;
        let controller = self
            .process_controller
            .as_ref()
            .ok_or(ActuatorError::TaskBackendUnavailable)?;
        verify_process_identity(proc_reader.as_ref(), identity)?;
        let scheduling = controller.read_scheduling(identity.pid)?;
        verify_process_identity(proc_reader.as_ref(), identity)?;
        Ok(scheduling)
    }

    /// Resolve the systemd unit containing a stable process identity.
    ///
    /// # Errors
    ///
    /// Returns an error when either backend is unavailable, the identity
    /// changes during the query, or systemd cannot resolve the process.
    pub fn unit_for_process(
        &self,
        identity: ProcessIdentity,
    ) -> Result<Option<String>, ActuatorError> {
        let proc_reader = self
            .proc_reader
            .as_ref()
            .ok_or(ActuatorError::TaskBackendUnavailable)?;
        let systemd = self
            .systemd
            .as_ref()
            .ok_or(ActuatorError::SystemdBackendUnavailable)?;
        verify_process_identity(proc_reader.as_ref(), identity)?;
        let unit = systemd.unit_for_process(identity.pid)?;
        verify_process_identity(proc_reader.as_ref(), identity)?;
        Ok(unit)
    }

    /// List processes currently assigned to a systemd unit.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend is unavailable, the name is unsafe,
    /// or systemd cannot enumerate the unit.
    pub fn unit_processes(&self, unit: &str) -> Result<Vec<ProcessId>, ActuatorError> {
        validate_unit_name(unit)?;
        let systemd = self
            .systemd
            .as_ref()
            .ok_or(ActuatorError::SystemdBackendUnavailable)?;
        systemd.unit_processes(unit).map_err(ActuatorError::from)
    }

    /// Recover state left by an earlier daemon from the same boot.
    ///
    /// A journal from another boot is discarded because process identities and
    /// kernel runtime policy no longer refer to the same resources.
    ///
    /// # Errors
    ///
    /// Returns an error and enters read-only degraded mode if journal identity,
    /// resource restoration, verification, or durable removal fails.
    #[allow(
        clippy::too_many_lines,
        reason = "recovery keeps validation, ordered restoration and durable completion together"
    )]
    pub fn recover_pending(&self) -> Result<(), ActuatorError> {
        let mut state = self.lock_state()?;
        ensure_read_write(&state.mode)?;
        if !state.recovery_required {
            return Ok(());
        }
        if state.journal.boot_id != self.boot_id {
            if let Err(error) = self.store.remove_durable() {
                return self.degrade_locked(
                    &mut state,
                    format!("cannot discard journal from another boot: {error}"),
                );
            }
            state.journal = Journal {
                schema_version: JOURNAL_SCHEMA_VERSION,
                boot_id: self.boot_id.clone(),
                device_fingerprint: self.device_fingerprint.clone(),
                generation: state.journal.generation.saturating_add(1),
                entries: BTreeMap::new(),
                scalars: BTreeMap::new(),
                tasks: BTreeMap::new(),
                units: BTreeMap::new(),
            };
            state.recovery_required = false;
            return Ok(());
        }
        if state.journal.device_fingerprint != self.device_fingerprint {
            return self.degrade_locked(
                &mut state,
                "journal device fingerprint does not match".to_owned(),
            );
        }
        if state.journal.is_empty() {
            state.recovery_required = false;
            return Ok(());
        }

        if !state.journal.tasks.is_empty()
            && (self.proc_reader.is_none() || self.process_controller.is_none())
        {
            return self.degrade_locked(
                &mut state,
                "task recovery backends are unavailable".to_owned(),
            );
        }
        if !state.journal.units.is_empty() && self.systemd.is_none() {
            return self.degrade_locked(
                &mut state,
                "systemd recovery backend is unavailable".to_owned(),
            );
        }
        let entries: Vec<JournalEntry> = state.journal.entries.values().cloned().collect();
        let mut frequency_recovery = Vec::new();
        for entry in &entries {
            let Some(target) = self.registry.get(&entry.target) else {
                return self.degrade_locked(
                    &mut state,
                    format!("journal target {} is not present", entry.target),
                );
            };
            if target.min_path != entry.min_path || target.max_path != entry.max_path {
                return self.degrade_locked(
                    &mut state,
                    format!("journal paths changed for {}", entry.target),
                );
            }
            if target.recovery_manifest() != *entry.recovery_frequency_manifest() {
                return self.degrade_locked(
                    &mut state,
                    format!("journal target identity changed for {}", entry.target),
                );
            }
            if entry.original != hardware_limits(target) {
                return self.degrade_locked(
                    &mut state,
                    format!(
                        "journal target {} was claimed from a constrained effective range; automatic recovery could make a transient cap permanent",
                        entry.target
                    ),
                );
            }
            if let Some(journal_entry) = state.journal.entries.get_mut(&entry.target) {
                journal_entry.applied = entry.desired;
                journal_entry.desired = entry.original;
                journal_entry.legal_pairs = transaction_legal_pairs(entry.desired, entry.original);
            }
            frequency_recovery.push((entry.clone(), target.clone()));
        }
        if !frequency_recovery.is_empty()
            && let Err(error) = persist_journal(self.store.as_ref(), &state.journal)
        {
            return self.degrade_locked(
                &mut state,
                format!("cannot persist frequency recovery intent: {error}"),
            );
        }
        for (entry, target) in &frequency_recovery {
            if let Err(error) = restore_frequency_request(self.io.as_ref(), target, entry.original)
            {
                return self.degrade_locked(
                    &mut state,
                    format!("recovery failed for {}: {error}", entry.target),
                );
            }
        }
        if let Err(error) = self.recover_scalars_locked(&mut state) {
            return self.degrade_locked(&mut state, format!("scalar recovery failed: {error}"));
        }
        if let Err(error) = self.recover_tasks_locked(&mut state) {
            return self.degrade_locked(&mut state, format!("task recovery failed: {error}"));
        }
        if let Err(error) = self.recover_units_locked(&mut state) {
            return self
                .degrade_locked(&mut state, format!("systemd unit recovery failed: {error}"));
        }
        if let Err(error) = self.store.remove_durable() {
            return self.degrade_locked(
                &mut state,
                format!("cannot remove completed recovery journal: {error}"),
            );
        }
        state.journal.entries.clear();
        state.journal.scalars.clear();
        state.journal.tasks.clear();
        state.journal.units.clear();
        state.journal.generation = state.journal.generation.saturating_add(1);
        state.recovery_required = false;
        Ok(())
    }

    /// Atomically apply a batch from the caller's perspective.
    ///
    /// Hardware does not expose a true multi-target transaction, so failure
    /// causes every attempted target to be restored to the actuator request
    /// recorded before this call.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid requests, unavailable I/O, failed durable
    /// journaling, mutation/readback mismatch, or incomplete rollback.
    #[allow(
        clippy::too_many_lines,
        reason = "the transaction phases are kept adjacent to make rollback coverage auditable"
    )]
    pub fn apply_batch(
        &self,
        requests: &[FrequencyRequest],
    ) -> Result<BatchOutcome, ActuatorError> {
        let mut state = self.lock_state()?;
        ensure_mutation_ready(&state)?;
        if requests.is_empty() {
            return Ok(BatchOutcome {
                applied: BTreeMap::new(),
            });
        }

        let mut seen = BTreeSet::new();
        let mut prepared = Vec::with_capacity(requests.len());
        for request in requests {
            if !seen.insert(request.target.clone()) {
                return Err(ActuatorError::DuplicateRequest(request.target.to_string()));
            }
            let target = self
                .registry
                .get(&request.target)
                .ok_or_else(|| ActuatorError::UnknownTarget(request.target.to_string()))?;
            let desired_request = target.snap_limits(request.limits)?;
            let before_effective = read_limits(self.io.as_ref(), target)?;
            let existing_entry = state.journal.entries.get(&target.id);
            let full_hardware_request = hardware_limits(target);
            let previous_request =
                existing_entry.map_or(full_hardware_request, |entry| entry.desired);
            let unclaimed_noop = existing_entry.is_none() && before_effective == desired_request;
            if existing_entry.is_none()
                && !unclaimed_noop
                && before_effective != full_hardware_request
            {
                return Err(ActuatorError::Transaction {
                    target: target.id.to_string(),
                    reason: format!(
                        "cannot safely claim an unowned target while its effective range is {}..{} instead of the hardware default {}..{}",
                        before_effective.min.get(),
                        before_effective.max.get(),
                        full_hardware_request.min.get(),
                        full_hardware_request.max.get()
                    ),
                });
            }
            let needs_write = if existing_entry.is_some() {
                previous_request != desired_request
            } else {
                !unclaimed_noop
            };
            prepared.push(PreparedFrequencyMutation {
                target: target.clone(),
                before_effective,
                previous_request,
                desired_request,
                needs_write,
            });
        }

        if prepared.iter().all(|mutation| !mutation.needs_write) {
            return Ok(BatchOutcome {
                applied: prepared
                    .into_iter()
                    .map(|mutation| (mutation.target.id, mutation.before_effective))
                    .collect(),
            });
        }
        let journal_before = state.journal.clone();
        state.journal.generation = state.journal.generation.saturating_add(1);
        for mutation in prepared.iter().filter(|mutation| mutation.needs_write) {
            let mut entry = state
                .journal
                .entries
                .get(&mutation.target.id)
                .cloned()
                .unwrap_or_else(|| JournalEntry {
                    target: mutation.target.id.clone(),
                    min_path: mutation.target.min_path.clone(),
                    max_path: mutation.target.max_path.clone(),
                    resource_manifest: RecoveryResourceManifest::FrequencyPair(
                        mutation.target.recovery_manifest(),
                    ),
                    original: hardware_limits(&mutation.target),
                    desired: mutation.previous_request,
                    applied: mutation.previous_request,
                    legal_pairs: vec![mutation.previous_request],
                });
            entry.applied = mutation.previous_request;
            entry.desired = mutation.desired_request;
            entry.legal_pairs =
                transaction_legal_pairs(mutation.previous_request, mutation.desired_request);
            state
                .journal
                .entries
                .insert(mutation.target.id.clone(), entry);
        }
        if let Err(error) = persist_journal(self.store.as_ref(), &state.journal) {
            state.journal = journal_before;
            return self.degrade_locked(
                &mut state,
                format!("cannot persist pre-mutation journal: {error}"),
            );
        }

        let mut applied = BTreeMap::new();
        let mut attempted = Vec::new();
        let mut failure = None;
        for (index, mutation) in prepared.iter().enumerate() {
            if !mutation.needs_write {
                applied.insert(mutation.target.id.clone(), mutation.before_effective);
                continue;
            }
            attempted.push(index);
            match apply_requested_limits(
                self.io.as_ref(),
                &mutation.target,
                mutation.previous_request,
                mutation.desired_request,
            ) {
                Ok(actual) => {
                    if let Some(entry) = state.journal.entries.get_mut(&mutation.target.id) {
                        entry.applied = actual;
                        entry.legal_pairs = vec![actual];
                    }
                    applied.insert(mutation.target.id.clone(), actual);
                }
                Err(error) => {
                    failure = Some((mutation.target.id.clone(), error));
                    break;
                }
            }
        }

        if let Some((failed_target, error)) = failure {
            if let Err(rollback) =
                rollback_frequency_requests(self.io.as_ref(), &prepared, &attempted)
            {
                return self.degrade_locked(
                    &mut state,
                    format!(
                        "transaction failed for {failed_target}: {error}; rollback failed: {rollback}"
                    ),
                );
            }
            state.journal = journal_before;
            if let Err(persist_error) =
                self.persist_or_remove_locked(&mut state, "frequency rollback")
            {
                return self.degrade_locked(
                    &mut state,
                    format!(
                        "transaction failed for {failed_target}: {error}; rollback succeeded but journal update failed: {persist_error}"
                    ),
                );
            }
            return Err(ActuatorError::Transaction {
                target: failed_target.to_string(),
                reason: error.to_string(),
            });
        }

        if let Err(error) = persist_journal(self.store.as_ref(), &state.journal) {
            if let Err(rollback) =
                rollback_frequency_requests(self.io.as_ref(), &prepared, &attempted)
            {
                return self.degrade_locked(
                    &mut state,
                    format!("post-mutation journal failed: {error}; rollback failed: {rollback}"),
                );
            }
            state.journal = journal_before;
            if let Err(persist_error) =
                self.persist_or_remove_locked(&mut state, "post-mutation frequency rollback")
            {
                return self.degrade_locked(
                    &mut state,
                    format!(
                        "post-mutation journal failed: {error}; rollback succeeded but journal update failed: {persist_error}"
                    ),
                );
            }
            return self.degrade_locked(
                &mut state,
                format!("post-mutation journal failed; batch rolled back: {error}"),
            );
        }

        Ok(BatchOutcome { applied })
    }

    /// Apply a verified frequency batch using an existing durable ownership
    /// claim, without persisting the individual transition.
    ///
    /// This is the high-rate counterpart to [`Self::apply_batch`]. Every target
    /// must already have an entry created and durably stored by
    /// [`Self::apply_batch`]; this method never claims a new target and never
    /// accesses the durable state store. Successful transitions update only
    /// the in-memory journal. If the process crashes, recovery uses the older
    /// durable claim to release the target to its full hardware range.
    ///
    /// Hardware does not expose a true multi-target transaction, so failure
    /// restores every attempted target to the actuator request recorded before
    /// this call. A failed rollback places the actuator in read-only degraded
    /// mode.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid or duplicate requests, targets without
    /// durable ownership, unavailable I/O, mutation/readback mismatch, or
    /// incomplete rollback.
    #[allow(
        clippy::too_many_lines,
        reason = "the fast transaction phases are kept adjacent to make the no-persistence and rollback invariants auditable"
    )]
    pub fn apply_owned_batch_fast(
        &self,
        requests: &[FrequencyRequest],
    ) -> Result<BatchOutcome, ActuatorError> {
        let mut state = self.lock_state()?;
        ensure_mutation_ready(&state)?;
        if requests.is_empty() {
            return Ok(BatchOutcome {
                applied: BTreeMap::new(),
            });
        }

        let mut seen = BTreeSet::new();
        let mut prepared = Vec::with_capacity(requests.len());
        for request in requests {
            if !seen.insert(request.target.clone()) {
                return Err(ActuatorError::DuplicateRequest(request.target.to_string()));
            }
            let target = self
                .registry
                .get(&request.target)
                .ok_or_else(|| ActuatorError::UnknownTarget(request.target.to_string()))?;
            let existing_entry = state
                .journal
                .entries
                .get(&target.id)
                .ok_or_else(|| ActuatorError::OwnershipRequired(target.id.to_string()))?;
            let desired_request = target.snap_limits(request.limits)?;
            let before_effective = read_limits(self.io.as_ref(), target)?;
            let previous_request = existing_entry.desired;
            prepared.push(PreparedFrequencyMutation {
                target: target.clone(),
                before_effective,
                previous_request,
                desired_request,
                needs_write: previous_request != desired_request
                    || before_effective != desired_request,
            });
        }

        if prepared.iter().all(|mutation| !mutation.needs_write) {
            return Ok(BatchOutcome {
                applied: prepared
                    .into_iter()
                    .map(|mutation| (mutation.target.id, mutation.before_effective))
                    .collect(),
            });
        }

        let generation_before = state.journal.generation;
        let entries_before = prepared
            .iter()
            .filter(|mutation| mutation.needs_write)
            .map(|mutation| {
                state
                    .journal
                    .entries
                    .get(&mutation.target.id)
                    .cloned()
                    .map(|entry| (mutation.target.id.clone(), entry))
                    .ok_or_else(|| ActuatorError::OwnershipRequired(mutation.target.id.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        state.journal.generation = state.journal.generation.saturating_add(1);
        for mutation in prepared.iter().filter(|mutation| mutation.needs_write) {
            let entry = state
                .journal
                .entries
                .get_mut(&mutation.target.id)
                .ok_or_else(|| ActuatorError::OwnershipRequired(mutation.target.id.to_string()))?;
            entry.applied = mutation.before_effective;
            entry.desired = mutation.desired_request;
            entry.legal_pairs =
                transaction_legal_pairs(mutation.before_effective, mutation.desired_request);
        }

        let mut applied = BTreeMap::new();
        let mut attempted = Vec::new();
        let mut failure = None;
        for (index, mutation) in prepared.iter().enumerate() {
            if !mutation.needs_write {
                applied.insert(mutation.target.id.clone(), mutation.before_effective);
                continue;
            }
            attempted.push(index);
            match apply_requested_limits(
                self.io.as_ref(),
                &mutation.target,
                mutation.before_effective,
                mutation.desired_request,
            ) {
                Ok(actual) => {
                    let entry = state
                        .journal
                        .entries
                        .get_mut(&mutation.target.id)
                        .ok_or_else(|| {
                            ActuatorError::OwnershipRequired(mutation.target.id.to_string())
                        })?;
                    entry.applied = actual;
                    entry.legal_pairs = vec![actual];
                    applied.insert(mutation.target.id.clone(), actual);
                }
                Err(error) => {
                    failure = Some((mutation.target.id.clone(), error));
                    break;
                }
            }
        }

        if let Some((failed_target, error)) = failure {
            if let Err(rollback) =
                rollback_frequency_requests(self.io.as_ref(), &prepared, &attempted)
            {
                return self.degrade_locked(
                    &mut state,
                    format!(
                        "fast transaction failed for {failed_target}: {error}; rollback failed: {rollback}"
                    ),
                );
            }
            state.journal.generation = generation_before;
            for (target, entry) in entries_before {
                state.journal.entries.insert(target, entry);
            }
            return Err(ActuatorError::Transaction {
                target: failed_target.to_string(),
                reason: error.to_string(),
            });
        }

        Ok(BatchOutcome { applied })
    }

    /// Apply an atomic, verified batch of typed scalar sysfs values.
    ///
    /// A scalar path is accepted only when it belongs to a [`ScalarTarget`] in
    /// the immutable registry. The first mutation durably records the exact
    /// typed original value and tagged recovery manifest before any write.
    /// Failed multi-target batches roll attempted writes back in reverse order.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid/duplicate requests, externally changed
    /// ownership, durable journal failure, mutation/readback mismatch, or
    /// incomplete rollback.
    #[allow(
        clippy::too_many_lines,
        reason = "scalar transaction phases stay adjacent so pre-journal and reverse-rollback coverage remain auditable"
    )]
    pub fn apply_scalars(
        &self,
        requests: &[ScalarRequest],
    ) -> Result<ScalarBatchOutcome, ActuatorError> {
        let mut state = self.lock_state()?;
        ensure_mutation_ready(&state)?;
        if requests.is_empty() {
            return Ok(ScalarBatchOutcome {
                applied: BTreeMap::new(),
            });
        }

        let mut seen = BTreeSet::new();
        let mut prepared = Vec::with_capacity(requests.len());
        for request in requests {
            if !seen.insert(request.target.clone()) {
                return Err(ActuatorError::DuplicateRequest(request.target.to_string()));
            }
            let target = self
                .registry
                .get_scalar(&request.target)
                .ok_or_else(|| ActuatorError::UnknownTarget(request.target.to_string()))?;
            target.validate_value(&request.value)?;
            let before = read_scalar(self.io.as_ref(), target)?;
            let existing = state.journal.scalars.get(&target.id);
            if let Some(entry) = existing {
                if entry.path != target.path
                    || entry.recovery_scalar_manifest() != &target.recovery_manifest()
                {
                    return Err(ActuatorError::InvalidTarget(format!(
                        "{} scalar journal identity changed",
                        target.id
                    )));
                }
                if before != entry.applied && before != entry.desired {
                    return Err(ActuatorError::ScalarOwnershipLost(target.id.to_string()));
                }
            }
            let needs_write = before != request.value;
            let needs_journal = existing.map_or(needs_write, |entry| {
                entry.desired != request.value || entry.applied != before
            });
            prepared.push(PreparedScalarMutation {
                target: target.clone(),
                before,
                desired: request.value.clone(),
                needs_write,
                needs_journal,
            });
        }

        if prepared
            .iter()
            .all(|mutation| !mutation.needs_write && !mutation.needs_journal)
        {
            return Ok(ScalarBatchOutcome {
                applied: prepared
                    .into_iter()
                    .map(|mutation| (mutation.target.id, mutation.before))
                    .collect(),
            });
        }

        let journal_before = state.journal.clone();
        state.journal.generation = state.journal.generation.saturating_add(1);
        for mutation in prepared.iter().filter(|mutation| mutation.needs_journal) {
            let mut entry = state
                .journal
                .scalars
                .get(&mutation.target.id)
                .cloned()
                .unwrap_or_else(|| ScalarJournalEntry {
                    target: mutation.target.id.clone(),
                    path: mutation.target.path.clone(),
                    manifest: RecoveryResourceManifest::Scalar(mutation.target.recovery_manifest()),
                    original: mutation.before.clone(),
                    desired: mutation.before.clone(),
                    applied: mutation.before.clone(),
                });
            entry.applied.clone_from(&mutation.before);
            entry.desired.clone_from(&mutation.desired);
            state
                .journal
                .scalars
                .insert(mutation.target.id.clone(), entry);
        }
        if let Err(error) = persist_journal(self.store.as_ref(), &state.journal) {
            state.journal = journal_before;
            return self.degrade_locked(
                &mut state,
                format!("cannot persist pre-mutation scalar journal: {error}"),
            );
        }

        let mut applied = BTreeMap::new();
        let mut attempted = Vec::new();
        let mut failure = None;
        for (index, mutation) in prepared.iter().enumerate() {
            if !mutation.needs_write {
                applied.insert(mutation.target.id.clone(), mutation.before.clone());
                continue;
            }
            attempted.push(index);
            match write_scalar(self.io.as_ref(), &mutation.target, &mutation.desired) {
                Ok(actual) => {
                    if let Some(entry) = state.journal.scalars.get_mut(&mutation.target.id) {
                        entry.applied.clone_from(&actual);
                        entry.desired.clone_from(&actual);
                    }
                    applied.insert(mutation.target.id.clone(), actual);
                }
                Err(error) => {
                    failure = Some((mutation.target.id.clone(), error));
                    break;
                }
            }
        }

        if let Some((failed_target, error)) = failure {
            if let Err(rollback) = rollback_scalars(self.io.as_ref(), &prepared, &attempted) {
                return self.degrade_locked(
                    &mut state,
                    format!(
                        "scalar transaction failed for {failed_target}: {error}; rollback failed: {rollback}"
                    ),
                );
            }
            state.journal = journal_before;
            if let Err(persist_error) = self.persist_or_remove_locked(&mut state, "scalar rollback")
            {
                return self.degrade_locked(
                    &mut state,
                    format!(
                        "scalar transaction failed for {failed_target}: {error}; rollback succeeded but journal update failed: {persist_error}"
                    ),
                );
            }
            return Err(ActuatorError::Transaction {
                target: failed_target.to_string(),
                reason: error.to_string(),
            });
        }

        if prepared.iter().any(|mutation| mutation.needs_write)
            && let Err(error) = persist_journal(self.store.as_ref(), &state.journal)
        {
            if let Err(rollback) = rollback_scalars(self.io.as_ref(), &prepared, &attempted) {
                return self.degrade_locked(
                    &mut state,
                    format!(
                        "post-mutation scalar journal failed: {error}; rollback failed: {rollback}"
                    ),
                );
            }
            state.journal = journal_before;
            if let Err(persist_error) =
                self.persist_or_remove_locked(&mut state, "post-mutation scalar rollback")
            {
                return self.degrade_locked(
                    &mut state,
                    format!(
                        "post-mutation scalar journal failed: {error}; rollback succeeded but journal update failed: {persist_error}"
                    ),
                );
            }
            return self.degrade_locked(
                &mut state,
                format!("post-mutation scalar journal failed; batch rolled back: {error}"),
            );
        }

        Ok(ScalarBatchOutcome { applied })
    }

    /// Apply a verified, journaled batch of task scheduling state.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate/stale identities, unavailable backends,
    /// failed journaling, mutation/readback mismatch, or incomplete rollback.
    #[allow(
        clippy::too_many_lines,
        reason = "the transaction phases are kept adjacent to make rollback coverage auditable"
    )]
    pub fn apply_tasks(&self, requests: &[TaskRequest]) -> Result<TaskBatchOutcome, ActuatorError> {
        let mut state = self.lock_state()?;
        ensure_mutation_ready(&state)?;
        if requests.is_empty() {
            return Ok(TaskBatchOutcome {
                applied: BTreeMap::new(),
            });
        }
        let proc_reader = self
            .proc_reader
            .as_ref()
            .ok_or(ActuatorError::TaskBackendUnavailable)?;
        let controller = self
            .process_controller
            .as_ref()
            .ok_or(ActuatorError::TaskBackendUnavailable)?;
        let mut seen = BTreeSet::new();
        let mut observed = BTreeMap::new();
        for request in requests {
            if !seen.insert(request.identity) {
                return Err(ActuatorError::DuplicateRequest(format!(
                    "pid:{}:{}",
                    request.identity.pid.get(),
                    request.identity.start_time_ticks
                )));
            }
            verify_process_identity(proc_reader.as_ref(), request.identity)?;
            observed.insert(
                request.identity,
                controller.read_scheduling(request.identity.pid)?,
            );
        }
        state.journal.generation = state.journal.generation.saturating_add(1);
        let mut prepared = BTreeMap::new();
        for request in requests {
            let current = observed[&request.identity].clone();
            let key = task_journal_key(request.identity);
            let existing = state.journal.tasks.remove(&key);
            let (mut entry, rollback_entry) = if let Some(mut entry) = existing {
                let owned = entry.owned_fields;
                let still_owned =
                    owned.fields_matching_either(&current, &entry.applied, &entry.applied);
                let lost = owned.without(still_owned);
                let relinquished = entry.relinquished_fields.union(lost);
                lost.copy(&mut entry.original, &current);
                entry.desired.clone_from(&current);
                entry.applied.clone_from(&current);
                entry.owned_fields = still_owned;
                entry.relinquished_fields = relinquished;
                let rollback_entry = Some(entry.clone());
                (entry, rollback_entry)
            } else {
                (
                    TaskJournalEntry {
                        identity: request.identity,
                        original: current.clone(),
                        desired: current.clone(),
                        applied: current.clone(),
                        owned_fields: TaskFieldMask::default(),
                        relinquished_fields: TaskFieldMask::default(),
                    },
                    None,
                )
            };
            let owned = entry.owned_fields;
            let relinquished = entry.relinquished_fields;
            let requested = TaskFieldMask::changed(&current, &request.desired);
            let next_owned = owned.union(requested.without(relinquished));
            let newly_owned = next_owned.without(owned);
            newly_owned.copy(&mut entry.original, &current);
            let mut desired = current.clone();
            next_owned.copy(&mut desired, &request.desired);
            entry.desired.clone_from(&desired);
            entry.applied.clone_from(&current);
            entry.owned_fields = next_owned;
            entry.relinquished_fields = relinquished;
            if !next_owned.is_empty() || !relinquished.is_empty() {
                state.journal.tasks.insert(key, entry);
            }
            prepared.insert(
                request.identity,
                PreparedTaskMutation {
                    before: current.clone(),
                    changed_fields: TaskFieldMask::changed(&current, &desired),
                    desired,
                    rollback_entry,
                },
            );
        }
        if let Err(error) = persist_or_remove_journal(self.store.as_ref(), &state.journal) {
            return self.degrade_locked(
                &mut state,
                format!("cannot persist pre-task journal: {error}"),
            );
        }

        let mut applied = BTreeMap::new();
        let mut attempted = BTreeSet::new();
        let mut failure = None;
        for request in requests {
            let mutation = &prepared[&request.identity];
            if mutation.changed_fields.is_empty() {
                applied.insert(request.identity, mutation.before.clone());
                continue;
            }
            if let Err(error) = verify_process_identity(proc_reader.as_ref(), request.identity) {
                failure = Some((request.identity, error));
                break;
            }
            attempted.insert(request.identity);
            match controller.write_scheduling(request.identity.pid, &mutation.desired) {
                Ok(actual) if actual == mutation.desired => {
                    if let Err(error) =
                        verify_process_identity(proc_reader.as_ref(), request.identity)
                    {
                        failure = Some((request.identity, error));
                        break;
                    }
                    if let Some(entry) = state
                        .journal
                        .tasks
                        .get_mut(&task_journal_key(request.identity))
                    {
                        entry.applied = actual.clone();
                        entry.desired = actual.clone();
                    }
                    applied.insert(request.identity, actual);
                }
                Ok(actual) => {
                    failure = Some((
                        request.identity,
                        ActuatorError::Transaction {
                            target: format!("pid {}", request.identity.pid.get()),
                            reason: format!(
                                "task readback {actual:?} differs from requested {:?}",
                                mutation.desired
                            ),
                        },
                    ));
                    break;
                }
                Err(error) => {
                    failure = Some((request.identity, ActuatorError::Platform(error)));
                    break;
                }
            }
        }
        if let Some((identity, error)) = failure {
            if let Err(rollback) = rollback_tasks(
                controller.as_ref(),
                proc_reader.as_ref(),
                &prepared,
                &attempted,
            ) {
                return self.degrade_locked(
                    &mut state,
                    format!(
                        "task transaction failed for pid {}: {error}; rollback failed: {rollback}",
                        identity.pid.get()
                    ),
                );
            }
            for (identity, mutation) in &prepared {
                let key = task_journal_key(*identity);
                state.journal.tasks.remove(&key);
                if let Some(entry) = &mutation.rollback_entry {
                    state.journal.tasks.insert(key, entry.clone());
                }
            }
            if let Err(persist_error) = self.persist_or_remove_locked(&mut state, "task rollback") {
                return self.degrade_locked(
                    &mut state,
                    format!(
                        "task transaction rollback succeeded but journal update failed: {persist_error}"
                    ),
                );
            }
            return Err(ActuatorError::Transaction {
                target: format!("pid {}", identity.pid.get()),
                reason: error.to_string(),
            });
        }
        if let Err(error) = persist_or_remove_journal(self.store.as_ref(), &state.journal) {
            if let Err(rollback) = rollback_tasks(
                controller.as_ref(),
                proc_reader.as_ref(),
                &prepared,
                &attempted,
            ) {
                return self.degrade_locked(
                    &mut state,
                    format!("post-task journal failed: {error}; rollback failed: {rollback}"),
                );
            }
            return self.degrade_locked(
                &mut state,
                format!("post-task journal failed; batch rolled back: {error}"),
            );
        }
        Ok(TaskBatchOutcome { applied })
    }

    /// Apply typed properties to caller-verified dedicated systemd units.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate/unsafe units, unavailable systemd,
    /// failed journaling, mutation/readback mismatch, or incomplete rollback.
    #[allow(
        clippy::too_many_lines,
        reason = "the transaction phases are kept adjacent to make rollback coverage auditable"
    )]
    pub fn apply_units(&self, requests: &[UnitRequest]) -> Result<UnitBatchOutcome, ActuatorError> {
        let mut state = self.lock_state()?;
        ensure_mutation_ready(&state)?;
        if requests.is_empty() {
            return Ok(UnitBatchOutcome {
                applied: BTreeMap::new(),
            });
        }
        let systemd = self
            .systemd
            .as_ref()
            .ok_or(ActuatorError::SystemdBackendUnavailable)?;
        let mut seen = BTreeSet::new();
        let mut observed = BTreeMap::new();
        for request in requests {
            validate_unit_name(&request.unit)?;
            if !seen.insert(request.unit.clone()) {
                return Err(ActuatorError::DuplicateRequest(request.unit.clone()));
            }
            let instance = systemd.unit_instance_identity(&request.unit)?;
            let properties = systemd.read_unit_properties(&request.unit)?;
            verify_unit_instance(systemd.as_ref(), &instance)?;
            observed.insert(request.unit.clone(), (instance, properties));
        }
        state.journal.generation = state.journal.generation.saturating_add(1);
        let mut prepared = BTreeMap::new();
        for request in requests {
            let (instance, current) = &observed[&request.unit];
            let existing = state
                .journal
                .units
                .remove(&request.unit)
                .filter(|entry| entry.instance == *instance);
            let (mut entry, rollback_entry) = if let Some(mut entry) = existing {
                let owned = entry.owned_fields;
                let still_owned =
                    owned.fields_matching_either(current, &entry.applied, &entry.applied);
                let lost = owned.without(still_owned);
                let relinquished = entry.relinquished_fields.union(lost);
                lost.copy(&mut entry.original, current);
                entry.desired.clone_from(current);
                entry.applied.clone_from(current);
                entry.owned_fields = still_owned;
                entry.relinquished_fields = relinquished;
                let rollback_entry = Some(entry.clone());
                (entry, rollback_entry)
            } else {
                (
                    UnitJournalEntry {
                        unit: request.unit.clone(),
                        instance: instance.clone(),
                        original: current.clone(),
                        desired: current.clone(),
                        applied: current.clone(),
                        owned_fields: UnitFieldMask::default(),
                        relinquished_fields: UnitFieldMask::default(),
                    },
                    None,
                )
            };
            let owned = entry.owned_fields;
            let relinquished = entry.relinquished_fields;
            let requested = UnitFieldMask::changed(current, &request.desired);
            let next_owned = owned.union(requested.without(relinquished));
            let newly_owned = next_owned.without(owned);
            newly_owned.copy(&mut entry.original, current);
            let mut desired = current.clone();
            next_owned.copy(&mut desired, &request.desired);
            entry.desired.clone_from(&desired);
            entry.applied.clone_from(current);
            entry.owned_fields = next_owned;
            entry.relinquished_fields = relinquished;
            if !next_owned.is_empty() || !relinquished.is_empty() {
                state.journal.units.insert(request.unit.clone(), entry);
            }
            prepared.insert(
                request.unit.clone(),
                PreparedUnitMutation {
                    instance: instance.clone(),
                    before: current.clone(),
                    changed_fields: UnitFieldMask::changed(current, &desired),
                    desired,
                    rollback_entry,
                },
            );
        }
        if let Err(error) = persist_or_remove_journal(self.store.as_ref(), &state.journal) {
            return self.degrade_locked(
                &mut state,
                format!("cannot persist pre-systemd journal: {error}"),
            );
        }
        let mut applied = BTreeMap::new();
        let mut attempted = BTreeSet::new();
        let mut failure = None;
        for request in requests {
            let mutation = &prepared[&request.unit];
            if mutation.changed_fields.is_empty() {
                applied.insert(request.unit.clone(), mutation.before.clone());
                continue;
            }
            if let Err(error) = verify_unit_instance(systemd.as_ref(), &mutation.instance) {
                failure = Some((request.unit.clone(), error.to_string()));
                break;
            }
            attempted.insert(request.unit.clone());
            match systemd.write_unit_properties(&request.unit, &mutation.desired) {
                Ok(actual) if actual == mutation.desired => {
                    if let Err(error) = verify_unit_instance(systemd.as_ref(), &mutation.instance) {
                        failure = Some((request.unit.clone(), error.to_string()));
                        break;
                    }
                    if let Some(entry) = state.journal.units.get_mut(&request.unit) {
                        entry.applied = actual.clone();
                        entry.desired = actual.clone();
                    }
                    applied.insert(request.unit.clone(), actual);
                }
                Ok(actual) => {
                    failure = Some((
                        request.unit.clone(),
                        format!(
                            "readback {actual:?} differs from requested {:?}",
                            mutation.desired
                        ),
                    ));
                    break;
                }
                Err(error) => {
                    failure = Some((request.unit.clone(), error.to_string()));
                    break;
                }
            }
        }
        if let Some((unit, reason)) = failure {
            if let Err(error) = rollback_units(systemd.as_ref(), &prepared, &attempted) {
                return self.degrade_locked(
                    &mut state,
                    format!(
                        "systemd transaction failed for {unit}: {reason}; rollback failed: {error}"
                    ),
                );
            }
            for (unit, mutation) in &prepared {
                state.journal.units.remove(unit);
                if let Some(entry) = &mutation.rollback_entry {
                    state.journal.units.insert(unit.clone(), entry.clone());
                }
            }
            if let Err(error) = self.persist_or_remove_locked(&mut state, "systemd rollback") {
                return self.degrade_locked(
                    &mut state,
                    format!(
                        "systemd transaction rollback succeeded but journal update failed: {error}"
                    ),
                );
            }
            return Err(ActuatorError::Transaction {
                target: unit,
                reason,
            });
        }
        if let Err(error) = persist_or_remove_journal(self.store.as_ref(), &state.journal) {
            if let Err(rollback) = rollback_units(systemd.as_ref(), &prepared, &attempted) {
                return self.degrade_locked(
                    &mut state,
                    format!("post-systemd journal failed: {error}; rollback failed: {rollback}"),
                );
            }
            return self.degrade_locked(
                &mut state,
                format!("post-systemd journal failed; batch rolled back: {error}"),
            );
        }
        Ok(UnitBatchOutcome { applied })
    }

    /// Restore all resources originally claimed by this daemon.
    ///
    /// # Errors
    ///
    /// Returns an error if any resource cannot be safely restored and verified
    /// or the completed journal cannot be durably updated.
    pub fn restore_all(&self) -> Result<(), ActuatorError> {
        let ids = {
            let state = self.lock_state()?;
            state.journal.entries.keys().cloned().collect::<Vec<_>>()
        };
        self.restore_targets(&ids)?;
        let scalar_ids = {
            let state = self.lock_state()?;
            state.journal.scalars.keys().cloned().collect::<Vec<_>>()
        };
        self.restore_scalars(&scalar_ids)?;
        let task_ids = {
            let state = self.lock_state()?;
            state
                .journal
                .tasks
                .values()
                .map(|entry| entry.identity)
                .collect::<Vec<_>>()
        };
        self.restore_tasks(&task_ids)?;
        let units = {
            let state = self.lock_state()?;
            state.journal.units.keys().cloned().collect::<Vec<_>>()
        };
        self.restore_units(&units)
    }

    /// Restore selected resources to their journaled original values.
    ///
    /// This is used when a manual-only target override is cleared while
    /// automatic CPU targets remain under daemon ownership.
    ///
    /// # Errors
    ///
    /// Returns an error and degrades the actuator when target restoration,
    /// verification, or durable journal completion fails.
    pub fn restore_targets(&self, ids: &[TargetId]) -> Result<(), ActuatorError> {
        let mut state = self.lock_state()?;
        ensure_mutation_ready(&state)?;
        let entries = ids
            .iter()
            .filter_map(|id| state.journal.entries.get(id).cloned())
            .collect::<Vec<_>>();
        let mut restoration = Vec::new();
        for entry in &entries {
            let Some(target) = self.registry.get(&entry.target) else {
                return self.degrade_locked(
                    &mut state,
                    format!("restore target {} is not present", entry.target),
                );
            };
            if entry.original != hardware_limits(target) {
                return self.degrade_locked(
                    &mut state,
                    format!(
                        "restore target {} was claimed from a constrained effective range; automatic restoration could make a transient cap permanent",
                        entry.target
                    ),
                );
            }
            if let Some(journal_entry) = state.journal.entries.get_mut(&entry.target) {
                journal_entry.applied = entry.desired;
                journal_entry.desired = entry.original;
                journal_entry.legal_pairs = transaction_legal_pairs(entry.desired, entry.original);
            }
            restoration.push((entry.clone(), target.clone()));
        }
        if !restoration.is_empty()
            && let Err(error) = persist_journal(self.store.as_ref(), &state.journal)
        {
            return self.degrade_locked(
                &mut state,
                format!("cannot persist frequency restore intent: {error}"),
            );
        }
        for (entry, target) in &restoration {
            if let Err(error) = restore_frequency_request(self.io.as_ref(), target, entry.original)
            {
                return self.degrade_locked(
                    &mut state,
                    format!("restore failed for {}: {error}", entry.target),
                );
            }
        }
        for entry in &entries {
            state.journal.entries.remove(&entry.target);
        }
        state.journal.generation = state.journal.generation.saturating_add(1);
        self.persist_or_remove_locked(&mut state, "frequency restore")
    }

    /// Restore selected scalar resources to their exact journaled originals.
    ///
    /// A scalar changed to a third value by another writer is treated as
    /// relinquished ownership: it is removed from the journal without
    /// overwriting that external value.
    ///
    /// # Errors
    ///
    /// Returns an error and degrades the actuator when identity validation,
    /// readback, restoration, or durable journal completion fails.
    pub fn restore_scalars(&self, ids: &[TargetId]) -> Result<(), ActuatorError> {
        let mut state = self.lock_state()?;
        ensure_mutation_ready(&state)?;
        let entries = ids
            .iter()
            .filter_map(|id| state.journal.scalars.get(id).cloned())
            .collect::<Vec<_>>();
        let mut restoration = Vec::new();
        for entry in &entries {
            let Some(target) = self.registry.get_scalar(&entry.target) else {
                return self.degrade_locked(
                    &mut state,
                    format!("restore scalar target {} is not present", entry.target),
                );
            };
            if entry.path != target.path
                || entry.recovery_scalar_manifest() != &target.recovery_manifest()
            {
                return self.degrade_locked(
                    &mut state,
                    format!(
                        "restore scalar target identity changed for {}",
                        entry.target
                    ),
                );
            }
            let current = match read_scalar(self.io.as_ref(), target) {
                Ok(current) => current,
                Err(error) => {
                    return self.degrade_locked(
                        &mut state,
                        format!(
                            "cannot read scalar target {} during restore: {error}",
                            entry.target
                        ),
                    );
                }
            };
            let owned = current == entry.applied || current == entry.desired;
            let needs_write = owned && current != entry.original;
            if needs_write && let Some(journal_entry) = state.journal.scalars.get_mut(&entry.target)
            {
                journal_entry.applied.clone_from(&current);
                journal_entry.desired.clone_from(&entry.original);
            }
            restoration.push((entry.clone(), target.clone(), needs_write));
        }
        if restoration.iter().any(|(_, _, needs_write)| *needs_write)
            && let Err(error) = persist_journal(self.store.as_ref(), &state.journal)
        {
            return self.degrade_locked(
                &mut state,
                format!("cannot persist scalar restore intent: {error}"),
            );
        }
        for (entry, target, needs_write) in &restoration {
            if *needs_write
                && let Err(error) = write_scalar(self.io.as_ref(), target, &entry.original)
            {
                return self.degrade_locked(
                    &mut state,
                    format!("scalar restore failed for {}: {error}", entry.target),
                );
            }
        }
        for entry in &entries {
            state.journal.scalars.remove(&entry.target);
        }
        state.journal.generation = state.journal.generation.saturating_add(1);
        self.persist_or_remove_locked(&mut state, "scalar restore")
    }

    /// Restore selected process identities when their current state is still
    /// owned by this actuator.
    ///
    /// # Errors
    ///
    /// Returns an error and degrades the actuator when identity verification,
    /// task restoration, readback, or durable journal completion fails.
    pub fn restore_tasks(&self, ids: &[ProcessIdentity]) -> Result<(), ActuatorError> {
        let mut state = self.lock_state()?;
        ensure_mutation_ready(&state)?;
        let entries = ids
            .iter()
            .filter_map(|id| state.journal.tasks.get(&task_journal_key(*id)).cloned())
            .collect::<Vec<_>>();
        if entries.is_empty() {
            return Ok(());
        }
        let Some(proc_reader) = self.proc_reader.as_ref() else {
            return self
                .degrade_locked(&mut state, "task restore requires a proc reader".to_owned());
        };
        let Some(controller) = self.process_controller.as_ref() else {
            return self.degrade_locked(
                &mut state,
                "task restore requires a process controller".to_owned(),
            );
        };
        for entry in &entries {
            let identity_is_current =
                match process_identity_is_current(proc_reader.as_ref(), entry.identity) {
                    Ok(current) => current,
                    Err(error) => {
                        return self.degrade_locked(
                            &mut state,
                            format!(
                                "cannot verify pid {} during task restore: {error}",
                                entry.identity.pid.get()
                            ),
                        );
                    }
                };
            if !identity_is_current {
                continue;
            }
            let current = match controller.read_scheduling(entry.identity.pid) {
                Ok(current) => current,
                Err(error) => {
                    return self.degrade_locked(
                        &mut state,
                        format!(
                            "cannot read pid {} during task restore: {error}",
                            entry.identity.pid.get()
                        ),
                    );
                }
            };
            let owned = entry.owned_fields;
            let restorable = owned.fields_matching_either(&current, &entry.applied, &entry.desired);
            let mut desired = current.clone();
            restorable.copy(&mut desired, &entry.original);
            let changed = TaskFieldMask::changed(&current, &desired);
            if changed.is_empty() {
                continue;
            }
            let actual = match controller.write_scheduling(entry.identity.pid, &desired) {
                Ok(actual) => actual,
                Err(error) => {
                    return self.degrade_locked(
                        &mut state,
                        format!(
                            "cannot restore pid {} scheduling: {error}",
                            entry.identity.pid.get()
                        ),
                    );
                }
            };
            if let Err(error) = verify_process_identity(proc_reader.as_ref(), entry.identity) {
                return self.degrade_locked(
                    &mut state,
                    format!(
                        "pid {} identity changed during task restore: {error}",
                        entry.identity.pid.get()
                    ),
                );
            }
            if !changed.values_equal(&actual, &entry.original) {
                return self.degrade_locked(
                    &mut state,
                    format!(
                        "task restore readback differs for pid {}",
                        entry.identity.pid.get()
                    ),
                );
            }
        }
        for entry in &entries {
            state
                .journal
                .tasks
                .remove(&task_journal_key(entry.identity));
        }
        state.journal.generation = state.journal.generation.saturating_add(1);
        self.persist_or_remove_locked(&mut state, "task restore")
    }

    /// Restore selected systemd unit properties when the current values still
    /// equal the actuator's last write.
    ///
    /// # Errors
    ///
    /// Returns an error and degrades the actuator when property observation,
    /// restoration, readback, or durable journal completion fails.
    pub fn restore_units(&self, units: &[String]) -> Result<(), ActuatorError> {
        let mut state = self.lock_state()?;
        ensure_mutation_ready(&state)?;
        let entries = units
            .iter()
            .filter_map(|unit| state.journal.units.get(unit).cloned())
            .collect::<Vec<_>>();
        if entries.is_empty() {
            return Ok(());
        }
        let Some(systemd) = self.systemd.as_ref() else {
            return self.degrade_locked(
                &mut state,
                "systemd restore backend is unavailable".to_owned(),
            );
        };
        for entry in &entries {
            let instance = &entry.instance;
            let identity_is_current = match unit_instance_is_current(systemd.as_ref(), instance) {
                Ok(current) => current,
                Err(error) => {
                    return self.degrade_locked(
                        &mut state,
                        format!(
                            "cannot verify {} instance during systemd restore: {error}",
                            entry.unit
                        ),
                    );
                }
            };
            if !identity_is_current {
                continue;
            }
            let current = match systemd.read_unit_properties(&entry.unit) {
                Ok(current) => current,
                Err(error) => {
                    return self.degrade_locked(
                        &mut state,
                        format!("cannot read {} during systemd restore: {error}", entry.unit),
                    );
                }
            };
            if let Err(error) = verify_unit_instance(systemd.as_ref(), instance) {
                return self.degrade_locked(
                    &mut state,
                    format!(
                        "{} instance changed during systemd restore observation: {error}",
                        entry.unit
                    ),
                );
            }
            let owned = entry.owned_fields;
            let restorable = owned.fields_matching_either(&current, &entry.applied, &entry.desired);
            let mut desired = current.clone();
            restorable.copy(&mut desired, &entry.original);
            let changed = UnitFieldMask::changed(&current, &desired);
            if changed.is_empty() {
                continue;
            }
            let actual = match systemd.write_unit_properties(&entry.unit, &desired) {
                Ok(actual) => actual,
                Err(error) => {
                    return self.degrade_locked(
                        &mut state,
                        format!("cannot restore {} systemd properties: {error}", entry.unit),
                    );
                }
            };
            if let Err(error) = verify_unit_instance(systemd.as_ref(), instance) {
                return self.degrade_locked(
                    &mut state,
                    format!(
                        "{} instance changed during systemd restore write: {error}",
                        entry.unit
                    ),
                );
            }
            if !changed.values_equal(&actual, &entry.original) {
                return self.degrade_locked(
                    &mut state,
                    format!("systemd restore readback differs for {}", entry.unit),
                );
            }
        }
        for entry in &entries {
            state.journal.units.remove(&entry.unit);
        }
        state.journal.generation = state.journal.generation.saturating_add(1);
        self.persist_or_remove_locked(&mut state, "systemd restore")
    }

    fn recover_tasks_locked(&self, state: &mut RuntimeState) -> Result<(), ActuatorError> {
        if state.journal.tasks.is_empty() {
            return Ok(());
        }
        let proc_reader = self
            .proc_reader
            .as_ref()
            .ok_or(ActuatorError::TaskBackendUnavailable)?;
        let controller = self
            .process_controller
            .as_ref()
            .ok_or(ActuatorError::TaskBackendUnavailable)?;
        let entries = state.journal.tasks.values().cloned().collect::<Vec<_>>();
        for entry in entries {
            if !process_identity_is_current(proc_reader.as_ref(), entry.identity)? {
                continue;
            }
            let current = controller.read_scheduling(entry.identity.pid)?;
            let owned = entry.owned_fields;
            let restorable = owned.fields_matching_either(&current, &entry.applied, &entry.desired);
            let mut desired = current.clone();
            restorable.copy(&mut desired, &entry.original);
            let changed = TaskFieldMask::changed(&current, &desired);
            if changed.is_empty() {
                continue;
            }
            let actual = controller.write_scheduling(entry.identity.pid, &desired)?;
            verify_process_identity(proc_reader.as_ref(), entry.identity).map_err(|error| {
                ActuatorError::Rollback(format!(
                    "pid {} identity changed during task recovery: {error}",
                    entry.identity.pid.get()
                ))
            })?;
            if !changed.values_equal(&actual, &entry.original) {
                return Err(ActuatorError::Rollback(format!(
                    "pid {} task recovery readback differs",
                    entry.identity.pid.get()
                )));
            }
        }
        Ok(())
    }

    fn recover_scalars_locked(&self, state: &mut RuntimeState) -> Result<(), ActuatorError> {
        if state.journal.scalars.is_empty() {
            return Ok(());
        }
        let entries = state.journal.scalars.values().cloned().collect::<Vec<_>>();
        let mut restoration = Vec::new();
        for entry in &entries {
            let target = self.registry.get_scalar(&entry.target).ok_or_else(|| {
                ActuatorError::InvalidJournal(format!(
                    "scalar journal target {} is not present",
                    entry.target
                ))
            })?;
            if entry.path != target.path
                || entry.recovery_scalar_manifest() != &target.recovery_manifest()
            {
                return Err(ActuatorError::InvalidJournal(format!(
                    "scalar journal target identity changed for {}",
                    entry.target
                )));
            }
            let current = read_scalar(self.io.as_ref(), target)?;
            let owned = current == entry.applied || current == entry.desired;
            let needs_write = owned && current != entry.original;
            if needs_write && let Some(journal_entry) = state.journal.scalars.get_mut(&entry.target)
            {
                journal_entry.applied.clone_from(&current);
                journal_entry.desired.clone_from(&entry.original);
            }
            restoration.push((entry.clone(), target.clone(), needs_write));
        }
        if restoration.iter().any(|(_, _, needs_write)| *needs_write) {
            persist_journal(self.store.as_ref(), &state.journal)?;
        }
        for (entry, target, needs_write) in restoration {
            if needs_write {
                write_scalar(self.io.as_ref(), &target, &entry.original)?;
            }
        }
        Ok(())
    }

    fn recover_units_locked(&self, state: &mut RuntimeState) -> Result<(), ActuatorError> {
        if state.journal.units.is_empty() {
            return Ok(());
        }
        let systemd = self
            .systemd
            .as_ref()
            .ok_or(ActuatorError::SystemdBackendUnavailable)?;
        let entries = state.journal.units.values().cloned().collect::<Vec<_>>();
        for entry in entries {
            let instance = &entry.instance;
            if !unit_instance_is_current(systemd.as_ref(), instance)? {
                continue;
            }
            let current = systemd.read_unit_properties(&entry.unit)?;
            verify_unit_instance(systemd.as_ref(), instance)?;
            let owned = entry.owned_fields;
            let restorable = owned.fields_matching_either(&current, &entry.applied, &entry.desired);
            let mut desired = current.clone();
            restorable.copy(&mut desired, &entry.original);
            let changed = UnitFieldMask::changed(&current, &desired);
            if changed.is_empty() {
                continue;
            }
            let actual = systemd.write_unit_properties(&entry.unit, &desired)?;
            verify_unit_instance(systemd.as_ref(), instance)?;
            if !changed.values_equal(&actual, &entry.original) {
                return Err(ActuatorError::Rollback(format!(
                    "{} systemd recovery readback differs",
                    entry.unit
                )));
            }
        }
        Ok(())
    }

    fn persist_or_remove_locked(
        &self,
        state: &mut RuntimeState,
        operation: &str,
    ) -> Result<(), ActuatorError> {
        let result = if state.journal.is_empty() {
            self.store.remove_durable().map_err(ActuatorError::from)
        } else {
            persist_journal(self.store.as_ref(), &state.journal)
        };
        if let Err(error) = result {
            return self.degrade_locked(state, format!("cannot persist {operation}: {error}"));
        }
        Ok(())
    }

    fn lock_state(&self) -> Result<MutexGuard<'_, RuntimeState>, ActuatorError> {
        self.state.lock().map_err(|_| ActuatorError::LockPoisoned)
    }

    #[allow(
        clippy::unused_self,
        reason = "method form keeps every degradation transition visibly tied to the actuator"
    )]
    fn degrade_locked<T>(
        &self,
        state: &mut RuntimeState,
        reason: String,
    ) -> Result<T, ActuatorError> {
        state.mode = ActuatorMode::ReadOnlyDegraded {
            reason: reason.clone(),
        };
        Err(ActuatorError::Degraded(reason))
    }
}

fn validate_sysfs_path(path: &Path) -> Result<(), ActuatorError> {
    if !path.is_absolute()
        || !path.starts_with("/sys")
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(ActuatorError::InvalidTarget(format!(
            "path is not an absolute normalized /sys path: {}",
            path.display()
        )));
    }
    Ok(())
}

fn ensure_read_write(mode: &ActuatorMode) -> Result<(), ActuatorError> {
    match mode {
        ActuatorMode::ReadWrite => Ok(()),
        ActuatorMode::ReadOnlyDegraded { reason } => Err(ActuatorError::Degraded(reason.clone())),
    }
}

fn ensure_mutation_ready(state: &RuntimeState) -> Result<(), ActuatorError> {
    ensure_read_write(&state.mode)?;
    if state.recovery_required {
        Err(ActuatorError::RecoveryRequired)
    } else {
        Ok(())
    }
}

fn parse_hertz(path: &Path, value: &str, hertz_per_unit: u64) -> Result<Hertz, ActuatorError> {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .and_then(|raw| raw.checked_mul(hertz_per_unit))
        .map(Hertz::new)
        .ok_or_else(|| ActuatorError::InvalidReadback {
            path: path.to_path_buf(),
            value: value.to_owned(),
        })
}

fn read_limits(
    io: &dyn SysfsIo,
    target: &FrequencyTarget,
) -> Result<FrequencyLimits, ActuatorError> {
    let minimum = parse_hertz(
        &target.min_path,
        &io.read_string(&target.min_path)?,
        target.hertz_per_unit,
    )?;
    let maximum = parse_hertz(
        &target.max_path,
        &io.read_string(&target.max_path)?,
        target.hertz_per_unit,
    )?;
    FrequencyLimits::new(minimum, maximum).map_err(|_| ActuatorError::InvalidReadback {
        path: target.min_path.clone(),
        value: format!("{}..{}", minimum.get(), maximum.get()),
    })
}

fn validate_scalar_string(id: &TargetId, value: &str) -> Result<(), ActuatorError> {
    if value.is_empty()
        || value.len() > MAX_SCALAR_STRING_BYTES
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(ActuatorError::InvalidTarget(format!(
            "{id}: invalid scalar string enum value {value:?}"
        )));
    }
    Ok(())
}

fn read_scalar(io: &dyn SysfsIo, target: &ScalarTarget) -> Result<ScalarValue, ActuatorError> {
    let raw = io.read_string(&target.path)?;
    let trimmed = raw.trim();
    let value = match &target.domain {
        ScalarDomain::IntegerRange { .. } | ScalarDomain::IntegerEnum { .. } => trimmed
            .parse::<i64>()
            .map(ScalarValue::Integer)
            .map_err(|_| ActuatorError::InvalidReadback {
                path: target.path.clone(),
                value: raw.clone(),
            })?,
        ScalarDomain::StringEnum { .. } => ScalarValue::String(trimmed.to_owned()),
        ScalarDomain::CpuList { .. } => {
            ScalarValue::CpuList(parse_scalar_cpu_list(&target.path, trimmed)?)
        }
    };
    target
        .validate_value(&value)
        .map_err(|_| ActuatorError::InvalidReadback {
            path: target.path.clone(),
            value: raw,
        })?;
    Ok(value)
}

fn write_scalar(
    io: &dyn SysfsIo,
    target: &ScalarTarget,
    desired: &ScalarValue,
) -> Result<ScalarValue, ActuatorError> {
    target.validate_value(desired)?;
    let encoded = encode_scalar_value(desired);
    io.write_string(&target.path, &encoded)?;
    let actual = read_scalar(io, target)?;
    if actual != *desired {
        return Err(ActuatorError::Transaction {
            target: target.id.to_string(),
            reason: format!("scalar readback differs: expected {desired:?}, got {actual:?}"),
        });
    }
    Ok(actual)
}

fn encode_scalar_value(value: &ScalarValue) -> String {
    match value {
        ScalarValue::Integer(value) => value.to_string(),
        ScalarValue::String(value) => value.clone(),
        ScalarValue::CpuList(cpus) => format_scalar_cpu_list(cpus),
    }
}

fn format_scalar_cpu_list(cpus: &CpuSet) -> String {
    let ids = cpus.iter().map(|cpu| cpu.get()).collect::<Vec<_>>();
    let mut ranges = Vec::new();
    let mut index = 0;
    while index < ids.len() {
        let start = ids[index];
        let mut end = start;
        while index + 1 < ids.len() && ids[index + 1] == end.saturating_add(1) {
            index += 1;
            end = ids[index];
        }
        if start == end {
            ranges.push(start.to_string());
        } else {
            ranges.push(format!("{start}-{end}"));
        }
        index += 1;
    }
    ranges.join(",")
}

fn parse_scalar_cpu_list(path: &Path, value: &str) -> Result<CpuSet, ActuatorError> {
    if value.is_empty() {
        return Ok(CpuSet::new());
    }
    let mut cpus = CpuSet::new();
    for comma_part in value.split(',') {
        if comma_part.trim().is_empty() {
            return Err(ActuatorError::InvalidReadback {
                path: path.to_path_buf(),
                value: value.to_owned(),
            });
        }
        for token in comma_part.split_ascii_whitespace() {
            let (start, end) = if let Some((start, end)) = token.split_once('-') {
                if end.contains('-') {
                    return Err(ActuatorError::InvalidReadback {
                        path: path.to_path_buf(),
                        value: value.to_owned(),
                    });
                }
                (
                    parse_scalar_cpu_id(path, value, start)?,
                    parse_scalar_cpu_id(path, value, end)?,
                )
            } else {
                let cpu = parse_scalar_cpu_id(path, value, token)?;
                (cpu, cpu)
            };
            if start > end {
                return Err(ActuatorError::InvalidReadback {
                    path: path.to_path_buf(),
                    value: value.to_owned(),
                });
            }
            for cpu in start..=end {
                cpus.insert(CpuId::new(cpu));
            }
        }
    }
    Ok(cpus)
}

fn parse_scalar_cpu_id(path: &Path, original: &str, value: &str) -> Result<u32, ActuatorError> {
    value
        .parse::<u32>()
        .ok()
        .filter(|cpu| *cpu <= MAX_SCALAR_CPU_ID)
        .ok_or_else(|| ActuatorError::InvalidReadback {
            path: path.to_path_buf(),
            value: original.to_owned(),
        })
}

fn rollback_scalars(
    io: &dyn SysfsIo,
    prepared: &[PreparedScalarMutation],
    attempted: &[usize],
) -> Result<(), ActuatorError> {
    let mut failures = Vec::new();
    for index in attempted.iter().rev() {
        let mutation = &prepared[*index];
        let current = match read_scalar(io, &mutation.target) {
            Ok(current) => current,
            Err(error) => {
                failures.push(format!("{}: {error}", mutation.target.id));
                continue;
            }
        };
        if current == mutation.before || current != mutation.desired {
            continue;
        }
        if let Err(error) = write_scalar(io, &mutation.target, &mutation.before) {
            failures.push(format!("{}: {error}", mutation.target.id));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(ActuatorError::Rollback(failures.join("; ")))
    }
}

fn apply_requested_limits(
    io: &dyn SysfsIo,
    target: &FrequencyTarget,
    current_request: FrequencyLimits,
    desired_request: FrequencyLimits,
) -> Result<FrequencyLimits, ActuatorError> {
    target.validate_limits(current_request)?;
    target.validate_limits(desired_request)?;
    write_ordered(io, target, current_request, desired_request)?;
    wait_for_limits(io, target, desired_request)
}

fn restore_frequency_request(
    io: &dyn SysfsIo,
    target: &FrequencyTarget,
    desired_request: FrequencyLimits,
) -> Result<FrequencyLimits, ActuatorError> {
    target.validate_limits(desired_request)?;
    reset_frequency_request(io, target, desired_request)?;
    wait_for_limits(io, target, desired_request)
}

fn wait_for_limits(
    io: &dyn SysfsIo,
    target: &FrequencyTarget,
    requested: FrequencyLimits,
) -> Result<FrequencyLimits, ActuatorError> {
    let deadline = Instant::now() + FREQUENCY_SETTLE_TIMEOUT;
    loop {
        let observation = match read_limits(io, target) {
            Ok(actual) if actual == requested => return Ok(actual),
            Ok(actual) => format!("{}..{}", actual.min.get(), actual.max.get()),
            Err(error @ ActuatorError::InvalidReadback { .. }) => {
                // The two sysfs attributes are separate files. A policy worker
                // can run between those reads and briefly produce a reversed
                // or otherwise torn pair, so retry it within the same bound.
                error.to_string()
            }
            Err(error) => return Err(error),
        };

        let now = Instant::now();
        if now >= deadline {
            return Err(ActuatorError::Transaction {
                target: target.id.to_string(),
                reason: format!(
                    "readback did not settle at {}..{} within {} ms; last observation: {observation}",
                    requested.min.get(),
                    requested.max.get(),
                    FREQUENCY_SETTLE_TIMEOUT.as_millis()
                ),
            });
        }
        thread::sleep(FREQUENCY_SETTLE_POLL_INTERVAL.min(deadline.duration_since(now)));
    }
}

fn write_ordered(
    io: &dyn SysfsIo,
    target: &FrequencyTarget,
    current: FrequencyLimits,
    desired: FrequencyLimits,
) -> Result<(), ActuatorError> {
    let minimum = encode_frequency(target, desired.min)?;
    let maximum = encode_frequency(target, desired.max)?;
    if desired.max < current.min {
        if desired.min != current.min {
            io.write_string(&target.min_path, &minimum)?;
        }
        if desired.max != current.max {
            io.write_string(&target.max_path, &maximum)?;
        }
    } else {
        if desired.max != current.max {
            io.write_string(&target.max_path, &maximum)?;
        }
        if desired.min != current.min {
            io.write_string(&target.min_path, &minimum)?;
        }
    }
    Ok(())
}

fn reset_frequency_request(
    io: &dyn SysfsIo,
    target: &FrequencyTarget,
    desired: FrequencyLimits,
) -> Result<(), ActuatorError> {
    let hardware_minimum = encode_frequency(target, target.hardware_min)?;
    let hardware_maximum = encode_frequency(target, target.hardware_max)?;
    io.write_string(&target.min_path, &hardware_minimum)?;
    io.write_string(&target.max_path, &hardware_maximum)?;
    if desired.max != target.hardware_max {
        io.write_string(&target.max_path, &encode_frequency(target, desired.max)?)?;
    }
    if desired.min != target.hardware_min {
        io.write_string(&target.min_path, &encode_frequency(target, desired.min)?)?;
    }
    Ok(())
}

fn encode_frequency(target: &FrequencyTarget, frequency: Hertz) -> Result<String, ActuatorError> {
    frequency
        .get()
        .checked_div(target.hertz_per_unit)
        .map(|value| value.to_string())
        .ok_or_else(|| {
            ActuatorError::InvalidTarget(format!("{} has an invalid kernel unit", target.id))
        })
}

fn rollback_frequency_requests(
    io: &dyn SysfsIo,
    prepared: &[PreparedFrequencyMutation],
    attempted: &[usize],
) -> Result<(), ActuatorError> {
    let mut failures = Vec::new();
    for index in attempted.iter().rev() {
        let mutation = &prepared[*index];
        let result =
            restore_frequency_request(io, &mutation.target, mutation.previous_request).map(|_| ());
        if let Err(error) = result {
            failures.push(format!("{}: {error}", mutation.target.id));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(ActuatorError::Rollback(failures.join("; ")))
    }
}

fn hardware_limits(target: &FrequencyTarget) -> FrequencyLimits {
    FrequencyLimits {
        min: target.hardware_min,
        max: target.hardware_max,
    }
}

fn ordered_write_states(
    current: FrequencyLimits,
    desired: FrequencyLimits,
) -> Vec<FrequencyLimits> {
    let mut states = vec![current];
    let mut next = current;
    if desired.max < current.min {
        if desired.min != next.min {
            next.min = desired.min;
            push_unique_pair(&mut states, next);
        }
        if desired.max != next.max {
            next.max = desired.max;
            push_unique_pair(&mut states, next);
        }
    } else {
        if desired.max != next.max {
            next.max = desired.max;
            push_unique_pair(&mut states, next);
        }
        if desired.min != next.min {
            next.min = desired.min;
            push_unique_pair(&mut states, next);
        }
    }
    states
}

fn transaction_legal_pairs(
    current: FrequencyLimits,
    desired: FrequencyLimits,
) -> Vec<FrequencyLimits> {
    let mut pairs = ordered_write_states(current, desired);
    for pair in ordered_write_states(desired, current) {
        push_unique_pair(&mut pairs, pair);
    }
    pairs
}

fn push_unique_pair(pairs: &mut Vec<FrequencyLimits>, pair: FrequencyLimits) {
    if !pairs.contains(&pair) {
        pairs.push(pair);
    }
}

fn task_journal_key(identity: ProcessIdentity) -> String {
    format!(
        "{}:{}:{}",
        identity.pid.get(),
        identity.start_time_ticks,
        identity.uid.get()
    )
}

fn verify_process_identity(
    proc_reader: &dyn ProcReader,
    expected: ProcessIdentity,
) -> Result<(), ActuatorError> {
    if process_identity_is_current(proc_reader, expected)? {
        Ok(())
    } else {
        Err(ActuatorError::ProcessIdentityChanged(expected))
    }
}

fn process_identity_is_current(
    proc_reader: &dyn ProcReader,
    expected: ProcessIdentity,
) -> Result<bool, ActuatorError> {
    match proc_reader.process_identity(expected.pid) {
        Ok(process) => Ok(process.identity == expected),
        Err(PlatformError::Disappeared(_)) => Ok(false),
        Err(error) => Err(ActuatorError::Platform(error)),
    }
}

/// Record a rollback failure only while the original process still exists.
///
/// A task can exit after the rollback's identity check but before the following
/// scheduler read or write. Once the numeric PID has disappeared there is no
/// scheduler object left to restore, so that race is equivalent to a completed
/// rollback. A reused PID remains a failure: it is present with a different
/// stable identity and must never be treated as the exited original task.
fn record_task_rollback_failure_if_present(
    failures: &mut Vec<String>,
    proc_reader: &dyn ProcReader,
    identity: ProcessIdentity,
    failure: &str,
) {
    match proc_reader.process_identity(identity.pid) {
        Err(PlatformError::Disappeared(_)) => {}
        Ok(_) => failures.push(format!("pid {}: {failure}", identity.pid.get())),
        Err(error) => failures.push(format!(
            "pid {}: {failure}; identity recheck failed: {error}",
            identity.pid.get()
        )),
    }
}

fn verify_unit_instance(
    systemd: &dyn SystemdClient,
    expected: &SystemdUnitInstanceIdentity,
) -> Result<(), ActuatorError> {
    if unit_instance_is_current(systemd, expected)? {
        Ok(())
    } else {
        Err(ActuatorError::UnitInstanceChanged(expected.unit.clone()))
    }
}

fn unit_instance_is_current(
    systemd: &dyn SystemdClient,
    expected: &SystemdUnitInstanceIdentity,
) -> Result<bool, ActuatorError> {
    match systemd.unit_instance_identity(&expected.unit) {
        Ok(current) => Ok(current == *expected),
        Err(PlatformError::Disappeared(_)) => Ok(false),
        Err(error) => Err(ActuatorError::Platform(error)),
    }
}

fn rollback_tasks(
    controller: &dyn ProcessController,
    proc_reader: &dyn ProcReader,
    prepared: &BTreeMap<ProcessIdentity, PreparedTaskMutation>,
    attempted: &BTreeSet<ProcessIdentity>,
) -> Result<(), ActuatorError> {
    let mut failures = Vec::new();
    for (identity, mutation) in prepared.iter().rev() {
        if !attempted.contains(identity) {
            continue;
        }
        match process_identity_is_current(proc_reader, *identity) {
            Ok(true) => {
                let current = match controller.read_scheduling(identity.pid) {
                    Ok(current) => current,
                    Err(error) => {
                        record_task_rollback_failure_if_present(
                            &mut failures,
                            proc_reader,
                            *identity,
                            &error.to_string(),
                        );
                        continue;
                    }
                };
                // Close the read-side identity window before mutating. If the
                // PID was reused while its scheduler state was being read, the
                // new process must not receive the old task's rollback values.
                if !process_identity_is_current(proc_reader, *identity)? {
                    continue;
                }
                let restorable = mutation.changed_fields.fields_matching_either(
                    &current,
                    &mutation.desired,
                    &mutation.before,
                );
                let mut rollback = current.clone();
                restorable.copy(&mut rollback, &mutation.before);
                if restorable.is_empty() || rollback == current {
                    continue;
                }
                match controller.write_scheduling(identity.pid, &rollback) {
                    Ok(actual) if restorable.values_equal(&actual, &mutation.before) => {
                        if let Err(error) = verify_process_identity(proc_reader, *identity) {
                            record_task_rollback_failure_if_present(
                                &mut failures,
                                proc_reader,
                                *identity,
                                &format!("identity changed during rollback: {error}"),
                            );
                        }
                    }
                    Ok(actual) => record_task_rollback_failure_if_present(
                        &mut failures,
                        proc_reader,
                        *identity,
                        &format!("read back {actual:?}"),
                    ),
                    Err(error) => record_task_rollback_failure_if_present(
                        &mut failures,
                        proc_reader,
                        *identity,
                        &error.to_string(),
                    ),
                }
            }
            // The journal owns a stable identity, not the numeric PID/TID.
            // Once that identity has exited or the number was reused there is
            // no old scheduler object left to restore, and touching the new
            // object would be the unsafe action.
            Ok(false) => {}
            Err(error) => failures.push(format!("pid {}: {error}", identity.pid.get())),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(ActuatorError::Rollback(failures.join("; ")))
    }
}

fn rollback_units(
    systemd: &dyn SystemdClient,
    prepared: &BTreeMap<String, PreparedUnitMutation>,
    attempted: &BTreeSet<String>,
) -> Result<(), ActuatorError> {
    let mut failures = Vec::new();
    for (unit, mutation) in prepared.iter().rev() {
        if !attempted.contains(unit) {
            continue;
        }
        match unit_instance_is_current(systemd, &mutation.instance) {
            Ok(false) => continue,
            Err(error) => {
                failures.push(format!("{unit}: {error}"));
                continue;
            }
            Ok(true) => {}
        }
        let current = match systemd.read_unit_properties(unit) {
            Ok(current) => current,
            Err(error) => {
                failures.push(format!("{unit}: {error}"));
                continue;
            }
        };
        let restorable = mutation.changed_fields.fields_matching_either(
            &current,
            &mutation.desired,
            &mutation.before,
        );
        let mut rollback = current.clone();
        restorable.copy(&mut rollback, &mutation.before);
        if restorable.is_empty() || rollback == current {
            continue;
        }
        match systemd.write_unit_properties(unit, &rollback) {
            Ok(actual) if restorable.values_equal(&actual, &mutation.before) => {
                if let Err(error) = verify_unit_instance(systemd, &mutation.instance) {
                    failures.push(format!("{unit}: {error}"));
                }
            }
            Ok(actual) => failures.push(format!("{unit} read back {actual:?}")),
            Err(error) => failures.push(format!("{unit}: {error}")),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(ActuatorError::Rollback(failures.join("; ")))
    }
}

fn validate_unit_name(unit: &str) -> Result<(), ActuatorError> {
    if unit.is_empty()
        || unit.len() > 255
        || !unit.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'@' | b':' | b'\\')
        })
    {
        return Err(ActuatorError::InvalidTarget(format!(
            "invalid systemd unit name {unit:?}"
        )));
    }
    Ok(())
}

fn validate_unit_instance_identity(
    identity: &SystemdUnitInstanceIdentity,
) -> Result<(), ActuatorError> {
    validate_unit_name(&identity.unit)
        .map_err(|error| ActuatorError::InvalidJournal(error.to_string()))?;
    match &identity.key {
        SystemdUnitInstanceKey::InvocationId(bytes) if bytes.iter().all(|byte| *byte == 0) => {
            Err(ActuatorError::InvalidJournal(format!(
                "{} has an all-zero systemd invocation ID",
                identity.unit
            )))
        }
        SystemdUnitInstanceKey::ControlGroup(path)
            if path.is_empty()
                || !path.starts_with('/')
                || Path::new(path)
                    .components()
                    .any(|component| matches!(component, Component::ParentDir)) =>
        {
            Err(ActuatorError::InvalidJournal(format!(
                "{} has an invalid systemd control group identity",
                identity.unit
            )))
        }
        SystemdUnitInstanceKey::InvocationId(_) | SystemdUnitInstanceKey::ControlGroup(_) => Ok(()),
    }
}

fn encode_journal(journal: &Journal) -> Result<Vec<u8>, ActuatorError> {
    let payload = serde_json::to_vec(journal)
        .map_err(|error| ActuatorError::InvalidJournal(error.to_string()))?;
    if payload.len() > MAX_JOURNAL_PAYLOAD_BYTES {
        return Err(ActuatorError::InvalidJournal(format!(
            "payload exceeds {MAX_JOURNAL_PAYLOAD_BYTES} bytes"
        )));
    }
    let envelope = JournalEnvelope {
        checksum: crc32(&payload),
        payload,
    };
    let encoded = serde_json::to_vec(&envelope)
        .map_err(|error| ActuatorError::InvalidJournal(error.to_string()))?;
    if encoded.len() > MAX_JOURNAL_ENVELOPE_BYTES {
        return Err(ActuatorError::InvalidJournal(format!(
            "envelope exceeds {MAX_JOURNAL_ENVELOPE_BYTES} bytes"
        )));
    }
    Ok(encoded)
}

fn decode_journal(bytes: &[u8]) -> Result<Journal, ActuatorError> {
    if bytes.len() > MAX_JOURNAL_ENVELOPE_BYTES {
        return Err(ActuatorError::InvalidJournal(format!(
            "envelope exceeds {MAX_JOURNAL_ENVELOPE_BYTES} bytes"
        )));
    }
    let envelope: JournalEnvelope = serde_json::from_slice(bytes)
        .map_err(|error| ActuatorError::InvalidJournal(error.to_string()))?;
    if envelope.payload.len() > MAX_JOURNAL_PAYLOAD_BYTES {
        return Err(ActuatorError::InvalidJournal(format!(
            "payload exceeds {MAX_JOURNAL_PAYLOAD_BYTES} bytes"
        )));
    }
    if crc32(&envelope.payload) != envelope.checksum {
        return Err(ActuatorError::InvalidJournal(
            "checksum mismatch".to_owned(),
        ));
    }
    let journal: Journal = serde_json::from_slice(&envelope.payload)
        .map_err(|error| ActuatorError::InvalidJournal(error.to_string()))?;
    validate_decoded_journal(&journal)?;
    Ok(journal)
}

#[allow(
    clippy::too_many_lines,
    reason = "schema-version and cross-field validation stays centralized for fail-closed decoding"
)]
fn validate_decoded_journal(journal: &Journal) -> Result<(), ActuatorError> {
    if journal.schema_version != JOURNAL_SCHEMA_VERSION {
        return Err(ActuatorError::InvalidJournal(format!(
            "unsupported schema version {}",
            journal.schema_version
        )));
    }
    if journal.boot_id.is_empty() || journal.device_fingerprint.is_empty() {
        return Err(ActuatorError::InvalidJournal(
            "boot ID and device fingerprint must be non-empty".to_owned(),
        ));
    }
    let mut claimed_sysfs_paths = BTreeMap::<PathBuf, String>::new();
    for (key, entry) in &journal.entries {
        if key != &entry.target {
            return Err(ActuatorError::InvalidJournal(format!(
                "frequency journal key does not match {}",
                entry.target
            )));
        }
        validate_sysfs_path(&entry.min_path)
            .map_err(|error| ActuatorError::InvalidJournal(error.to_string()))?;
        validate_sysfs_path(&entry.max_path)
            .map_err(|error| ActuatorError::InvalidJournal(error.to_string()))?;
        for path in [&entry.min_path, &entry.max_path] {
            if let Some(owner) =
                claimed_sysfs_paths.insert(path.clone(), format!("frequency {}", entry.target))
            {
                return Err(ActuatorError::InvalidJournal(format!(
                    "{} and frequency {} both claim {}",
                    owner,
                    entry.target,
                    path.display()
                )));
            }
        }
        for (name, limits) in [
            ("original", entry.original),
            ("desired", entry.desired),
            ("applied", entry.applied),
        ] {
            if !limits.is_valid() {
                return Err(ActuatorError::InvalidJournal(format!(
                    "{} {name} limits are reversed",
                    entry.target
                )));
            }
        }
        let RecoveryResourceManifest::FrequencyPair(manifest) = &entry.resource_manifest else {
            return Err(ActuatorError::InvalidJournal(format!(
                "{} frequency entry has a non-frequency manifest",
                entry.target
            )));
        };
        if manifest.id != entry.target
            || manifest.min_path != entry.min_path
            || manifest.max_path != entry.max_path
        {
            return Err(ActuatorError::InvalidJournal(format!(
                "{} recovery manifest identity does not match its entry",
                entry.target
            )));
        }
        let target = manifest
            .to_frequency_target()
            .map_err(|error| ActuatorError::InvalidJournal(error.to_string()))?;
        for limits in [entry.original, entry.desired, entry.applied] {
            target
                .validate_limits(limits)
                .map_err(|error| ActuatorError::InvalidJournal(error.to_string()))?;
        }
        if entry.original != hardware_limits(&target) {
            return Err(ActuatorError::InvalidJournal(format!(
                "{} original request is not the full hardware range",
                entry.target
            )));
        }
        if entry.legal_pairs.is_empty() {
            return Err(ActuatorError::InvalidJournal(format!(
                "{} entry has no legal frequency pairs",
                entry.target
            )));
        }
        let mut unique = Vec::with_capacity(entry.legal_pairs.len());
        for pair in &entry.legal_pairs {
            target
                .validate_limits(*pair)
                .map_err(|error| ActuatorError::InvalidJournal(error.to_string()))?;
            if unique.contains(pair) {
                return Err(ActuatorError::InvalidJournal(format!(
                    "{} legal frequency pairs contain a duplicate",
                    entry.target
                )));
            }
            unique.push(*pair);
        }
        let expected = if entry.desired == entry.applied {
            vec![entry.applied]
        } else {
            transaction_legal_pairs(entry.applied, entry.desired)
        };
        if entry.legal_pairs != expected {
            return Err(ActuatorError::InvalidJournal(format!(
                "{} legal frequency pairs do not match its exact ordered transition",
                entry.target
            )));
        }
    }
    for (key, entry) in &journal.scalars {
        if key != &entry.target || journal.entries.contains_key(key) {
            return Err(ActuatorError::InvalidJournal(format!(
                "scalar journal key does not uniquely match {}",
                entry.target
            )));
        }
        validate_sysfs_path(&entry.path)
            .map_err(|error| ActuatorError::InvalidJournal(error.to_string()))?;
        if let Some(owner) =
            claimed_sysfs_paths.insert(entry.path.clone(), format!("scalar {}", entry.target))
        {
            return Err(ActuatorError::InvalidJournal(format!(
                "{} and scalar {} both claim {}",
                owner,
                entry.target,
                entry.path.display()
            )));
        }
        let RecoveryResourceManifest::Scalar(manifest) = &entry.manifest else {
            return Err(ActuatorError::InvalidJournal(format!(
                "{} scalar entry has a non-scalar manifest",
                entry.target
            )));
        };
        if manifest.id != entry.target || manifest.path != entry.path {
            return Err(ActuatorError::InvalidJournal(format!(
                "{} scalar recovery manifest identity does not match its entry",
                entry.target
            )));
        }
        let target = manifest
            .to_scalar_target()
            .map_err(|error| ActuatorError::InvalidJournal(error.to_string()))?;
        for value in [&entry.original, &entry.desired, &entry.applied] {
            target
                .validate_value(value)
                .map_err(|error| ActuatorError::InvalidJournal(error.to_string()))?;
        }
    }
    if journal
        .tasks
        .iter()
        .any(|(key, entry)| *key != task_journal_key(entry.identity))
    {
        return Err(ActuatorError::InvalidJournal(
            "task journal key does not match its stable identity".to_owned(),
        ));
    }
    for entry in journal.tasks.values() {
        for (name, state) in [
            ("original", &entry.original),
            ("desired", &entry.desired),
            ("applied", &entry.applied),
        ] {
            validate_journal_scheduling_state(state).map_err(|detail| {
                ActuatorError::InvalidJournal(format!(
                    "task journal {name} state for pid {} is invalid: {detail}",
                    entry.identity.pid.get()
                ))
            })?;
        }
        if entry.owned_fields.intersects(entry.relinquished_fields) {
            return Err(ActuatorError::InvalidJournal(format!(
                "task journal masks overlap for pid {}",
                entry.identity.pid.get()
            )));
        }
    }
    for (key, entry) in &journal.units {
        if key != &entry.unit {
            return Err(ActuatorError::InvalidJournal(format!(
                "systemd journal key does not match {}",
                entry.unit
            )));
        }
        validate_unit_name(&entry.unit)
            .map_err(|error| ActuatorError::InvalidJournal(error.to_string()))?;
        validate_unit_instance_identity(&entry.instance)?;
        if entry.instance.unit != entry.unit {
            return Err(ActuatorError::InvalidJournal(format!(
                "systemd instance identity does not match {}",
                entry.unit
            )));
        }
        if entry.owned_fields.intersects(entry.relinquished_fields) {
            return Err(ActuatorError::InvalidJournal(format!(
                "systemd journal masks overlap for {}",
                entry.unit
            )));
        }
    }
    Ok(())
}

fn persist_journal(store: &dyn StateStore, journal: &Journal) -> Result<(), ActuatorError> {
    store.store_durable(&encode_journal(journal)?)?;
    Ok(())
}

fn persist_or_remove_journal(
    store: &dyn StateStore,
    journal: &Journal,
) -> Result<(), ActuatorError> {
    if journal.is_empty() {
        store.remove_durable()?;
        Ok(())
    } else {
        persist_journal(store, journal)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet, VecDeque},
        io,
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
    };

    use uperf_core::{
        CpuId, CpuSet, FrequencyLimits, Hertz, ProcessId, ProcessIdentity, ProcessInfo, TargetId,
        UserId,
    };
    use uperf_platform::{
        PlatformError, PlatformResult, ProcessController, ProcessSchedulingState, SchedulingClass,
        StateStore, SysfsIo, SystemdClient, SystemdUnitInstanceIdentity, SystemdUnitInstanceKey,
        SystemdUnitProperties,
    };
    use uperf_testkit::FakeProc;

    use super::{
        ActuatorError, ActuatorMode, FrequencyActuator, FrequencyRequest, FrequencyTarget,
        JOURNAL_SCHEMA_VERSION, Journal, RecoveryResourceManifest, RecoveryResourceTarget,
        ScalarDomain, ScalarRequest, ScalarTarget, ScalarValue, TargetRegistry, TaskRequest,
        UnitRequest, decode_journal, encode_journal, inspect_recovery_journal,
        transaction_legal_pairs,
    };

    #[derive(Default)]
    struct MemorySysfs {
        values: Mutex<BTreeMap<PathBuf, String>>,
        writes: Mutex<Vec<(PathBuf, String)>>,
        fail_on_writes: Mutex<BTreeSet<usize>>,
        mutate_on_failure: Mutex<BTreeMap<PathBuf, String>>,
        scripted_reads: Mutex<BTreeMap<PathBuf, VecDeque<String>>>,
    }

    impl MemorySysfs {
        fn with_pair(minimum: &str, maximum: &str) -> Self {
            Self {
                values: Mutex::new(BTreeMap::from([
                    (PathBuf::from("/sys/test/min"), minimum.to_owned()),
                    (PathBuf::from("/sys/test/max"), maximum.to_owned()),
                ])),
                ..Self::default()
            }
        }

        fn with_values(values: impl IntoIterator<Item = (PathBuf, String)>) -> Self {
            Self {
                values: Mutex::new(values.into_iter().collect()),
                ..Self::default()
            }
        }

        fn fail_on(&self, write_number: usize) {
            let mut failures = self.fail_on_writes.lock().expect("fault lock");
            failures.clear();
            failures.insert(write_number);
        }

        fn fail_on_many(&self, write_numbers: impl IntoIterator<Item = usize>) {
            let mut failures = self.fail_on_writes.lock().expect("fault lock");
            failures.clear();
            failures.extend(write_numbers);
        }

        fn mutate_on_failed_write(&self, values: impl IntoIterator<Item = (PathBuf, String)>) {
            self.mutate_on_failure
                .lock()
                .expect("failure mutations lock")
                .extend(values);
        }

        fn script_reads(
            &self,
            path: impl Into<PathBuf>,
            values: impl IntoIterator<Item = impl Into<String>>,
        ) {
            self.scripted_reads
                .lock()
                .expect("scripted reads lock")
                .insert(
                    path.into(),
                    values.into_iter().map(Into::into).collect::<VecDeque<_>>(),
                );
        }

        fn writes(&self) -> Vec<(PathBuf, String)> {
            self.writes.lock().expect("writes lock").clone()
        }

        fn set_admin_pair(&self, minimum: &str, maximum: &str) {
            self.values.lock().expect("values lock").extend([
                (PathBuf::from("/sys/test/min"), minimum.to_owned()),
                (PathBuf::from("/sys/test/max"), maximum.to_owned()),
            ]);
        }

        fn set_value(&self, path: impl Into<PathBuf>, value: impl Into<String>) {
            self.values
                .lock()
                .expect("values lock")
                .insert(path.into(), value.into());
        }
    }

    impl SysfsIo for MemorySysfs {
        fn read_string(&self, path: &Path) -> PlatformResult<String> {
            if let Some(value) = self
                .scripted_reads
                .lock()
                .expect("scripted reads lock")
                .get_mut(path)
                .and_then(VecDeque::pop_front)
            {
                return Ok(value);
            }
            self.values
                .lock()
                .expect("values lock")
                .get(path)
                .cloned()
                .ok_or_else(|| {
                    PlatformError::io("read", path, io::Error::from(io::ErrorKind::NotFound))
                })
        }

        fn write_string(&self, path: &Path, value: &str) -> PlatformResult<()> {
            let mut writes = self.writes.lock().expect("writes lock");
            writes.push((path.to_path_buf(), value.to_owned()));
            if self
                .fail_on_writes
                .lock()
                .expect("fault lock")
                .contains(&writes.len())
            {
                let mutations = self
                    .mutate_on_failure
                    .lock()
                    .expect("failure mutations lock")
                    .clone();
                self.values.lock().expect("values lock").extend(mutations);
                return Err(PlatformError::io(
                    "write",
                    path,
                    io::Error::other("injected failure"),
                ));
            }
            self.values
                .lock()
                .expect("values lock")
                .insert(path.to_path_buf(), value.to_owned());
            Ok(())
        }
    }

    #[derive(Default)]
    struct MemoryStore {
        bytes: Mutex<Option<Vec<u8>>>,
        store_calls: Mutex<usize>,
        fail_on_store: Mutex<Option<usize>>,
        fail_remove: Mutex<bool>,
    }

    impl MemoryStore {
        fn corrupt() -> Self {
            Self {
                bytes: Mutex::new(Some(b"not a journal".to_vec())),
                ..Self::default()
            }
        }

        fn fail_on_store(&self, store_number: usize) {
            *self.fail_on_store.lock().expect("store fault lock") = Some(store_number);
        }

        fn store_calls(&self) -> usize {
            *self.store_calls.lock().expect("store calls lock")
        }

        fn fail_remove(&self) {
            *self.fail_remove.lock().expect("remove fault lock") = true;
        }
    }

    impl StateStore for MemoryStore {
        fn load(&self) -> PlatformResult<Option<Vec<u8>>> {
            Ok(self.bytes.lock().expect("store lock").clone())
        }

        fn store_durable(&self, bytes: &[u8]) -> PlatformResult<()> {
            let mut calls = self.store_calls.lock().expect("store calls lock");
            *calls += 1;
            if *self.fail_on_store.lock().expect("store fault lock") == Some(*calls) {
                return Err(PlatformError::invalid(
                    "memory-store",
                    "injected durable-store failure",
                ));
            }
            *self.bytes.lock().expect("store lock") = Some(bytes.to_vec());
            Ok(())
        }

        fn remove_durable(&self) -> PlatformResult<()> {
            if *self.fail_remove.lock().expect("remove fault lock") {
                return Err(PlatformError::invalid(
                    "memory-store",
                    "injected durable-remove failure",
                ));
            }
            *self.bytes.lock().expect("store lock") = None;
            Ok(())
        }
    }

    enum LifecycleMutation {
        Exit {
            proc_reader: Arc<FakeProc>,
            pid: ProcessId,
        },
        ExitBeforeRollbackRead {
            proc_reader: Arc<FakeProc>,
            pid: ProcessId,
        },
        Reuse {
            proc_reader: Arc<FakeProc>,
            process: ProcessInfo,
        },
    }

    #[derive(Default)]
    struct FaultingProcessController {
        states: Mutex<BTreeMap<ProcessId, ProcessSchedulingState>>,
        writes: Mutex<Vec<(ProcessId, ProcessSchedulingState)>>,
        write_attempts: Mutex<usize>,
        fail_on_write: Mutex<Option<usize>>,
        lifecycle_on_failure: Mutex<Option<LifecycleMutation>>,
        exit_before_read: Mutex<Option<(Arc<FakeProc>, ProcessId)>>,
        admin_on_failure: Mutex<Option<(ProcessId, ProcessSchedulingState)>>,
    }

    impl FaultingProcessController {
        fn insert(&self, pid: ProcessId, state: ProcessSchedulingState) {
            self.states
                .lock()
                .expect("process states lock")
                .insert(pid, state);
        }

        fn fail_on(&self, write_number: usize) {
            *self.fail_on_write.lock().expect("process fault lock") = Some(write_number);
        }

        fn set_admin(&self, process: ProcessId, state: ProcessSchedulingState) {
            self.states
                .lock()
                .expect("process states lock")
                .insert(process, state);
        }

        fn set_admin_on_failure(&self, process: ProcessId, state: ProcessSchedulingState) {
            *self
                .admin_on_failure
                .lock()
                .expect("process admin failure lock") = Some((process, state));
        }

        fn exit_on_failure(&self, proc_reader: Arc<FakeProc>, pid: ProcessId) {
            *self
                .lifecycle_on_failure
                .lock()
                .expect("process lifecycle lock") =
                Some(LifecycleMutation::Exit { proc_reader, pid });
        }

        fn exit_before_rollback_read_on_failure(&self, proc_reader: Arc<FakeProc>, pid: ProcessId) {
            *self
                .lifecycle_on_failure
                .lock()
                .expect("process lifecycle lock") =
                Some(LifecycleMutation::ExitBeforeRollbackRead { proc_reader, pid });
        }

        fn reuse_on_failure(&self, proc_reader: Arc<FakeProc>, process: ProcessInfo) {
            *self
                .lifecycle_on_failure
                .lock()
                .expect("process lifecycle lock") = Some(LifecycleMutation::Reuse {
                proc_reader,
                process,
            });
        }

        fn writes(&self) -> Vec<(ProcessId, ProcessSchedulingState)> {
            self.writes.lock().expect("process writes lock").clone()
        }
    }

    impl ProcessController for FaultingProcessController {
        fn read_scheduling(&self, process: ProcessId) -> PlatformResult<ProcessSchedulingState> {
            let pending_exit = {
                let mut pending = self
                    .exit_before_read
                    .lock()
                    .expect("process read lifecycle lock");
                pending
                    .as_ref()
                    .is_some_and(|(_, pid)| *pid == process)
                    .then(|| pending.take().expect("matched pending process exit"))
            };
            if let Some((proc_reader, pid)) = pending_exit {
                proc_reader.remove_process(pid);
                self.states
                    .lock()
                    .expect("process states lock")
                    .remove(&pid);
            }
            self.states
                .lock()
                .expect("process states lock")
                .get(&process)
                .cloned()
                .ok_or_else(|| {
                    PlatformError::Disappeared(format!(
                        "missing scheduler state for pid {}",
                        process.get()
                    ))
                })
        }

        fn write_scheduling(
            &self,
            process: ProcessId,
            desired: &ProcessSchedulingState,
        ) -> PlatformResult<ProcessSchedulingState> {
            let mut attempts = self.write_attempts.lock().expect("process attempts lock");
            *attempts += 1;
            if *self.fail_on_write.lock().expect("process fault lock") == Some(*attempts) {
                if let Some((process, state)) = self
                    .admin_on_failure
                    .lock()
                    .expect("process admin failure lock")
                    .take()
                {
                    self.states
                        .lock()
                        .expect("process states lock")
                        .insert(process, state);
                }
                if let Some(mutation) = self
                    .lifecycle_on_failure
                    .lock()
                    .expect("process lifecycle lock")
                    .take()
                {
                    match mutation {
                        LifecycleMutation::Exit { proc_reader, pid } => {
                            proc_reader.remove_process(pid);
                        }
                        LifecycleMutation::ExitBeforeRollbackRead { proc_reader, pid } => {
                            *self
                                .exit_before_read
                                .lock()
                                .expect("process read lifecycle lock") = Some((proc_reader, pid));
                        }
                        LifecycleMutation::Reuse {
                            proc_reader,
                            process,
                        } => {
                            proc_reader.insert_process(process);
                        }
                    }
                }
                return Err(PlatformError::invalid(
                    format!("pid:{}", process.get()),
                    "injected scheduler write failure",
                ));
            }
            let mut states = self.states.lock().expect("process states lock");
            let Some(state) = states.get_mut(&process) else {
                return Err(PlatformError::Disappeared(format!(
                    "missing scheduler state for pid {}",
                    process.get()
                )));
            };
            state.clone_from(desired);
            self.writes
                .lock()
                .expect("process writes lock")
                .push((process, desired.clone()));
            Ok(desired.clone())
        }
    }

    #[derive(Default)]
    struct FaultingSystemd {
        process_units: Mutex<BTreeMap<ProcessId, String>>,
        unit_processes: Mutex<BTreeMap<String, Vec<ProcessId>>>,
        units: Mutex<BTreeMap<String, SystemdUnitProperties>>,
        instances: Mutex<BTreeMap<String, SystemdUnitInstanceIdentity>>,
        next_instance: Mutex<u128>,
        writes: Mutex<Vec<(String, SystemdUnitProperties)>>,
        write_attempts: Mutex<usize>,
        fail_on_write: Mutex<Option<usize>>,
    }

    impl FaultingSystemd {
        fn insert(&self, unit: &str, processes: Vec<ProcessId>, properties: SystemdUnitProperties) {
            for process in &processes {
                self.process_units
                    .lock()
                    .expect("process units lock")
                    .insert(*process, unit.to_owned());
            }
            self.unit_processes
                .lock()
                .expect("unit processes lock")
                .insert(unit.to_owned(), processes);
            self.units
                .lock()
                .expect("systemd units lock")
                .insert(unit.to_owned(), properties);
            let mut next = self.next_instance.lock().expect("instance counter lock");
            *next += 1;
            self.instances
                .lock()
                .expect("systemd instances lock")
                .insert(
                    unit.to_owned(),
                    SystemdUnitInstanceIdentity {
                        unit: unit.to_owned(),
                        key: SystemdUnitInstanceKey::InvocationId(next.to_be_bytes()),
                    },
                );
        }

        fn set_admin(&self, unit: &str, properties: SystemdUnitProperties) {
            self.units
                .lock()
                .expect("systemd units lock")
                .insert(unit.to_owned(), properties);
        }

        fn remove(&self, unit: &str) {
            self.units.lock().expect("systemd units lock").remove(unit);
            self.instances
                .lock()
                .expect("systemd instances lock")
                .remove(unit);
        }

        fn fail_on(&self, write_number: usize) {
            *self.fail_on_write.lock().expect("systemd fault lock") = Some(write_number);
        }

        fn writes(&self) -> Vec<(String, SystemdUnitProperties)> {
            self.writes.lock().expect("systemd writes lock").clone()
        }
    }

    impl SystemdClient for FaultingSystemd {
        fn unit_for_process(&self, process: ProcessId) -> PlatformResult<Option<String>> {
            Ok(self
                .process_units
                .lock()
                .expect("process units lock")
                .get(&process)
                .cloned())
        }

        fn unit_processes(&self, unit: &str) -> PlatformResult<Vec<ProcessId>> {
            self.unit_processes
                .lock()
                .expect("unit processes lock")
                .get(unit)
                .cloned()
                .ok_or_else(|| PlatformError::Disappeared(format!("missing unit {unit}")))
        }

        fn unit_instance_identity(
            &self,
            unit: &str,
        ) -> PlatformResult<SystemdUnitInstanceIdentity> {
            self.instances
                .lock()
                .expect("systemd instances lock")
                .get(unit)
                .cloned()
                .ok_or_else(|| PlatformError::Disappeared(format!("missing unit {unit}")))
        }

        fn read_unit_properties(&self, unit: &str) -> PlatformResult<SystemdUnitProperties> {
            self.units
                .lock()
                .expect("systemd units lock")
                .get(unit)
                .cloned()
                .ok_or_else(|| PlatformError::Disappeared(format!("missing unit {unit}")))
        }

        fn write_unit_properties(
            &self,
            unit: &str,
            desired: &SystemdUnitProperties,
        ) -> PlatformResult<SystemdUnitProperties> {
            let mut attempts = self.write_attempts.lock().expect("systemd attempts lock");
            *attempts += 1;
            if *self.fail_on_write.lock().expect("systemd fault lock") == Some(*attempts) {
                return Err(PlatformError::invalid(
                    unit,
                    "injected systemd write failure",
                ));
            }
            let mut units = self.units.lock().expect("systemd units lock");
            let Some(properties) = units.get_mut(unit) else {
                return Err(PlatformError::Disappeared(format!("missing unit {unit}")));
            };
            properties.clone_from(desired);
            self.writes
                .lock()
                .expect("systemd writes lock")
                .push((unit.to_owned(), desired.clone()));
            Ok(desired.clone())
        }
    }

    fn id() -> TargetId {
        TargetId::new("cpu.test").expect("target ID")
    }

    fn target() -> FrequencyTarget {
        FrequencyTarget::new(
            id(),
            "/sys/test/min",
            "/sys/test/max",
            Hertz::new(1_000),
            Hertz::new(3_000),
            vec![Hertz::new(1_000), Hertz::new(2_000), Hertz::new(3_000)],
        )
        .expect("target")
    }

    fn actuator(
        io: Arc<MemorySysfs>,
        store: Arc<MemoryStore>,
        boot_id: &str,
        fingerprint: &str,
    ) -> FrequencyActuator {
        FrequencyActuator::new(
            io,
            store,
            TargetRegistry::new([target()]).expect("registry"),
            boot_id,
            fingerprint,
        )
    }

    fn scalar_id(name: &str) -> TargetId {
        TargetId::new(name).expect("scalar target ID")
    }

    fn integer_scalar(name: &str, path: &str) -> ScalarTarget {
        ScalarTarget::new(
            scalar_id(name),
            path,
            ScalarDomain::IntegerRange {
                minimum: 0,
                maximum: 100,
            },
        )
        .expect("integer scalar target")
    }

    fn scalar_actuator(
        io: Arc<MemorySysfs>,
        store: Arc<MemoryStore>,
        targets: impl IntoIterator<Item = ScalarTarget>,
        boot_id: &str,
    ) -> FrequencyActuator {
        FrequencyActuator::new(
            io,
            store,
            TargetRegistry::default()
                .with_scalar_targets(targets)
                .expect("scalar registry"),
            boot_id,
            "device-a",
        )
    }

    fn limits(minimum: u64, maximum: u64) -> FrequencyLimits {
        FrequencyLimits::new(Hertz::new(minimum), Hertz::new(maximum)).expect("limits")
    }

    fn process_identity(pid: u32, start_time_ticks: u64) -> ProcessIdentity {
        ProcessIdentity {
            pid: ProcessId::new(pid),
            start_time_ticks,
            uid: UserId::new(1_000),
        }
    }

    fn process_info(identity: ProcessIdentity) -> ProcessInfo {
        ProcessInfo {
            identity,
            owner_control_safe: true,
            comm: format!("process-{}", identity.pid.get()),
            executable: None,
            desktop_id: None,
        }
    }

    fn insert_process(proc_reader: &FakeProc, identity: ProcessIdentity) {
        proc_reader.insert_process(process_info(identity));
    }

    fn scheduling(nice: i8) -> ProcessSchedulingState {
        ProcessSchedulingState {
            affinity: CpuSet::from_ids([CpuId::new(0), CpuId::new(2)]),
            nice,
            policy: SchedulingClass::Other,
            rt_priority: None,
            uclamp_min: Some(128),
            uclamp_max: Some(896),
        }
    }

    fn fifo_scheduling(nice: i8, priority: u8) -> ProcessSchedulingState {
        ProcessSchedulingState {
            policy: SchedulingClass::Fifo,
            rt_priority: Some(priority),
            ..scheduling(nice)
        }
    }

    fn unit_properties(cpu_weight: u64) -> SystemdUnitProperties {
        SystemdUnitProperties {
            cpu_weight: Some(cpu_weight),
            allowed_cpus: Some(CpuSet::from_ids([CpuId::new(0), CpuId::new(1)])),
        }
    }

    fn control_actuator(
        store: Arc<MemoryStore>,
        boot_id: &str,
        proc_reader: Option<Arc<FakeProc>>,
        process_controller: Option<Arc<FaultingProcessController>>,
        systemd: Option<Arc<FaultingSystemd>>,
    ) -> FrequencyActuator {
        let mut actuator = FrequencyActuator::new(
            Arc::new(MemorySysfs::default()),
            store,
            TargetRegistry::default(),
            boot_id,
            "device-a",
        );
        if let (Some(proc_reader), Some(process_controller)) = (proc_reader, process_controller) {
            actuator = actuator.with_process_backend(proc_reader, process_controller);
        }
        if let Some(systemd) = systemd {
            actuator = actuator.with_systemd_backend(systemd);
        }
        actuator
    }

    #[test]
    fn batch_is_verified_and_shutdown_restore_is_durable() {
        let io = Arc::new(MemorySysfs::with_pair("1000", "3000"));
        let store = Arc::new(MemoryStore::default());
        let actuator = actuator(io.clone(), store.clone(), "boot-a", "device-a");

        let outcome = actuator
            .apply_batch(&[FrequencyRequest {
                target: id(),
                limits: limits(2_000, 3_000),
            }])
            .expect("apply");
        assert_eq!(outcome.applied[&id()], limits(2_000, 3_000));
        assert!(store.load().expect("load journal").is_some());
        assert!(
            !actuator
                .startup_recovery_required()
                .expect("startup recovery")
        );
        assert!(actuator.has_owned_resources().expect("owned resources"));
        assert_eq!(
            io.writes(),
            vec![(PathBuf::from("/sys/test/min"), "2000".to_owned())]
        );

        actuator.restore_all().expect("restore");
        assert_eq!(
            actuator.read_limits(&id()).expect("read"),
            limits(1_000, 3_000)
        );
        assert!(store.load().expect("load journal").is_none());
        assert!(!actuator.has_owned_resources().expect("owned resources"));
    }

    #[test]
    fn fast_batch_requires_prior_durable_ownership() {
        let io = Arc::new(MemorySysfs::with_pair("1000", "3000"));
        let store = Arc::new(MemoryStore::default());
        let actuator = actuator(io.clone(), store.clone(), "boot-a", "device-a");

        assert!(matches!(
            actuator.apply_owned_batch_fast(&[FrequencyRequest {
                target: id(),
                limits: limits(2_000, 3_000),
            }]),
            Err(ActuatorError::OwnershipRequired(target)) if target == id().to_string()
        ));
        assert!(io.writes().is_empty());
        assert_eq!(store.store_calls(), 0);
        assert!(!actuator.has_owned_resources().expect("owned resources"));
    }

    #[test]
    fn fast_batch_updates_memory_without_persisting_each_transition() {
        let io = Arc::new(MemorySysfs::with_pair("1000", "3000"));
        let store = Arc::new(MemoryStore::default());
        let actuator = actuator(io.clone(), store.clone(), "boot-a", "device-a");
        actuator
            .apply_batch(&[FrequencyRequest {
                target: id(),
                limits: limits(2_000, 3_000),
            }])
            .expect("seed durable ownership");
        let store_calls = store.store_calls();
        let durable_before = store.load().expect("load durable ownership");
        store.fail_on_store(store_calls + 1);

        let first = actuator
            .apply_owned_batch_fast(&[FrequencyRequest {
                target: id(),
                limits: limits(1_000, 1_000),
            }])
            .expect("first fast transition");
        assert_eq!(first.applied[&id()], limits(1_000, 1_000));
        let second = actuator
            .apply_owned_batch_fast(&[FrequencyRequest {
                target: id(),
                limits: limits(3_000, 3_000),
            }])
            .expect("second fast transition uses in-memory request");
        assert_eq!(second.applied[&id()], limits(3_000, 3_000));

        assert_eq!(
            io.writes(),
            vec![
                (PathBuf::from("/sys/test/min"), "2000".to_owned()),
                (PathBuf::from("/sys/test/min"), "1000".to_owned()),
                (PathBuf::from("/sys/test/max"), "1000".to_owned()),
                (PathBuf::from("/sys/test/max"), "3000".to_owned()),
                (PathBuf::from("/sys/test/min"), "3000".to_owned()),
            ]
        );
        assert_eq!(store.store_calls(), store_calls);
        assert_eq!(
            store.load().expect("load unchanged durable ownership"),
            durable_before
        );
        let durable =
            decode_journal(&durable_before.expect("durable ownership must remain present"))
                .expect("decode durable ownership");
        assert_eq!(durable.entries[&id()].desired, limits(2_000, 3_000));
    }

    #[test]
    fn fast_batch_reasserts_an_unchanged_owned_request_after_external_drift() {
        let io = Arc::new(MemorySysfs::with_pair("1000", "3000"));
        let store = Arc::new(MemoryStore::default());
        let actuator = actuator(io.clone(), store.clone(), "boot-a", "device-a");
        let request = FrequencyRequest {
            target: id(),
            limits: limits(2_000, 3_000),
        };
        actuator
            .apply_batch(std::slice::from_ref(&request))
            .expect("seed durable ownership");
        let writes_before_drift = io.writes().len();
        let store_calls = store.store_calls();

        io.set_admin_pair("1000", "2000");
        let outcome = actuator
            .apply_owned_batch_fast(&[request])
            .expect("reassert the owned request");

        assert_eq!(outcome.applied[&id()], limits(2_000, 3_000));
        assert_eq!(
            actuator.read_limits(&id()).expect("corrected limits"),
            limits(2_000, 3_000)
        );
        assert_eq!(
            &io.writes()[writes_before_drift..],
            &[
                (PathBuf::from("/sys/test/max"), "3000".to_owned()),
                (PathBuf::from("/sys/test/min"), "2000".to_owned()),
            ]
        );
        assert_eq!(
            store.store_calls(),
            store_calls,
            "drift correction must remain on the owned fast path"
        );
    }

    #[test]
    fn crash_after_fast_batch_recovers_from_the_original_durable_claim() {
        let io = Arc::new(MemorySysfs::with_pair("1000", "3000"));
        let store = Arc::new(MemoryStore::default());
        let first_process = actuator(io.clone(), store.clone(), "boot-a", "device-a");
        first_process
            .apply_batch(&[FrequencyRequest {
                target: id(),
                limits: limits(2_000, 3_000),
            }])
            .expect("seed durable ownership");
        first_process
            .apply_owned_batch_fast(&[FrequencyRequest {
                target: id(),
                limits: limits(3_000, 3_000),
            }])
            .expect("unpersisted fast transition");
        assert_eq!(
            first_process.read_limits(&id()).expect("fast limits"),
            limits(3_000, 3_000)
        );
        drop(first_process);

        let restarted = actuator(io.clone(), store.clone(), "boot-a", "device-a");
        assert!(
            restarted
                .startup_recovery_required()
                .expect("startup recovery state")
        );
        restarted.recover_pending().expect("recover durable claim");
        assert_eq!(
            restarted.read_limits(&id()).expect("recovered limits"),
            limits(1_000, 3_000)
        );
        assert!(store.load().expect("journal removed").is_none());
    }

    #[test]
    fn failed_fast_batch_rolls_back_without_persisting() {
        let io = Arc::new(MemorySysfs::with_pair("1000", "3000"));
        let store = Arc::new(MemoryStore::default());
        let actuator = actuator(io.clone(), store.clone(), "boot-a", "device-a");
        actuator
            .apply_batch(&[FrequencyRequest {
                target: id(),
                limits: limits(2_000, 3_000),
            }])
            .expect("seed durable ownership");
        let store_calls = store.store_calls();
        let durable_before = store.load().expect("load durable ownership");
        store.fail_on_store(store_calls + 1);
        io.fail_on(3);

        assert!(matches!(
            actuator.apply_owned_batch_fast(&[FrequencyRequest {
                target: id(),
                limits: limits(1_000, 1_000),
            }]),
            Err(ActuatorError::Transaction { .. })
        ));
        assert_eq!(
            actuator.read_limits(&id()).expect("rolled-back request"),
            limits(2_000, 3_000)
        );
        assert_eq!(store.store_calls(), store_calls);
        assert_eq!(
            store.load().expect("load unchanged durable ownership"),
            durable_before
        );
        assert!(matches!(
            actuator.mode().expect("mode"),
            ActuatorMode::ReadWrite
        ));

        let retry = actuator
            .apply_owned_batch_fast(&[FrequencyRequest {
                target: id(),
                limits: limits(3_000, 3_000),
            }])
            .expect("retry from restored in-memory request");
        assert_eq!(retry.applied[&id()], limits(3_000, 3_000));
        assert_eq!(store.store_calls(), store_calls);
    }

    #[test]
    fn failed_fast_rollback_degrades_without_persisting_unknown_state() {
        let io = Arc::new(MemorySysfs::with_pair("1000", "3000"));
        let store = Arc::new(MemoryStore::default());
        let actuator = actuator(io.clone(), store.clone(), "boot-a", "device-a");
        actuator
            .apply_batch(&[FrequencyRequest {
                target: id(),
                limits: limits(2_000, 3_000),
            }])
            .expect("seed durable ownership");
        let store_calls = store.store_calls();
        io.fail_on_many([3, 4]);

        assert!(matches!(
            actuator.apply_owned_batch_fast(&[FrequencyRequest {
                target: id(),
                limits: limits(1_000, 2_000),
            }]),
            Err(ActuatorError::Degraded(_))
        ));
        assert_eq!(
            actuator
                .read_limits(&id())
                .expect("partially mutated limits"),
            limits(2_000, 2_000)
        );
        assert_eq!(store.store_calls(), store_calls);
        assert!(matches!(
            actuator.apply_owned_batch_fast(&[]),
            Err(ActuatorError::Degraded(_))
        ));
    }

    #[test]
    fn scalar_registry_rejects_invalid_domains_and_path_aliases() {
        assert!(matches!(
            ScalarTarget::new(
                scalar_id("scalar.bad-range"),
                "/sys/test/bad",
                ScalarDomain::IntegerRange {
                    minimum: 10,
                    maximum: 1,
                },
            ),
            Err(ActuatorError::InvalidTarget(_))
        ));
        assert!(matches!(
            ScalarTarget::new(
                scalar_id("scalar.bad-string"),
                "/sys/test/bad",
                ScalarDomain::StringEnum {
                    values: vec!["bad\nvalue".to_owned()],
                },
            ),
            Err(ActuatorError::InvalidTarget(_))
        ));
        let alias = integer_scalar("scalar.alias", "/sys/test/min");
        assert!(matches!(
            TargetRegistry::new([target()])
                .expect("frequency registry")
                .with_scalar_targets([alias]),
            Err(ActuatorError::InvalidTarget(_))
        ));
    }

    #[test]
    fn scalar_apply_is_typed_verified_and_restores_the_exact_original() {
        let path = PathBuf::from("/sys/test/scalar");
        let io = Arc::new(MemorySysfs::with_values([(
            path.clone(),
            "10\n".to_owned(),
        )]));
        let store = Arc::new(MemoryStore::default());
        let scalar = integer_scalar("scalar.bus", path.to_str().expect("UTF-8 path"));
        let actuator = scalar_actuator(io.clone(), store.clone(), [scalar], "boot-a");

        let outcome = actuator
            .apply_scalars(&[ScalarRequest {
                target: scalar_id("scalar.bus"),
                value: ScalarValue::Integer(20),
            }])
            .expect("apply scalar");
        assert_eq!(
            outcome.applied[&scalar_id("scalar.bus")],
            ScalarValue::Integer(20)
        );
        assert_eq!(io.writes(), vec![(path.clone(), "20".to_owned())]);
        let journal = decode_journal(
            &store
                .load()
                .expect("load scalar journal")
                .expect("owned scalar journal"),
        )
        .expect("decode scalar journal");
        assert_eq!(journal.schema_version, JOURNAL_SCHEMA_VERSION);
        let entry = &journal.scalars[&scalar_id("scalar.bus")];
        assert_eq!(entry.original, ScalarValue::Integer(10));
        assert_eq!(entry.desired, ScalarValue::Integer(20));
        assert_eq!(entry.applied, ScalarValue::Integer(20));
        assert!(matches!(
            entry.manifest,
            RecoveryResourceManifest::Scalar(_)
        ));

        actuator
            .restore_scalars(&[scalar_id("scalar.bus")])
            .expect("restore scalar");
        assert_eq!(
            actuator
                .read_scalar(&scalar_id("scalar.bus"))
                .expect("read restored scalar"),
            ScalarValue::Integer(10)
        );
        assert_eq!(
            io.writes(),
            vec![(path.clone(), "20".to_owned()), (path, "10".to_owned()),]
        );
        assert!(store.load().expect("journal removed").is_none());
    }

    #[test]
    fn unchanged_scalar_value_does_not_write_or_claim_ownership() {
        let path = PathBuf::from("/sys/test/scalar");
        let io = Arc::new(MemorySysfs::with_values([(path, "10".to_owned())]));
        let store = Arc::new(MemoryStore::default());
        let actuator = scalar_actuator(
            io.clone(),
            store.clone(),
            [integer_scalar("scalar.bus", "/sys/test/scalar")],
            "boot-a",
        );
        let request = ScalarRequest {
            target: scalar_id("scalar.bus"),
            value: ScalarValue::Integer(10),
        };

        actuator
            .apply_scalars(std::slice::from_ref(&request))
            .expect("unclaimed no-op");
        assert!(io.writes().is_empty());
        assert_eq!(store.store_calls(), 0);
        assert!(!actuator.has_owned_resources().expect("ownership"));

        let changed = ScalarRequest {
            value: ScalarValue::Integer(20),
            ..request
        };
        actuator
            .apply_scalars(std::slice::from_ref(&changed))
            .expect("claim scalar");
        let writes = io.writes().len();
        let stores = store.store_calls();
        actuator.apply_scalars(&[changed]).expect("owned no-op");
        assert_eq!(io.writes().len(), writes);
        assert_eq!(store.store_calls(), stores);
    }

    #[test]
    fn scalar_batch_supports_integer_string_and_canonical_cpu_list_values() {
        let integer_path = PathBuf::from("/sys/test/integer");
        let string_path = PathBuf::from("/sys/test/string");
        let cpus_path = PathBuf::from("/sys/test/cpus");
        let io = Arc::new(MemorySysfs::with_values([
            (integer_path.clone(), "1".to_owned()),
            (string_path.clone(), "powersave\n".to_owned()),
            (cpus_path.clone(), "0-1,4\n".to_owned()),
        ]));
        let store = Arc::new(MemoryStore::default());
        let integer = ScalarTarget::new(
            scalar_id("scalar.integer"),
            &integer_path,
            ScalarDomain::IntegerEnum {
                values: vec![3, 1, 3],
            },
        )
        .expect("integer enum");
        let string = ScalarTarget::new(
            scalar_id("scalar.string"),
            &string_path,
            ScalarDomain::StringEnum {
                values: vec!["powersave".to_owned(), "performance".to_owned()],
            },
        )
        .expect("string enum");
        let cpus = ScalarTarget::new(
            scalar_id("scalar.cpus"),
            &cpus_path,
            ScalarDomain::CpuList {
                allowed_cpus: CpuSet::from_ids((0..=4).map(CpuId::new)),
                allow_empty: false,
            },
        )
        .expect("CPU-list target");
        let actuator = scalar_actuator(io.clone(), store, [integer, string, cpus], "boot-a");
        let desired_cpus =
            CpuSet::from_ids([CpuId::new(0), CpuId::new(1), CpuId::new(2), CpuId::new(4)]);

        actuator
            .apply_scalars(&[
                ScalarRequest {
                    target: scalar_id("scalar.integer"),
                    value: ScalarValue::Integer(3),
                },
                ScalarRequest {
                    target: scalar_id("scalar.string"),
                    value: ScalarValue::String("performance".to_owned()),
                },
                ScalarRequest {
                    target: scalar_id("scalar.cpus"),
                    value: ScalarValue::CpuList(desired_cpus.clone()),
                },
            ])
            .expect("apply typed scalar batch");

        assert_eq!(
            io.writes(),
            vec![
                (integer_path, "3".to_owned()),
                (string_path, "performance".to_owned()),
                (cpus_path, "0-2,4".to_owned()),
            ]
        );
        assert_eq!(
            actuator
                .read_scalar(&scalar_id("scalar.cpus"))
                .expect("read CPU list"),
            ScalarValue::CpuList(desired_cpus)
        );
    }

    #[test]
    fn invalid_scalar_request_is_rejected_before_journaling_or_writing() {
        let io = Arc::new(MemorySysfs::with_values([(
            PathBuf::from("/sys/test/scalar"),
            "10".to_owned(),
        )]));
        let store = Arc::new(MemoryStore::default());
        let actuator = scalar_actuator(
            io.clone(),
            store.clone(),
            [integer_scalar("scalar.bus", "/sys/test/scalar")],
            "boot-a",
        );

        assert!(matches!(
            actuator.apply_scalars(&[ScalarRequest {
                target: scalar_id("scalar.bus"),
                value: ScalarValue::String("20".to_owned()),
            }]),
            Err(ActuatorError::InvalidScalarValue { .. })
        ));
        assert!(matches!(
            actuator.apply_scalars(&[ScalarRequest {
                target: scalar_id("scalar.bus"),
                value: ScalarValue::Integer(101),
            }]),
            Err(ActuatorError::InvalidScalarValue { .. })
        ));
        assert!(io.writes().is_empty());
        assert_eq!(store.store_calls(), 0);
    }

    #[test]
    fn scalar_readback_mismatch_is_detected_and_rolled_back() {
        let path = PathBuf::from("/sys/test/scalar");
        let io = Arc::new(MemorySysfs::with_values([(path.clone(), "10".to_owned())]));
        io.script_reads(&path, ["10", "30"]);
        let store = Arc::new(MemoryStore::default());
        let actuator = scalar_actuator(
            io.clone(),
            store.clone(),
            [integer_scalar("scalar.bus", "/sys/test/scalar")],
            "boot-a",
        );

        assert!(matches!(
            actuator.apply_scalars(&[ScalarRequest {
                target: scalar_id("scalar.bus"),
                value: ScalarValue::Integer(20),
            }]),
            Err(ActuatorError::Transaction { .. })
        ));
        assert_eq!(
            io.writes(),
            vec![(path.clone(), "20".to_owned()), (path, "10".to_owned()),]
        );
        assert_eq!(
            actuator
                .read_scalar(&scalar_id("scalar.bus"))
                .expect("rolled-back scalar"),
            ScalarValue::Integer(10)
        );
        assert!(store.load().expect("journal removed").is_none());
    }

    #[test]
    fn scalar_batch_failure_rolls_back_attempted_targets_in_reverse_order() {
        let first_path = PathBuf::from("/sys/test/first");
        let second_path = PathBuf::from("/sys/test/second");
        let io = Arc::new(MemorySysfs::with_values([
            (first_path.clone(), "10".to_owned()),
            (second_path.clone(), "20".to_owned()),
        ]));
        io.fail_on(2);
        let store = Arc::new(MemoryStore::default());
        let actuator = scalar_actuator(
            io.clone(),
            store.clone(),
            [
                integer_scalar("scalar.first", "/sys/test/first"),
                integer_scalar("scalar.second", "/sys/test/second"),
            ],
            "boot-a",
        );

        assert!(matches!(
            actuator.apply_scalars(&[
                ScalarRequest {
                    target: scalar_id("scalar.first"),
                    value: ScalarValue::Integer(20),
                },
                ScalarRequest {
                    target: scalar_id("scalar.second"),
                    value: ScalarValue::Integer(30),
                },
            ]),
            Err(ActuatorError::Transaction { .. })
        ));
        assert_eq!(
            io.writes(),
            vec![
                (first_path, "20".to_owned()),
                (second_path, "30".to_owned()),
                (PathBuf::from("/sys/test/first"), "10".to_owned()),
            ]
        );
        assert_eq!(
            actuator
                .read_scalar(&scalar_id("scalar.first"))
                .expect("first rollback"),
            ScalarValue::Integer(10)
        );
        assert!(store.load().expect("journal rollback").is_none());
        assert!(matches!(
            actuator.mode().expect("mode"),
            ActuatorMode::ReadWrite
        ));
    }

    #[test]
    fn failed_scalar_rollback_degrades_and_restart_recovers_the_durable_intent() {
        let first_path = PathBuf::from("/sys/test/first");
        let second_path = PathBuf::from("/sys/test/second");
        let io = Arc::new(MemorySysfs::with_values([
            (first_path, "10".to_owned()),
            (second_path, "20".to_owned()),
        ]));
        io.fail_on_many([2, 3]);
        let store = Arc::new(MemoryStore::default());
        let targets = || {
            [
                integer_scalar("scalar.first", "/sys/test/first"),
                integer_scalar("scalar.second", "/sys/test/second"),
            ]
        };
        let first_process = scalar_actuator(io.clone(), store.clone(), targets(), "boot-a");

        assert!(matches!(
            first_process.apply_scalars(&[
                ScalarRequest {
                    target: scalar_id("scalar.first"),
                    value: ScalarValue::Integer(20),
                },
                ScalarRequest {
                    target: scalar_id("scalar.second"),
                    value: ScalarValue::Integer(30),
                },
            ]),
            Err(ActuatorError::Degraded(_))
        ));
        assert!(store.load().expect("durable recovery intent").is_some());
        drop(first_process);

        let restarted = scalar_actuator(io.clone(), store.clone(), targets(), "boot-a");
        restarted
            .recover_pending()
            .expect("recover failed scalar rollback");
        assert_eq!(
            restarted
                .read_scalar(&scalar_id("scalar.first"))
                .expect("first recovered"),
            ScalarValue::Integer(10)
        );
        assert_eq!(
            restarted
                .read_scalar(&scalar_id("scalar.second"))
                .expect("second recovered"),
            ScalarValue::Integer(20)
        );
        assert!(store.load().expect("journal removed").is_none());
    }

    #[test]
    fn post_scalar_journal_failure_rolls_back_and_degrades() {
        let first_path = PathBuf::from("/sys/test/first");
        let second_path = PathBuf::from("/sys/test/second");
        let io = Arc::new(MemorySysfs::with_values([
            (first_path.clone(), "10".to_owned()),
            (second_path.clone(), "20".to_owned()),
        ]));
        let store = Arc::new(MemoryStore::default());
        store.fail_on_store(2);
        let actuator = scalar_actuator(
            io.clone(),
            store.clone(),
            [
                integer_scalar("scalar.first", "/sys/test/first"),
                integer_scalar("scalar.second", "/sys/test/second"),
            ],
            "boot-a",
        );

        assert!(matches!(
            actuator.apply_scalars(&[
                ScalarRequest {
                    target: scalar_id("scalar.first"),
                    value: ScalarValue::Integer(20),
                },
                ScalarRequest {
                    target: scalar_id("scalar.second"),
                    value: ScalarValue::Integer(30),
                },
            ]),
            Err(ActuatorError::Degraded(_))
        ));
        assert_eq!(
            io.writes(),
            vec![
                (first_path.clone(), "20".to_owned()),
                (second_path.clone(), "30".to_owned()),
                (second_path, "20".to_owned()),
                (first_path, "10".to_owned()),
            ]
        );
        assert!(store.load().expect("rolled-back journal").is_none());
    }

    #[test]
    fn scalar_restore_preserves_an_external_value_and_relinquishes_ownership() {
        let path = PathBuf::from("/sys/test/scalar");
        let io = Arc::new(MemorySysfs::with_values([(path.clone(), "10".to_owned())]));
        let store = Arc::new(MemoryStore::default());
        let actuator = scalar_actuator(
            io.clone(),
            store.clone(),
            [integer_scalar("scalar.bus", "/sys/test/scalar")],
            "boot-a",
        );
        actuator
            .apply_scalars(&[ScalarRequest {
                target: scalar_id("scalar.bus"),
                value: ScalarValue::Integer(20),
            }])
            .expect("claim scalar");
        io.set_value(&path, "30");
        assert!(matches!(
            actuator.apply_scalars(&[ScalarRequest {
                target: scalar_id("scalar.bus"),
                value: ScalarValue::Integer(40),
            }]),
            Err(ActuatorError::ScalarOwnershipLost(_))
        ));
        let writes = io.writes().len();

        actuator
            .restore_scalars(&[scalar_id("scalar.bus")])
            .expect("relinquish externally changed scalar");
        assert_eq!(
            actuator
                .read_scalar(&scalar_id("scalar.bus"))
                .expect("external scalar"),
            ScalarValue::Integer(30)
        );
        assert_eq!(io.writes().len(), writes);
        assert!(store.load().expect("journal removed").is_none());
    }

    #[test]
    fn tagged_recovery_manifest_unifies_frequency_and_scalar_resources() {
        let scalar_path = PathBuf::from("/sys/test/scalar");
        let io = Arc::new(MemorySysfs::with_values([
            (PathBuf::from("/sys/test/min"), "1000".to_owned()),
            (PathBuf::from("/sys/test/max"), "3000".to_owned()),
            (scalar_path, "10".to_owned()),
        ]));
        let store = Arc::new(MemoryStore::default());
        let registry = TargetRegistry::new([target()])
            .expect("frequency registry")
            .with_scalar_targets([integer_scalar("scalar.bus", "/sys/test/scalar")])
            .expect("combined registry");
        let first_process =
            FrequencyActuator::new(io.clone(), store.clone(), registry, "boot-a", "device-a");
        first_process
            .apply_batch(&[FrequencyRequest {
                target: id(),
                limits: limits(2_000, 3_000),
            }])
            .expect("claim frequency");
        first_process
            .apply_scalars(&[ScalarRequest {
                target: scalar_id("scalar.bus"),
                value: ScalarValue::Integer(20),
            }])
            .expect("claim scalar");
        let manifest = inspect_recovery_journal(store.as_ref())
            .expect("inspect tagged-resource journal")
            .expect("tagged recovery manifest");
        assert_eq!(manifest.schema_version, JOURNAL_SCHEMA_VERSION);
        assert!(matches!(
            manifest.resource_targets.as_slice(),
            [
                RecoveryResourceTarget::FrequencyPair(_),
                RecoveryResourceTarget::Scalar(_),
            ]
        ));
        assert_eq!(
            manifest.scalar_write_paths(),
            [PathBuf::from("/sys/test/scalar")]
        );
        let recovery_registry = manifest
            .self_describing_registry()
            .expect("combined self-describing registry");
        drop(first_process);

        let restarted =
            FrequencyActuator::new(io, store.clone(), recovery_registry, "boot-a", "device-a");
        restarted
            .recover_pending()
            .expect("recover combined resources");
        assert_eq!(
            restarted
                .read_scalar(&scalar_id("scalar.bus"))
                .expect("recovered scalar"),
            ScalarValue::Integer(10)
        );
        assert_eq!(
            restarted.read_limits(&id()).expect("recovered frequency"),
            limits(1_000, 3_000)
        );
        assert!(store.load().expect("journal removed").is_none());
    }

    #[test]
    fn unchanged_plan_performs_no_sysfs_write() {
        let io = Arc::new(MemorySysfs::with_pair("1000", "3000"));
        let actuator = actuator(
            io.clone(),
            Arc::new(MemoryStore::default()),
            "boot-a",
            "device-a",
        );
        actuator
            .apply_batch(&[FrequencyRequest {
                target: id(),
                limits: limits(1_000, 3_000),
            }])
            .expect("no-op apply");
        assert!(io.writes().is_empty());
    }

    #[test]
    fn unchanged_owned_request_does_not_rewrite_after_effective_limits_change() {
        let io = Arc::new(MemorySysfs::with_pair("1000", "3000"));
        let actuator = actuator(
            io.clone(),
            Arc::new(MemoryStore::default()),
            "boot-a",
            "device-a",
        );
        let request = FrequencyRequest {
            target: id(),
            limits: limits(2_000, 3_000),
        };
        actuator
            .apply_batch(std::slice::from_ref(&request))
            .expect("claim request");

        io.set_admin_pair("1000", "2000");
        let writes_before = io.writes().len();
        let outcome = actuator
            .apply_batch(&[request])
            .expect("unchanged request intent");

        assert_eq!(outcome.applied[&id()], limits(1_000, 2_000));
        assert_eq!(
            io.writes().len(),
            writes_before,
            "a changed effective aggregate must not cause request rewrites"
        );
    }

    #[test]
    fn delayed_cpufreq_readback_accepts_a_min_equals_max_request() {
        let io = Arc::new(MemorySysfs::with_pair("1000", "3000"));
        // The initial observation and first two post-write observations still
        // expose the old policy before the asynchronous worker catches up.
        io.script_reads("/sys/test/min", ["1000", "1000", "1000"]);
        let actuator = actuator(
            io.clone(),
            Arc::new(MemoryStore::default()),
            "boot-a",
            "device-a",
        );

        let outcome = actuator
            .apply_batch(&[FrequencyRequest {
                target: id(),
                limits: limits(3_000, 3_000),
            }])
            .expect("async cpufreq policy update must settle");

        assert_eq!(outcome.applied[&id()], limits(3_000, 3_000));
        assert_eq!(
            actuator.read_limits(&id()).expect("settled limits"),
            limits(3_000, 3_000)
        );
        assert_eq!(
            io.writes(),
            vec![(PathBuf::from("/sys/test/min"), "3000".to_owned())]
        );
        assert!(matches!(
            actuator.mode().expect("mode"),
            ActuatorMode::ReadWrite
        ));
    }

    #[test]
    fn constrained_effective_range_is_not_claimed_as_the_restore_target() {
        let io = Arc::new(MemorySysfs::with_pair("1000", "1000"));
        let store = Arc::new(MemoryStore::default());
        let actuator = actuator(io.clone(), store.clone(), "boot-a", "device-a");

        assert!(matches!(
            actuator.apply_batch(&[FrequencyRequest {
                target: id(),
                limits: limits(2_000, 2_000),
            }]),
            Err(ActuatorError::Transaction { .. })
        ));
        assert!(io.writes().is_empty());
        assert!(store.load().expect("journal read").is_none());
    }

    #[test]
    fn failed_pre_journal_never_enables_a_frequency_write_or_restore() {
        let io = Arc::new(MemorySysfs::with_pair("1000", "3000"));
        let store = Arc::new(MemoryStore::default());
        store.fail_on_store(1);
        let actuator = actuator(io.clone(), store, "boot-a", "device-a");

        assert!(matches!(
            actuator.apply_batch(&[FrequencyRequest {
                target: id(),
                limits: limits(2_000, 3_000),
            }]),
            Err(ActuatorError::Degraded(_))
        ));
        assert!(io.writes().is_empty());
        assert!(matches!(
            actuator.restore_all(),
            Err(ActuatorError::Degraded(_))
        ));
        assert!(io.writes().is_empty());
    }

    #[test]
    fn frequency_settle_timeout_rewrites_the_original_requests() {
        let io = Arc::new(MemorySysfs::with_pair("1000", "3000"));
        io.script_reads("/sys/test/min", std::iter::repeat_n("1000", 128));
        let actuator = actuator(
            io.clone(),
            Arc::new(MemoryStore::default()),
            "boot-a",
            "device-a",
        );

        assert!(matches!(
            actuator.apply_batch(&[FrequencyRequest {
                target: id(),
                limits: limits(3_000, 3_000),
            }]),
            Err(ActuatorError::Transaction { .. })
        ));
        assert_eq!(
            actuator.read_limits(&id()).expect("rolled-back limits"),
            limits(1_000, 3_000)
        );
        assert_eq!(
            io.writes(),
            vec![
                (PathBuf::from("/sys/test/min"), "3000".to_owned()),
                (PathBuf::from("/sys/test/min"), "1000".to_owned()),
                (PathBuf::from("/sys/test/max"), "3000".to_owned()),
            ],
            "rollback must rewrite both request endpoints even while effective readback is stale"
        );
        assert!(matches!(
            actuator.mode().expect("mode"),
            ActuatorMode::ReadWrite
        ));
    }

    #[test]
    fn failed_update_rolls_back_to_the_previous_request_not_stale_readback() {
        let io = Arc::new(MemorySysfs::with_pair("1000", "3000"));
        let store = Arc::new(MemoryStore::default());
        let actuator = actuator(io.clone(), store.clone(), "boot-a", "device-a");
        actuator
            .apply_batch(&[FrequencyRequest {
                target: id(),
                limits: limits(2_000, 3_000),
            }])
            .expect("seed owned request");

        io.script_reads("/sys/test/min", ["1000"]);
        io.fail_on(2);
        assert!(matches!(
            actuator.apply_batch(&[FrequencyRequest {
                target: id(),
                limits: limits(3_000, 3_000),
            }]),
            Err(ActuatorError::Transaction { .. })
        ));
        assert_eq!(
            actuator.read_limits(&id()).expect("rolled-back request"),
            limits(2_000, 3_000)
        );
        let journal = decode_journal(&store.load().expect("journal read").expect("owned journal"))
            .expect("journal decode");
        assert_eq!(journal.entries[&id()].desired, limits(2_000, 3_000));
    }

    #[test]
    fn partial_pair_failure_rolls_back_to_call_entry_state() {
        let io = Arc::new(MemorySysfs::with_pair("1000", "3000"));
        io.fail_on(2);
        let actuator = actuator(io, Arc::new(MemoryStore::default()), "boot-a", "device-a");
        let error = actuator
            .apply_batch(&[FrequencyRequest {
                target: id(),
                limits: limits(2_000, 2_000),
            }])
            .expect_err("injected write must fail");
        assert!(matches!(error, ActuatorError::Transaction { .. }));
        assert_eq!(
            actuator.read_limits(&id()).expect("read"),
            limits(1_000, 3_000)
        );
    }

    #[test]
    fn frequency_rollback_does_not_touch_an_unattempted_target() {
        let first_id = TargetId::new("cpu.first").expect("first ID");
        let second_id = TargetId::new("cpu.second").expect("second ID");
        let first = FrequencyTarget::new(
            first_id.clone(),
            "/sys/first/min",
            "/sys/first/max",
            Hertz::new(1_000),
            Hertz::new(3_000),
            vec![Hertz::new(1_000), Hertz::new(2_000), Hertz::new(3_000)],
        )
        .expect("first target");
        let second = FrequencyTarget::new(
            second_id.clone(),
            "/sys/second/min",
            "/sys/second/max",
            Hertz::new(1_000),
            Hertz::new(3_000),
            vec![Hertz::new(1_000), Hertz::new(2_000), Hertz::new(3_000)],
        )
        .expect("second target");
        let io = Arc::new(MemorySysfs {
            values: Mutex::new(BTreeMap::from([
                (PathBuf::from("/sys/first/min"), "1000".to_owned()),
                (PathBuf::from("/sys/first/max"), "3000".to_owned()),
                (PathBuf::from("/sys/second/min"), "1000".to_owned()),
                (PathBuf::from("/sys/second/max"), "3000".to_owned()),
            ])),
            ..MemorySysfs::default()
        });
        io.fail_on(1);
        io.mutate_on_failed_write([(PathBuf::from("/sys/second/min"), "2000".to_owned())]);
        let actuator = FrequencyActuator::new(
            io.clone(),
            Arc::new(MemoryStore::default()),
            TargetRegistry::new([first, second]).expect("registry"),
            "boot-a",
            "device-a",
        );

        assert!(matches!(
            actuator.apply_batch(&[
                FrequencyRequest {
                    target: first_id,
                    limits: limits(2_000, 3_000),
                },
                FrequencyRequest {
                    target: second_id.clone(),
                    limits: limits(2_000, 3_000),
                },
            ]),
            Err(ActuatorError::Transaction { .. })
        ));
        assert_eq!(
            actuator.read_limits(&second_id).expect("second limits"),
            limits(2_000, 3_000)
        );
        assert!(
            io.writes()
                .iter()
                .all(|(path, _)| !path.starts_with("/sys/second"))
        );
    }

    #[test]
    fn corrupt_journal_forces_read_only_degraded_mode() {
        let store = Arc::new(MemoryStore::corrupt());
        assert!(matches!(
            inspect_recovery_journal(store.as_ref()),
            Err(ActuatorError::InvalidJournal(_))
        ));
        let actuator = actuator(
            Arc::new(MemorySysfs::with_pair("1000", "3000")),
            store,
            "boot-a",
            "device-a",
        );
        assert!(matches!(
            actuator.mode().expect("mode"),
            ActuatorMode::ReadOnlyDegraded { .. }
        ));
        assert!(matches!(
            actuator.apply_batch(&[]),
            Err(ActuatorError::Degraded(_))
        ));
        assert!(matches!(
            actuator.restore_all(),
            Err(ActuatorError::Degraded(_))
        ));
        assert!(
            actuator
                .startup_recovery_failed()
                .expect("startup recovery failure")
        );
    }

    #[test]
    fn noncurrent_journal_schema_is_rejected_without_upgrade() {
        let journal = Journal {
            schema_version: JOURNAL_SCHEMA_VERSION - 1,
            boot_id: "boot-a".to_owned(),
            device_fingerprint: "device-a".to_owned(),
            generation: 0,
            entries: BTreeMap::new(),
            scalars: BTreeMap::new(),
            tasks: BTreeMap::new(),
            units: BTreeMap::new(),
        };
        let store = Arc::new(MemoryStore::default());
        store
            .store_durable(&encode_journal(&journal).expect("encode noncurrent journal"))
            .expect("store noncurrent journal");

        assert!(matches!(
            inspect_recovery_journal(store.as_ref()),
            Err(ActuatorError::InvalidJournal(message))
                if message.contains("unsupported schema version")
        ));
        let actuator = actuator(
            Arc::new(MemorySysfs::with_pair("1000", "3000")),
            store,
            "boot-a",
            "device-a",
        );
        assert!(matches!(
            actuator.mode().expect("mode"),
            ActuatorMode::ReadOnlyDegraded { .. }
        ));
    }

    #[test]
    fn propagated_startup_recovery_failure_is_distinct_from_active_ownership() {
        let actuator = actuator(
            Arc::new(MemorySysfs::with_pair("1000", "3000")),
            Arc::new(MemoryStore::default()),
            "boot-a",
            "device-a",
        );
        assert!(
            !actuator
                .startup_recovery_required()
                .expect("startup recovery")
        );
        actuator
            .mark_startup_recovery_failed("prior recovery fsync failed")
            .expect("mark failure");

        assert!(
            actuator
                .startup_recovery_required()
                .expect("startup recovery")
        );
        assert!(actuator.startup_recovery_failed().expect("startup failure"));
        assert!(!actuator.has_owned_resources().expect("owned resources"));
        assert!(matches!(
            actuator.apply_batch(&[]),
            Err(ActuatorError::Degraded(_))
        ));
    }

    #[test]
    fn same_boot_recovery_restores_owned_values() {
        let io = Arc::new(MemorySysfs::with_pair("1000", "3000"));
        let store = Arc::new(MemoryStore::default());
        actuator(io.clone(), store.clone(), "boot-a", "device-a")
            .apply_batch(&[FrequencyRequest {
                target: id(),
                limits: limits(2_000, 3_000),
            }])
            .expect("apply");

        let restarted = actuator(io.clone(), store.clone(), "boot-a", "device-a");
        let writes_before_restore = io.writes().len();
        assert!(matches!(
            restarted.restore_all(),
            Err(ActuatorError::RecoveryRequired)
        ));
        assert_eq!(
            io.writes().len(),
            writes_before_restore,
            "startup recovery validation cannot be bypassed through restore_all"
        );
        restarted.recover_pending().expect("recover");
        assert_eq!(
            restarted.read_limits(&id()).expect("read"),
            limits(1_000, 3_000)
        );
        assert!(store.load().expect("load journal").is_none());
    }

    #[test]
    fn recovery_releases_an_owned_frequency_request_after_effective_limits_change() {
        let io = Arc::new(MemorySysfs::with_pair("1000", "3000"));
        let store = Arc::new(MemoryStore::default());
        actuator(io.clone(), store.clone(), "boot-a", "device-a")
            .apply_batch(&[FrequencyRequest {
                target: id(),
                limits: limits(2_000, 2_000),
            }])
            .expect("apply");
        let writes_before_admin = io.writes().len();

        // Effective sysfs limits cannot distinguish a direct write from a
        // separate kernel QoS constraint. While journaled, this target is
        // therefore exclusive to the actuator so recovery cannot leave a
        // hidden 2000..2000 request behind.
        io.set_admin_pair("1000", "2000");
        let restarted = actuator(io.clone(), store.clone(), "boot-a", "device-a");
        restarted
            .recover_pending()
            .expect("release the owned request");

        assert_eq!(
            restarted.read_limits(&id()).expect("restored full range"),
            limits(1_000, 3_000)
        );
        assert_eq!(io.writes().len(), writes_before_admin + 2);
        assert!(store.load().expect("cleared journal").is_none());
    }

    #[test]
    fn frequency_recovery_accepts_an_exact_crash_intermediate() {
        let io = Arc::new(MemorySysfs::with_pair("1000", "3000"));
        let store = Arc::new(MemoryStore::default());
        actuator(io.clone(), store.clone(), "boot-a", "device-a")
            .apply_batch(&[FrequencyRequest {
                target: id(),
                limits: limits(2_000, 2_000),
            }])
            .expect("seed journal");

        let bytes = store.load().expect("load journal").expect("journal");
        let mut journal = decode_journal(&bytes).expect("decode journal");
        let entry = journal.entries.get_mut(&id()).expect("frequency entry");
        entry.applied = entry.original;
        entry.legal_pairs = transaction_legal_pairs(entry.original, entry.desired);
        store
            .store_durable(&encode_journal(&journal).expect("encode crash journal"))
            .expect("store crash journal");
        // write_ordered lowers max before raising min for this transition.
        io.set_admin_pair("1000", "2000");

        let restarted = actuator(io, store.clone(), "boot-a", "device-a");
        restarted
            .recover_pending()
            .expect("recover exact ordered-write intermediate");
        assert_eq!(
            restarted.read_limits(&id()).expect("restored pair"),
            limits(1_000, 3_000)
        );
        assert!(store.load().expect("cleared journal").is_none());
    }

    #[test]
    fn self_describing_manifest_recovers_without_configuration_registry() {
        let io = Arc::new(MemorySysfs::with_pair("1000", "3000"));
        let store = Arc::new(MemoryStore::default());
        actuator(io.clone(), store.clone(), "boot-a", "device-a")
            .apply_batch(&[FrequencyRequest {
                target: id(),
                limits: limits(2_000, 3_000),
            }])
            .expect("apply");

        let manifest = inspect_recovery_journal(store.as_ref())
            .expect("inspect journal")
            .expect("recovery manifest");
        assert_eq!(manifest.schema_version, JOURNAL_SCHEMA_VERSION);
        assert_eq!(manifest.frequency_targets.len(), 1);
        let recovery_registry = manifest
            .self_describing_registry()
            .expect("manifest registry");
        let restarted =
            FrequencyActuator::new(io, store.clone(), recovery_registry, "boot-a", "device-a");
        assert!(
            restarted
                .startup_recovery_required()
                .expect("startup recovery")
        );
        restarted
            .recover_pending()
            .expect("configuration-independent recovery");

        assert_eq!(
            restarted.read_limits(&id()).expect("restored limits"),
            limits(1_000, 3_000)
        );
        assert!(
            !restarted
                .startup_recovery_required()
                .expect("startup recovery")
        );
        assert!(store.load().expect("cleared journal").is_none());
    }

    #[test]
    fn inconsistent_recovery_manifest_is_rejected_fail_closed() {
        let io = Arc::new(MemorySysfs::with_pair("1000", "3000"));
        let store = Arc::new(MemoryStore::default());
        actuator(io.clone(), store.clone(), "boot-a", "device-a")
            .apply_batch(&[FrequencyRequest {
                target: id(),
                limits: limits(2_000, 3_000),
            }])
            .expect("apply");
        let bytes = store.load().expect("load journal").expect("journal bytes");
        let mut journal = decode_journal(&bytes).expect("decode journal");
        let resource_manifest = &mut journal
            .entries
            .values_mut()
            .next()
            .expect("frequency entry")
            .resource_manifest;
        let super::RecoveryResourceManifest::FrequencyPair(manifest) = resource_manifest else {
            panic!("frequency recovery manifest");
        };
        manifest.min_path = PathBuf::from("/sys/other/min");
        store
            .store_durable(&encode_journal(&journal).expect("encode inconsistent journal"))
            .expect("store inconsistent journal");

        assert!(matches!(
            inspect_recovery_journal(store.as_ref()),
            Err(ActuatorError::InvalidJournal(_))
        ));
        let restarted = actuator(io, store, "boot-a", "device-a");
        assert!(matches!(
            restarted.mode().expect("mode"),
            ActuatorMode::ReadOnlyDegraded { .. }
        ));
        assert!(
            restarted
                .startup_recovery_failed()
                .expect("startup recovery failure")
        );
    }

    #[test]
    fn other_boot_discards_stale_journal_without_writing_hardware() {
        let io = Arc::new(MemorySysfs::with_pair("1000", "3000"));
        let store = Arc::new(MemoryStore::default());
        actuator(io.clone(), store.clone(), "boot-a", "device-a")
            .apply_batch(&[FrequencyRequest {
                target: id(),
                limits: limits(2_000, 3_000),
            }])
            .expect("apply");
        let writes_before_restart = io.writes().len();

        actuator(io.clone(), store.clone(), "boot-b", "device-a")
            .recover_pending()
            .expect("discard stale boot");
        assert_eq!(io.writes().len(), writes_before_restart);
        assert!(store.load().expect("load journal").is_none());
    }

    #[test]
    fn fingerprint_change_blocks_recovery_and_all_mutations() {
        let io = Arc::new(MemorySysfs::with_pair("1000", "3000"));
        let store = Arc::new(MemoryStore::default());
        actuator(io.clone(), store.clone(), "boot-a", "device-a")
            .apply_batch(&[FrequencyRequest {
                target: id(),
                limits: limits(2_000, 3_000),
            }])
            .expect("apply");

        let restarted = actuator(io, store, "boot-a", "device-b");
        assert!(matches!(
            restarted.recover_pending(),
            Err(ActuatorError::Degraded(_))
        ));
        assert!(matches!(
            restarted.mode().expect("mode"),
            ActuatorMode::ReadOnlyDegraded { .. }
        ));
    }

    #[test]
    fn cpufreq_kernel_units_are_scaled_without_rounding() {
        let io = Arc::new(MemorySysfs::with_pair("1000", "3000"));
        let store = Arc::new(MemoryStore::default());
        let scaled = FrequencyTarget::new(
            id(),
            "/sys/test/min",
            "/sys/test/max",
            Hertz::new(1_000_000),
            Hertz::new(3_000_000),
            vec![
                Hertz::new(1_000_000),
                Hertz::new(2_000_000),
                Hertz::new(3_000_000),
            ],
        )
        .expect("target")
        .with_hertz_per_unit(1_000)
        .expect("unit");
        let actuator = FrequencyActuator::new(
            io.clone(),
            store,
            TargetRegistry::new([scaled]).expect("registry"),
            "boot-a",
            "device-a",
        );

        actuator
            .apply_batch(&[FrequencyRequest {
                target: id(),
                limits: limits(2_000_000, 3_000_000),
            }])
            .expect("apply");
        assert_eq!(
            io.writes(),
            vec![(PathBuf::from("/sys/test/min"), "2000".to_owned())]
        );
    }

    #[test]
    fn task_apply_and_restore_are_verified_and_journaled() {
        let store = Arc::new(MemoryStore::default());
        let proc_reader = Arc::new(FakeProc::default());
        let controller = Arc::new(FaultingProcessController::default());
        let identity = process_identity(41, 10);
        let original = scheduling(0);
        let desired = scheduling(-5);
        insert_process(&proc_reader, identity);
        controller.insert(identity.pid, original.clone());
        let actuator = control_actuator(
            store.clone(),
            "boot-a",
            Some(proc_reader),
            Some(controller.clone()),
            None,
        );

        assert_eq!(
            actuator.read_task_state(identity).expect("read task"),
            original
        );
        let outcome = actuator
            .apply_tasks(&[TaskRequest {
                identity,
                desired: desired.clone(),
            }])
            .expect("apply task");
        assert_eq!(outcome.applied[&identity], desired);
        assert_eq!(
            controller
                .read_scheduling(identity.pid)
                .expect("read applied task"),
            desired
        );
        assert!(
            !actuator
                .startup_recovery_required()
                .expect("startup recovery")
        );
        assert!(actuator.has_owned_resources().expect("owned resources"));
        assert!(store.load().expect("load task journal").is_some());

        actuator.restore_tasks(&[identity]).expect("restore task");
        assert_eq!(
            controller
                .read_scheduling(identity.pid)
                .expect("read restored task"),
            original
        );
        assert!(
            !actuator
                .startup_recovery_required()
                .expect("pending journal")
        );
        assert!(!actuator.has_owned_resources().expect("owned resources"));
        assert!(store.load().expect("load cleared journal").is_none());
    }

    #[test]
    fn fifo_priority_is_journaled_and_restored_after_restart() {
        let store = Arc::new(MemoryStore::default());
        let proc_reader = Arc::new(FakeProc::default());
        let controller = Arc::new(FaultingProcessController::default());
        let identity = process_identity(41, 10);
        let original = scheduling(0);
        let desired = fifo_scheduling(-5, 20);
        insert_process(&proc_reader, identity);
        controller.insert(identity.pid, original.clone());
        control_actuator(
            store.clone(),
            "boot-a",
            Some(proc_reader.clone()),
            Some(controller.clone()),
            None,
        )
        .apply_tasks(&[TaskRequest {
            identity,
            desired: desired.clone(),
        }])
        .expect("apply FIFO task");

        assert_eq!(
            controller
                .read_scheduling(identity.pid)
                .expect("read FIFO state"),
            desired
        );
        let bytes = store.load().expect("load journal").expect("task journal");
        let journal = decode_journal(&bytes).expect("decode task journal");
        assert_eq!(journal.schema_version, JOURNAL_SCHEMA_VERSION);
        assert_eq!(
            journal.tasks[&super::task_journal_key(identity)]
                .applied
                .rt_priority,
            Some(20)
        );

        control_actuator(
            store.clone(),
            "boot-a",
            Some(proc_reader),
            Some(controller.clone()),
            None,
        )
        .recover_pending()
        .expect("recover FIFO task after restart");
        assert_eq!(
            controller
                .read_scheduling(identity.pid)
                .expect("read restored task"),
            original
        );
        assert!(store.load().expect("cleared task journal").is_none());
    }

    #[test]
    fn unchanged_task_and_unit_requests_claim_no_ownership() {
        let task_store = Arc::new(MemoryStore::default());
        let proc_reader = Arc::new(FakeProc::default());
        let controller = Arc::new(FaultingProcessController::default());
        let identity = process_identity(41, 10);
        let scheduling = scheduling(0);
        insert_process(&proc_reader, identity);
        controller.insert(identity.pid, scheduling.clone());
        let task_actuator = control_actuator(
            task_store.clone(),
            "boot-a",
            Some(proc_reader),
            Some(controller.clone()),
            None,
        );
        task_actuator
            .apply_tasks(&[TaskRequest {
                identity,
                desired: scheduling,
            }])
            .expect("unchanged task");
        assert!(controller.writes().is_empty());
        assert!(!task_actuator.has_owned_resources().expect("task ownership"));
        assert!(task_store.load().expect("task journal").is_none());

        let unit_store = Arc::new(MemoryStore::default());
        let systemd = Arc::new(FaultingSystemd::default());
        let unit = "app-game.scope";
        let properties = unit_properties(100);
        systemd.insert(unit, vec![identity.pid], properties.clone());
        let unit_actuator = control_actuator(
            unit_store.clone(),
            "boot-a",
            None,
            None,
            Some(systemd.clone()),
        );
        unit_actuator
            .apply_units(&[UnitRequest {
                unit: unit.to_owned(),
                desired: properties,
            }])
            .expect("unchanged unit");
        assert!(systemd.writes().is_empty());
        assert!(!unit_actuator.has_owned_resources().expect("unit ownership"));
        assert!(unit_store.load().expect("unit journal").is_none());
    }

    #[test]
    fn task_ownership_is_per_field_and_relinquishes_administrator_changes() {
        let store = Arc::new(MemoryStore::default());
        let proc_reader = Arc::new(FakeProc::default());
        let controller = Arc::new(FaultingProcessController::default());
        let identity = process_identity(41, 10);
        let original = scheduling(0);
        let mut first_desired = original.clone();
        first_desired.affinity = CpuSet::from_ids([CpuId::new(1)]);
        first_desired.nice = -5;
        insert_process(&proc_reader, identity);
        controller.insert(identity.pid, original.clone());
        let actuator = control_actuator(
            store.clone(),
            "boot-a",
            Some(proc_reader),
            Some(controller.clone()),
            None,
        );
        actuator
            .apply_tasks(&[TaskRequest {
                identity,
                desired: first_desired.clone(),
            }])
            .expect("claim affinity and nice");

        let mut administrator = first_desired;
        administrator.nice = 7;
        administrator.policy = SchedulingClass::Batch;
        controller.set_admin(identity.pid, administrator.clone());

        let mut next_desired = administrator.clone();
        next_desired.affinity = CpuSet::from_ids([CpuId::new(2)]);
        next_desired.nice = -10;
        let outcome = actuator
            .apply_tasks(&[TaskRequest {
                identity,
                desired: next_desired,
            }])
            .expect("update only fields that remain owned");
        let applied = &outcome.applied[&identity];
        assert_eq!(applied.affinity, CpuSet::from_ids([CpuId::new(2)]));
        assert_eq!(applied.nice, 7);
        assert_eq!(applied.policy, SchedulingClass::Batch);

        actuator
            .restore_tasks(&[identity])
            .expect("restore remaining owned task fields");
        let restored = controller
            .read_scheduling(identity.pid)
            .expect("restored task");
        assert_eq!(restored.affinity, original.affinity);
        assert_eq!(restored.nice, 7);
        assert_eq!(restored.policy, SchedulingClass::Batch);
        assert!(store.load().expect("cleared journal").is_none());
    }

    #[test]
    fn fifo_policy_and_priority_relinquish_as_one_owned_field() {
        let store = Arc::new(MemoryStore::default());
        let proc_reader = Arc::new(FakeProc::default());
        let controller = Arc::new(FaultingProcessController::default());
        let identity = process_identity(41, 10);
        insert_process(&proc_reader, identity);
        controller.insert(identity.pid, scheduling(0));
        let actuator = control_actuator(
            store.clone(),
            "boot-a",
            Some(proc_reader),
            Some(controller.clone()),
            None,
        );
        actuator
            .apply_tasks(&[TaskRequest {
                identity,
                desired: fifo_scheduling(0, 10),
            }])
            .expect("claim FIFO tuple");

        let administrator = fifo_scheduling(0, 15);
        controller.set_admin(identity.pid, administrator.clone());
        let outcome = actuator
            .apply_tasks(&[TaskRequest {
                identity,
                desired: fifo_scheduling(0, 20),
            }])
            .expect("relinquish externally changed FIFO tuple");
        assert_eq!(outcome.applied[&identity], administrator);

        actuator
            .restore_tasks(&[identity])
            .expect("clear relinquished FIFO tuple");
        assert_eq!(
            controller
                .read_scheduling(identity.pid)
                .expect("administrator FIFO tuple"),
            administrator
        );
        assert!(store.load().expect("cleared journal").is_none());
    }

    #[test]
    fn task_batch_failure_rolls_back_only_attempted_tasks() {
        let store = Arc::new(MemoryStore::default());
        let proc_reader = Arc::new(FakeProc::default());
        let controller = Arc::new(FaultingProcessController::default());
        let first = process_identity(41, 10);
        let second = process_identity(42, 20);
        let first_original = scheduling(0);
        let second_original = scheduling(1);
        insert_process(&proc_reader, first);
        insert_process(&proc_reader, second);
        controller.insert(first.pid, first_original.clone());
        controller.insert(second.pid, second_original.clone());
        controller.fail_on(2);
        let actuator = control_actuator(
            store,
            "boot-a",
            Some(proc_reader),
            Some(controller.clone()),
            None,
        );

        let error = actuator
            .apply_tasks(&[
                TaskRequest {
                    identity: first,
                    desired: scheduling(-5),
                },
                TaskRequest {
                    identity: second,
                    desired: scheduling(-4),
                },
            ])
            .expect_err("second task write must fail");
        assert!(matches!(error, ActuatorError::Transaction { .. }));
        assert_eq!(
            controller
                .read_scheduling(first.pid)
                .expect("first rollback"),
            first_original
        );
        assert_eq!(
            controller
                .read_scheduling(second.pid)
                .expect("second rollback"),
            second_original
        );
        assert!(matches!(
            actuator.mode().expect("mode"),
            ActuatorMode::ReadWrite
        ));
    }

    #[test]
    fn fifo_batch_failure_rolls_back_exact_original_priority() {
        let store = Arc::new(MemoryStore::default());
        let proc_reader = Arc::new(FakeProc::default());
        let controller = Arc::new(FaultingProcessController::default());
        let first = process_identity(41, 10);
        let second = process_identity(42, 20);
        // Existing external priorities above our configurable hard cap still
        // have to be read and restored exactly; they are never generated by a
        // uperf task plan.
        let first_original = fifo_scheduling(0, 98);
        let second_original = scheduling(1);
        insert_process(&proc_reader, first);
        insert_process(&proc_reader, second);
        controller.insert(first.pid, first_original.clone());
        controller.insert(second.pid, second_original.clone());
        controller.fail_on(2);
        let actuator = control_actuator(
            store,
            "boot-a",
            Some(proc_reader),
            Some(controller.clone()),
            None,
        );

        let error = actuator
            .apply_tasks(&[
                TaskRequest {
                    identity: first,
                    desired: fifo_scheduling(-5, 20),
                },
                TaskRequest {
                    identity: second,
                    desired: fifo_scheduling(-4, 10),
                },
            ])
            .expect_err("second task write must fail");

        assert!(matches!(error, ActuatorError::Transaction { .. }));
        assert_eq!(
            controller
                .read_scheduling(first.pid)
                .expect("first exact FIFO rollback"),
            first_original
        );
        assert_eq!(
            controller
                .read_scheduling(second.pid)
                .expect("second exact rollback"),
            second_original
        );
    }

    #[test]
    fn task_fault_rollback_preserves_an_administrator_unowned_field() {
        let store = Arc::new(MemoryStore::default());
        let proc_reader = Arc::new(FakeProc::default());
        let controller = Arc::new(FaultingProcessController::default());
        let first = process_identity(41, 10);
        let second = process_identity(42, 20);
        let first_original = scheduling(0);
        let first_desired = scheduling(-5);
        let second_original = scheduling(1);
        insert_process(&proc_reader, first);
        insert_process(&proc_reader, second);
        controller.insert(first.pid, first_original.clone());
        controller.insert(second.pid, second_original.clone());
        controller.fail_on(2);
        let mut administrator = first_desired.clone();
        administrator.policy = SchedulingClass::Batch;
        controller.set_admin_on_failure(first.pid, administrator);
        let actuator = control_actuator(
            store,
            "boot-a",
            Some(proc_reader),
            Some(controller.clone()),
            None,
        );

        assert!(matches!(
            actuator.apply_tasks(&[
                TaskRequest {
                    identity: first,
                    desired: first_desired,
                },
                TaskRequest {
                    identity: second,
                    desired: scheduling(-4),
                },
            ]),
            Err(ActuatorError::Transaction { .. })
        ));
        let first_after_rollback = controller
            .read_scheduling(first.pid)
            .expect("first rollback");
        assert_eq!(first_after_rollback.nice, first_original.nice);
        assert_eq!(first_after_rollback.policy, SchedulingClass::Batch);
        assert_eq!(
            controller
                .read_scheduling(second.pid)
                .expect("second remains original"),
            second_original
        );
    }

    #[test]
    fn task_rollback_treats_an_exited_tid_as_already_restored() {
        let store = Arc::new(MemoryStore::default());
        let proc_reader = Arc::new(FakeProc::default());
        let controller = Arc::new(FaultingProcessController::default());
        let exited = process_identity(4_101, 10);
        let failing = process_identity(4_102, 20);
        let exited_original = scheduling(0);
        let exited_desired = scheduling(-5);
        let failing_original = scheduling(1);
        insert_process(&proc_reader, exited);
        insert_process(&proc_reader, failing);
        controller.insert(exited.pid, exited_original.clone());
        controller.insert(failing.pid, failing_original.clone());
        controller.fail_on(2);
        controller.exit_on_failure(proc_reader.clone(), exited.pid);
        let actuator = control_actuator(
            store.clone(),
            "boot-a",
            Some(proc_reader),
            Some(controller.clone()),
            None,
        );

        let error = actuator
            .apply_tasks(&[
                TaskRequest {
                    identity: exited,
                    desired: exited_desired.clone(),
                },
                TaskRequest {
                    identity: failing,
                    desired: scheduling(-4),
                },
            ])
            .expect_err("second write must fail after the first TID exits");

        assert!(matches!(error, ActuatorError::Transaction { .. }));
        assert!(matches!(
            actuator.mode().expect("mode"),
            ActuatorMode::ReadWrite
        ));
        assert_eq!(
            controller
                .writes()
                .into_iter()
                .filter(|(pid, _)| *pid == exited.pid)
                .collect::<Vec<_>>(),
            vec![(exited.pid, exited_desired)]
        );
        assert_eq!(
            controller
                .read_scheduling(failing.pid)
                .expect("failing TID rollback"),
            failing_original
        );

        actuator
            .restore_tasks(&[exited, failing])
            .expect("clear journal for exited TID");
        assert!(store.load().expect("cleared task journal").is_none());
    }

    #[test]
    fn task_rollback_tolerates_exit_between_identity_check_and_scheduler_read() {
        let store = Arc::new(MemoryStore::default());
        let proc_reader = Arc::new(FakeProc::default());
        let controller = Arc::new(FaultingProcessController::default());
        let exited = process_identity(4_101, 10);
        let failing = process_identity(4_102, 20);
        let exited_original = scheduling(0);
        let exited_desired = scheduling(-5);
        let failing_original = scheduling(1);
        insert_process(&proc_reader, exited);
        insert_process(&proc_reader, failing);
        controller.insert(exited.pid, exited_original);
        controller.insert(failing.pid, failing_original.clone());
        controller.fail_on(2);
        controller.exit_before_rollback_read_on_failure(proc_reader.clone(), exited.pid);
        let actuator = control_actuator(
            store.clone(),
            "boot-a",
            Some(proc_reader),
            Some(controller.clone()),
            None,
        );

        let error = actuator
            .apply_tasks(&[
                TaskRequest {
                    identity: exited,
                    desired: exited_desired.clone(),
                },
                TaskRequest {
                    identity: failing,
                    desired: scheduling(-4),
                },
            ])
            .expect_err("second write must fail while the first TID exits during rollback");

        assert!(matches!(error, ActuatorError::Transaction { .. }));
        assert!(matches!(
            actuator.mode().expect("mode"),
            ActuatorMode::ReadWrite
        ));
        assert_eq!(
            controller
                .writes()
                .into_iter()
                .filter(|(pid, _)| *pid == exited.pid)
                .collect::<Vec<_>>(),
            vec![(exited.pid, exited_desired)]
        );
        assert_eq!(
            controller
                .read_scheduling(failing.pid)
                .expect("failing TID remains original"),
            failing_original
        );
        assert!(store.load().expect("cleared task journal").is_none());
    }

    #[test]
    fn task_rollback_never_writes_to_a_reused_tid() {
        let store = Arc::new(MemoryStore::default());
        let proc_reader = Arc::new(FakeProc::default());
        let controller = Arc::new(FaultingProcessController::default());
        let old = process_identity(4_101, 10);
        let reused = process_identity(4_101, 999);
        let failing = process_identity(4_102, 20);
        let old_desired = scheduling(-5);
        insert_process(&proc_reader, old);
        insert_process(&proc_reader, failing);
        controller.insert(old.pid, scheduling(0));
        controller.insert(failing.pid, scheduling(1));
        controller.fail_on(2);
        controller.reuse_on_failure(proc_reader.clone(), process_info(reused));
        let actuator = control_actuator(
            store.clone(),
            "boot-a",
            Some(proc_reader),
            Some(controller.clone()),
            None,
        );

        let error = actuator
            .apply_tasks(&[
                TaskRequest {
                    identity: old,
                    desired: old_desired.clone(),
                },
                TaskRequest {
                    identity: failing,
                    desired: scheduling(-4),
                },
            ])
            .expect_err("second write must fail after TID reuse");

        assert!(matches!(error, ActuatorError::Transaction { .. }));
        assert!(matches!(
            actuator.mode().expect("mode"),
            ActuatorMode::ReadWrite
        ));
        assert_eq!(
            controller
                .writes()
                .into_iter()
                .filter(|(pid, _)| *pid == old.pid)
                .collect::<Vec<_>>(),
            vec![(old.pid, old_desired)]
        );
        assert!(matches!(
            actuator.read_task_state(old),
            Err(ActuatorError::ProcessIdentityChanged(_))
        ));

        actuator
            .restore_tasks(&[old, failing])
            .expect("clear journal without touching reused TID");
        assert!(store.load().expect("cleared task journal").is_none());
    }

    #[test]
    fn post_task_journal_failure_rolls_back_and_degrades() {
        let store = Arc::new(MemoryStore::default());
        store.fail_on_store(2);
        let proc_reader = Arc::new(FakeProc::default());
        let controller = Arc::new(FaultingProcessController::default());
        let identity = process_identity(41, 10);
        let original = scheduling(0);
        insert_process(&proc_reader, identity);
        controller.insert(identity.pid, original.clone());
        let actuator = control_actuator(
            store.clone(),
            "boot-a",
            Some(proc_reader),
            Some(controller.clone()),
            None,
        );

        assert!(matches!(
            actuator.apply_tasks(&[TaskRequest {
                identity,
                desired: scheduling(-5),
            }]),
            Err(ActuatorError::Degraded(_))
        ));
        assert_eq!(
            controller
                .read_scheduling(identity.pid)
                .expect("task rollback"),
            original
        );
        assert!(matches!(
            actuator.mode().expect("mode"),
            ActuatorMode::ReadOnlyDegraded { .. }
        ));
        assert!(store.load().expect("durable pre-journal").is_some());
    }

    #[test]
    fn systemd_batch_failure_rolls_back_attempted_units() {
        let store = Arc::new(MemoryStore::default());
        let systemd = Arc::new(FaultingSystemd::default());
        let first = "app-first.scope";
        let second = "app-second.scope";
        let first_original = unit_properties(100);
        let second_original = unit_properties(200);
        systemd.insert(first, vec![ProcessId::new(41)], first_original.clone());
        systemd.insert(second, vec![ProcessId::new(42)], second_original.clone());
        systemd.fail_on(2);
        let actuator = control_actuator(store, "boot-a", None, None, Some(systemd.clone()));

        let error = actuator
            .apply_units(&[
                UnitRequest {
                    unit: first.to_owned(),
                    desired: unit_properties(800),
                },
                UnitRequest {
                    unit: second.to_owned(),
                    desired: unit_properties(900),
                },
            ])
            .expect_err("second unit write must fail");
        assert!(matches!(error, ActuatorError::Transaction { .. }));
        assert_eq!(
            systemd
                .read_unit_properties(first)
                .expect("first unit rollback"),
            first_original
        );
        assert_eq!(
            systemd
                .read_unit_properties(second)
                .expect("second unit rollback"),
            second_original
        );
        assert!(matches!(
            actuator.mode().expect("mode"),
            ActuatorMode::ReadWrite
        ));
    }

    #[test]
    fn pid_reuse_is_never_restored_into_the_new_process() {
        let store = Arc::new(MemoryStore::default());
        let proc_reader = Arc::new(FakeProc::default());
        let controller = Arc::new(FaultingProcessController::default());
        let old_identity = process_identity(41, 10);
        insert_process(&proc_reader, old_identity);
        controller.insert(old_identity.pid, scheduling(0));
        control_actuator(
            store.clone(),
            "boot-a",
            Some(proc_reader.clone()),
            Some(controller.clone()),
            None,
        )
        .apply_tasks(&[TaskRequest {
            identity: old_identity,
            desired: scheduling(-5),
        }])
        .expect("apply old process");
        let writes_before_recovery = controller.writes().len();

        let new_identity = process_identity(41, 999);
        insert_process(&proc_reader, new_identity);
        let restarted = control_actuator(
            store.clone(),
            "boot-a",
            Some(proc_reader),
            Some(controller.clone()),
            None,
        );
        restarted.recover_pending().expect("skip reused PID");

        assert_eq!(controller.writes().len(), writes_before_recovery);
        assert!(store.load().expect("cleared reused PID journal").is_none());
        assert!(matches!(
            restarted.read_task_state(old_identity),
            Err(ActuatorError::ProcessIdentityChanged(_))
        ));
    }

    #[test]
    fn exited_tid_is_a_successful_noop_during_recovery() {
        let store = Arc::new(MemoryStore::default());
        let proc_reader = Arc::new(FakeProc::default());
        let controller = Arc::new(FaultingProcessController::default());
        let identity = process_identity(4_101, 10);
        insert_process(&proc_reader, identity);
        controller.insert(identity.pid, scheduling(0));
        control_actuator(
            store.clone(),
            "boot-a",
            Some(proc_reader.clone()),
            Some(controller.clone()),
            None,
        )
        .apply_tasks(&[TaskRequest {
            identity,
            desired: scheduling(-5),
        }])
        .expect("apply task before exit");
        let writes_before_recovery = controller.writes().len();
        proc_reader.remove_process(identity.pid);

        let restarted = control_actuator(
            store.clone(),
            "boot-a",
            Some(proc_reader),
            Some(controller.clone()),
            None,
        );
        restarted
            .recover_pending()
            .expect("exited TID needs no restoration");

        assert_eq!(controller.writes().len(), writes_before_recovery);
        assert!(matches!(
            restarted.mode().expect("mode"),
            ActuatorMode::ReadWrite
        ));
        assert!(
            !restarted
                .startup_recovery_required()
                .expect("pending journal")
        );
        assert!(store.load().expect("cleared recovery journal").is_none());
    }

    #[test]
    fn administrator_unit_change_is_not_overwritten_on_restore() {
        let store = Arc::new(MemoryStore::default());
        let systemd = Arc::new(FaultingSystemd::default());
        let unit = "app-game.scope";
        systemd.insert(unit, vec![ProcessId::new(41)], unit_properties(100));
        let actuator = control_actuator(store.clone(), "boot-a", None, None, Some(systemd.clone()));
        actuator
            .apply_units(&[UnitRequest {
                unit: unit.to_owned(),
                desired: unit_properties(900),
            }])
            .expect("apply unit");
        let administrator_value = unit_properties(777);
        systemd.set_admin(unit, administrator_value.clone());
        let writes_before_restore = systemd.writes().len();

        actuator
            .restore_units(&[unit.to_owned()])
            .expect("preserve administrator value");

        assert_eq!(systemd.writes().len(), writes_before_restore);
        assert_eq!(
            systemd
                .read_unit_properties(unit)
                .expect("administrator value"),
            administrator_value
        );
        assert!(store.load().expect("cleared unit journal").is_none());
    }

    #[test]
    fn systemd_ownership_is_per_field_and_relinquishes_administrator_changes() {
        let store = Arc::new(MemoryStore::default());
        let systemd = Arc::new(FaultingSystemd::default());
        let unit = "app-game.scope";
        let original = unit_properties(100);
        let mut first_desired = unit_properties(900);
        first_desired.allowed_cpus = Some(CpuSet::from_ids([CpuId::new(2)]));
        systemd.insert(unit, vec![ProcessId::new(41)], original.clone());
        let actuator = control_actuator(store.clone(), "boot-a", None, None, Some(systemd.clone()));
        actuator
            .apply_units(&[UnitRequest {
                unit: unit.to_owned(),
                desired: first_desired.clone(),
            }])
            .expect("claim both properties");

        let mut administrator = first_desired;
        administrator.cpu_weight = Some(777);
        systemd.set_admin(unit, administrator.clone());
        let mut next_desired = administrator;
        next_desired.cpu_weight = Some(800);
        next_desired.allowed_cpus = Some(CpuSet::from_ids([CpuId::new(3)]));
        let outcome = actuator
            .apply_units(&[UnitRequest {
                unit: unit.to_owned(),
                desired: next_desired,
            }])
            .expect("update only the property that remains owned");
        let applied = &outcome.applied[unit];
        assert_eq!(applied.cpu_weight, Some(777));
        assert_eq!(
            applied.allowed_cpus,
            Some(CpuSet::from_ids([CpuId::new(3)]))
        );

        actuator
            .restore_units(&[unit.to_owned()])
            .expect("restore remaining owned property");
        let restored = systemd
            .read_unit_properties(unit)
            .expect("restored properties");
        assert_eq!(restored.cpu_weight, Some(777));
        assert_eq!(restored.allowed_cpus, original.allowed_cpus);
        assert!(store.load().expect("cleared journal").is_none());
    }

    #[test]
    fn same_name_new_systemd_instance_is_never_recovered() {
        let store = Arc::new(MemoryStore::default());
        let systemd = Arc::new(FaultingSystemd::default());
        let unit = "app-game.scope";
        let pid = ProcessId::new(41);
        systemd.insert(unit, vec![pid], unit_properties(100));
        control_actuator(store.clone(), "boot-a", None, None, Some(systemd.clone()))
            .apply_units(&[UnitRequest {
                unit: unit.to_owned(),
                desired: unit_properties(900),
            }])
            .expect("apply old instance");
        let writes_before_restart = systemd.writes().len();

        let replacement = unit_properties(555);
        systemd.insert(unit, vec![pid], replacement.clone());
        let restarted =
            control_actuator(store.clone(), "boot-a", None, None, Some(systemd.clone()));
        restarted
            .recover_pending()
            .expect("skip same-name replacement instance");

        assert_eq!(
            systemd
                .read_unit_properties(unit)
                .expect("replacement properties"),
            replacement
        );
        assert_eq!(systemd.writes().len(), writes_before_restart);
        assert!(
            store
                .load()
                .expect("cleared old-instance journal")
                .is_none()
        );
    }

    #[test]
    fn missing_recovery_backends_force_read_only_degraded_mode() {
        let task_store = Arc::new(MemoryStore::default());
        let proc_reader = Arc::new(FakeProc::default());
        let controller = Arc::new(FaultingProcessController::default());
        let identity = process_identity(41, 10);
        insert_process(&proc_reader, identity);
        controller.insert(identity.pid, scheduling(0));
        control_actuator(
            task_store.clone(),
            "boot-a",
            Some(proc_reader),
            Some(controller),
            None,
        )
        .apply_tasks(&[TaskRequest {
            identity,
            desired: scheduling(-5),
        }])
        .expect("apply task");
        let task_restart = control_actuator(task_store, "boot-a", None, None, None);
        assert!(matches!(
            task_restart.recover_pending(),
            Err(ActuatorError::Degraded(_))
        ));
        assert!(matches!(
            task_restart.mode().expect("task mode"),
            ActuatorMode::ReadOnlyDegraded { .. }
        ));

        let unit_store = Arc::new(MemoryStore::default());
        let systemd = Arc::new(FaultingSystemd::default());
        let unit = "app-game.scope";
        systemd.insert(unit, vec![identity.pid], unit_properties(100));
        control_actuator(unit_store.clone(), "boot-a", None, None, Some(systemd))
            .apply_units(&[UnitRequest {
                unit: unit.to_owned(),
                desired: unit_properties(900),
            }])
            .expect("apply unit");
        let unit_restart = control_actuator(unit_store, "boot-a", None, None, None);
        assert!(matches!(
            unit_restart.recover_pending(),
            Err(ActuatorError::Degraded(_))
        ));
        assert!(matches!(
            unit_restart.mode().expect("unit mode"),
            ActuatorMode::ReadOnlyDegraded { .. }
        ));
    }

    #[test]
    fn restore_backend_failures_and_remove_failures_degrade() {
        let task_store = Arc::new(MemoryStore::default());
        let proc_reader = Arc::new(FakeProc::default());
        let controller = Arc::new(FaultingProcessController::default());
        let identity = process_identity(41, 10);
        insert_process(&proc_reader, identity);
        controller.insert(identity.pid, scheduling(0));
        let task_actuator = control_actuator(
            task_store,
            "boot-a",
            Some(proc_reader),
            Some(controller.clone()),
            None,
        );
        task_actuator
            .apply_tasks(&[TaskRequest {
                identity,
                desired: scheduling(-5),
            }])
            .expect("apply task");
        controller.fail_on(2);
        assert!(matches!(
            task_actuator.restore_tasks(&[identity]),
            Err(ActuatorError::Degraded(_))
        ));

        let unit_store = Arc::new(MemoryStore::default());
        let systemd = Arc::new(FaultingSystemd::default());
        let unit = "app-game.scope";
        systemd.insert(unit, vec![identity.pid], unit_properties(100));
        let unit_actuator =
            control_actuator(unit_store, "boot-a", None, None, Some(systemd.clone()));
        unit_actuator
            .apply_units(&[UnitRequest {
                unit: unit.to_owned(),
                desired: unit_properties(900),
            }])
            .expect("apply unit");
        systemd.remove(unit);
        unit_actuator
            .restore_units(&[unit.to_owned()])
            .expect("a disappeared unit instance needs no restoration");

        let remove_store = Arc::new(MemoryStore::default());
        let remove_systemd = Arc::new(FaultingSystemd::default());
        remove_systemd.insert(unit, vec![identity.pid], unit_properties(100));
        let remove_actuator = control_actuator(
            remove_store.clone(),
            "boot-a",
            None,
            None,
            Some(remove_systemd),
        );
        remove_actuator
            .apply_units(&[UnitRequest {
                unit: unit.to_owned(),
                desired: unit_properties(900),
            }])
            .expect("apply unit before remove fault");
        remove_store.fail_remove();
        assert!(matches!(
            remove_actuator.restore_units(&[unit.to_owned()]),
            Err(ActuatorError::Degraded(_))
        ));
    }

    #[test]
    fn systemd_read_helpers_validate_process_identity_and_unit_name() {
        let proc_reader = Arc::new(FakeProc::default());
        let controller = Arc::new(FaultingProcessController::default());
        let systemd = Arc::new(FaultingSystemd::default());
        let identity = process_identity(41, 10);
        let unit = "app-game.scope";
        insert_process(&proc_reader, identity);
        controller.insert(identity.pid, scheduling(0));
        systemd.insert(unit, vec![identity.pid], unit_properties(100));
        let actuator = control_actuator(
            Arc::new(MemoryStore::default()),
            "boot-a",
            Some(proc_reader.clone()),
            Some(controller),
            Some(systemd),
        );

        assert_eq!(
            actuator
                .unit_for_process(identity)
                .expect("unit lookup")
                .as_deref(),
            Some(unit)
        );
        assert_eq!(
            actuator.unit_processes(unit).expect("unit processes"),
            vec![identity.pid]
        );
        assert!(matches!(
            actuator.unit_processes("../unsafe.scope"),
            Err(ActuatorError::InvalidTarget(_))
        ));

        insert_process(&proc_reader, process_identity(41, 999));
        assert!(matches!(
            actuator.unit_for_process(identity),
            Err(ActuatorError::ProcessIdentityChanged(_))
        ));
    }
}

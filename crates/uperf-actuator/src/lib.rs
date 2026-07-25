//! Transactional mutations, durable journaling and crash recovery.
//!
//! This crate is the only layer allowed to turn a desired frequency range into
//! machine mutations.  It deliberately accepts discovered logical targets
//! instead of arbitrary paths from API clients.
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
use uperf_core::{FrequencyLimits, Hertz, ProcessId, ProcessIdentity, TargetId};
use uperf_platform::{
    PlatformError, ProcReader, ProcessController, ProcessSchedulingState, StateStore, SysfsIo,
    SystemdClient, SystemdUnitInstanceIdentity, SystemdUnitInstanceKey, SystemdUnitProperties,
};

// v1 stores only frequency paths; v2 adds self-describing frequency targets.
// Neither version records per-field ownership or a stable systemd activation
// identity. During same-boot recovery, frequency/task entries are upgraded
// conservatively after live target resolution. A v1/v2 unit entry is
// deliberately fail-closed because a reused unit name cannot be disambiguated.
const LEGACY_JOURNAL_SCHEMA_VERSION: u32 = 1;
const MANIFEST_JOURNAL_SCHEMA_VERSION: u32 = 2;
// v3 adds exact legal frequency pairs, per-field task/unit ownership, and
// stable systemd unit instance identities.
const OWNERSHIP_JOURNAL_SCHEMA_VERSION: u32 = 3;
// v4 defines a frequency entry's original request as the full hardware range,
// so restoring it releases the actuator's userspace QoS request.
const JOURNAL_SCHEMA_VERSION: u32 = 4;
const MAX_JOURNAL_PAYLOAD_BYTES: usize = 1024 * 1024;
const MAX_JOURNAL_ENVELOPE_BYTES: usize = 5 * 1024 * 1024;
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

/// Immutable allowlist of discovered mutation targets.
#[derive(Clone, Debug, Default)]
pub struct TargetRegistry {
    targets: BTreeMap<TargetId, FrequencyTarget>,
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
}

/// One desired range in an atomic batch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrequencyRequest {
    pub target: TargetId,
    pub limits: FrequencyLimits,
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

/// Minimal identity retained by schema-v1 journals.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LegacyRecoveryFrequencyTarget {
    pub id: TargetId,
    pub min_path: PathBuf,
    pub max_path: PathBuf,
}

/// One frequency resource discovered while inspecting a durable journal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecoveryFrequencyTarget {
    SelfDescribing(RecoveryFrequencyTargetManifest),
    Legacy(LegacyRecoveryFrequencyTarget),
}

impl RecoveryFrequencyTarget {
    #[must_use]
    pub fn id(&self) -> &TargetId {
        match self {
            Self::SelfDescribing(target) => &target.id,
            Self::Legacy(target) => &target.id,
        }
    }

    #[must_use]
    pub fn min_path(&self) -> &Path {
        match self {
            Self::SelfDescribing(target) => &target.min_path,
            Self::Legacy(target) => &target.min_path,
        }
    }

    #[must_use]
    pub fn max_path(&self) -> &Path {
        match self {
            Self::SelfDescribing(target) => &target.max_path,
            Self::Legacy(target) => &target.max_path,
        }
    }
}

/// Read-only recovery metadata obtained without loading device configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryManifest {
    pub schema_version: u32,
    pub boot_id: String,
    pub device_fingerprint: String,
    pub frequency_targets: Vec<RecoveryFrequencyTarget>,
    pub has_tasks: bool,
    pub has_systemd_units: bool,
}

impl RecoveryManifest {
    /// Build a registry when every journal resource is self-describing.
    ///
    /// Schema-v1 resources must instead be resolved against read-only hardware
    /// discovery by matching their exact paths.
    ///
    /// # Errors
    ///
    /// Returns an error for a legacy resource or an invalid/duplicate target.
    pub fn self_describing_registry(&self) -> Result<TargetRegistry, ActuatorError> {
        self.frequency_targets
            .iter()
            .map(|target| match target {
                RecoveryFrequencyTarget::SelfDescribing(target) => target.to_frequency_target(),
                RecoveryFrequencyTarget::Legacy(target) => {
                    Err(ActuatorError::LegacyRecoveryTarget(target.id.to_string()))
                }
            })
            .collect::<Result<Vec<_>, _>>()
            .and_then(TargetRegistry::new)
    }

    /// Exact logical sysfs paths that recovery may need to mutate.
    #[must_use]
    pub fn frequency_write_paths(&self) -> Vec<PathBuf> {
        self.frequency_targets
            .iter()
            .flat_map(|target| {
                [
                    target.min_path().to_path_buf(),
                    target.max_path().to_path_buf(),
                ]
            })
            .collect()
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
        .map(|entry| match &entry.manifest {
            Some(manifest) => RecoveryFrequencyTarget::SelfDescribing(manifest.clone()),
            None => RecoveryFrequencyTarget::Legacy(LegacyRecoveryFrequencyTarget {
                id: entry.target.clone(),
                min_path: entry.min_path.clone(),
                max_path: entry.max_path.clone(),
            }),
        })
        .collect();
    Ok(Some(RecoveryManifest {
        schema_version: journal.schema_version,
        boot_id: journal.boot_id,
        device_fingerprint: journal.device_fingerprint,
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
    #[error("duplicate request for target ID: {0}")]
    DuplicateRequest(String),
    #[error("invalid frequency range for {target}: {minimum}..{maximum} Hz")]
    InvalidLimits {
        target: String,
        minimum: u64,
        maximum: u64,
    },
    #[error("invalid numeric value read from {path}: {value:?}")]
    InvalidReadback { path: PathBuf, value: String },
    #[error("platform operation failed: {0}")]
    Platform(#[from] PlatformError),
    #[error("journal is invalid: {0}")]
    InvalidJournal(String),
    #[error("actuator is read-only degraded: {0}")]
    Degraded(String),
    #[error("a durable journal must be recovered before accepting mutations")]
    RecoveryRequired,
    #[error("schema-v1 recovery target requires live discovery: {0}")]
    LegacyRecoveryTarget(String),
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    manifest: Option<RecoveryFrequencyTargetManifest>,
    original: FrequencyLimits,
    desired: FrequencyLimits,
    applied: FrequencyLimits,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    legal_pairs: Option<Vec<FrequencyLimits>>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "explicit named journal fields are safer and more migration-friendly than bit positions"
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
            policy: before.policy != after.policy,
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
                && (current.policy == first.policy || current.policy == second.policy),
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
            && (!self.policy || left.policy == right.policy)
            && (!self.uclamp_min || left.uclamp_min == right.uclamp_min)
            && (!self.uclamp_max || left.uclamp_max == right.uclamp_max)
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owned_fields: Option<TaskFieldMask>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    relinquished_fields: Option<TaskFieldMask>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UnitJournalEntry {
    unit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    instance: Option<SystemdUnitInstanceIdentity>,
    original: SystemdUnitProperties,
    desired: SystemdUnitProperties,
    applied: SystemdUnitProperties,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owned_fields: Option<UnitFieldMask>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    relinquished_fields: Option<UnitFieldMask>,
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
    #[serde(default)]
    entries: BTreeMap<TargetId, JournalEntry>,
    #[serde(default)]
    tasks: BTreeMap<String, TaskJournalEntry>,
    #[serde(default)]
    units: BTreeMap<String, UnitJournalEntry>,
}

impl Journal {
    fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.tasks.is_empty() && self.units.is_empty()
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

/// Transactional frequency actuator.
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
        let tasks_owned = state.journal.tasks.values().any(|entry| {
            !entry
                .owned_fields
                .unwrap_or_else(|| legacy_task_owned_fields(entry))
                .is_empty()
        });
        let units_owned = state.journal.units.values().any(|entry| {
            entry.owned_fields.map_or(
                state.journal.schema_version < OWNERSHIP_JOURNAL_SCHEMA_VERSION,
                |mask| !mask.is_empty(),
            )
        });
        Ok(!state.journal.entries.is_empty() || tasks_owned || units_owned)
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
        if state.journal.schema_version < OWNERSHIP_JOURNAL_SCHEMA_VERSION
            && !state.journal.units.is_empty()
        {
            return self.degrade_locked(
                &mut state,
                "schema-v1/v2 systemd journal has no stable unit instance identity; automatic recovery is unsafe"
                    .to_owned(),
            );
        }
        if state.journal.schema_version < JOURNAL_SCHEMA_VERSION {
            let mut upgraded = state.journal.clone();
            if let Err(error) = upgrade_legacy_journal(&mut upgraded, &self.registry) {
                return self.degrade_locked(
                    &mut state,
                    format!("cannot upgrade legacy recovery journal: {error}"),
                );
            }
            if let Err(error) = persist_journal(self.store.as_ref(), &upgraded) {
                return self.degrade_locked(
                    &mut state,
                    format!("cannot persist upgraded recovery journal: {error}"),
                );
            }
            state.journal = upgraded;
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
            if entry
                .manifest
                .as_ref()
                .is_some_and(|manifest| target.recovery_manifest() != *manifest)
            {
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
                journal_entry.legal_pairs =
                    Some(transaction_legal_pairs(entry.desired, entry.original));
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
                    manifest: Some(mutation.target.recovery_manifest()),
                    original: hardware_limits(&mutation.target),
                    desired: mutation.previous_request,
                    applied: mutation.previous_request,
                    legal_pairs: Some(vec![mutation.previous_request]),
                });
            entry.applied = mutation.previous_request;
            entry.desired = mutation.desired_request;
            entry.legal_pairs = Some(transaction_legal_pairs(
                mutation.previous_request,
                mutation.desired_request,
            ));
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
                        entry.legal_pairs = Some(vec![actual]);
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

    /// Apply a verified, journaled batch of non-real-time task scheduling state.
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
                let owned = entry
                    .owned_fields
                    .unwrap_or_else(|| legacy_task_owned_fields(&entry));
                let still_owned =
                    owned.fields_matching_either(&current, &entry.applied, &entry.applied);
                let lost = owned.without(still_owned);
                let relinquished = entry.relinquished_fields.unwrap_or_default().union(lost);
                lost.copy(&mut entry.original, &current);
                entry.desired.clone_from(&current);
                entry.applied.clone_from(&current);
                entry.owned_fields = Some(still_owned);
                entry.relinquished_fields = Some(relinquished);
                let rollback_entry = Some(entry.clone());
                (entry, rollback_entry)
            } else {
                (
                    TaskJournalEntry {
                        identity: request.identity,
                        original: current.clone(),
                        desired: current.clone(),
                        applied: current.clone(),
                        owned_fields: Some(TaskFieldMask::default()),
                        relinquished_fields: Some(TaskFieldMask::default()),
                    },
                    None,
                )
            };
            let owned = entry.owned_fields.unwrap_or_default();
            let relinquished = entry.relinquished_fields.unwrap_or_default();
            let requested = TaskFieldMask::changed(&current, &request.desired);
            let next_owned = owned.union(requested.without(relinquished));
            let newly_owned = next_owned.without(owned);
            newly_owned.copy(&mut entry.original, &current);
            let mut desired = current.clone();
            next_owned.copy(&mut desired, &request.desired);
            entry.desired.clone_from(&desired);
            entry.applied.clone_from(&current);
            entry.owned_fields = Some(next_owned);
            entry.relinquished_fields = Some(relinquished);
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
            let existing = state.journal.units.remove(&request.unit).filter(|entry| {
                entry
                    .instance
                    .as_ref()
                    .is_some_and(|owned| owned == instance)
            });
            let (mut entry, rollback_entry) = if let Some(mut entry) = existing {
                let owned = entry.owned_fields.unwrap_or_default();
                let still_owned =
                    owned.fields_matching_either(current, &entry.applied, &entry.applied);
                let lost = owned.without(still_owned);
                let relinquished = entry.relinquished_fields.unwrap_or_default().union(lost);
                lost.copy(&mut entry.original, current);
                entry.desired.clone_from(current);
                entry.applied.clone_from(current);
                entry.owned_fields = Some(still_owned);
                entry.relinquished_fields = Some(relinquished);
                let rollback_entry = Some(entry.clone());
                (entry, rollback_entry)
            } else {
                (
                    UnitJournalEntry {
                        unit: request.unit.clone(),
                        instance: Some(instance.clone()),
                        original: current.clone(),
                        desired: current.clone(),
                        applied: current.clone(),
                        owned_fields: Some(UnitFieldMask::default()),
                        relinquished_fields: Some(UnitFieldMask::default()),
                    },
                    None,
                )
            };
            let owned = entry.owned_fields.unwrap_or_default();
            let relinquished = entry.relinquished_fields.unwrap_or_default();
            let requested = UnitFieldMask::changed(current, &request.desired);
            let next_owned = owned.union(requested.without(relinquished));
            let newly_owned = next_owned.without(owned);
            newly_owned.copy(&mut entry.original, current);
            let mut desired = current.clone();
            next_owned.copy(&mut desired, &request.desired);
            entry.desired.clone_from(&desired);
            entry.applied.clone_from(current);
            entry.owned_fields = Some(next_owned);
            entry.relinquished_fields = Some(relinquished);
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
                journal_entry.legal_pairs =
                    Some(transaction_legal_pairs(entry.desired, entry.original));
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
            let owned = entry
                .owned_fields
                .unwrap_or_else(|| legacy_task_owned_fields(entry));
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
            let Some(instance) = entry.instance.as_ref() else {
                return self.degrade_locked(
                    &mut state,
                    format!(
                        "systemd journal entry {} has no stable instance identity",
                        entry.unit
                    ),
                );
            };
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
            let owned = entry.owned_fields.unwrap_or_default();
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
            let owned = entry
                .owned_fields
                .unwrap_or_else(|| legacy_task_owned_fields(&entry));
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
            let instance = entry.instance.as_ref().ok_or_else(|| {
                ActuatorError::InvalidJournal(format!(
                    "systemd journal entry {} has no stable instance identity",
                    entry.unit
                ))
            })?;
            if !unit_instance_is_current(systemd.as_ref(), instance)? {
                continue;
            }
            let current = systemd.read_unit_properties(&entry.unit)?;
            verify_unit_instance(systemd.as_ref(), instance)?;
            let owned = entry.owned_fields.unwrap_or_default();
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

fn legacy_frequency_pairs(entry: &JournalEntry) -> Vec<FrequencyLimits> {
    if entry.desired == entry.applied {
        vec![entry.applied]
    } else {
        transaction_legal_pairs(entry.applied, entry.desired)
    }
}

fn upgrade_legacy_journal(
    journal: &mut Journal,
    registry: &TargetRegistry,
) -> Result<(), ActuatorError> {
    if journal.schema_version >= JOURNAL_SCHEMA_VERSION {
        return Ok(());
    }
    let old_schema_version = journal.schema_version;
    if old_schema_version < OWNERSHIP_JOURNAL_SCHEMA_VERSION && !journal.units.is_empty() {
        return Err(ActuatorError::InvalidJournal(
            "legacy systemd entries have no stable instance identity".to_owned(),
        ));
    }
    for entry in journal.entries.values_mut() {
        let target = registry.get(&entry.target).ok_or_else(|| {
            ActuatorError::InvalidJournal(format!(
                "legacy recovery target {} is not present",
                entry.target
            ))
        })?;
        if target.min_path != entry.min_path || target.max_path != entry.max_path {
            return Err(ActuatorError::InvalidJournal(format!(
                "legacy recovery paths changed for {}",
                entry.target
            )));
        }
        if entry
            .manifest
            .as_ref()
            .is_some_and(|manifest| target.recovery_manifest() != *manifest)
        {
            return Err(ActuatorError::InvalidJournal(format!(
                "legacy recovery target identity changed for {}",
                entry.target
            )));
        }
        entry.manifest = Some(target.recovery_manifest());
        entry.legal_pairs = Some(legacy_frequency_pairs(entry));
        // Older schemas stored an effective aggregate here. Replaying that
        // value could turn another kernel client's transient cap into a
        // persistent userspace request, so migration releases our request.
        entry.original = hardware_limits(target);
    }
    if old_schema_version < OWNERSHIP_JOURNAL_SCHEMA_VERSION {
        for entry in journal.tasks.values_mut() {
            entry.owned_fields = Some(legacy_task_owned_fields(entry));
            entry.relinquished_fields = Some(TaskFieldMask::default());
        }
    }
    journal.schema_version = JOURNAL_SCHEMA_VERSION;
    validate_decoded_journal(journal)
}

fn legacy_task_owned_fields(entry: &TaskJournalEntry) -> TaskFieldMask {
    TaskFieldMask::changed(&entry.original, &entry.desired)
        .union(TaskFieldMask::changed(&entry.original, &entry.applied))
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
                        failures.push(format!("pid {}: {error}", identity.pid.get()));
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
                match controller.write_scheduling(identity.pid, &rollback) {
                    Ok(actual) if restorable.values_equal(&actual, &mutation.before) => {
                        if let Err(error) = verify_process_identity(proc_reader, *identity) {
                            failures.push(format!(
                                "pid {} identity changed during rollback: {error}",
                                identity.pid.get()
                            ));
                        }
                    }
                    Ok(actual) => {
                        failures.push(format!("pid {} read back {actual:?}", identity.pid.get()));
                    }
                    Err(error) => failures.push(format!("pid {}: {error}", identity.pid.get())),
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
    if !matches!(
        journal.schema_version,
        LEGACY_JOURNAL_SCHEMA_VERSION
            | MANIFEST_JOURNAL_SCHEMA_VERSION
            | OWNERSHIP_JOURNAL_SCHEMA_VERSION
            | JOURNAL_SCHEMA_VERSION
    ) {
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
        match (journal.schema_version, &entry.manifest) {
            (
                MANIFEST_JOURNAL_SCHEMA_VERSION
                | OWNERSHIP_JOURNAL_SCHEMA_VERSION
                | JOURNAL_SCHEMA_VERSION,
                Some(manifest),
            ) => {
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
                if journal.schema_version == JOURNAL_SCHEMA_VERSION
                    && entry.original != hardware_limits(&target)
                {
                    return Err(ActuatorError::InvalidJournal(format!(
                        "{} schema-v{} original request is not the full hardware range",
                        entry.target, journal.schema_version
                    )));
                }
            }
            (
                MANIFEST_JOURNAL_SCHEMA_VERSION
                | OWNERSHIP_JOURNAL_SCHEMA_VERSION
                | JOURNAL_SCHEMA_VERSION,
                None,
            ) => {
                return Err(ActuatorError::InvalidJournal(format!(
                    "{} schema-v{} entry has no recovery manifest",
                    entry.target, journal.schema_version
                )));
            }
            (LEGACY_JOURNAL_SCHEMA_VERSION, None) => {}
            (LEGACY_JOURNAL_SCHEMA_VERSION, Some(_)) => {
                return Err(ActuatorError::InvalidJournal(format!(
                    "{} schema-v1 entry unexpectedly contains a manifest",
                    entry.target
                )));
            }
            _ => unreachable!("schema version was checked above"),
        }
        match (journal.schema_version, &entry.legal_pairs) {
            (OWNERSHIP_JOURNAL_SCHEMA_VERSION | JOURNAL_SCHEMA_VERSION, Some(pairs))
                if !pairs.is_empty() =>
            {
                let mut unique = Vec::with_capacity(pairs.len());
                let target = entry
                    .manifest
                    .as_ref()
                    .expect("schema-v3+ manifest checked above")
                    .to_frequency_target()
                    .map_err(|error| ActuatorError::InvalidJournal(error.to_string()))?;
                for pair in pairs {
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
                if *pairs != expected {
                    return Err(ActuatorError::InvalidJournal(format!(
                        "{} legal frequency pairs do not match its exact ordered transition",
                        entry.target
                    )));
                }
            }
            (OWNERSHIP_JOURNAL_SCHEMA_VERSION | JOURNAL_SCHEMA_VERSION, _) => {
                return Err(ActuatorError::InvalidJournal(format!(
                    "{} schema-v{} entry has no legal frequency pairs",
                    entry.target, journal.schema_version
                )));
            }
            (LEGACY_JOURNAL_SCHEMA_VERSION | MANIFEST_JOURNAL_SCHEMA_VERSION, None) => {}
            (LEGACY_JOURNAL_SCHEMA_VERSION | MANIFEST_JOURNAL_SCHEMA_VERSION, Some(_)) => {
                return Err(ActuatorError::InvalidJournal(format!(
                    "{} legacy entry unexpectedly contains legal frequency pairs",
                    entry.target
                )));
            }
            _ => unreachable!("schema version was checked above"),
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
        match (
            journal.schema_version,
            entry.owned_fields,
            entry.relinquished_fields,
        ) {
            (
                OWNERSHIP_JOURNAL_SCHEMA_VERSION | JOURNAL_SCHEMA_VERSION,
                Some(owned),
                Some(relinquished),
            ) => {
                if owned.intersects(relinquished) {
                    return Err(ActuatorError::InvalidJournal(format!(
                        "task journal masks overlap for pid {}",
                        entry.identity.pid.get()
                    )));
                }
            }
            (OWNERSHIP_JOURNAL_SCHEMA_VERSION | JOURNAL_SCHEMA_VERSION, _, _) => {
                return Err(ActuatorError::InvalidJournal(format!(
                    "schema-v{} task entry for pid {} has no ownership masks",
                    journal.schema_version,
                    entry.identity.pid.get(),
                )));
            }
            (LEGACY_JOURNAL_SCHEMA_VERSION | MANIFEST_JOURNAL_SCHEMA_VERSION, None, None) => {}
            (LEGACY_JOURNAL_SCHEMA_VERSION | MANIFEST_JOURNAL_SCHEMA_VERSION, _, _) => {
                return Err(ActuatorError::InvalidJournal(format!(
                    "legacy task entry for pid {} unexpectedly contains ownership masks",
                    entry.identity.pid.get()
                )));
            }
            _ => unreachable!("schema version was checked above"),
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
        match (
            journal.schema_version,
            &entry.instance,
            entry.owned_fields,
            entry.relinquished_fields,
        ) {
            (
                OWNERSHIP_JOURNAL_SCHEMA_VERSION | JOURNAL_SCHEMA_VERSION,
                Some(instance),
                Some(owned),
                Some(relinquished),
            ) => {
                validate_unit_instance_identity(instance)?;
                if instance.unit != entry.unit {
                    return Err(ActuatorError::InvalidJournal(format!(
                        "systemd instance identity does not match {}",
                        entry.unit
                    )));
                }
                if owned.intersects(relinquished) {
                    return Err(ActuatorError::InvalidJournal(format!(
                        "systemd journal masks overlap for {}",
                        entry.unit
                    )));
                }
            }
            (OWNERSHIP_JOURNAL_SCHEMA_VERSION | JOURNAL_SCHEMA_VERSION, _, _, _) => {
                return Err(ActuatorError::InvalidJournal(format!(
                    "schema-v{} systemd entry {} lacks identity or ownership masks",
                    journal.schema_version, entry.unit
                )));
            }
            (LEGACY_JOURNAL_SCHEMA_VERSION | MANIFEST_JOURNAL_SCHEMA_VERSION, None, None, None) => {
            }
            (LEGACY_JOURNAL_SCHEMA_VERSION | MANIFEST_JOURNAL_SCHEMA_VERSION, _, _, _) => {
                return Err(ActuatorError::InvalidJournal(format!(
                    "legacy systemd entry {} unexpectedly contains v3 ownership metadata",
                    entry.unit
                )));
            }
            _ => unreachable!("schema version was checked above"),
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
        collections::{BTreeMap, VecDeque},
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
        JOURNAL_SCHEMA_VERSION, LEGACY_JOURNAL_SCHEMA_VERSION, MANIFEST_JOURNAL_SCHEMA_VERSION,
        OWNERSHIP_JOURNAL_SCHEMA_VERSION, RecoveryFrequencyTarget, TargetRegistry, TaskRequest,
        UnitRequest, decode_journal, encode_journal, inspect_recovery_journal,
        transaction_legal_pairs,
    };

    #[derive(Default)]
    struct MemorySysfs {
        values: Mutex<BTreeMap<PathBuf, String>>,
        writes: Mutex<Vec<(PathBuf, String)>>,
        fail_on_write: Mutex<Option<usize>>,
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

        fn fail_on(&self, write_number: usize) {
            *self.fail_on_write.lock().expect("fault lock") = Some(write_number);
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
            if *self.fail_on_write.lock().expect("fault lock") == Some(writes.len()) {
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
            uclamp_min: Some(128),
            uclamp_max: Some(896),
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
        entry.legal_pairs = Some(transaction_legal_pairs(entry.original, entry.desired));
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
        assert!(matches!(
            manifest.frequency_targets.as_slice(),
            [RecoveryFrequencyTarget::SelfDescribing(_)]
        ));
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
    fn schema_v1_manifest_can_be_resolved_without_old_configuration() {
        let io = Arc::new(MemorySysfs::with_pair("1000", "3000"));
        let store = Arc::new(MemoryStore::default());
        actuator(io.clone(), store.clone(), "boot-a", "device-a")
            .apply_batch(&[FrequencyRequest {
                target: id(),
                limits: limits(2_000, 3_000),
            }])
            .expect("apply");
        let bytes = store.load().expect("load journal").expect("journal bytes");
        let mut legacy = decode_journal(&bytes).expect("decode current journal");
        legacy.schema_version = LEGACY_JOURNAL_SCHEMA_VERSION;
        for entry in legacy.entries.values_mut() {
            entry.manifest = None;
            entry.legal_pairs = None;
        }
        store
            .store_durable(&encode_journal(&legacy).expect("encode legacy journal"))
            .expect("store legacy journal");

        let manifest = inspect_recovery_journal(store.as_ref())
            .expect("inspect legacy journal")
            .expect("legacy manifest");
        assert!(matches!(
            manifest.frequency_targets.as_slice(),
            [RecoveryFrequencyTarget::Legacy(_)]
        ));
        assert!(matches!(
            manifest.self_describing_registry(),
            Err(ActuatorError::LegacyRecoveryTarget(_))
        ));

        // Live discovery supplies this registry in the daemon; no old
        // configuration file is needed.
        let restarted = actuator(io, store.clone(), "boot-a", "device-a");
        restarted.recover_pending().expect("legacy recovery");
        assert_eq!(
            restarted.read_limits(&id()).expect("restored limits"),
            limits(1_000, 3_000)
        );
        assert!(store.load().expect("cleared journal").is_none());
    }

    #[test]
    fn invalid_schema_v1_limits_are_never_persisted_as_v4() {
        let io = Arc::new(MemorySysfs::with_pair("1000", "3000"));
        let store = Arc::new(MemoryStore::default());
        actuator(io.clone(), store.clone(), "boot-a", "device-a")
            .apply_batch(&[FrequencyRequest {
                target: id(),
                limits: limits(2_000, 3_000),
            }])
            .expect("apply");

        let bytes = store.load().expect("load journal").expect("journal bytes");
        let mut legacy = decode_journal(&bytes).expect("decode current journal");
        legacy.schema_version = LEGACY_JOURNAL_SCHEMA_VERSION;
        for entry in legacy.entries.values_mut() {
            entry.manifest = None;
            entry.legal_pairs = None;
            entry.desired = limits(4_000, 4_000);
            entry.applied = limits(4_000, 4_000);
        }
        let legacy_bytes = encode_journal(&legacy).expect("encode legacy journal");
        store
            .store_durable(&legacy_bytes)
            .expect("store legacy journal");
        let writes_before_recovery = io.writes().len();

        let restarted = actuator(io.clone(), store.clone(), "boot-a", "device-a");
        assert!(matches!(
            restarted.recover_pending(),
            Err(ActuatorError::Degraded(_))
        ));
        assert_eq!(io.writes().len(), writes_before_recovery);
        assert_eq!(
            store.load().expect("journal remains").as_deref(),
            Some(legacy_bytes.as_slice())
        );
    }

    #[test]
    fn schema_v3_constrained_original_migrates_to_a_released_request() {
        let io = Arc::new(MemorySysfs::with_pair("1000", "3000"));
        let store = Arc::new(MemoryStore::default());
        actuator(io.clone(), store.clone(), "boot-a", "device-a")
            .apply_batch(&[FrequencyRequest {
                target: id(),
                limits: limits(2_000, 3_000),
            }])
            .expect("apply");

        let bytes = store.load().expect("load journal").expect("journal bytes");
        let mut legacy = decode_journal(&bytes).expect("decode current journal");
        legacy.schema_version = OWNERSHIP_JOURNAL_SCHEMA_VERSION;
        legacy
            .entries
            .get_mut(&id())
            .expect("frequency entry")
            .original = limits(1_000, 2_000);
        store
            .store_durable(&encode_journal(&legacy).expect("encode schema-v3 journal"))
            .expect("store schema-v3 journal");

        let restarted = actuator(io, store.clone(), "boot-a", "device-a");
        restarted.recover_pending().expect("migrate and recover");
        assert_eq!(
            restarted.read_limits(&id()).expect("released limits"),
            limits(1_000, 3_000)
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
        journal
            .entries
            .values_mut()
            .next()
            .expect("frequency entry")
            .manifest
            .as_mut()
            .expect("recovery manifest")
            .min_path = PathBuf::from("/sys/other/min");
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
    fn schema_v2_task_recovery_derives_a_conservative_field_mask() {
        let store = Arc::new(MemoryStore::default());
        let proc_reader = Arc::new(FakeProc::default());
        let controller = Arc::new(FaultingProcessController::default());
        let identity = process_identity(41, 10);
        let original = scheduling(0);
        let desired = scheduling(-5);
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
        .expect("apply task");

        let bytes = store.load().expect("load journal").expect("journal");
        let mut journal = decode_journal(&bytes).expect("decode journal");
        journal.schema_version = MANIFEST_JOURNAL_SCHEMA_VERSION;
        for entry in journal.tasks.values_mut() {
            entry.owned_fields = None;
            entry.relinquished_fields = None;
        }
        store
            .store_durable(&encode_journal(&journal).expect("encode schema-v2 journal"))
            .expect("store schema-v2 journal");

        let mut administrator = desired;
        administrator.affinity = CpuSet::from_ids([CpuId::new(3)]);
        controller.set_admin(identity.pid, administrator);
        let restarted = control_actuator(
            store.clone(),
            "boot-a",
            Some(proc_reader),
            Some(controller.clone()),
            None,
        );
        restarted
            .recover_pending()
            .expect("recover schema-v2 task entry");
        let recovered = controller
            .read_scheduling(identity.pid)
            .expect("recovered task");
        assert_eq!(recovered.nice, original.nice);
        assert_eq!(recovered.affinity, CpuSet::from_ids([CpuId::new(3)]));
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
    fn schema_v3_recovery_restores_tasks_and_units() {
        let store = Arc::new(MemoryStore::default());
        let proc_reader = Arc::new(FakeProc::default());
        let controller = Arc::new(FaultingProcessController::default());
        let systemd = Arc::new(FaultingSystemd::default());
        let identity = process_identity(41, 10);
        let original_task = scheduling(0);
        let desired_task = scheduling(-5);
        let unit = "app-game.scope";
        let original_unit = unit_properties(100);
        let desired_unit = unit_properties(900);
        insert_process(&proc_reader, identity);
        controller.insert(identity.pid, original_task.clone());
        systemd.insert(unit, vec![identity.pid], original_unit.clone());

        let running = control_actuator(
            store.clone(),
            "boot-a",
            Some(proc_reader.clone()),
            Some(controller.clone()),
            Some(systemd.clone()),
        );
        running
            .apply_tasks(&[TaskRequest {
                identity,
                desired: desired_task,
            }])
            .expect("apply task");
        running
            .apply_units(&[UnitRequest {
                unit: unit.to_owned(),
                desired: desired_unit,
            }])
            .expect("apply unit");
        let bytes = store.load().expect("load journal").expect("journal");
        let mut legacy = decode_journal(&bytes).expect("decode journal");
        legacy.schema_version = OWNERSHIP_JOURNAL_SCHEMA_VERSION;
        store
            .store_durable(&encode_journal(&legacy).expect("encode schema-v3 journal"))
            .expect("store schema-v3 journal");

        let restarted = control_actuator(
            store.clone(),
            "boot-a",
            Some(proc_reader),
            Some(controller.clone()),
            Some(systemd.clone()),
        );
        assert!(
            restarted
                .startup_recovery_required()
                .expect("pending journal")
        );
        assert!(matches!(
            restarted.apply_tasks(&[TaskRequest {
                identity,
                desired: scheduling(-2),
            }]),
            Err(ActuatorError::RecoveryRequired)
        ));
        restarted.recover_pending().expect("same-boot recovery");

        assert_eq!(
            controller
                .read_scheduling(identity.pid)
                .expect("recovered task"),
            original_task
        );
        assert_eq!(
            systemd.read_unit_properties(unit).expect("recovered unit"),
            original_unit
        );
        assert!(
            !restarted
                .startup_recovery_required()
                .expect("pending journal")
        );
        assert!(store.load().expect("cleared recovery journal").is_none());
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
    fn schema_v2_systemd_journal_fails_closed_without_an_instance_identity() {
        let store = Arc::new(MemoryStore::default());
        let systemd = Arc::new(FaultingSystemd::default());
        let unit = "app-game.scope";
        systemd.insert(unit, vec![ProcessId::new(41)], unit_properties(100));
        control_actuator(store.clone(), "boot-a", None, None, Some(systemd.clone()))
            .apply_units(&[UnitRequest {
                unit: unit.to_owned(),
                desired: unit_properties(900),
            }])
            .expect("apply unit");
        let writes_before_restart = systemd.writes().len();

        let bytes = store.load().expect("load journal").expect("journal");
        let mut journal = decode_journal(&bytes).expect("decode journal");
        journal.schema_version = MANIFEST_JOURNAL_SCHEMA_VERSION;
        for entry in journal.units.values_mut() {
            entry.instance = None;
            entry.owned_fields = None;
            entry.relinquished_fields = None;
        }
        store
            .store_durable(&encode_journal(&journal).expect("encode schema-v2 journal"))
            .expect("store schema-v2 journal");

        let restarted =
            control_actuator(store.clone(), "boot-a", None, None, Some(systemd.clone()));
        assert!(matches!(
            restarted.recover_pending(),
            Err(ActuatorError::Degraded(_))
        ));
        assert!(matches!(
            restarted.mode().expect("mode"),
            ActuatorMode::ReadOnlyDegraded { .. }
        ));
        assert_eq!(systemd.writes().len(), writes_before_restart);
        assert!(store.load().expect("journal remains").is_some());
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

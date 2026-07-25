//! Fundamental units, identities, capabilities, and scheduler state.

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    fmt,
    iter::FromIterator,
};

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Frequency in hertz.
#[derive(
    Debug,
    Default,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    JsonSchema,
)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct Hertz(pub u64);

impl Hertz {
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for Hertz {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} Hz", self.0)
    }
}

/// Temperature in thousandths of one degree Celsius.
#[derive(
    Debug,
    Default,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    JsonSchema,
)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct MilliCelsius(pub i64);

impl MilliCelsius {
    #[must_use]
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }

    #[must_use]
    #[allow(clippy::cast_precision_loss)] // Kernel temperatures are far below 2^53.
    pub fn as_celsius(self) -> f64 {
        self.0 as f64 / 1_000.0
    }
}

impl fmt::Display for MilliCelsius {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:.3} °C", self.as_celsius())
    }
}

/// Milliseconds on a monotonic clock.  It is not wall-clock or Unix time.
#[derive(
    Debug,
    Default,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    JsonSchema,
)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct MonotonicMillis(pub u64);

impl MonotonicMillis {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    #[must_use]
    pub const fn saturating_add(self, duration_ms: u64) -> Self {
        Self(self.0.saturating_add(duration_ms))
    }

    #[must_use]
    pub const fn saturating_duration_since(self, earlier: Self) -> u64 {
        self.0.saturating_sub(earlier.0)
    }
}

macro_rules! numeric_id {
    ($name:ident) => {
        #[derive(
            Debug,
            Default,
            Clone,
            Copy,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            Serialize,
            Deserialize,
            JsonSchema,
        )]
        #[serde(transparent)]
        #[schemars(transparent)]
        pub struct $name(pub u32);

        impl $name {
            #[must_use]
            pub const fn new(value: u32) -> Self {
                Self(value)
            }

            #[must_use]
            pub const fn get(self) -> u32 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

numeric_id!(CpuId);
numeric_id!(ProcessId);
numeric_id!(UserId);

/// Error returned when a user/config supplied logical target identifier is invalid.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error(
    "invalid target id `{value}`; expected 1..=64 ASCII characters \
     ([A-Za-z0-9] followed by [A-Za-z0-9._-]*)"
)]
pub struct InvalidTargetId {
    pub value: String,
}

/// Stable, non-path identifier exposed through configuration and the public API.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TargetId(String);

impl TargetId {
    pub const MAX_LEN: usize = 64;

    /// Construct a validated logical target identifier.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidTargetId`] when the value is empty, too long, or contains
    /// path separators/characters outside the public identifier alphabet.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidTargetId> {
        let value = value.into();
        let mut bytes = value.bytes();
        let valid = !value.is_empty()
            && value.len() <= Self::MAX_LEN
            && bytes
                .next()
                .is_some_and(|byte| byte.is_ascii_alphanumeric())
            && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
        if valid {
            Ok(Self(value))
        } else {
            Err(InvalidTargetId { value })
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for TargetId {
    type Error = InvalidTargetId;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<TargetId> for String {
    fn from(value: TargetId) -> Self {
        value.0
    }
}

impl fmt::Display for TargetId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl JsonSchema for TargetId {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        "TargetId".into()
    }

    fn schema_id() -> Cow<'static, str> {
        concat!(module_path!(), "::TargetId").into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "minLength": 1,
            "maxLength": 64,
            "pattern": r"^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$"
        })
    }
}

/// A dynamically sized set of logical CPU IDs.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct CpuSet(BTreeSet<CpuId>);

impl CpuSet {
    #[must_use]
    pub const fn new() -> Self {
        Self(BTreeSet::new())
    }

    #[must_use]
    pub fn from_ids(ids: impl IntoIterator<Item = CpuId>) -> Self {
        ids.into_iter().collect()
    }

    pub fn insert(&mut self, cpu: CpuId) -> bool {
        self.0.insert(cpu)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &CpuId> {
        self.0.iter()
    }

    #[must_use]
    pub fn intersection(&self, other: &Self) -> Self {
        Self(self.0.intersection(&other.0).copied().collect())
    }

    #[must_use]
    pub fn is_disjoint(&self, other: &Self) -> bool {
        self.0.is_disjoint(&other.0)
    }

    #[must_use]
    pub fn is_subset(&self, other: &Self) -> bool {
        self.0.is_subset(&other.0)
    }
}

impl FromIterator<CpuId> for CpuSet {
    fn from_iter<T: IntoIterator<Item = CpuId>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl IntoIterator for CpuSet {
    type Item = CpuId;
    type IntoIter = std::collections::btree_set::IntoIter<CpuId>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a CpuSet {
    type Item = &'a CpuId;
    type IntoIter = std::collections::btree_set::Iter<'a, CpuId>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl From<Vec<CpuId>> for CpuSet {
    fn from(value: Vec<CpuId>) -> Self {
        value.into_iter().collect()
    }
}

/// PID identity resistant to PID reuse.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ProcessIdentity {
    pub pid: ProcessId,
    pub start_time_ticks: u64,
    pub uid: UserId,
}

/// Metadata associated with a process identity.  It is intentionally not part of
/// equality/ownership checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessInfo {
    pub identity: ProcessIdentity,
    /// Whether real/effective/saved/fs UIDs were identical when observed.
    ///
    /// A non-root control caller may only claim such a process. Root may still
    /// explicitly manage a setuid process.
    #[serde(default)]
    pub owner_control_safe: bool,
    pub comm: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desktop_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SensorHealth {
    Healthy,
    Stale,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ThermalReading {
    pub temperature: Option<MilliCelsius>,
    pub sampled_at: MonotonicMillis,
    pub health: SensorHealth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum InvalidFrequencyLimits {
    #[error("frequency minimum {min} exceeds maximum {max}")]
    Reversed { min: Hertz, max: Hertz },
}

/// Inclusive minimum and maximum frequency.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FrequencyLimits {
    pub min: Hertz,
    pub max: Hertz,
}

impl FrequencyLimits {
    /// Build a non-inverted frequency pair.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidFrequencyLimits`] when `min` exceeds `max`.
    pub fn new(min: Hertz, max: Hertz) -> Result<Self, InvalidFrequencyLimits> {
        if min <= max {
            Ok(Self { min, max })
        } else {
            Err(InvalidFrequencyLimits::Reversed { min, max })
        }
    }

    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.min.0 <= self.max.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CpuPolicyCapability {
    pub id: TargetId,
    pub policy_name: String,
    pub cpus: CpuSet,
    pub limits: FrequencyLimits,
    #[serde(default)]
    pub available_frequencies: Vec<Hertz>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub governor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DevfreqCapability {
    pub id: TargetId,
    pub device_name: String,
    #[serde(default)]
    pub compatible: Vec<String>,
    pub limits: FrequencyLimits,
    #[serde(default)]
    pub available_frequencies: Vec<Hertz>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub governor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ThermalZoneCapability {
    pub id: String,
    pub zone_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<ThermalReading>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InputDeviceCapability {
    pub id: String,
    pub name: String,
    pub multi_touch: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeviceCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,
    /// Device-tree compatible strings in most-specific-first order.
    #[serde(default)]
    pub compatible: Vec<String>,
    #[serde(default)]
    pub cpu_policies: Vec<CpuPolicyCapability>,
    #[serde(default)]
    pub devfreq_targets: Vec<DevfreqCapability>,
    #[serde(default)]
    pub thermal_zones: Vec<ThermalZoneCapability>,
    #[serde(default)]
    pub input_devices: Vec<InputDeviceCapability>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ObservedFrequency {
    pub limits: FrequencyLimits,
    /// Instantaneous frequency when the kernel exposes a readable source.
    pub current: Option<Hertz>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ObservedState {
    pub timestamp: MonotonicMillis,
    #[serde(default)]
    pub cpu_loads: BTreeMap<CpuId, f64>,
    #[serde(default)]
    pub frequencies: BTreeMap<TargetId, ObservedFrequency>,
    #[serde(default)]
    pub thermal: BTreeMap<String, ThermalReading>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SchedulingClass {
    Other,
    Batch,
    Idle,
}

/// Partial scheduling intent for one process or thread.
///
/// Every absent field preserves the task's current value. In particular,
/// `uclamp_min` and `uclamp_max` are independent so a policy can raise a floor
/// without also widening an administrator-provided ceiling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct TaskPlan {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affinity: Option<CpuSet>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nice: Option<i8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduling_class: Option<SchedulingClass>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(max = 1_024))]
    pub uclamp_min: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(max = 1_024))]
    pub uclamp_max: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DesiredPlan {
    pub generation: u64,
    pub effective_profile: crate::policy::ProfileId,
    pub dominant_scene: crate::policy::Scene,
    #[serde(default)]
    pub frequencies: BTreeMap<TargetId, FrequencyLimits>,
    #[serde(default)]
    pub tasks: BTreeMap<ProcessIdentity, TaskPlan>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct AppliedState {
    pub generation: u64,
    #[serde(default)]
    pub frequencies: BTreeMap<TargetId, FrequencyLimits>,
    #[serde(default)]
    pub tasks: BTreeMap<ProcessIdentity, TaskPlan>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_id_rejects_paths_and_accepts_logical_names() {
        assert_eq!(
            TargetId::new("cpu.prime-0").expect("valid id").as_str(),
            "cpu.prime-0"
        );
        assert!(TargetId::new("/sys/devices").is_err());
        assert!(TargetId::new("cpu/prime").is_err());
        assert!(TargetId::new("").is_err());
        assert!(TargetId::new("x".repeat(65)).is_err());
    }

    #[test]
    fn target_id_validation_also_applies_during_deserialization() {
        assert!(serde_json::from_str::<TargetId>("\"cpu.0\"").is_ok());
        assert!(serde_json::from_str::<TargetId>("\"../cpu0\"").is_err());
    }

    #[test]
    fn target_id_json_schema_is_a_string_not_an_internal_object() {
        let schema =
            serde_json::to_value(schemars::schema_for!(TargetId)).expect("serialize schema");
        assert_eq!(
            schema.get("type").and_then(serde_json::Value::as_str),
            Some("string")
        );
        assert_eq!(
            schema.get("pattern").and_then(serde_json::Value::as_str),
            Some(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$")
        );
    }

    #[test]
    fn cpu_set_is_dynamic_sorted_and_unique() {
        let set = CpuSet::from_ids([CpuId(128), CpuId(7), CpuId(7)]);
        assert_eq!(set.iter().count(), 2);
        assert_eq!(
            set.iter().copied().collect::<Vec<_>>(),
            [CpuId(7), CpuId(128)]
        );
        assert_eq!(serde_json::to_string(&set).expect("serialize"), "[7,128]");
    }

    #[test]
    fn process_identity_does_not_depend_on_mutable_metadata() {
        let identity = ProcessIdentity {
            pid: ProcessId(42),
            start_time_ticks: 123,
            uid: UserId(1000),
        };
        let a = ProcessInfo {
            identity,
            owner_control_safe: true,
            comm: "old".into(),
            executable: None,
            desktop_id: None,
        };
        let b = ProcessInfo {
            identity,
            owner_control_safe: true,
            comm: "renamed".into(),
            executable: Some("/usr/bin/game".into()),
            desktop_id: None,
        };
        assert_eq!(a.identity, b.identity);
    }
}

//! Read-only Linux evdev touch observation.
//!
//! The low-level [`TouchStateMachine`] is independent of device files and is
//! intentionally public so event traces can be replayed without root or
//! `/dev/input`. [`EvdevInputSource`] adds conservative hotplug discovery and
//! multiplexes supported type-B multitouch devices.
//!
//! `InputSource::next_event` is a blocking API. The daemon should run it on a
//! dedicated operating-system thread and forward its normalized events into a
//! bounded channel. Device descriptors themselves are opened nonblocking so a
//! quiet touchscreen cannot prevent another device or hotplug from being
//! observed.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    error::Error,
    ffi::OsStr,
    fmt,
    fs::OpenOptions,
    io,
    os::unix::fs::MetadataExt,
    os::{fd::OwnedFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

use evdev::{AbsoluteAxisCode, EventSummary, SynchronizationCode, raw_stream::RawDevice};
use uperf_core::InputConfig;
use uperf_platform::{
    InputDeviceId, InputEvent, InputSource, PlatformError, PlatformResult, TouchContactId,
};

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(4);
const DEFAULT_SCAN_INTERVAL: Duration = Duration::from_millis(500);
const MAX_IDLE_POLL_INTERVAL: Duration = Duration::from_millis(32);
const MAX_DEVICE_RETRY_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EventNodeIdentity {
    device: u64,
    inode: u64,
}

#[derive(Clone, Copy, Debug)]
struct FailedEventNode {
    identity: EventNodeIdentity,
    retry_at: Instant,
    next_delay: Duration,
}

/// Inclusive raw coordinate or slot range reported by an evdev device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AxisRange {
    minimum: i32,
    maximum: i32,
}

impl AxisRange {
    /// Build a non-empty axis range.
    ///
    /// # Errors
    ///
    /// Returns an error if `maximum` is smaller than `minimum`. A singleton
    /// range is useful for a device exposing exactly one MT slot.
    pub fn new(minimum: i32, maximum: i32) -> Result<Self, TouchConfigurationError> {
        if maximum < minimum {
            return Err(TouchConfigurationError::InvalidAxisRange { minimum, maximum });
        }
        Ok(Self { minimum, maximum })
    }

    /// Smallest advertised raw value.
    #[must_use]
    pub const fn minimum(self) -> i32 {
        self.minimum
    }

    /// Largest advertised raw value.
    #[must_use]
    pub const fn maximum(self) -> i32 {
        self.maximum
    }

    #[must_use]
    fn contains(self, value: i32) -> bool {
        (self.minimum..=self.maximum).contains(&value)
    }

    #[must_use]
    fn normalize(self, value: i32) -> f64 {
        if self.maximum == self.minimum {
            return 0.0;
        }
        let numerator = f64::from(value) - f64::from(self.minimum);
        let denominator = f64::from(self.maximum) - f64::from(self.minimum);
        (numerator / denominator).clamp(0.0, 1.0)
    }
}

/// Axis metadata required for Linux type-B multitouch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TouchAxes {
    pub x: AxisRange,
    pub y: AxisRange,
    pub slots: AxisRange,
}

/// Gesture thresholds expressed in device-independent normalized coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GestureConfig {
    swipe_distance: f64,
    edge_width: f64,
}

impl GestureConfig {
    /// Validate normalized gesture thresholds.
    ///
    /// # Errors
    ///
    /// Returns an error unless `swipe_distance` is in `(0, 1]` and
    /// `edge_width` is in `[0, 0.5]`.
    pub fn new(swipe_distance: f64, edge_width: f64) -> Result<Self, TouchConfigurationError> {
        if !(swipe_distance.is_finite() && 0.0 < swipe_distance && swipe_distance <= 1.0) {
            return Err(TouchConfigurationError::InvalidSwipeDistance(
                swipe_distance,
            ));
        }
        if !edge_width.is_finite() || !(0.0..=0.5).contains(&edge_width) {
            return Err(TouchConfigurationError::InvalidEdgeWidth(edge_width));
        }
        Ok(Self {
            swipe_distance,
            edge_width,
        })
    }

    /// Build gesture thresholds from the policy configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when the policy contains an invalid normalized value.
    pub fn from_input_config(config: &InputConfig) -> Result<Self, TouchConfigurationError> {
        Self::new(config.swipe_distance, config.edge_width)
    }

    /// Normalized ordinary swipe threshold.
    #[must_use]
    pub const fn swipe_distance(self) -> f64 {
        self.swipe_distance
    }

    /// Normalized edge activation band.
    #[must_use]
    pub const fn edge_width(self) -> f64 {
        self.edge_width
    }
}

impl Default for GestureConfig {
    fn default() -> Self {
        Self {
            swipe_distance: 0.03,
            edge_width: 0.03,
        }
    }
}

/// Invalid touch-device or gesture configuration.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TouchConfigurationError {
    InvalidAxisRange { minimum: i32, maximum: i32 },
    InvalidSwipeDistance(f64),
    InvalidEdgeWidth(f64),
}

impl fmt::Display for TouchConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAxisRange { minimum, maximum } => {
                write!(
                    formatter,
                    "axis maximum ({maximum}) must not be smaller than minimum ({minimum})"
                )
            }
            Self::InvalidSwipeDistance(distance) => {
                write!(
                    formatter,
                    "swipe distance must be finite and in (0, 1], got {distance}"
                )
            }
            Self::InvalidEdgeWidth(width) => {
                write!(
                    formatter,
                    "edge width must be finite and in [0, 0.5], got {width}"
                )
            }
        }
    }
}

impl Error for TouchConfigurationError {}

/// Relevant subset of the Linux type-B multitouch protocol.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RawTouchEvent {
    Slot(i32),
    TrackingId(i32),
    PositionX(i32),
    PositionY(i32),
    SyncReport,
    SyncDropped,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Point {
    x: f64,
    y: f64,
}

impl Point {
    #[must_use]
    fn distance(self, other: Self) -> f64 {
        (self.x - other.x).hypot(self.y - other.y)
    }
}

#[derive(Clone, Debug)]
struct Contact {
    tracking_id: u32,
    raw_x: Option<i32>,
    raw_y: Option<i32>,
    start: Option<Point>,
    last: Option<Point>,
    maximum_distance: f64,
    maximum_edge_inward: f64,
    down_emitted: bool,
}

impl Contact {
    fn new(tracking_id: u32) -> Self {
        Self {
            tracking_id,
            raw_x: None,
            raw_y: None,
            start: None,
            last: None,
            maximum_distance: 0.0,
            maximum_edge_inward: 0.0,
            down_emitted: false,
        }
    }

    fn refresh_point(&mut self, axes: TouchAxes, edge_width: f64) {
        let (Some(raw_x), Some(raw_y)) = (self.raw_x, self.raw_y) else {
            return;
        };
        let point = Point {
            x: axes.x.normalize(raw_x),
            y: axes.y.normalize(raw_y),
        };
        let start = *self.start.get_or_insert(point);
        self.last = Some(point);
        self.maximum_distance = self.maximum_distance.max(start.distance(point));
        self.maximum_edge_inward = self
            .maximum_edge_inward
            .max(edge_inward_distance(start, point, edge_width));
    }

    #[must_use]
    fn is_gesture(&self, config: GestureConfig) -> bool {
        if self.maximum_distance >= config.swipe_distance {
            return true;
        }
        if config.edge_width <= 0.0 {
            return false;
        }
        let edge_threshold = config.swipe_distance.min(config.edge_width);
        self.maximum_edge_inward >= edge_threshold
    }
}

#[derive(Clone, Debug, Default)]
struct SlotState {
    active: Option<Contact>,
}

/// Deterministic, per-device and per-slot type-B touch state machine.
#[derive(Clone, Debug)]
pub struct TouchStateMachine {
    device_id: InputDeviceId,
    axes: TouchAxes,
    config: GestureConfig,
    current_slot: Option<i32>,
    slots: BTreeMap<i32, SlotState>,
    pending_releases: Vec<Contact>,
    desynchronized: bool,
}

impl TouchStateMachine {
    /// Create a clean machine for one physical evdev device.
    #[must_use]
    pub fn new(device_id: InputDeviceId, axes: TouchAxes, config: GestureConfig) -> Self {
        Self {
            device_id,
            axes,
            config,
            current_slot: Some(axes.slots.minimum),
            slots: BTreeMap::new(),
            pending_releases: Vec::new(),
            desynchronized: false,
        }
    }

    /// Consume one raw protocol event.
    ///
    /// Output is only committed at `SYN_REPORT`, except that `SYN_DROPPED`
    /// immediately emits a device-scoped [`InputEvent::Resync`]. After a drop
    /// all partial contacts are discarded and corrupt events are ignored
    /// through the next report boundary. The evdev adapter then attempts a
    /// kernel-state rebuild where that can be done without guessing.
    #[must_use]
    pub fn handle(&mut self, event: RawTouchEvent) -> Vec<InputEvent> {
        if matches!(event, RawTouchEvent::SyncDropped) {
            self.reset_after_drop();
            return vec![InputEvent::Resync {
                device: Some(self.device_id),
            }];
        }

        if self.desynchronized {
            if matches!(event, RawTouchEvent::SyncReport) {
                self.desynchronized = false;
            }
            return Vec::new();
        }

        match event {
            RawTouchEvent::Slot(slot) => {
                self.current_slot = self.axes.slots.contains(slot).then_some(slot);
                Vec::new()
            }
            RawTouchEvent::TrackingId(tracking_id) => {
                self.handle_tracking_id(tracking_id);
                Vec::new()
            }
            RawTouchEvent::PositionX(value) => {
                self.update_position(Some(value), None);
                Vec::new()
            }
            RawTouchEvent::PositionY(value) => {
                self.update_position(None, Some(value));
                Vec::new()
            }
            RawTouchEvent::SyncReport => self.commit_frame(),
            RawTouchEvent::SyncDropped => unreachable!("handled before desynchronization gate"),
        }
    }

    /// Number of contacts currently tracked on this device.
    #[must_use]
    pub fn active_contacts(&self) -> usize {
        self.slots
            .values()
            .filter(|slot| slot.active.is_some())
            .count()
    }

    /// Whether events are currently ignored while waiting for the report after
    /// a dropped kernel frame.
    #[must_use]
    pub const fn is_desynchronized(&self) -> bool {
        self.desynchronized
    }

    /// Rebuild a single-slot device from an authoritative kernel snapshot.
    ///
    /// This is called only after the `SYN_REPORT` boundary following
    /// `SYN_DROPPED`. A negative tracking ID means that the slot is inactive.
    #[must_use]
    fn rebuild_single_slot(&mut self, tracking_id: i32, raw_x: i32, raw_y: i32) -> Vec<InputEvent> {
        self.slots.clear();
        self.pending_releases.clear();
        self.current_slot = Some(self.axes.slots.minimum);
        self.desynchronized = false;
        let Ok(tracking_id) = u32::try_from(tracking_id) else {
            return Vec::new();
        };
        let mut contact = Contact::new(tracking_id);
        contact.raw_x = Some(raw_x);
        contact.raw_y = Some(raw_y);
        self.slots
            .entry(self.axes.slots.minimum)
            .or_default()
            .active = Some(contact);
        self.commit_frame()
    }

    fn reset_after_drop(&mut self) {
        self.slots.clear();
        self.pending_releases.clear();
        self.current_slot = Some(self.axes.slots.minimum);
        self.desynchronized = true;
    }

    fn handle_tracking_id(&mut self, tracking_id: i32) {
        let Some(slot_id) = self.current_slot else {
            return;
        };
        let slot = self.slots.entry(slot_id).or_default();
        if tracking_id == -1 {
            if let Some(contact) = slot.active.take() {
                self.pending_releases.push(contact);
            }
            return;
        }
        let Ok(tracking_id) = u32::try_from(tracking_id) else {
            return;
        };
        if slot
            .active
            .as_ref()
            .is_some_and(|contact| contact.tracking_id == tracking_id)
        {
            return;
        }
        if let Some(contact) = slot.active.replace(Contact::new(tracking_id)) {
            self.pending_releases.push(contact);
        }
    }

    fn update_position(&mut self, x: Option<i32>, y: Option<i32>) {
        let Some(slot_id) = self.current_slot else {
            return;
        };
        let Some(contact) = self
            .slots
            .get_mut(&slot_id)
            .and_then(|slot| slot.active.as_mut())
        else {
            return;
        };
        if let Some(x) = x {
            contact.raw_x = Some(x);
        }
        if let Some(y) = y {
            contact.raw_y = Some(y);
        }
        contact.refresh_point(self.axes, self.config.edge_width);
    }

    fn commit_frame(&mut self) -> Vec<InputEvent> {
        let mut output = Vec::new();
        let device_id = self.device_id;
        for mut contact in self.pending_releases.drain(..) {
            contact.refresh_point(self.axes, self.config.edge_width);
            append_finished_contact(&mut output, device_id, &mut contact, self.config);
        }
        for slot in self.slots.values_mut() {
            let Some(contact) = slot.active.as_mut() else {
                continue;
            };
            contact.refresh_point(self.axes, self.config.edge_width);
            if !contact.down_emitted
                && let Some(point) = contact.last
            {
                output.push(InputEvent::TouchDown {
                    contact: TouchContactId::new(device_id, contact.tracking_id),
                    x: point.x,
                    y: point.y,
                });
                contact.down_emitted = true;
            }
        }
        output
    }
}

fn append_finished_contact(
    output: &mut Vec<InputEvent>,
    device_id: InputDeviceId,
    contact: &mut Contact,
    config: GestureConfig,
) {
    let Some(point) = contact.last else {
        return;
    };
    if !contact.down_emitted {
        output.push(InputEvent::TouchDown {
            contact: TouchContactId::new(device_id, contact.tracking_id),
            x: point.x,
            y: point.y,
        });
        contact.down_emitted = true;
    }
    output.push(InputEvent::TouchUp {
        contact: TouchContactId::new(device_id, contact.tracking_id),
        x: point.x,
        y: point.y,
    });
    if contact.is_gesture(config) {
        output.push(InputEvent::Gesture {
            contact: TouchContactId::new(device_id, contact.tracking_id),
            distance: contact.maximum_distance,
        });
    }
}

#[must_use]
fn edge_inward_distance(start: Point, current: Point, edge_width: f64) -> f64 {
    if edge_width <= 0.0 {
        return 0.0;
    }
    let mut maximum: f64 = 0.0;
    if start.x <= edge_width {
        maximum = maximum.max(current.x - start.x);
    }
    if start.x >= 1.0 - edge_width {
        maximum = maximum.max(start.x - current.x);
    }
    if start.y <= edge_width {
        maximum = maximum.max(current.y - start.y);
    }
    if start.y >= 1.0 - edge_width {
        maximum = maximum.max(start.y - current.y);
    }
    maximum.max(0.0)
}

#[derive(Debug)]
struct OpenTouchDevice {
    device: RawDevice,
    machine: TouchStateMachine,
}

impl OpenTouchDevice {
    fn open(
        path: &Path,
        device_id: InputDeviceId,
        config: GestureConfig,
    ) -> io::Result<Option<Self>> {
        // `custom_flags` only affects how this read-only descriptor is opened;
        // no ioctl that grabs, sends to, or otherwise mutates the device is
        // ever used.
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)?;
        let descriptor: OwnedFd = file.into();
        let device = RawDevice::from_fd(descriptor)?;
        let Some(axes) = touch_axes(&device)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            device,
            machine: TouchStateMachine::new(device_id, axes, config),
        }))
    }

    fn read_available(&mut self, output: &mut VecDeque<InputEvent>) -> io::Result<()> {
        let events = self.device.fetch_events()?.collect::<Vec<_>>();
        for event in events {
            let Some(event) = map_evdev_event(event) else {
                continue;
            };
            let was_desynchronized = self.machine.is_desynchronized();
            output.extend(self.machine.handle(event));
            if was_desynchronized
                && matches!(event, RawTouchEvent::SyncReport)
                && !self.machine.is_desynchronized()
                && let Some((tracking_id, raw_x, raw_y)) = self.single_slot_kernel_snapshot()?
            {
                output.extend(self.machine.rebuild_single_slot(tracking_id, raw_x, raw_y));
            }
        }
        Ok(())
    }

    /// Query enough current state to safely rebuild a single-slot device.
    ///
    /// Linux exposes complete multi-slot state through `EVIOCGMTSLOTS`, but
    /// evdev 0.13 does not expose that ioctl through its safe API and this
    /// workspace forbids unsafe code. For a multi-slot device we therefore
    /// deliberately keep the device-scoped reset and wait for fresh tracking
    /// events instead of fabricating an incomplete contact set. For a
    /// single-slot device, `EVIOCGABS` is complete and evdev exposes it safely.
    fn single_slot_kernel_snapshot(&self) -> io::Result<Option<(i32, i32, i32)>> {
        let axes = self.machine.axes;
        if axes.slots.minimum != axes.slots.maximum {
            return Ok(None);
        }
        let mut tracking_id = None;
        let mut raw_x = None;
        let mut raw_y = None;
        for (axis, info) in self.device.get_absinfo()? {
            match axis {
                AbsoluteAxisCode::ABS_MT_TRACKING_ID => tracking_id = Some(info.value()),
                AbsoluteAxisCode::ABS_MT_POSITION_X => raw_x = Some(info.value()),
                AbsoluteAxisCode::ABS_MT_POSITION_Y => raw_y = Some(info.value()),
                _ => {}
            }
        }
        tracking_id
            .zip(raw_x)
            .zip(raw_y)
            .map(|((tracking_id, raw_x), raw_y)| (tracking_id, raw_x, raw_y))
            .map(Some)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "single-slot touch device omitted required absolute-axis state",
                )
            })
    }
}

/// Multiplexed, read-only source for dynamically discovered touch devices.
///
/// The source periodically scans `eventN` nodes below its root. Supported
/// devices can appear and disappear at runtime. Removal emits a device-scoped
/// `Resync`; addition receives a fresh opaque identity and cannot disturb
/// contacts belonging to other devices.
#[derive(Debug)]
pub struct EvdevInputSource {
    root: PathBuf,
    config: GestureConfig,
    devices: BTreeMap<PathBuf, OpenTouchDevice>,
    device_identities: BTreeMap<PathBuf, EventNodeIdentity>,
    ignored_nodes: BTreeMap<PathBuf, EventNodeIdentity>,
    failed_nodes: BTreeMap<PathBuf, FailedEventNode>,
    pending: VecDeque<InputEvent>,
    device_errors: BTreeMap<PathBuf, String>,
    poll_interval: Duration,
    scan_interval: Duration,
    next_scan: Instant,
    next_device_id: u64,
}

impl EvdevInputSource {
    /// Observe `/dev/input` with production polling intervals.
    ///
    /// A missing `/dev/input` directory is valid and yields an initially empty
    /// source. The directory will continue to be checked for hotplug.
    ///
    /// # Errors
    ///
    /// Returns an error if an existing input directory cannot be enumerated.
    pub fn host(config: GestureConfig) -> PlatformResult<Self> {
        Self::open("/dev/input", config)
    }

    /// Observe an alternate input root, useful for containers and diagnostics.
    ///
    /// # Errors
    ///
    /// Returns an error if an existing input directory cannot be enumerated.
    pub fn open(root: impl Into<PathBuf>, config: GestureConfig) -> PlatformResult<Self> {
        Self::with_intervals(root, config, DEFAULT_POLL_INTERVAL, DEFAULT_SCAN_INTERVAL)
    }

    /// Construct with explicit polling intervals.
    ///
    /// This is primarily useful for integration tests. A zero interval is
    /// replaced by one millisecond to avoid a busy loop.
    ///
    /// # Errors
    ///
    /// Returns an error if an existing input directory cannot be enumerated.
    pub fn with_intervals(
        root: impl Into<PathBuf>,
        config: GestureConfig,
        poll_interval: Duration,
        scan_interval: Duration,
    ) -> PlatformResult<Self> {
        let mut source = Self {
            root: root.into(),
            config,
            devices: BTreeMap::new(),
            device_identities: BTreeMap::new(),
            ignored_nodes: BTreeMap::new(),
            failed_nodes: BTreeMap::new(),
            pending: VecDeque::new(),
            device_errors: BTreeMap::new(),
            poll_interval: nonzero_duration(poll_interval),
            scan_interval: nonzero_duration(scan_interval),
            next_scan: Instant::now(),
            next_device_id: 1,
        };
        source.refresh_devices()?;
        source.pending.clear();
        Ok(source)
    }

    /// Number of currently open type-B multitouch devices.
    #[must_use]
    pub fn device_count(&self) -> usize {
        self.devices.len()
    }

    /// Last open/read failure for device paths, for health diagnostics.
    #[must_use]
    pub fn device_errors(&self) -> &BTreeMap<PathBuf, String> {
        &self.device_errors
    }

    /// Wait for an input event while periodically checking a cancellation flag.
    ///
    /// This gives a daemon-owned input thread a bounded shutdown path without
    /// grabbing or writing the evdev device.
    ///
    /// # Errors
    ///
    /// Returns an observation error if hotplug enumeration fails.
    pub fn next_event_until(
        &mut self,
        cancelled: &AtomicBool,
    ) -> PlatformResult<Option<InputEvent>> {
        self.next_event_until_interrupt(cancelled, || false)
    }

    /// Wait for an event until shutdown or a caller-defined reconfiguration
    /// condition becomes true.
    ///
    /// The condition is checked at the normal evdev polling cadence, allowing a
    /// daemon to atomically replace gesture configuration without grabbing the
    /// input device or leaking a blocked reader thread.
    ///
    /// # Errors
    ///
    /// Returns an observation error if hotplug enumeration fails.
    pub fn next_event_until_interrupt(
        &mut self,
        cancelled: &AtomicBool,
        mut interrupted: impl FnMut() -> bool,
    ) -> PlatformResult<Option<InputEvent>> {
        let mut idle_poll_interval = self.poll_interval;
        loop {
            if cancelled.load(Ordering::Acquire) || interrupted() {
                return Ok(None);
            }
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }
            if Instant::now() >= self.next_scan {
                self.refresh_devices()?;
                if let Some(event) = self.pending.pop_front() {
                    return Ok(Some(event));
                }
            }
            if self.poll_devices() {
                idle_poll_interval = self.poll_interval;
            } else {
                idle_poll_interval = idle_poll_interval
                    .saturating_mul(2)
                    .min(MAX_IDLE_POLL_INTERVAL);
            }
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }

            let until_scan = self.next_scan.saturating_duration_since(Instant::now());
            let sleep_for = if self.devices.is_empty() {
                until_scan
            } else {
                idle_poll_interval.min(until_scan)
            };
            thread::sleep(sleep_for);
        }
    }

    fn refresh_devices(&mut self) -> PlatformResult<()> {
        let discovered = discover_event_nodes(&self.root)?;

        let removed = self
            .devices
            .keys()
            .chain(self.ignored_nodes.keys())
            .chain(self.failed_nodes.keys())
            .filter(|path| !discovered.contains_key(*path))
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        for path in removed {
            if let Some(device) = self.devices.remove(&path) {
                self.pending.push_back(InputEvent::Resync {
                    device: Some(device.machine.device_id),
                });
            }
            self.device_identities.remove(&path);
            self.ignored_nodes.remove(&path);
            self.failed_nodes.remove(&path);
            self.device_errors.remove(&path);
        }

        let now = Instant::now();
        for (path, identity) in discovered {
            if self
                .device_identities
                .get(&path)
                .is_some_and(|known| *known == identity)
            {
                continue;
            }
            if self.device_identities.remove(&path).is_some()
                && let Some(device) = self.devices.remove(&path)
            {
                self.pending.push_back(InputEvent::Resync {
                    device: Some(device.machine.device_id),
                });
            }
            if self
                .ignored_nodes
                .get(&path)
                .is_some_and(|known| *known == identity)
            {
                continue;
            }
            self.ignored_nodes.remove(&path);
            let previous_failure = self.failed_nodes.remove(&path);
            if previous_failure
                .is_some_and(|failure| failure.identity == identity && now < failure.retry_at)
            {
                self.failed_nodes
                    .insert(path, previous_failure.expect("checked failure"));
                continue;
            }
            let retry_delay = previous_failure
                .filter(|failure| failure.identity == identity)
                .map_or(self.scan_interval, |failure| failure.next_delay);
            let device_id = InputDeviceId::new(self.next_device_id);
            match OpenTouchDevice::open(&path, device_id, self.config) {
                Ok(Some(device)) => {
                    self.next_device_id = self.next_device_id.checked_add(1).ok_or_else(|| {
                        PlatformError::invalid(
                            &self.root,
                            "input device identity space was exhausted",
                        )
                    })?;
                    self.devices.insert(path.clone(), device);
                    self.device_identities.insert(path.clone(), identity);
                    self.device_errors.remove(&path);
                }
                Ok(None) => {
                    self.ignored_nodes.insert(path.clone(), identity);
                    self.device_errors.remove(&path);
                }
                Err(error) => {
                    self.device_errors.insert(path.clone(), error.to_string());
                    self.failed_nodes.insert(
                        path,
                        FailedEventNode {
                            identity,
                            retry_at: now + retry_delay,
                            next_delay: retry_delay
                                .saturating_mul(2)
                                .min(MAX_DEVICE_RETRY_INTERVAL),
                        },
                    );
                }
            }
        }

        self.next_scan = now + self.scan_interval;
        Ok(())
    }

    fn poll_devices(&mut self) -> bool {
        let pending_before = self.pending.len();
        let paths = self.devices.keys().cloned().collect::<Vec<_>>();
        let mut disconnected = Vec::new();
        for path in paths {
            let result = self
                .devices
                .get_mut(&path)
                .expect("path came from the same map")
                .read_available(&mut self.pending);
            match result {
                Ok(()) => {
                    self.device_errors.remove(&path);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => {
                    self.device_errors.insert(path.clone(), error.to_string());
                    disconnected.push(path);
                }
            }
        }
        let had_disconnected = !disconnected.is_empty();
        if had_disconnected {
            for path in disconnected {
                if let Some(device) = self.devices.remove(&path) {
                    self.pending.push_back(InputEvent::Resync {
                        device: Some(device.machine.device_id),
                    });
                }
                if let Some(identity) = self.device_identities.remove(&path) {
                    self.failed_nodes.insert(
                        path,
                        FailedEventNode {
                            identity,
                            retry_at: Instant::now() + self.scan_interval,
                            next_delay: self
                                .scan_interval
                                .saturating_mul(2)
                                .min(MAX_DEVICE_RETRY_INTERVAL),
                        },
                    );
                }
            }
            self.next_scan = Instant::now() + self.scan_interval;
        }
        self.pending.len() != pending_before || had_disconnected
    }
}

impl InputSource for EvdevInputSource {
    fn next_event(&mut self) -> PlatformResult<InputEvent> {
        self.next_event_until(&AtomicBool::new(false))?
            .ok_or(PlatformError::Disappeared(
                "input source was unexpectedly cancelled".to_owned(),
            ))
    }
}

fn touch_axes(device: &RawDevice) -> io::Result<Option<TouchAxes>> {
    let Some(supported) = device.supported_absolute_axes() else {
        return Ok(None);
    };
    let required = [
        AbsoluteAxisCode::ABS_MT_SLOT,
        AbsoluteAxisCode::ABS_MT_TRACKING_ID,
        AbsoluteAxisCode::ABS_MT_POSITION_X,
        AbsoluteAxisCode::ABS_MT_POSITION_Y,
    ];
    if !required.into_iter().all(|axis| supported.contains(axis)) {
        return Ok(None);
    }

    let mut x = None;
    let mut y = None;
    let mut slots = None;
    for (axis, info) in device.get_absinfo()? {
        let range = || AxisRange::new(info.minimum(), info.maximum()).ok();
        match axis {
            AbsoluteAxisCode::ABS_MT_POSITION_X if info.maximum() > info.minimum() => x = range(),
            AbsoluteAxisCode::ABS_MT_POSITION_Y if info.maximum() > info.minimum() => y = range(),
            AbsoluteAxisCode::ABS_MT_SLOT => slots = range(),
            _ => {}
        }
    }
    Ok(x.zip(y)
        .zip(slots)
        .map(|((x, y), slots)| TouchAxes { x, y, slots }))
}

#[must_use]
fn map_evdev_event(event: evdev::InputEvent) -> Option<RawTouchEvent> {
    match event.destructure() {
        EventSummary::AbsoluteAxis(_, AbsoluteAxisCode::ABS_MT_SLOT, value) => {
            Some(RawTouchEvent::Slot(value))
        }
        EventSummary::AbsoluteAxis(_, AbsoluteAxisCode::ABS_MT_TRACKING_ID, value) => {
            Some(RawTouchEvent::TrackingId(value))
        }
        EventSummary::AbsoluteAxis(_, AbsoluteAxisCode::ABS_MT_POSITION_X, value) => {
            Some(RawTouchEvent::PositionX(value))
        }
        EventSummary::AbsoluteAxis(_, AbsoluteAxisCode::ABS_MT_POSITION_Y, value) => {
            Some(RawTouchEvent::PositionY(value))
        }
        EventSummary::Synchronization(_, SynchronizationCode::SYN_REPORT, _) => {
            Some(RawTouchEvent::SyncReport)
        }
        EventSummary::Synchronization(_, SynchronizationCode::SYN_DROPPED, _) => {
            Some(RawTouchEvent::SyncDropped)
        }
        _ => None,
    }
}

fn discover_event_nodes(root: &Path) -> PlatformResult<BTreeMap<PathBuf, EventNodeIdentity>> {
    let entries = match root.read_dir() {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => {
            return Err(PlatformError::io(
                "enumerate evdev input directory",
                root,
                error,
            ));
        }
    };
    let mut paths = BTreeMap::new();
    for entry in entries {
        let entry = entry
            .map_err(|error| PlatformError::io("read evdev input directory entry", root, error))?;
        if is_event_node(&entry.file_name()) {
            let path = entry.path();
            let metadata = entry
                .metadata()
                .map_err(|error| PlatformError::io("stat evdev input node", &path, error))?;
            paths.insert(
                path,
                EventNodeIdentity {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                },
            );
        }
    }
    Ok(paths)
}

#[must_use]
fn is_event_node(name: &OsStr) -> bool {
    name.to_str()
        .and_then(|name| name.strip_prefix("event"))
        .is_some_and(|suffix| {
            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
        })
}

#[must_use]
fn nonzero_duration(duration: Duration) -> Duration {
    if duration.is_zero() {
        Duration::from_millis(1)
    } else {
        duration
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICE: InputDeviceId = InputDeviceId::new(41);

    fn axes() -> TouchAxes {
        TouchAxes {
            x: AxisRange::new(-100, 900).unwrap(),
            y: AxisRange::new(50, 550).unwrap(),
            slots: AxisRange::new(0, 9).unwrap(),
        }
    }

    fn config(swipe_distance: f64, edge_width: f64) -> GestureConfig {
        GestureConfig::new(swipe_distance, edge_width).unwrap()
    }

    fn feed(machine: &mut TouchStateMachine, events: &[RawTouchEvent]) -> Vec<InputEvent> {
        events
            .iter()
            .flat_map(|event| machine.handle(*event))
            .collect()
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < 1.0e-9, "{actual} != {expected}");
    }

    #[test]
    fn normalizes_each_devices_advertised_ranges() {
        let mut machine = TouchStateMachine::new(DEVICE, axes(), config(0.2, 0.05));
        let output = feed(
            &mut machine,
            &[
                RawTouchEvent::TrackingId(7),
                RawTouchEvent::PositionX(400),
                RawTouchEvent::PositionY(175),
                RawTouchEvent::SyncReport,
            ],
        );
        assert_eq!(output.len(), 1);
        let InputEvent::TouchDown { contact, x, y } = output[0] else {
            panic!("expected touch down");
        };
        assert_eq!(contact, TouchContactId::new(DEVICE, 7));
        assert_close(x, 0.5);
        assert_close(y, 0.25);
    }

    #[test]
    fn tracks_multiple_slots_independently() {
        let mut machine = TouchStateMachine::new(DEVICE, axes(), config(0.2, 0.05));
        let downs = feed(
            &mut machine,
            &[
                RawTouchEvent::Slot(0),
                RawTouchEvent::TrackingId(10),
                RawTouchEvent::PositionX(-100),
                RawTouchEvent::PositionY(50),
                RawTouchEvent::Slot(3),
                RawTouchEvent::TrackingId(11),
                RawTouchEvent::PositionX(900),
                RawTouchEvent::PositionY(550),
                RawTouchEvent::SyncReport,
            ],
        );
        assert_eq!(downs.len(), 2);
        assert_eq!(machine.active_contacts(), 2);

        let release = feed(
            &mut machine,
            &[
                RawTouchEvent::Slot(3),
                RawTouchEvent::TrackingId(-1),
                RawTouchEvent::SyncReport,
            ],
        );
        assert_eq!(release.len(), 1);
        let InputEvent::TouchUp { contact, x, y } = release[0] else {
            panic!("expected touch up");
        };
        assert_eq!(contact, TouchContactId::new(DEVICE, 11));
        assert_close(x, 1.0);
        assert_close(y, 1.0);
        assert_eq!(machine.active_contacts(), 1);
    }

    #[test]
    fn emits_gesture_for_maximum_swipe_even_if_contact_returns() {
        let mut machine = TouchStateMachine::new(DEVICE, axes(), config(0.25, 0.05));
        let _ = feed(
            &mut machine,
            &[
                RawTouchEvent::TrackingId(1),
                RawTouchEvent::PositionX(0),
                RawTouchEvent::PositionY(300),
                RawTouchEvent::SyncReport,
            ],
        );
        let _ = feed(
            &mut machine,
            &[
                RawTouchEvent::PositionX(700),
                RawTouchEvent::SyncReport,
                RawTouchEvent::PositionX(0),
                RawTouchEvent::SyncReport,
            ],
        );
        let release = feed(
            &mut machine,
            &[RawTouchEvent::TrackingId(-1), RawTouchEvent::SyncReport],
        );
        assert_eq!(release.len(), 2);
        assert!(matches!(release[0], InputEvent::TouchUp { .. }));
        let InputEvent::Gesture { distance, .. } = release[1] else {
            panic!("expected gesture");
        };
        assert_close(distance, 0.7);
    }

    #[test]
    fn recognizes_short_inward_edge_gesture() {
        let custom_axes = TouchAxes {
            x: AxisRange::new(0, 1_000).unwrap(),
            y: AxisRange::new(0, 2_000).unwrap(),
            slots: AxisRange::new(0, 4).unwrap(),
        };
        let mut machine = TouchStateMachine::new(DEVICE, custom_axes, config(0.3, 0.05));
        let output = feed(
            &mut machine,
            &[
                RawTouchEvent::TrackingId(5),
                RawTouchEvent::PositionX(10),
                RawTouchEvent::PositionY(1_000),
                RawTouchEvent::SyncReport,
                RawTouchEvent::PositionX(80),
                RawTouchEvent::SyncReport,
                RawTouchEvent::TrackingId(-1),
                RawTouchEvent::SyncReport,
            ],
        );
        assert_eq!(output.len(), 3);
        assert!(matches!(output[0], InputEvent::TouchDown { .. }));
        assert!(matches!(output[1], InputEvent::TouchUp { .. }));
        let InputEvent::Gesture { distance, .. } = output[2] else {
            panic!("expected edge gesture");
        };
        assert_close(distance, 0.07);
    }

    #[test]
    fn dropped_frame_clears_slots_and_ignores_until_report() {
        let mut machine = TouchStateMachine::new(DEVICE, axes(), config(0.2, 0.05));
        let _ = feed(
            &mut machine,
            &[
                RawTouchEvent::TrackingId(1),
                RawTouchEvent::PositionX(0),
                RawTouchEvent::PositionY(300),
                RawTouchEvent::SyncReport,
            ],
        );
        assert_eq!(machine.active_contacts(), 1);

        assert_eq!(
            machine.handle(RawTouchEvent::SyncDropped),
            vec![InputEvent::Resync {
                device: Some(DEVICE)
            }]
        );
        assert!(machine.is_desynchronized());
        assert_eq!(machine.active_contacts(), 0);
        let ignored = feed(
            &mut machine,
            &[
                RawTouchEvent::TrackingId(2),
                RawTouchEvent::PositionX(900),
                RawTouchEvent::PositionY(550),
                RawTouchEvent::SyncReport,
            ],
        );
        assert!(ignored.is_empty());
        assert!(!machine.is_desynchronized());

        let resumed = feed(
            &mut machine,
            &[
                RawTouchEvent::TrackingId(3),
                RawTouchEvent::PositionX(900),
                RawTouchEvent::PositionY(550),
                RawTouchEvent::SyncReport,
            ],
        );
        assert_eq!(resumed.len(), 1);
        assert!(matches!(resumed[0], InputEvent::TouchDown { .. }));
    }

    #[test]
    fn single_slot_snapshot_rebuilds_held_contact_after_drop_boundary() {
        let single_slot_axes = TouchAxes {
            x: AxisRange::new(0, 1_000).unwrap(),
            y: AxisRange::new(0, 2_000).unwrap(),
            slots: AxisRange::new(0, 0).unwrap(),
        };
        let mut machine = TouchStateMachine::new(DEVICE, single_slot_axes, config(0.2, 0.05));
        let _ = feed(
            &mut machine,
            &[
                RawTouchEvent::TrackingId(17),
                RawTouchEvent::PositionX(100),
                RawTouchEvent::PositionY(200),
                RawTouchEvent::SyncReport,
            ],
        );
        let reset = machine.handle(RawTouchEvent::SyncDropped);
        assert!(matches!(
            reset.as_slice(),
            [InputEvent::Resync {
                device: Some(DEVICE)
            }]
        ));
        assert!(machine.handle(RawTouchEvent::SyncReport).is_empty());

        let rebuilt = machine.rebuild_single_slot(17, 400, 1_000);
        assert_eq!(machine.active_contacts(), 1);
        assert!(matches!(
            rebuilt.as_slice(),
            [InputEvent::TouchDown {
                contact: TouchContactId {
                    device: DEVICE,
                    tracking_id: 17
                },
                ..
            }]
        ));
    }

    #[test]
    fn tracking_ids_are_scoped_to_the_open_device_instance() {
        let mut first = TouchStateMachine::new(InputDeviceId::new(1), axes(), config(0.2, 0.05));
        let mut second = TouchStateMachine::new(InputDeviceId::new(2), axes(), config(0.2, 0.05));
        let events = [
            RawTouchEvent::TrackingId(9),
            RawTouchEvent::PositionX(100),
            RawTouchEvent::PositionY(100),
            RawTouchEvent::SyncReport,
        ];
        let first = feed(&mut first, &events);
        let second = feed(&mut second, &events);
        let InputEvent::TouchDown { contact: first, .. } = first[0] else {
            panic!("expected first contact");
        };
        let InputEvent::TouchDown {
            contact: second, ..
        } = second[0]
        else {
            panic!("expected second contact");
        };
        assert_ne!(first, second);
    }

    #[test]
    fn invalid_slot_does_not_allocate_unbounded_state() {
        let mut machine = TouchStateMachine::new(DEVICE, axes(), config(0.2, 0.05));
        let output = feed(
            &mut machine,
            &[
                RawTouchEvent::Slot(i32::MAX),
                RawTouchEvent::TrackingId(1),
                RawTouchEvent::PositionX(0),
                RawTouchEvent::PositionY(0),
                RawTouchEvent::SyncReport,
            ],
        );
        assert!(output.is_empty());
        assert_eq!(machine.active_contacts(), 0);
    }

    #[test]
    fn missing_input_root_is_valid_and_empty() {
        let temporary = tempfile::tempdir().unwrap();
        let mut source =
            EvdevInputSource::open(temporary.path().join("missing"), GestureConfig::default())
                .unwrap();
        assert_eq!(source.device_count(), 0);
        assert!(
            source
                .next_event_until_interrupt(&AtomicBool::new(false), || true)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn recognizes_only_numeric_event_nodes() {
        assert!(is_event_node(OsStr::new("event0")));
        assert!(is_event_node(OsStr::new("event123")));
        assert!(!is_event_node(OsStr::new("event")));
        assert!(!is_event_node(OsStr::new("event3-old")));
        assert!(!is_event_node(OsStr::new("mouse0")));
    }

    #[test]
    fn failed_event_node_is_backed_off_until_its_instance_changes() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("event0");
        std::fs::write(&path, b"not an evdev device").unwrap();
        let mut source = EvdevInputSource::with_intervals(
            temporary.path(),
            GestureConfig::default(),
            Duration::from_millis(1),
            Duration::from_secs(1),
        )
        .unwrap();
        let first = source.failed_nodes[&path];
        source.refresh_devices().unwrap();
        assert_eq!(source.failed_nodes[&path].retry_at, first.retry_at);

        std::fs::rename(&path, temporary.path().join("old-event0")).unwrap();
        std::fs::write(&path, b"a replacement node").unwrap();
        source.refresh_devices().unwrap();
        assert_ne!(source.failed_nodes[&path].identity, first.identity);
    }
}

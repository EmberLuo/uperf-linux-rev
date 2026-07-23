use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};
use tokio::{
    signal::unix::{SignalKind, signal},
    sync::watch,
};
use uperf_actuator::{
    FileStateStore, FrequencyActuator, FrequencyTarget, RecoveryFrequencyTarget,
    RecoveryFrequencyTargetManifest, RecoveryManifest, TargetRegistry, inspect_recovery_journal,
};
use uperf_api::{OBJECT_PATH, SERVICE_NAME};
use uperf_core::{FrequencyLimits, Hertz};
use uperf_daemon::{
    auth::{AuthorizationMode, Authorizer},
    config::{ConfigurationPaths, ResolvedConfiguration},
    observers::{spawn_input_observer, spawn_logind_observer},
    runtime::{RuntimeParts, spawn_linux_observers, spawn_runtime},
    service::{DaemonService, run_signal_pump},
};
use uperf_linux::{
    LinuxDiscovery, LinuxEnvironment, LinuxProcessController, SystemRoots, SystemdDbusClient,
};
use uperf_platform::{ProcessController, StateStore, SystemdClient};
use zbus::Connection;

#[derive(Debug)]
struct Options {
    config_dir: PathBuf,
    state_dir: PathBuf,
    runtime_dir: PathBuf,
    fixture_root: Option<PathBuf>,
    session_bus: bool,
    read_only: bool,
}

#[derive(Clone, Default)]
struct MutationBackends {
    process: Option<Arc<dyn ProcessController>>,
    systemd: Option<Arc<dyn SystemdClient>>,
}

struct MutationSetup {
    boot_id: String,
    fingerprint: String,
    store: Arc<FileStateStore>,
    backends: MutationBackends,
    recovery_failure: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            config_dir: PathBuf::from("/etc/uperf-linux"),
            state_dir: PathBuf::from("/var/lib/uperf-linux"),
            runtime_dir: PathBuf::from("/run/uperf-linux"),
            fixture_root: None,
            session_bus: false,
            read_only: false,
        }
    }
}

// The control plane uses one executor thread. Periodic telemetry and durable
// actuator operations run on their dedicated or blocking workers.
#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(error) = run(parse_options(env::args().skip(1))).await {
        eprintln!("uperf-linux: {error:#}");
        std::process::exit(1);
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "startup ordering keeps recovery, configuration, observers and D-Bus acquisition auditable"
)]
async fn run(options: Result<Options>) -> Result<()> {
    let options = options?;
    let started_at = Instant::now();
    let environment = Arc::new(match &options.fixture_root {
        Some(root) => LinuxEnvironment::new(SystemRoots::below(root))
            .with_context(|| format!("open fixture environment {}", root.display()))?,
        None => LinuxEnvironment::host().context("open Linux environment")?,
    });
    let paths = ConfigurationPaths::below(&options.config_dir, &options.state_dir);
    let mutation_setup = if options.read_only {
        None
    } else {
        let boot_id = read_boot_id(environment.roots().proc.as_path())?;
        let fingerprint = device_fingerprint(&environment.discover_device_identity());
        let store = Arc::new(FileStateStore::new(
            options.runtime_dir.join("recovery.json"),
        ));
        let process = match LinuxProcessController::host() {
            Ok(controller) => Some(Arc::new(controller) as Arc<dyn ProcessController>),
            Err(error) => {
                eprintln!("uperf-linux: process scheduling backend unavailable: {error}");
                None
            }
        };
        let systemd = match tokio::task::spawn_blocking(SystemdDbusClient::connect_system)
            .await
            .context("join systemd backend initialization")?
        {
            Ok(systemd) => Some(Arc::new(systemd) as Arc<dyn SystemdClient>),
            Err(error) => {
                eprintln!("uperf-linux: systemd cgroup backend unavailable: {error}");
                None
            }
        };
        let backends = MutationBackends { process, systemd };
        let recovery_failure = recover_before_configuration(
            &environment,
            None,
            store.clone(),
            &boot_id,
            &fingerprint,
            &backends,
        );
        if let Some(reason) = &recovery_failure {
            eprintln!(
                "uperf-linux: pre-configuration recovery failed; mutations remain disabled: {reason}"
            );
        }
        Some(MutationSetup {
            boot_id,
            fingerprint,
            store,
            backends,
            recovery_failure,
        })
    };

    let discovery = environment
        .discover()
        .context("discover Linux capabilities")?;

    // Parsing current configuration is intentionally after crash recovery.
    // Broken/replaced configuration must not strand state owned by a previous
    // daemon from this boot.
    let configuration =
        ResolvedConfiguration::load(&paths, &discovery).context("load configuration generation")?;
    let actuator = if let Some(setup) = mutation_setup {
        let sysfs = Arc::new(
            environment
                .open_actuator_sysfs(configuration.targets.values().map(|target| &target.paths))
                .context("construct sysfs mutation allowlist")?,
        );
        let actuator = FrequencyActuator::new(
            sysfs,
            setup.store,
            configuration.actuator_registry()?,
            setup.boot_id,
            setup.fingerprint,
        );
        let actuator = attach_backends(actuator, &environment, &setup.backends);
        if let Some(reason) = setup.recovery_failure {
            actuator
                .mark_startup_recovery_failed(reason)
                .context("retain recovery failure in final actuator")?;
        }
        Some(Arc::new(actuator))
    } else {
        None
    };

    let (runtime, ingress, state_task) = spawn_runtime(RuntimeParts {
        environment: environment.clone(),
        discovery,
        configuration,
        configuration_paths: paths,
        actuator,
        started_at,
    });
    let observers = spawn_linux_observers(environment, &ingress)
        .map_err(|error| anyhow!("start Linux observers: {error}"))?;
    let input_observer = spawn_input_observer(ingress.clone())
        .map_err(|error| anyhow!("start evdev observer: {error}"))?;

    let connection = if options.session_bus {
        Connection::session()
            .await
            .context("connect to session bus")?
    } else {
        Connection::system()
            .await
            .context("connect to system bus")?
    };
    let authorization = if options.session_bus {
        AuthorizationMode::DevelopmentSession
    } else {
        AuthorizationMode::PolicyKit
    };
    connection
        .object_server()
        .at(
            OBJECT_PATH,
            DaemonService::new(runtime.clone(), Authorizer::new(authorization)),
        )
        .await
        .context("export D-Bus object")?;

    // The name is deliberately acquired only after recovery, configuration,
    // reducer, and observers are ready.
    connection
        .request_name(SERVICE_NAME)
        .await
        .context("acquire D-Bus service name")?;

    let (service_shutdown, service_shutdown_rx) = watch::channel(false);
    let signal_task = tokio::spawn(run_signal_pump(
        connection.clone(),
        runtime.clone(),
        service_shutdown_rx.clone(),
    ));
    let logind_task = (!options.session_bus)
        .then(|| spawn_logind_observer(connection.clone(), ingress, service_shutdown_rx));
    runtime
        .activate()
        .await
        .context("activate runtime mutation control")?;

    let mut shutdown_error = wait_for_shutdown_or_reload(&runtime, &state_task, &signal_task)
        .await
        .err();

    // Close the externally reachable control plane first. The actor barrier
    // also rejects a method call that was already dispatched but had not yet
    // submitted its mutation command.
    if let Err(error) = connection
        .object_server()
        .remove::<DaemonService, _>(OBJECT_PATH)
        .await
    {
        record_shutdown_error(
            &mut shutdown_error,
            anyhow!("unexport D-Bus control object: {error}"),
        );
    }
    if let Err(error) = runtime.begin_shutdown().await {
        record_shutdown_error(
            &mut shutdown_error,
            anyhow!("close runtime control plane: {error}"),
        );
    }

    observers.stop().await;
    if let Some(input_observer) = input_observer {
        input_observer.stop().await;
    }
    service_shutdown.send_replace(true);
    if let Some(task) = logind_task {
        match task.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => record_shutdown_error(
                &mut shutdown_error,
                anyhow!("logind observer stopped: {error}"),
            ),
            Err(error) => {
                record_shutdown_error(&mut shutdown_error, anyhow!("logind task failed: {error}"));
            }
        }
    }
    match signal_task.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => record_shutdown_error(
            &mut shutdown_error,
            anyhow!("run D-Bus signal task: {error}"),
        ),
        Err(error) => record_shutdown_error(
            &mut shutdown_error,
            anyhow!("join D-Bus signal task: {error}"),
        ),
    }

    // The well-known name remains owned until restoration and state-task
    // shutdown have completed, so clients cannot race a replacement daemon
    // into the recovery window.
    if let Err(error) = runtime.stop().await {
        record_shutdown_error(
            &mut shutdown_error,
            anyhow!("restore resources during shutdown: {error}"),
        );
    }
    if let Err(error) = connection.release_name(SERVICE_NAME).await {
        record_shutdown_error(
            &mut shutdown_error,
            anyhow!("release D-Bus service name: {error}"),
        );
    }
    match state_task.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            record_shutdown_error(&mut shutdown_error, anyhow!("run state task: {error}"));
        }
        Err(error) => {
            record_shutdown_error(&mut shutdown_error, anyhow!("join state task: {error}"));
        }
    }
    shutdown_error.map_or(Ok(()), Err)
}

fn recover_before_configuration(
    environment: &Arc<LinuxEnvironment>,
    supplied_discovery: Option<&LinuxDiscovery>,
    store: Arc<dyn StateStore>,
    boot_id: &str,
    fingerprint: &str,
    backends: &MutationBackends,
) -> Option<String> {
    let manifest = match inspect_recovery_journal(store.as_ref()) {
        Ok(Some(manifest)) => manifest,
        Ok(None) => return None,
        Err(error) => return Some(format!("cannot inspect recovery journal: {error}")),
    };

    // Another boot never receives writes. The actuator can discard that stale
    // journal before attempting to resolve paths that may no longer exist.
    let same_identity = manifest.boot_id == boot_id && manifest.device_fingerprint == fingerprint;
    let (registry, write_paths) = if same_identity {
        let discovered;
        let discovery = if let Some(discovery) = supplied_discovery {
            discovery
        } else {
            discovered = if manifest.frequency_targets.is_empty() {
                environment.discover_device_identity()
            } else {
                match environment.discover_recovery_targets() {
                    Ok(discovery) => discovery,
                    Err(error) => {
                        return Some(format!(
                            "cannot discover journal-owned frequency targets: {error}"
                        ));
                    }
                }
            };
            &discovered
        };
        let registry = match recovery_registry(&manifest, discovery) {
            Ok(registry) => registry,
            Err(error) => return Some(format!("cannot resolve recovery manifest: {error:#}")),
        };
        (registry, manifest.frequency_write_paths())
    } else {
        (TargetRegistry::default(), Vec::new())
    };
    let sysfs = match environment.open_recovery_sysfs(&write_paths) {
        Ok(sysfs) => Arc::new(sysfs),
        Err(error) => {
            return Some(format!(
                "cannot construct recovery sysfs allowlist: {error}"
            ));
        }
    };
    let actuator = FrequencyActuator::new(
        sysfs,
        store,
        registry,
        boot_id.to_owned(),
        fingerprint.to_owned(),
    );
    let actuator = attach_backends(actuator, environment, backends);
    actuator
        .recover_pending()
        .err()
        .map(|error| error.to_string())
}

fn attach_backends(
    mut actuator: FrequencyActuator,
    environment: &Arc<LinuxEnvironment>,
    backends: &MutationBackends,
) -> FrequencyActuator {
    if let Some(process) = &backends.process {
        actuator = actuator.with_process_backend(environment.clone(), process.clone());
    }
    if let Some(systemd) = &backends.systemd {
        actuator = actuator.with_systemd_backend(systemd.clone());
    }
    actuator
}

fn recovery_registry(
    manifest: &RecoveryManifest,
    discovery: &LinuxDiscovery,
) -> Result<TargetRegistry> {
    let targets = manifest
        .frequency_targets
        .iter()
        .map(|target| match target {
            RecoveryFrequencyTarget::SelfDescribing(target) => {
                validate_self_describing_target(target, discovery)?;
                target.to_frequency_target().map_err(anyhow::Error::from)
            }
            RecoveryFrequencyTarget::Legacy(target) => {
                let live = live_target_for_paths(discovery, &target.min_path, &target.max_path)?;
                FrequencyTarget::new(
                    target.id.clone(),
                    target.min_path.clone(),
                    target.max_path.clone(),
                    live.limits.min,
                    live.limits.max,
                    live.opps,
                )
                .and_then(|target| target.with_hertz_per_unit(live.hertz_per_unit))
                .map_err(anyhow::Error::from)
            }
        })
        .collect::<Result<Vec<_>>>()?;
    TargetRegistry::new(targets).map_err(anyhow::Error::from)
}

fn validate_self_describing_target(
    target: &RecoveryFrequencyTargetManifest,
    discovery: &LinuxDiscovery,
) -> Result<()> {
    let live = live_target_for_paths(discovery, &target.min_path, &target.max_path)?;
    if target.hardware_min != live.limits.min
        || target.hardware_max != live.limits.max
        || target.opps != live.opps
        || target.hertz_per_unit != live.hertz_per_unit
    {
        bail!(
            "{} recovery manifest no longer matches live hardware identity",
            target.id
        );
    }
    Ok(())
}

struct LiveFrequencyTarget {
    limits: FrequencyLimits,
    opps: Vec<Hertz>,
    hertz_per_unit: u64,
}

fn live_target_for_paths(
    discovery: &LinuxDiscovery,
    minimum: &Path,
    maximum: &Path,
) -> Result<LiveFrequencyTarget> {
    let matching = discovery
        .frequency_targets
        .values()
        .filter(|paths| paths.minimum == minimum && paths.maximum == maximum)
        .collect::<Vec<_>>();
    let [paths] = matching.as_slice() else {
        bail!(
            "recovery paths {} and {} do not identify exactly one discovered target",
            minimum.display(),
            maximum.display()
        );
    };
    let capability = discovery
        .capabilities
        .cpu_policies
        .iter()
        .find(|capability| capability.id == paths.id)
        .map(|capability| (capability.limits, capability.available_frequencies.clone()))
        .or_else(|| {
            discovery
                .capabilities
                .devfreq_targets
                .iter()
                .find(|capability| capability.id == paths.id)
                .map(|capability| (capability.limits, capability.available_frequencies.clone()))
        })
        .ok_or_else(|| {
            anyhow!(
                "discovered recovery paths have no matching capability {}",
                paths.id
            )
        })?;
    Ok(LiveFrequencyTarget {
        limits: capability.0,
        opps: capability.1,
        hertz_per_unit: paths.hertz_per_unit,
    })
}

async fn wait_for_shutdown_or_reload(
    runtime: &uperf_daemon::runtime::RuntimeHandle,
    state_task: &tokio::task::JoinHandle<Result<(), uperf_daemon::runtime::RuntimeError>>,
    signal_task: &tokio::task::JoinHandle<zbus::Result<()>>,
) -> Result<()> {
    let mut terminate = signal(SignalKind::terminate()).context("listen for SIGTERM")?;
    let mut interrupt = signal(SignalKind::interrupt()).context("listen for SIGINT")?;
    let mut hangup = signal(SignalKind::hangup()).context("listen for SIGHUP")?;
    let mut supervision = tokio::time::interval(std::time::Duration::from_millis(250));
    supervision.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = terminate.recv() => return Ok(()),
            _ = interrupt.recv() => return Ok(()),
            _ = hangup.recv() => {
                match runtime.reload().await {
                    Ok(report) => {
                        eprintln!(
                            "uperf-linux: reloaded configuration generation {}",
                            report.config_generation
                        );
                    }
                    Err(error) => {
                        eprintln!("uperf-linux: reload rejected; old generation retained: {error}");
                    }
                }
            }
            _ = supervision.tick() => {
                if state_task.is_finished() {
                    bail!("state task stopped unexpectedly");
                }
                if signal_task.is_finished() {
                    bail!("D-Bus signal task stopped unexpectedly");
                }
            }
        }
    }
}

fn record_shutdown_error(first: &mut Option<anyhow::Error>, error: anyhow::Error) {
    if first.is_none() {
        *first = Some(error);
    } else {
        eprintln!("uperf-linux: additional shutdown error: {error:#}");
    }
}

fn read_boot_id(proc_root: &Path) -> Result<String> {
    let path = proc_root.join("sys/kernel/random/boot_id");
    let value =
        fs::read_to_string(&path).with_context(|| format!("read boot ID {}", path.display()))?;
    let value = value.trim();
    if value.is_empty() {
        bail!("kernel boot ID is empty; refusing to enable mutation");
    }
    Ok(value.to_owned())
}

fn device_fingerprint(discovery: &LinuxDiscovery) -> String {
    let mut digest = Sha256::new();
    digest.update(b"uperf-linux-device-fingerprint-v2\0");
    if let Some(name) = &discovery.capabilities.device_name {
        digest.update(name.as_bytes());
        digest.update([0]);
    }
    for compatible in &discovery.capabilities.compatible {
        digest.update(compatible.as_bytes());
        digest.update([0]);
    }
    format!("{:x}", digest.finalize())
}

fn parse_options(arguments: impl IntoIterator<Item = String>) -> Result<Options> {
    let mut options = Options::default();
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--config-dir" => {
                options.config_dir = next_path(&mut arguments, "--config-dir")?;
            }
            "--state-dir" => {
                options.state_dir = next_path(&mut arguments, "--state-dir")?;
            }
            "--runtime-dir" => {
                options.runtime_dir = next_path(&mut arguments, "--runtime-dir")?;
            }
            "--fixture-root" => {
                options.fixture_root = Some(next_path(&mut arguments, "--fixture-root")?);
            }
            "--session" => options.session_bus = true,
            "--read-only" => options.read_only = true,
            "--version" => {
                println!("uperf-linux {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--help" | "-h" => {
                println!(
                    "Usage: uperf-linux [--config-dir PATH] [--state-dir PATH] \\\n+                     [--runtime-dir PATH] [--read-only] [--session] [--fixture-root PATH]"
                );
                std::process::exit(0);
            }
            unknown => bail!("unknown option {unknown}"),
        }
    }
    if options.fixture_root.is_some() && !options.read_only {
        bail!("--fixture-root requires --read-only");
    }
    if options.session_bus && !options.read_only {
        bail!(
            "--session requires --read-only; mutation is exposed only on the protected system bus"
        );
    }
    Ok(options)
}

fn next_path(arguments: &mut impl Iterator<Item = String>, option: &str) -> Result<PathBuf> {
    arguments
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("{option} requires a path"))
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs, sync::Arc};

    use tempfile::TempDir;
    use uperf_actuator::{ActuatorMode, FrequencyRequest};
    use uperf_core::{
        CpuId, CpuPolicyCapability, CpuSet, DeviceCapabilities, InputDeviceCapability,
        MilliCelsius, MonotonicMillis, SensorHealth, TargetId, ThermalReading,
        ThermalZoneCapability,
    };
    use uperf_linux::FrequencyTargetPaths;

    use super::*;

    struct RecoveryFixture {
        temporary: TempDir,
        environment: Arc<LinuxEnvironment>,
        discovery: LinuxDiscovery,
        store: Arc<FileStateStore>,
        target: FrequencyTarget,
        physical_minimum: PathBuf,
    }

    #[test]
    fn development_session_bus_is_always_read_only() {
        assert!(parse_options(["--session".to_owned()]).is_err());
        let options = parse_options(["--session".to_owned(), "--read-only".to_owned()])
            .expect("read-only development session");
        assert!(options.session_bus);
        assert!(options.read_only);
    }

    fn recovery_fixture() -> RecoveryFixture {
        let temporary = tempfile::tempdir().expect("temporary root");
        for directory in [
            "sys/devices/system/cpu/cpufreq/policy0",
            "proc",
            "etc",
            "run",
        ] {
            fs::create_dir_all(temporary.path().join(directory)).expect("fixture directory");
        }
        let policy_directory = temporary
            .path()
            .join("sys/devices/system/cpu/cpufreq/policy0");
        let physical_minimum = policy_directory.join("scaling_min_freq");
        let physical_maximum = policy_directory.join("scaling_max_freq");
        let physical_current = policy_directory.join("scaling_cur_freq");
        fs::write(&physical_minimum, "1000\n").expect("minimum");
        fs::write(&physical_maximum, "3000\n").expect("maximum");
        fs::write(&physical_current, "1000\n").expect("current");
        fs::write(policy_directory.join("related_cpus"), "0\n").expect("related CPUs");
        fs::write(policy_directory.join("cpuinfo_min_freq"), "1000\n").expect("hardware minimum");
        fs::write(policy_directory.join("cpuinfo_max_freq"), "3000\n").expect("hardware maximum");
        fs::write(
            policy_directory.join("scaling_available_frequencies"),
            "1000 2000 3000\n",
        )
        .expect("operating points");
        fs::write(policy_directory.join("scaling_governor"), "schedutil\n").expect("governor");
        let environment = Arc::new(
            LinuxEnvironment::new(SystemRoots::below(temporary.path())).expect("environment"),
        );
        let discovered_id = TargetId::new("cpu.discovery").expect("discovery ID");
        let logical_id = TargetId::new("cpu.old-config").expect("logical ID");
        let limits =
            FrequencyLimits::new(Hertz::new(1_000_000), Hertz::new(3_000_000)).expect("limits");
        let logical_directory = PathBuf::from("/sys/devices/system/cpu/cpufreq/policy0");
        let logical_minimum = logical_directory.join("scaling_min_freq");
        let logical_maximum = logical_directory.join("scaling_max_freq");
        let discovery = LinuxDiscovery {
            capabilities: DeviceCapabilities {
                device_name: Some("test-board".to_owned()),
                compatible: vec!["vendor,test-board".to_owned()],
                matched_profile: None,
                cpu_policies: vec![CpuPolicyCapability {
                    id: discovered_id.clone(),
                    policy_name: "policy-test".to_owned(),
                    cpus: CpuSet::from_ids([CpuId::new(0)]),
                    limits,
                    available_frequencies: vec![
                        Hertz::new(1_000_000),
                        Hertz::new(2_000_000),
                        Hertz::new(3_000_000),
                    ],
                    governor: Some("schedutil".to_owned()),
                }],
                devfreq_targets: Vec::new(),
                thermal_zones: Vec::new(),
                input_devices: Vec::new(),
            },
            frequency_targets: BTreeMap::from([(
                discovered_id.clone(),
                FrequencyTargetPaths {
                    id: discovered_id,
                    minimum: logical_minimum.clone(),
                    maximum: logical_maximum.clone(),
                    current: logical_directory.join("scaling_cur_freq"),
                    hertz_per_unit: 1_000,
                },
            )]),
            thermal_zone_paths: BTreeMap::new(),
            warnings: Vec::new(),
        };
        let target = FrequencyTarget::new(
            logical_id,
            logical_minimum,
            logical_maximum,
            limits.min,
            limits.max,
            vec![
                Hertz::new(1_000_000),
                Hertz::new(2_000_000),
                Hertz::new(3_000_000),
            ],
        )
        .and_then(|target| target.with_hertz_per_unit(1_000))
        .expect("frequency target");
        let store = Arc::new(FileStateStore::new(
            temporary.path().join("run/recovery.json"),
        ));
        RecoveryFixture {
            temporary,
            environment,
            discovery,
            store,
            target,
            physical_minimum,
        }
    }

    #[test]
    fn recovery_fingerprint_ignores_live_and_unrelated_target_state() {
        let mut discovery = LinuxDiscovery {
            capabilities: DeviceCapabilities {
                device_name: Some("test-board".to_owned()),
                compatible: vec!["vendor,test-board".to_owned()],
                matched_profile: Some("test".to_owned()),
                cpu_policies: Vec::new(),
                devfreq_targets: Vec::new(),
                thermal_zones: vec![ThermalZoneCapability {
                    id: "thermal_zone0".to_owned(),
                    zone_type: "soc".to_owned(),
                    current: Some(ThermalReading {
                        temperature: Some(MilliCelsius(40_000)),
                        sampled_at: MonotonicMillis(10),
                        health: SensorHealth::Healthy,
                    }),
                }],
                input_devices: Vec::new(),
            },
            frequency_targets: BTreeMap::new(),
            thermal_zone_paths: BTreeMap::from([(
                "thermal_zone0".to_owned(),
                PathBuf::from("/sys/class/thermal/thermal_zone0"),
            )]),
            warnings: Vec::new(),
        };
        let before = device_fingerprint(&discovery);
        discovery.capabilities.thermal_zones[0].current = Some(ThermalReading {
            temperature: Some(MilliCelsius(75_000)),
            sampled_at: MonotonicMillis(999),
            health: SensorHealth::Stale,
        });
        discovery
            .capabilities
            .input_devices
            .push(InputDeviceCapability {
                id: "event9".to_owned(),
                name: "hotplug touch".to_owned(),
                multi_touch: true,
            });
        let unrelated = TargetId::new("devfreq.hotplug").expect("target ID");
        discovery
            .capabilities
            .devfreq_targets
            .push(uperf_core::DevfreqCapability {
                id: unrelated.clone(),
                device_name: "hotplug-device".to_owned(),
                compatible: vec!["vendor,hotplug".to_owned()],
                limits: FrequencyLimits {
                    min: Hertz::new(1_001),
                    max: Hertz::new(9_999),
                },
                available_frequencies: Vec::new(),
                governor: None,
            });
        discovery.frequency_targets.insert(
            unrelated.clone(),
            FrequencyTargetPaths {
                id: unrelated,
                minimum: PathBuf::from("/sys/class/devfreq/hotplug/min_freq"),
                maximum: PathBuf::from("/sys/class/devfreq/hotplug/max_freq"),
                current: PathBuf::from("/sys/class/devfreq/hotplug/cur_freq"),
                hertz_per_unit: 1,
            },
        );

        assert_eq!(before, device_fingerprint(&discovery));
    }

    #[test]
    fn recovery_finishes_before_missing_configuration_is_loaded() {
        let fixture = recovery_fixture();
        let writer = Arc::new(
            fixture
                .environment
                .open_recovery_sysfs(&[
                    fixture.target.min_path.clone(),
                    fixture.target.max_path.clone(),
                ])
                .expect("writer"),
        );
        let target_id = fixture.target.id.clone();
        FrequencyActuator::new(
            writer,
            fixture.store.clone(),
            TargetRegistry::new([fixture.target.clone()]).expect("registry"),
            "boot-a",
            device_fingerprint(&fixture.discovery),
        )
        .apply_batch(&[FrequencyRequest {
            target: target_id,
            limits: FrequencyLimits::new(Hertz::new(2_000_000), Hertz::new(3_000_000))
                .expect("limits"),
        }])
        .expect("apply before simulated crash");
        assert_eq!(
            fs::read_to_string(&fixture.physical_minimum).expect("applied minimum"),
            "2000"
        );

        let recovery_failure = recover_before_configuration(
            &fixture.environment,
            None,
            fixture.store.clone(),
            "boot-a",
            &device_fingerprint(&fixture.discovery),
            &MutationBackends::default(),
        );
        assert_eq!(recovery_failure, None);
        assert_eq!(
            fs::read_to_string(&fixture.physical_minimum).expect("restored minimum"),
            "1000"
        );
        assert!(fixture.store.load().expect("journal state").is_none());

        let missing_paths = ConfigurationPaths::below(
            fixture.temporary.path().join("missing-config"),
            fixture.temporary.path().join("missing-state"),
        );
        assert!(ResolvedConfiguration::load(&missing_paths, &fixture.discovery).is_err());
    }

    #[test]
    fn corrupt_journal_keeps_the_final_actuator_degraded() {
        let fixture = recovery_fixture();
        fs::write(fixture.store.path(), b"corrupt").expect("corrupt journal");
        let failure = recover_before_configuration(
            &fixture.environment,
            Some(&fixture.discovery),
            fixture.store.clone(),
            "boot-a",
            &device_fingerprint(&fixture.discovery),
            &MutationBackends::default(),
        )
        .expect("recovery failure");
        let writer = Arc::new(
            fixture
                .environment
                .open_actuator_sysfs(fixture.discovery.frequency_targets.values())
                .expect("normal writer"),
        );
        let actuator = FrequencyActuator::new(
            writer,
            fixture.store,
            TargetRegistry::new([fixture.target]).expect("registry"),
            "boot-a",
            device_fingerprint(&fixture.discovery),
        );
        actuator
            .mark_startup_recovery_failed(failure)
            .expect("retain degraded state");

        assert!(matches!(
            actuator.mode().expect("mode"),
            ActuatorMode::ReadOnlyDegraded { .. }
        ));
        assert!(actuator.startup_recovery_failed().expect("recovery status"));
    }

    #[test]
    fn recovery_rejects_a_target_missing_from_live_discovery() {
        let mut fixture = recovery_fixture();
        let writer = Arc::new(
            fixture
                .environment
                .open_recovery_sysfs(&[
                    fixture.target.min_path.clone(),
                    fixture.target.max_path.clone(),
                ])
                .expect("writer"),
        );
        let target_id = fixture.target.id.clone();
        let fingerprint = device_fingerprint(&fixture.discovery);
        FrequencyActuator::new(
            writer,
            fixture.store.clone(),
            TargetRegistry::new([fixture.target.clone()]).expect("registry"),
            "boot-a",
            fingerprint.clone(),
        )
        .apply_batch(&[FrequencyRequest {
            target: target_id,
            limits: FrequencyLimits::new(Hertz::new(2_000_000), Hertz::new(3_000_000))
                .expect("limits"),
        }])
        .expect("apply before simulated crash");
        fixture.discovery.frequency_targets.clear();

        let failure = recover_before_configuration(
            &fixture.environment,
            Some(&fixture.discovery),
            fixture.store.clone(),
            "boot-a",
            &fingerprint,
            &MutationBackends::default(),
        )
        .expect("target mismatch must fail closed");

        assert!(failure.contains("do not identify exactly one discovered target"));
        assert_eq!(
            fs::read_to_string(&fixture.physical_minimum).expect("minimum"),
            "2000"
        );
        assert!(fixture.store.load().expect("journal state").is_some());
    }
}

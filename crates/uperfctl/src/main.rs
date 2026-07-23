mod args;
mod config;

use std::{process::ExitCode, time::Duration};

use anyhow::{Context, Result, bail};
use args::{Bus, Cli, Command, ConfigAction, FrequencyAction, ModeAction, WorkloadAction};
use serde_json::json;
use tokio::time::timeout;
use uperf_api::{
    Capabilities, DaemonClient, DaemonStatus, DiagnosticReport, FrequencyOverride, FrequencyStatus,
    HealthStatus, MutationReceipt, ReloadReport, TargetCapability, WorkloadRequest,
};

const EXIT_UNHEALTHY: u8 = 2;
const EXIT_UNAVAILABLE: u8 = 3;
const EXIT_NOT_AUTHORIZED: u8 = 4;
const EXIT_CONFLICT: u8 = 5;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("uperfctl: {error:#}");
            ExitCode::from(classify_error(&error))
        }
    }
}

async fn run() -> Result<ExitCode> {
    let cli = Cli::parse(std::env::args().skip(1))?;

    match &cli.command {
        Command::Help(topic) => {
            print!("{}", args::help(topic.as_deref()));
            return Ok(ExitCode::SUCCESS);
        }
        Command::Version => {
            println!(
                "uperfctl {} (D-Bus API {})",
                env!("CARGO_PKG_VERSION"),
                uperf_api::ApiVersion::CURRENT
            );
            return Ok(ExitCode::SUCCESS);
        }
        Command::Config(action) => return run_config(action, cli.json),
        _ => {}
    }

    let client = match cli.bus {
        Bus::System => DaemonClient::system().await,
        Bus::Session => DaemonClient::session().await,
    }
    .context("connect to uperf-linux D-Bus service")?;

    let result = timeout(
        Duration::from_millis(cli.timeout_ms),
        run_remote(&client, &cli.command, cli.json),
    )
    .await
    .with_context(|| format!("D-Bus operation timed out after {} ms", cli.timeout_ms))??;

    Ok(if result {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(EXIT_UNHEALTHY)
    })
}

async fn run_remote(client: &DaemonClient, command: &Command, json_output: bool) -> Result<bool> {
    match command {
        Command::Status => {
            let status = client.status().await?;
            print_status(&status, json_output)?;
            Ok(true)
        }
        Command::Health => {
            let health = client.status().await?.health;
            print_health(&health, json_output)?;
            Ok(health.state == "healthy")
        }
        Command::Mode(action) => run_mode(client, action, json_output).await,
        Command::Workload(action) => run_workload(client, action, json_output).await,
        Command::Targets(target_id) => {
            let capabilities = client.capabilities().await?;
            print_targets(&capabilities, target_id.as_deref(), json_output)?;
            Ok(true)
        }
        Command::Frequency(action) => run_frequency(client, action, json_output).await,
        Command::Reload => {
            let report = client.reload_config().await?;
            print_reload(&report, json_output)?;
            Ok(true)
        }
        Command::Diagnose => {
            let report = client.diagnose().await?;
            print_diagnostics(&report, json_output)?;
            Ok(report.healthy)
        }
        Command::Config(_) | Command::Help(_) | Command::Version => {
            unreachable!("non-remote commands are handled before connecting")
        }
    }
}

async fn run_mode(client: &DaemonClient, action: &ModeAction, json_output: bool) -> Result<bool> {
    match action {
        ModeAction::Show => {
            let status = client.status().await?;
            if json_output {
                print_json(&json!({
                    "mode": status.mode,
                    "effective_profile": status.effective_profile,
                    "dominant_scene": status.dominant_scene,
                }))?;
            } else {
                println!(
                    "{} (effective: {}, scene: {})",
                    status.mode, status.effective_profile, status.dominant_scene
                );
            }
        }
        ModeAction::List => {
            let capabilities = client.capabilities().await?;
            if json_output {
                print_json(&capabilities.modes)?;
            } else {
                for mode in capabilities.modes {
                    println!(
                        "{:<16} {:<20} {}",
                        mode.id, mode.display_name, mode.description
                    );
                }
            }
        }
        ModeAction::Set(mode) => {
            let capabilities = client.capabilities().await?;
            require_mode(&capabilities, mode)?;
            let receipt = client.set_mode(mode).await?;
            print_receipt(&receipt, json_output)?;
        }
    }
    Ok(true)
}

async fn run_workload(
    client: &DaemonClient,
    action: &WorkloadAction,
    json_output: bool,
) -> Result<bool> {
    match action {
        WorkloadAction::Show => {
            let workload = client.active_workload().await?;
            if json_output {
                print_json(&workload)?;
            } else if workload.present {
                println!(
                    "pid={} start={} uid={} name={} requested={} effective={} source={}",
                    workload.identity.pid,
                    workload.identity.start_time_ticks,
                    workload.identity.uid,
                    workload.name,
                    display_or_dash(&workload.requested_mode),
                    display_or_dash(&workload.effective_mode),
                    display_or_dash(&workload.source)
                );
            } else {
                println!("no active workload");
            }
        }
        WorkloadAction::Set { pid, mode, reason } => {
            if let Some(mode) = mode {
                let capabilities = client.capabilities().await?;
                require_mode(&capabilities, mode)?;
            }
            let receipt = client
                .set_active_workload(WorkloadRequest {
                    pid: *pid,
                    mode: mode.clone().unwrap_or_default(),
                    reason: reason.clone(),
                })
                .await?;
            print_receipt(&receipt, json_output)?;
        }
        WorkloadAction::Clear => {
            let receipt = client.clear_active_workload().await?;
            print_receipt(&receipt, json_output)?;
        }
    }
    Ok(true)
}

async fn run_frequency(
    client: &DaemonClient,
    action: &FrequencyAction,
    json_output: bool,
) -> Result<bool> {
    match action {
        FrequencyAction::Show(target_id) => {
            let status = client.status().await?;
            print_frequencies(&status.frequencies, target_id.as_deref(), json_output)?;
        }
        FrequencyAction::Set {
            target_id,
            minimum,
            maximum,
            ttl,
            reason,
        } => {
            let capabilities = client.capabilities().await?;
            let target = require_target(&capabilities, target_id)?;
            if !target.can_override {
                bail!("target '{target_id}' does not support frequency overrides");
            }

            let minimum_hz = parse_frequency_hz(minimum)?;
            let maximum_hz = parse_frequency_hz(maximum)?;
            if minimum_hz > maximum_hz {
                bail!("minimum frequency exceeds maximum frequency");
            }
            if minimum_hz < target.minimum_hz || maximum_hz > target.maximum_hz {
                bail!(
                    "requested {}..{} Hz is outside target '{}' range {}..{} Hz",
                    minimum_hz,
                    maximum_hz,
                    target_id,
                    target.minimum_hz,
                    target.maximum_hz
                );
            }
            let ttl_ms = ttl
                .as_deref()
                .map(parse_duration_ms)
                .transpose()?
                .unwrap_or(0);
            let receipt = client
                .set_frequency_overrides(vec![FrequencyOverride {
                    target_id: target_id.clone(),
                    min_hz: minimum_hz,
                    max_hz: maximum_hz,
                    ttl_ms,
                    reason: reason.clone(),
                }])
                .await?;
            print_receipt(&receipt, json_output)?;
        }
        FrequencyAction::Clear(target_ids) => {
            if !target_ids.is_empty() {
                let capabilities = client.capabilities().await?;
                for target_id in target_ids {
                    require_target(&capabilities, target_id)?;
                }
            }
            let receipt = client.clear_frequency_overrides(target_ids.clone()).await?;
            print_receipt(&receipt, json_output)?;
        }
    }
    Ok(true)
}

fn run_config(action: &ConfigAction, json_output: bool) -> Result<ExitCode> {
    match action {
        ConfigAction::Validate(path) => {
            let report = config::validate_path(path)?;
            if json_output {
                print_json(&report.as_json(path))?;
            } else {
                report.print_human(path);
            }
            Ok(if report.valid() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(EXIT_UNHEALTHY)
            })
        }
        ConfigAction::Migrate {
            input,
            output_dir,
            force,
        } => {
            let migration = config::migrate_path(input)?;
            let outputs = config::write_migration(output_dir, &migration, *force)?;
            if json_output {
                print_json(&json!({
                    "input": input,
                    "output_directory": output_dir,
                    "device": outputs.device,
                    "policy": outputs.policy,
                    "apps": outputs.apps,
                    "schema_version": uperf_core::CONFIG_SCHEMA_VERSION,
                    "warnings": migration.warnings,
                }))?;
            } else {
                println!("migrated {} -> {}", input.display(), output_dir.display());
                println!("  {}", outputs.device.display());
                println!("  {}", outputs.policy.display());
                println!("  {}", outputs.apps.display());
                for warning in &migration.warnings {
                    eprintln!("warning: {}: {}", warning.path, warning.message);
                }
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn require_mode(capabilities: &Capabilities, mode: &str) -> Result<()> {
    if capabilities
        .modes
        .iter()
        .any(|candidate| candidate.id == mode)
    {
        Ok(())
    } else {
        let supported = capabilities
            .modes
            .iter()
            .map(|candidate| candidate.id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        bail!("unsupported mode '{mode}'; daemon advertises: {supported}")
    }
}

fn require_target<'a>(
    capabilities: &'a Capabilities,
    target_id: &str,
) -> Result<&'a TargetCapability> {
    capabilities
        .targets
        .iter()
        .find(|target| target.id == target_id)
        .with_context(|| format!("daemon did not advertise target '{target_id}'"))
}

fn print_status(status: &DaemonStatus, json_output: bool) -> Result<()> {
    if json_output {
        return print_json(status);
    }
    println!(
        "daemon: {} {} (API {})",
        status.state, status.daemon_version, status.api_version
    );
    println!(
        "health: {}{} — {}",
        status.health.state,
        if status.health.read_only {
            " (read-only)"
        } else {
            ""
        },
        status.health.summary
    );
    println!(
        "mode: {} (effective: {}, scene: {})",
        status.mode, status.effective_profile, status.dominant_scene
    );
    if status.active_workload.present {
        println!(
            "workload: {} (pid {}, start {})",
            status.active_workload.name,
            status.active_workload.identity.pid,
            status.active_workload.identity.start_time_ticks
        );
    } else {
        println!("workload: none");
    }
    println!(
        "thermal: {} ({}.{:03} °C{})",
        status.thermal.state,
        status.thermal.max_temperature_millicelsius / 1000,
        status.thermal.max_temperature_millicelsius.unsigned_abs() % 1000,
        if status.thermal.sensors_stale {
            ", stale"
        } else {
            ""
        }
    );
    println!(
        "generation: config={} reconciled={}",
        status.config_generation, status.reconcile_generation
    );
    for frequency in &status.frequencies {
        print_frequency(frequency);
    }
    Ok(())
}

fn print_health(health: &HealthStatus, json_output: bool) -> Result<()> {
    if json_output {
        return print_json(health);
    }
    println!("{}: {}", health.state, health.summary);
    println!(
        "read-only={} recovery-pending={}",
        health.read_only, health.recovery_pending
    );
    for issue in &health.issues {
        println!(
            "[{}] {} {}: {}",
            issue.severity, issue.component, issue.code, issue.message
        );
    }
    Ok(())
}

fn print_targets(
    capabilities: &Capabilities,
    selected: Option<&str>,
    json_output: bool,
) -> Result<()> {
    let targets = capabilities
        .targets
        .iter()
        .filter(|target| selected.is_none_or(|id| target.id == id))
        .collect::<Vec<_>>();
    if selected.is_some() && targets.is_empty() {
        bail!(
            "daemon did not advertise target '{}'",
            selected.unwrap_or_default()
        );
    }
    if json_output {
        return print_json(&targets);
    }
    for target in targets {
        let cpus = if target.cpus.is_empty() {
            "-".into()
        } else {
            target
                .cpus
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",")
        };
        println!(
            "{}\n  kind={} label={} cpus={} range={}..{} Hz override={}",
            target.id,
            target.kind,
            target.label,
            cpus,
            target.minimum_hz,
            target.maximum_hz,
            target.can_override
        );
    }
    Ok(())
}

fn print_frequencies(
    frequencies: &[FrequencyStatus],
    selected: Option<&str>,
    json_output: bool,
) -> Result<()> {
    let values = frequencies
        .iter()
        .filter(|frequency| selected.is_none_or(|id| frequency.target_id == id))
        .collect::<Vec<_>>();
    if selected.is_some() && values.is_empty() {
        bail!(
            "no frequency state for target '{}'",
            selected.unwrap_or_default()
        );
    }
    if json_output {
        return print_json(&values);
    }
    for value in values {
        print_frequency(value);
    }
    Ok(())
}

fn print_frequency(value: &FrequencyStatus) {
    let observed = frequency_range(
        value.observed_available,
        value.observed_min_hz,
        value.observed_max_hz,
    );
    let desired = frequency_range(
        value.desired_available,
        value.desired_min_hz,
        value.desired_max_hz,
    );
    let applied = frequency_range(
        value.applied_verified,
        value.applied_min_hz,
        value.applied_max_hz,
    );
    println!(
        "{}: observed={} desired={} applied={}{}{}",
        value.target_id,
        observed,
        desired,
        applied,
        if value.override_active {
            " override"
        } else {
            ""
        },
        if value.stale { " stale" } else { "" }
    );
}

fn frequency_range(available: bool, minimum_hz: u64, maximum_hz: u64) -> String {
    if available {
        format!("{minimum_hz}..{maximum_hz} Hz")
    } else {
        "unavailable".to_owned()
    }
}

fn print_receipt(receipt: &MutationReceipt, json_output: bool) -> Result<()> {
    if json_output {
        print_json(receipt)
    } else {
        println!(
            "{} (generation {}; changed: {})",
            receipt.message,
            receipt.generation,
            if receipt.changed_ids.is_empty() {
                "none".into()
            } else {
                receipt.changed_ids.join(", ")
            }
        );
        Ok(())
    }
}

fn print_reload(report: &ReloadReport, json_output: bool) -> Result<()> {
    if json_output {
        return print_json(report);
    }
    println!(
        "{} (config generation {})",
        report.message, report.config_generation
    );
    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }
    Ok(())
}

fn print_diagnostics(report: &DiagnosticReport, json_output: bool) -> Result<()> {
    if json_output {
        return print_json(report);
    }
    for check in &report.checks {
        println!(
            "{} {:<24} {}",
            if check.passed { "PASS" } else { "FAIL" },
            check.id,
            check.message
        );
    }
    println!(
        "overall: {}",
        if report.healthy {
            "healthy"
        } else {
            "unhealthy"
        }
    );
    Ok(())
}

fn print_json(value: &impl serde::Serialize) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).context("serialize JSON output")?
    );
    Ok(())
}

fn parse_frequency_hz(input: &str) -> Result<u64> {
    let normalized = input.trim().to_ascii_lowercase();
    let (number, multiplier): (&str, u64) = if let Some(value) = normalized.strip_suffix("ghz") {
        (value, 1_000_000_000)
    } else if let Some(value) = normalized.strip_suffix("mhz") {
        (value, 1_000_000)
    } else if let Some(value) = normalized.strip_suffix("khz") {
        (value, 1_000)
    } else if let Some(value) = normalized.strip_suffix("hz") {
        (value, 1)
    } else {
        (normalized.as_str(), 1)
    };
    parse_decimal_scaled(number, multiplier).with_context(|| format!("invalid frequency '{input}'"))
}

fn parse_duration_ms(input: &str) -> Result<u64> {
    let normalized = input.trim().to_ascii_lowercase();
    let (number, multiplier): (&str, u64) = if let Some(value) = normalized.strip_suffix("ms") {
        (value, 1)
    } else if let Some(value) = normalized.strip_suffix('s') {
        (value, 1_000)
    } else if let Some(value) = normalized.strip_suffix('m') {
        (value, 60_000)
    } else if let Some(value) = normalized.strip_suffix('h') {
        (value, 3_600_000)
    } else {
        (normalized.as_str(), 1)
    };
    parse_decimal_scaled(number, multiplier).with_context(|| format!("invalid duration '{input}'"))
}

fn parse_decimal_scaled(number: &str, multiplier: u64) -> Result<u64> {
    if number.is_empty() || number.starts_with('-') || number.starts_with('+') {
        bail!("expected a non-negative decimal number");
    }
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        bail!("expected a decimal number");
    }
    if fraction.len() > 6 {
        bail!("more than six fractional digits are not supported");
    }
    let whole = whole
        .parse::<u64>()
        .context("decimal number is too large")?;
    let scale = 10_u64
        .checked_pow(u32::try_from(fraction.len()).expect("fraction length is bounded"))
        .context("decimal scale overflow")?;
    let fraction = if fraction.is_empty() {
        0
    } else {
        fraction
            .parse::<u64>()
            .context("fractional number is too large")?
    };
    let numerator = whole
        .checked_mul(scale)
        .and_then(|value| value.checked_add(fraction))
        .context("decimal number is too large")?;
    let scaled = numerator
        .checked_mul(multiplier)
        .context("scaled number is too large")?;
    if scaled % scale != 0 {
        bail!("value does not resolve to a whole base unit");
    }
    Ok(scaled / scale)
}

fn display_or_dash(value: &str) -> &str {
    if value.is_empty() { "-" } else { value }
}

fn classify_error(error: &anyhow::Error) -> u8 {
    if error
        .downcast_ref::<tokio::time::error::Elapsed>()
        .is_some()
    {
        return EXIT_UNAVAILABLE;
    }
    let Some(client_error) = error.downcast_ref::<uperf_api::ClientError>() else {
        return 1;
    };
    match client_error {
        uperf_api::ClientError::Transport(_) | uperf_api::ClientError::IncompatibleApi { .. } => {
            EXIT_UNAVAILABLE
        }
        uperf_api::ClientError::Remote { name, .. } if name.ends_with(".NotAuthorized") => {
            EXIT_NOT_AUTHORIZED
        }
        uperf_api::ClientError::Remote { name, .. } if name.ends_with(".Conflict") => EXIT_CONFLICT,
        uperf_api::ClientError::Remote { name, .. }
            if name.ends_with(".Unavailable") || name.ends_with(".Degraded") =>
        {
            EXIT_UNAVAILABLE
        }
        uperf_api::ClientError::Remote { .. } | uperf_api::ClientError::InvalidRequest(_) => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        EXIT_CONFLICT, EXIT_NOT_AUTHORIZED, classify_error, parse_duration_ms, parse_frequency_hz,
    };

    #[test]
    fn parses_frequency_units_exactly() {
        assert_eq!(parse_frequency_hz("1001").unwrap(), 1_001);
        assert_eq!(parse_frequency_hz("1001Hz").unwrap(), 1_001);
        assert_eq!(parse_frequency_hz("400MHz").unwrap(), 400_000_000);
        assert_eq!(parse_frequency_hz("1.8GHz").unwrap(), 1_800_000_000);
        assert_eq!(parse_frequency_hz("0.001kHz").unwrap(), 1);
        assert!(parse_frequency_hz("1.0001kHz").is_err());
    }

    #[test]
    fn parses_duration_units_exactly() {
        assert_eq!(parse_duration_ms("250ms").unwrap(), 250);
        assert_eq!(parse_duration_ms("1.5s").unwrap(), 1_500);
        assert_eq!(parse_duration_ms("2m").unwrap(), 120_000);
        assert_eq!(parse_duration_ms("1h").unwrap(), 3_600_000);
    }

    #[test]
    fn remote_errors_have_stable_exit_codes() {
        let denied = anyhow::Error::new(uperf_api::ClientError::Remote {
            name: "org.uperflinux.Daemon1.Error.NotAuthorized".into(),
            message: "denied".into(),
        });
        assert_eq!(classify_error(&denied), EXIT_NOT_AUTHORIZED);

        let conflict = anyhow::Error::new(uperf_api::ClientError::Remote {
            name: "org.uperflinux.Daemon1.Error.Conflict".into(),
            message: "stale".into(),
        });
        assert_eq!(classify_error(&conflict), EXIT_CONFLICT);
    }
}

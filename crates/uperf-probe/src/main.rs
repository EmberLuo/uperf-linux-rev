//! Read-only hardware and operating-system capability probe.

use std::{
    collections::BTreeMap,
    env,
    ffi::OsString,
    io::{self, Write},
    path::PathBuf,
};

use anyhow::{Context, Result, bail};
use uperf_core::{
    CONFIG_SCHEMA_VERSION, CpuPolicyConfig, CpuSet, DevfreqTargetConfig, DeviceConfig, DeviceMatch,
    Hertz, Validate,
};
use uperf_linux::{LinuxEnvironment, SystemRoots};

fn main() -> Result<()> {
    let arguments = Arguments::parse(env::args_os().skip(1))?;
    if arguments.help {
        print_help();
        return Ok(());
    }

    let environment = match arguments.fixture_root {
        Some(root) => LinuxEnvironment::new(SystemRoots::below(root)),
        None => LinuxEnvironment::host(),
    }
    .context("failed to open Linux observation roots")?;
    let report = environment
        .probe()
        .context("read-only hardware probe failed")?;

    let document = if arguments.device_draft {
        eprintln!(
            "uperf-probe: generated device.json is a non-activatable draft: review inferred frequency values and add explicitly trusted thermal zones"
        );
        serde_json::to_value(device_draft(&report.capabilities)?)
            .context("serialize device configuration draft")?
    } else {
        serde_json::to_value(&report).context("serialize probe report")?
    };
    let stdout = io::stdout();
    let mut output = stdout.lock();
    if arguments.pretty {
        serde_json::to_writer_pretty(&mut output, &document)
    } else {
        serde_json::to_writer(&mut output, &document)
    }
    .context("failed to serialize probe report")?;
    writeln!(output).context("failed to write probe report")?;
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
struct Arguments {
    pretty: bool,
    device_draft: bool,
    fixture_root: Option<PathBuf>,
    help: bool,
}

impl Arguments {
    fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Self> {
        let mut arguments = arguments.into_iter();
        let mut parsed = Self {
            pretty: true,
            device_draft: false,
            fixture_root: None,
            help: false,
        };
        while let Some(argument) = arguments.next() {
            match argument.to_str() {
                Some("--pretty") => parsed.pretty = true,
                Some("--compact") => parsed.pretty = false,
                Some("--device-draft") => parsed.device_draft = true,
                Some("--root") => {
                    let Some(root) = arguments.next() else {
                        bail!("--root requires a fixture directory");
                    };
                    parsed.fixture_root = Some(PathBuf::from(root));
                }
                Some("--help" | "-h") => parsed.help = true,
                Some(value) => bail!("unknown argument `{value}`; use --help"),
                None => bail!("arguments must be valid UTF-8"),
            }
        }
        Ok(parsed)
    }
}

fn print_help() {
    println!(
        "\
uperf-probe - inspect Linux performance-control capabilities without writing them

Usage: uperf-probe [--pretty|--compact] [--device-draft] [--root FIXTURE]

Options:
  --pretty         Pretty-print JSON (default)
  --compact        Emit compact JSON
  --device-draft   Emit a review-only device.json draft instead of the report
  --root FIXTURE   Read FIXTURE/sys, FIXTURE/proc and FIXTURE/etc instead of host roots
  -h, --help       Show this help

The command performs no sysfs, procfs, systemd or device mutations."
    );
}

fn device_draft(capabilities: &uperf_core::DeviceCapabilities) -> Result<DeviceConfig> {
    let device_match = capabilities
        .compatible
        .first()
        .cloned()
        .map(|compatible| DeviceMatch {
            compatible: Some(compatible),
            product_name: None,
        })
        .or_else(|| {
            capabilities
                .device_name
                .clone()
                .map(|product_name| DeviceMatch {
                    compatible: None,
                    product_name: Some(product_name),
                })
        });
    let cpu_policies = capabilities
        .cpu_policies
        .iter()
        .map(|policy| {
            let (reference_hz, efficient_cap_hz) =
                draft_cpu_model(&policy.available_frequencies, policy.limits);
            CpuPolicyConfig {
                id: policy.id.clone(),
                related_cpus: policy.cpus.clone(),
                sysfs_path: None,
                floor_hz: policy.limits.min,
                reference_hz,
                efficient_cap_hz,
                admin_cap_hz: Some(policy.limits.max),
                critical_cap_hz: Some(policy.limits.min),
                sensor_failure_cap_hz: Some(policy.limits.min),
            }
        })
        .collect();
    let devfreq_targets = capabilities
        .devfreq_targets
        .iter()
        .filter(|target| !target.available_frequencies.is_empty())
        .map(|target| DevfreqTargetConfig {
            id: target.id.clone(),
            device_name: target.device_name.clone(),
            compatible: target.compatible.clone(),
            sysfs_path: None,
            manual_only: true,
            admin_cap_hz: Some(target.limits.max),
            critical_cap_hz: Some(target.limits.min),
            sensor_failure_cap_hz: Some(target.limits.min),
        })
        .collect();
    let all_cpus = capabilities
        .cpu_policies
        .iter()
        .flat_map(|policy| policy.cpus.iter().copied())
        .collect::<CpuSet>();
    let mut cpu_groups = BTreeMap::from([
        ("all".to_owned(), all_cpus.clone()),
        ("balanced".to_owned(), all_cpus),
    ]);
    if let Some(first) = capabilities.cpu_policies.first() {
        cpu_groups.insert("efficient".to_owned(), first.cpus.clone());
    }
    if let Some(last) = capabilities.cpu_policies.last() {
        cpu_groups.insert("performance".to_owned(), last.cpus.clone());
    }
    let draft = DeviceConfig {
        schema_version: CONFIG_SCHEMA_VERSION,
        device_id: capabilities.compatible.first().map_or_else(
            || "draft-unidentified".to_owned(),
            |id| format!("draft-{id}"),
        ),
        device_match,
        cpu_groups,
        cpu_policies,
        devfreq_targets,
        // Trust and thermal thresholds require administrator review. An empty
        // list deliberately makes ConfigBundle activation validation fail.
        thermal_zones: Vec::new(),
    };
    draft
        .validate()
        .map_err(|error| anyhow::anyhow!("generated draft was invalid: {error}"))?;
    Ok(draft)
}

fn draft_cpu_model(frequencies: &[Hertz], limits: uperf_core::FrequencyLimits) -> (Hertz, Hertz) {
    if frequencies.is_empty() {
        let midpoint = limits
            .min
            .get()
            .saturating_add(limits.max.get().saturating_sub(limits.min.get()) / 2);
        let representable = Hertz::new((midpoint / 1_000 * 1_000).max(limits.min.get()));
        return (representable, limits.max);
    }
    (
        frequencies[frequencies.len() / 2],
        frequencies[(frequencies.len() * 3 / 4).min(frequencies.len() - 1)],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_output_and_fixture_options() {
        let arguments = Arguments::parse([
            OsString::from("--compact"),
            OsString::from("--root"),
            OsString::from("/tmp/fixture"),
        ])
        .unwrap();
        assert_eq!(
            arguments,
            Arguments {
                pretty: false,
                device_draft: false,
                fixture_root: Some(PathBuf::from("/tmp/fixture")),
                help: false,
            }
        );
    }

    #[test]
    fn rejects_missing_root_value() {
        assert!(Arguments::parse([OsString::from("--root")]).is_err());
    }

    #[test]
    fn parses_device_draft_mode() {
        let arguments = Arguments::parse([OsString::from("--device-draft")]).unwrap();
        assert!(arguments.device_draft);
    }
}

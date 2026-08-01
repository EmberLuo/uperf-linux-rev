use std::{path::PathBuf, str::FromStr};

use anyhow::{Result, bail};

const DEFAULT_TIMEOUT_MS: u64 = 30_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bus {
    System,
    Session,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Cli {
    pub json: bool,
    pub bus: Bus,
    pub timeout_ms: u64,
    pub command: Command,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Status,
    Health,
    Trace(TraceOptions),
    GovernorStatus,
    Mode(ModeAction),
    Workload(WorkloadAction),
    Foreground(ForegroundAction),
    Targets(Option<String>),
    Frequency(FrequencyAction),
    Reload,
    Config(ConfigAction),
    Diagnose,
    Help(Option<String>),
    Version,
}

#[derive(Debug, PartialEq, Eq)]
pub struct TraceOptions {
    pub after_id: u64,
    pub limit: u32,
    pub extended: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ModeAction {
    Show,
    List,
    Set(String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum WorkloadAction {
    Show,
    Set {
        pid: u32,
        mode: Option<String>,
        reason: String,
    },
    Clear,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ForegroundAction {
    Set { pid: u32, reason: String },
    Clear,
}

#[derive(Debug, PartialEq, Eq)]
pub enum FrequencyAction {
    Show(Option<String>),
    Set {
        target_id: String,
        minimum: String,
        maximum: String,
        ttl: Option<String>,
        reason: String,
    },
    Clear(Vec<String>),
}

#[derive(Debug, PartialEq, Eq)]
pub enum ConfigAction {
    Validate(PathBuf),
}

impl Cli {
    pub fn parse(arguments: impl IntoIterator<Item = String>) -> Result<Self> {
        let mut cursor = Cursor::new(arguments.into_iter().collect());
        let mut json = false;
        let mut bus = Bus::System;
        let mut timeout_ms = DEFAULT_TIMEOUT_MS;

        loop {
            match cursor.peek() {
                Some("--json") => {
                    cursor.next();
                    json = true;
                }
                Some("--session") => {
                    cursor.next();
                    bus = Bus::Session;
                }
                Some("--bus") => {
                    cursor.next();
                    bus = match cursor.required("bus kind")?.as_str() {
                        "system" => Bus::System,
                        "session" => Bus::Session,
                        value => bail!("unknown bus '{value}'; expected system or session"),
                    };
                }
                Some("--timeout") => {
                    cursor.next();
                    timeout_ms =
                        parse_positive::<u64>(&cursor.required("timeout in ms")?, "timeout")?;
                }
                Some("-h" | "--help") => {
                    cursor.next();
                    return Ok(Self {
                        json,
                        bus,
                        timeout_ms,
                        command: Command::Help(cursor.next()),
                    });
                }
                Some("-V" | "--version") => {
                    cursor.next();
                    cursor.finish()?;
                    return Ok(Self {
                        json,
                        bus,
                        timeout_ms,
                        command: Command::Version,
                    });
                }
                _ => break,
            }
        }

        let command_name = cursor.next().unwrap_or_else(|| "help".into());
        let command = match command_name.as_str() {
            "status" => {
                cursor.finish()?;
                Command::Status
            }
            "health" => {
                cursor.finish()?;
                Command::Health
            }
            "trace" => Command::Trace(parse_trace(&mut cursor)?),
            "governor-status" => {
                cursor.finish()?;
                Command::GovernorStatus
            }
            "mode" => Command::Mode(parse_mode(&mut cursor)?),
            "workload" => Command::Workload(parse_workload(&mut cursor)?),
            "foreground" => Command::Foreground(parse_foreground(&mut cursor)?),
            "targets" => {
                let target = cursor.next();
                cursor.finish()?;
                Command::Targets(target)
            }
            "frequency" | "freq" => Command::Frequency(parse_frequency(&mut cursor)?),
            "reload" => {
                cursor.finish()?;
                Command::Reload
            }
            "config" => Command::Config(parse_config(&mut cursor)?),
            "diagnose" => {
                cursor.finish()?;
                Command::Diagnose
            }
            "help" => {
                let topic = cursor.next();
                cursor.finish()?;
                Command::Help(topic)
            }
            "version" => {
                cursor.finish()?;
                Command::Version
            }
            other => bail!("unknown command '{other}'; run 'uperfctl help'"),
        };

        Ok(Self {
            json,
            bus,
            timeout_ms,
            command,
        })
    }
}

fn parse_trace(cursor: &mut Cursor) -> Result<TraceOptions> {
    let mut after_id = 0;
    let mut limit = 128;
    let mut after_seen = false;
    let mut limit_seen = false;
    let mut extended = false;
    while let Some(option) = cursor.next() {
        match option.as_str() {
            "--after" => {
                if after_seen {
                    bail!("--after was specified more than once");
                }
                after_id = parse_number(&cursor.required("decision ID")?, "decision ID")?;
                after_seen = true;
            }
            "--limit" => {
                if limit_seen {
                    bail!("--limit was specified more than once");
                }
                limit = parse_number(&cursor.required("trace limit")?, "trace limit")?;
                if limit > uperf_api::MAX_DECISION_TRACE_PAGE {
                    bail!("trace limit exceeds {}", uperf_api::MAX_DECISION_TRACE_PAGE);
                }
                limit_seen = true;
            }
            "--extended" => {
                if extended {
                    bail!("--extended was specified more than once");
                }
                extended = true;
            }
            other => bail!("unknown trace option '{other}'"),
        }
    }
    Ok(TraceOptions {
        after_id,
        limit,
        extended,
    })
}

fn parse_mode(cursor: &mut Cursor) -> Result<ModeAction> {
    let action = match cursor.next().as_deref() {
        None | Some("show") => ModeAction::Show,
        Some("list") => ModeAction::List,
        Some("set") => ModeAction::Set(cursor.required("mode name")?),
        Some(mode) => ModeAction::Set(mode.into()),
    };
    cursor.finish()?;
    Ok(action)
}

fn parse_workload(cursor: &mut Cursor) -> Result<WorkloadAction> {
    let Some(action) = cursor.next() else {
        return Ok(WorkloadAction::Show);
    };
    match action.as_str() {
        "show" => {
            cursor.finish()?;
            Ok(WorkloadAction::Show)
        }
        "set" => {
            let pid = parse_positive(&cursor.required("PID")?, "PID")?;
            let mut mode = None;
            let mut reason = "uperfctl workload set".into();
            while let Some(option) = cursor.next() {
                match option.as_str() {
                    "--mode" => set_once(&mut mode, cursor.required("mode")?, "--mode")?,
                    "--reason" => reason = cursor.required("reason")?,
                    other => bail!("unknown workload set option '{other}'"),
                }
            }
            Ok(WorkloadAction::Set { pid, mode, reason })
        }
        "clear" => {
            cursor.finish()?;
            Ok(WorkloadAction::Clear)
        }
        other => bail!("unknown workload action '{other}'; expected show, set, or clear"),
    }
}

fn parse_foreground(cursor: &mut Cursor) -> Result<ForegroundAction> {
    match cursor.next().as_deref() {
        Some("set") => {
            let pid = parse_positive(&cursor.required("PID")?, "PID")?;
            let mut reason = "uperfctl foreground set".into();
            while let Some(option) = cursor.next() {
                match option.as_str() {
                    "--reason" => reason = cursor.required("reason")?,
                    other => bail!("unknown foreground set option '{other}'"),
                }
            }
            Ok(ForegroundAction::Set { pid, reason })
        }
        Some("clear") => {
            cursor.finish()?;
            Ok(ForegroundAction::Clear)
        }
        Some(other) => bail!("unknown foreground action '{other}'; expected set or clear"),
        None => bail!("foreground requires an action; expected set or clear"),
    }
}

fn parse_frequency(cursor: &mut Cursor) -> Result<FrequencyAction> {
    let Some(action) = cursor.next() else {
        return Ok(FrequencyAction::Show(None));
    };
    match action.as_str() {
        "show" | "get" => {
            let target = cursor.next();
            cursor.finish()?;
            Ok(FrequencyAction::Show(target))
        }
        "set" => {
            let target_id = cursor.required("target ID")?;
            let minimum = cursor.required("minimum frequency")?;
            let maximum = cursor.required("maximum frequency")?;
            let mut ttl = None;
            let mut reason = "uperfctl frequency set".into();
            while let Some(option) = cursor.next() {
                match option.as_str() {
                    "--ttl" => set_once(&mut ttl, cursor.required("TTL")?, "--ttl")?,
                    "--reason" => reason = cursor.required("reason")?,
                    other => bail!("unknown frequency set option '{other}'"),
                }
            }
            Ok(FrequencyAction::Set {
                target_id,
                minimum,
                maximum,
                ttl,
                reason,
            })
        }
        "clear" => Ok(FrequencyAction::Clear(cursor.remaining())),
        other => {
            cursor.finish()?;
            Ok(FrequencyAction::Show(Some(other.into())))
        }
    }
}

fn parse_config(cursor: &mut Cursor) -> Result<ConfigAction> {
    match cursor.required("config action")?.as_str() {
        "validate" => {
            let path = PathBuf::from(cursor.required("configuration path")?);
            cursor.finish()?;
            Ok(ConfigAction::Validate(path))
        }
        action => bail!("unknown config action '{action}'; expected validate"),
    }
}

fn parse_number<T>(value: &str, name: &str) -> Result<T>
where
    T: FromStr,
{
    value
        .parse()
        .map_err(|_| anyhow::anyhow!("{name} must be a non-negative integer"))
}

fn parse_positive<T>(value: &str, name: &str) -> Result<T>
where
    T: FromStr + Default + PartialEq,
{
    let parsed = parse_number(value, name)?;
    if parsed == T::default() {
        bail!("{name} must be greater than zero");
    }
    Ok(parsed)
}

fn set_once<T>(slot: &mut Option<T>, value: T, name: &str) -> Result<()> {
    if slot.replace(value).is_some() {
        bail!("{name} was specified more than once");
    }
    Ok(())
}

struct Cursor {
    arguments: Vec<String>,
    index: usize,
}

impl Cursor {
    const fn new(arguments: Vec<String>) -> Self {
        Self {
            arguments,
            index: 0,
        }
    }

    fn peek(&self) -> Option<&str> {
        self.arguments.get(self.index).map(String::as_str)
    }

    fn next(&mut self) -> Option<String> {
        let value = self.arguments.get(self.index).cloned();
        self.index += usize::from(value.is_some());
        value
    }

    fn required(&mut self, name: &str) -> Result<String> {
        self.next().ok_or_else(|| anyhow::anyhow!("missing {name}"))
    }

    fn remaining(&mut self) -> Vec<String> {
        let remaining = self.arguments.split_off(self.index);
        self.index = self.arguments.len();
        remaining
    }

    fn finish(&self) -> Result<()> {
        if let Some(extra) = self.peek() {
            bail!("unexpected argument '{extra}'");
        }
        Ok(())
    }
}

pub fn help(topic: Option<&str>) -> &'static str {
    match topic {
        Some("mode") => MODE_HELP,
        Some("workload") => WORKLOAD_HELP,
        Some("foreground") => FOREGROUND_HELP,
        Some("frequency" | "freq") => FREQUENCY_HELP,
        Some("trace") => TRACE_HELP,
        Some("governor-status") => GOVERNOR_STATUS_HELP,
        Some("config") => CONFIG_HELP,
        Some("targets") => TARGETS_HELP,
        _ => HELP,
    }
}

const HELP: &str = "\
Usage: uperfctl [GLOBAL OPTIONS] <COMMAND>

Global options:
  --json                 Emit machine-readable JSON
  --bus system|session   Select D-Bus (default: system)
  --session              Shorthand for --bus session
  --timeout MS           Whole-operation timeout (default: 30000)
  -h, --help             Show help
  -V, --version          Show client and API versions

Commands:
  status                 Show coherent daemon state
  health                 Show health; exits 2 when unhealthy
  trace                  Show the bounded decision/reconciliation timeline
  governor-status        Show current governor inputs, budget, and OPP choices
  mode [list|set MODE]   Inspect or change the policy mode
  workload ...           Inspect, select, or clear the active workload
  foreground ...         Report or release the focused process
  targets [ID]           Show discovered stable target IDs
  frequency ...          Inspect or change bounded frequency overrides
  reload                 Transactionally reload daemon configuration
  config validate PATH   Validate an offline v2 configuration
  diagnose               Run API, health, recovery, and target checks

Run 'uperfctl help COMMAND' for command-specific syntax.

Exit status:
  0  command succeeded
  1  invalid arguments, local I/O, or other operation failure
  2  unhealthy daemon, failed diagnostics, or invalid configuration
  3  daemon unavailable, incompatible, degraded, or timed out
  4  authorization denied
  5  state conflict; refresh status and retry
";

const TRACE_HELP: &str = "\
Usage:
  uperfctl trace [--after DECISION_ID] [--limit COUNT] [--extended]

The pagination key is exclusive. The default page size is 128 and the maximum
is 512. Trace state is process-local and bounded, so old entries may have
already expired from the daemon's ring. --extended adds trigger-to-readback
latency, governor diagnostics, and desired/applied scalar values.
";

const GOVERNOR_STATUS_HELP: &str = "\
Usage:
  uperfctl governor-status

Shows the current rollout, load transform, power budget and energy bucket,
per-target OPP reasons, and desired/applied scalar resources.
";

const MODE_HELP: &str = "\
Usage:
  uperfctl mode
  uperfctl mode list
  uperfctl mode set MODE
";

const WORKLOAD_HELP: &str = "\
Usage:
  uperfctl workload [show]
  uperfctl workload set PID [--mode MODE] [--reason TEXT]
  uperfctl workload clear

The daemon resolves PID, start time, and UID before authorization. Clear
operates on the exact stable identity currently held by the daemon.
";

const FOREGROUND_HELP: &str = "\
Usage:
  uperfctl foreground set PID [--reason TEXT]
  uperfctl foreground clear

This is the same lane a compositor uses. An explicit 'workload set' always wins
over focus, and the lease expires unless it is renewed, so a reporter that dies
never pins the boost. A rejected PID appears as a focus.rejected health issue
rather than an error, because the receipt is returned before resolution.
";

const TARGETS_HELP: &str = "\
Usage:
  uperfctl targets [TARGET_ID]
";

const FREQUENCY_HELP: &str = "\
Usage:
  uperfctl frequency [show [TARGET_ID]]
  uperfctl frequency set TARGET_ID MIN MAX [--ttl DURATION] [--reason TEXT]
  uperfctl frequency clear [TARGET_ID ...]

Bare frequencies are Hz. Units Hz, kHz, MHz, and GHz are accepted. TTL units
are ms, s, m, and h; zero or an omitted TTL remains until explicitly cleared.
";

const CONFIG_HELP: &str = "\
Usage:
  uperfctl config validate PATH

PATH may be one v2 JSON file or a directory containing device.json, policy.json,
and apps.json. Directory validation also checks cross-file references.
";

#[cfg(test)]
mod tests {
    use super::{Bus, Cli, Command, FrequencyAction, TraceOptions, WorkloadAction};

    fn parse(arguments: &[&str]) -> Cli {
        Cli::parse(arguments.iter().map(ToString::to_string)).unwrap()
    }

    #[test]
    fn parses_global_options_and_frequency_set() {
        let cli = parse(&[
            "--json",
            "--session",
            "--timeout",
            "1000",
            "frequency",
            "set",
            "cpu.policy0",
            "400MHz",
            "1.8GHz",
            "--ttl",
            "30s",
        ]);
        assert!(cli.json);
        assert_eq!(cli.bus, Bus::Session);
        assert_eq!(cli.timeout_ms, 1000);
        assert!(matches!(
            cli.command,
            Command::Frequency(FrequencyAction::Set { .. })
        ));
    }

    #[test]
    fn workload_clear_has_no_client_supplied_identity() {
        assert_eq!(
            parse(&["workload", "clear"]).command,
            Command::Workload(WorkloadAction::Clear)
        );
        assert!(Cli::parse(["workload", "clear", "42"].map(str::to_owned)).is_err());
    }

    #[test]
    fn trace_pagination_is_bounded_at_parse_time() {
        assert_eq!(
            parse(&["trace", "--after", "42", "--limit", "64"]).command,
            Command::Trace(TraceOptions {
                after_id: 42,
                limit: 64,
                extended: false,
            })
        );
        assert_eq!(
            parse(&["trace", "--extended"]).command,
            Command::Trace(TraceOptions {
                after_id: 0,
                limit: 128,
                extended: true,
            })
        );
        assert!(Cli::parse(["trace", "--limit", "513"].map(str::to_owned)).is_err());
        assert!(Cli::parse(["trace", "--after", "1", "--after", "2"].map(str::to_owned)).is_err());
        assert!(Cli::parse(["trace", "--extended", "--extended"].map(str::to_owned)).is_err());
    }

    #[test]
    fn workload_set_contains_only_pid_and_policy_inputs() {
        assert_eq!(
            parse(&[
                "workload",
                "set",
                "42",
                "--mode",
                "performance",
                "--reason",
                "foreground game",
            ])
            .command,
            Command::Workload(WorkloadAction::Set {
                pid: 42,
                mode: Some("performance".into()),
                reason: "foreground game".into(),
            })
        );
    }

    #[test]
    fn workload_rejects_removed_identity_options() {
        assert!(
            Cli::parse(["workload", "set", "42", "--start-time", "123"].map(str::to_owned))
                .is_err()
        );
        assert!(Cli::parse(["workload", "set", "42", "--uid", "1000"].map(str::to_owned)).is_err());
    }

    #[test]
    fn duplicate_single_value_option_is_rejected() {
        assert!(
            Cli::parse(
                [
                    "frequency",
                    "set",
                    "cpu",
                    "1",
                    "2",
                    "--ttl",
                    "1s",
                    "--ttl",
                    "2s",
                ]
                .into_iter()
                .map(ToString::to_string)
            )
            .is_err()
        );
    }
}

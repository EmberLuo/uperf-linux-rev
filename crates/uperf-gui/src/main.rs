mod focus_reporter;
mod i18n;
mod view_model;

use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
    thread,
    time::Duration,
};

use adw::prelude::*;
use async_channel::{Receiver, Sender};
use futures_util::StreamExt;
use gtk::glib;
use i18n::{
    LanguageChoice, language_choice, localized_mode_label, localized_protocol_value,
    save_language_choice, tr, translate_known,
};
use uperf_api::{
    ActiveWorkload, AppRule, Capabilities, ClientError, DaemonClient, DaemonStatus,
    FrequencyOverride, FrequencyStatus, RunningWorkload, SchedulerStatus, TargetCapability,
    TelemetrySnapshot, WorkloadRequest, feature,
};
use view_model::{
    FocusAction, FocusState, FocusView, ReporterState, TargetView, ViewModel, cpu_load_percent,
    frequency_override,
};

const SERVICE_UNIT: &str = "uperf-linux.service";
const INITIAL_RECONNECT_DELAY: Duration = Duration::from_millis(250);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(8);
/// The daemon samples fast enough for policy decisions, but a human-readable
/// dashboard gains nothing from relaying every 4 Hz sample into GTK layout.
/// Keep only the latest status and telemetry and repaint them together.
const GUI_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
/// How often the reporter is re-probed while its state is something the user may
/// be fixing right now. A working reporter is left alone and only re-read when
/// GNOME Shell announces a change, so the idle desktop stays idle.
const REPORTER_RECHECK_INTERVAL: Duration = Duration::from_secs(20);
type UnitFileChanges = (bool, Vec<(String, String, String)>);

#[derive(Debug)]
enum ClientCommand {
    SetMode(String),
    SetFrequency(FrequencyOverride),
    ClearFrequency(String),
    ClearAllFrequency(Vec<String>),
    SetWorkload(WorkloadRequest),
    ClearWorkload(WorkloadClearTarget),
    SetAppRule(AppRule),
    RemoveAppRule(String),
    ReloadConfig,
    EnableAndStartService,
}

/// Session-bus work, kept on its own channel because it must keep working while
/// the system-bus daemon is unreachable: a user whose daemon is down can still
/// switch the reporter on, and a user with no reporter still needs the daemon.
#[derive(Debug)]
enum ReporterCommand {
    Enable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkloadClearTarget {
    Explicit,
    Focus,
}

#[derive(Debug)]
enum UiEvent {
    Snapshot {
        capabilities: Capabilities,
        status: DaemonStatus,
        rules: Vec<AppRule>,
        workloads: Vec<RunningWorkload>,
    },
    Status(DaemonStatus),
    Capabilities(Capabilities),
    Telemetry(TelemetrySnapshot),
    AppRules(Vec<AppRule>),
    RunningWorkloads(Vec<RunningWorkload>),
    Notice(String),
    Connection(ConnectionState),
    RequestError {
        kind: RequestErrorKind,
        message: String,
    },
    ServiceActivationStarted,
    ServiceActivationFinished(Result<(), String>),
    /// Reporter state observed on the session bus, which the daemon cannot see.
    Reporter(ReporterState),
}

#[derive(Debug)]
enum ConnectionState {
    Connecting,
    Connected,
    Reconnecting { delay: Duration, reason: String },
    Unavailable(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequestErrorKind {
    NotAuthorized,
    InvalidRequest,
    IncompatibleApi,
    Rejected,
}

impl RequestErrorKind {
    fn label(self) -> &'static str {
        match self {
            Self::NotAuthorized => tr("Not authorized"),
            Self::InvalidRequest => tr("Invalid request"),
            Self::IncompatibleApi => tr("Incompatible API"),
            Self::Rejected => tr("Request rejected"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ErrorDisposition {
    ConnectionLost,
    Request(RequestErrorKind),
}

struct CpuLoadRow {
    row: adw::ActionRow,
    value: gtk::Label,
}

struct FrequencyOverrideCard {
    row: adw::ExpanderRow,
    restore: Option<gtk::Button>,
}

#[derive(Debug)]
struct ReconnectBackoff {
    next: Duration,
}

impl ReconnectBackoff {
    const fn new() -> Self {
        Self {
            next: INITIAL_RECONNECT_DELAY,
        }
    }

    fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = self.next.saturating_mul(2).min(MAX_RECONNECT_DELAY);
        delay
    }

    const fn reset(&mut self) {
        self.next = INITIAL_RECONNECT_DELAY;
    }
}

struct Ui {
    window: adw::ApplicationWindow,
    overlay: adw::ToastOverlay,

    // Dashboard: overview
    connection_row: adw::ActionRow,
    service_button: gtk::Button,
    service_action_running: Cell<bool>,
    state_row: adw::ActionRow,
    health_row: adw::ActionRow,
    health_issues_group: adw::PreferencesGroup,
    health_issue_rows: RefCell<Vec<adw::ActionRow>>,
    profile_row: adw::ActionRow,
    scene_row: adw::ActionRow,

    // Dashboard: mode selector (linked toggle group, rebuilt from capabilities)
    mode_box: gtk::Box,
    mode_buttons: RefCell<BTreeMap<String, gtk::ToggleButton>>,
    syncing_modes: Cell<bool>,

    // Dashboard: thermal
    thermal_group: adw::PreferencesGroup,
    thermal_row: adw::ActionRow,
    thermal_bar: gtk::ProgressBar,

    // Dashboard: focus, the headline of the whole product
    focus_group: adw::PreferencesGroup,
    focus_row: adw::ActionRow,
    focus_icon: gtk::Image,
    focus_button: gtk::Button,
    focus_action: Cell<FocusAction>,
    focus_holder_row: adw::ActionRow,
    focus_command_row: adw::ActionRow,
    focus_command_label: gtk::Label,

    // Dashboard: workload / scheduler
    workload_group: adw::PreferencesGroup,
    workload_row: adw::ActionRow,
    scheduler_row: adw::ActionRow,
    cgroup_row: adw::ActionRow,
    clear_workload_button: gtk::Button,

    // Apps page: manual PID selection, deliberately not a first-screen action
    pid_entry: adw::EntryRow,

    // Dashboard: per-CPU utilization (rebuilt as CPU IDs are discovered)
    load_group: adw::PreferencesGroup,
    load_rows: RefCell<BTreeMap<u32, CpuLoadRow>>,

    // Dashboard: per-target frequency (rebuilt from capabilities)
    freq_group: adw::PreferencesGroup,
    freq_dynamic_rows: RefCell<Vec<adw::ActionRow>>,
    freq_rows: RefCell<BTreeMap<String, gtk::Label>>,

    // Frequency page (rebuilt from capabilities)
    override_group: adw::PreferencesGroup,
    override_dynamic_rows: RefCell<Vec<adw::ExpanderRow>>,
    target_status: RefCell<BTreeMap<String, FrequencyOverrideCard>>,
    override_ids: RefCell<Vec<String>>,
    restore_all_button: gtk::Button,

    // Apps page: running candidates and persistent rules
    running_group: adw::PreferencesGroup,
    running_placeholder: adw::ActionRow,
    running_rows: RefCell<Vec<gtk::Widget>>,
    apps_group: adw::PreferencesGroup,
    apps_placeholder: adw::ActionRow,
    app_rows: RefCell<Vec<gtk::Widget>>,
    rule_exe_entry: adw::EntryRow,
    rule_comm_entry: adw::EntryRow,
    rule_mode_dropdown: gtk::DropDown,

    // Logs page
    log_buffer: gtk::TextBuffer,

    // Shared state
    capabilities: RefCell<Capabilities>,
    status: RefCell<DaemonStatus>,
    reporter: Cell<ReporterState>,
    rules: RefCell<Vec<AppRule>>,
    workloads: RefCell<Vec<RunningWorkload>>,
    mode_ids: RefCell<Vec<String>>,
    connected: Cell<bool>,
    daemon_controls: RefCell<Vec<gtk::Widget>>,
    pending_status: RefCell<Option<DaemonStatus>>,
    pending_telemetry: RefCell<Option<TelemetrySnapshot>>,
    runtime_refresh_scheduled: Cell<bool>,
    commands: Sender<ClientCommand>,
    reporter_commands: Sender<ReporterCommand>,
}

fn status_row(title: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).build();
    row.set_subtitle("—");
    row
}

fn value_label() -> gtk::Label {
    let label = gtk::Label::new(Some("—"));
    label.add_css_class("dim-label");
    label.set_xalign(1.0);
    label
}

fn new_prefs_page(title: &str, icon: &str) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();
    page.set_title(title);
    page.set_icon_name(Some(icon));
    page
}

impl Ui {
    #[allow(clippy::too_many_lines)]
    fn new(
        application: &adw::Application,
        commands: Sender<ClientCommand>,
        reporter_commands: Sender<ReporterCommand>,
    ) -> Rc<Self> {
        let window = adw::ApplicationWindow::builder()
            .application(application)
            .title("Uperf Linux")
            .default_width(720)
            .default_height(760)
            .build();

        // Dashboard widgets
        let connection_row = status_row(tr("Connection"));
        connection_row.set_subtitle(tr("Connecting…"));
        let service_button = gtk::Button::with_label(tr("Enable & Start"));
        service_button.set_tooltip_text(Some(tr(
            "Start at boot and connect the GUI to the privileged daemon",
        )));
        service_button.set_valign(gtk::Align::Center);
        service_button.add_css_class("suggested-action");
        connection_row.add_suffix(&service_button);
        let state_row = status_row(tr("Lifecycle"));
        let health_row = status_row(tr("Health"));
        let health_issues_group = adw::PreferencesGroup::builder()
            .title(tr("Health issues"))
            .description(tr(
                "Detailed daemon findings, including informational reports",
            ))
            .build();
        health_issues_group.set_visible(false);
        let profile_row = status_row(tr("Effective profile"));
        let scene_row = status_row(tr("Dominant scene"));
        let mode_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        mode_box.add_css_class("linked");
        mode_box.set_margin_top(4);
        mode_box.set_margin_bottom(4);
        let thermal_group = adw::PreferencesGroup::builder()
            .title(tr("Thermal safety"))
            .description(tr(
                "Safety state is authoritative; manual settings cannot bypass it",
            ))
            .build();
        let thermal_row = status_row(tr("Temperature"));
        let thermal_bar = gtk::ProgressBar::new();
        let focus_group = adw::PreferencesGroup::builder()
            .title(tr("Focus following"))
            .description(tr(
                "Scheduling follows the application you are using; the daemon authorizes every report",
            ))
            .build();
        let focus_row = adw::ActionRow::builder().title(tr("Checking…")).build();
        let focus_icon = gtk::Image::from_icon_name("content-loading-symbolic");
        focus_row.add_prefix(&focus_icon);
        let focus_button = gtk::Button::new();
        focus_button.set_valign(gtk::Align::Center);
        focus_button.set_visible(false);
        focus_row.add_suffix(&focus_button);
        let focus_holder_row = adw::ActionRow::builder()
            .title(tr("Focused application"))
            .build();
        focus_holder_row.add_prefix(&gtk::Image::from_icon_name("view-reveal-symbolic"));
        focus_holder_row.set_visible(false);
        let focus_command_row = adw::ActionRow::builder()
            .title(tr("Run this to fix it"))
            .build();
        let focus_command_label = gtk::Label::new(None);
        focus_command_label.add_css_class("monospace");
        focus_command_label.add_css_class("dim-label");
        focus_command_label.set_selectable(true);
        focus_command_label.set_wrap(true);
        focus_command_label.set_xalign(0.0);
        focus_command_row.add_suffix(&focus_command_label);
        focus_command_row.set_visible(false);
        let workload_group = adw::PreferencesGroup::builder()
            .title(tr("Effective workload"))
            .description(tr(
                "What the daemon is actually tuning for, and how far the plan was applied",
            ))
            .build();
        let workload_row = status_row(tr("Selection"));
        let scheduler_row = status_row(tr("Task scheduler"));
        let cgroup_row = status_row(tr("Systemd cgroup"));
        let pid_entry = adw::EntryRow::builder().title(tr("Workload PID")).build();
        let clear_workload_button = gtk::Button::with_label(tr("Clear active workload"));
        clear_workload_button.set_sensitive(false);
        let load_group = adw::PreferencesGroup::builder()
            .title(tr("CPU utilization"))
            .description(tr("Per-CPU load reported by daemon telemetry"))
            .build();
        let freq_group = adw::PreferencesGroup::builder()
            .title(tr("Cluster frequency"))
            .build();

        // Frequency-page widgets
        let override_group = adw::PreferencesGroup::builder()
            .title(tr("Manual frequency limits"))
            .description(tr(
                "Set temporary allowed ranges. The kernel still chooses the actual frequency from load, and thermal safety may tighten these limits.",
            ))
            .build();
        let restore_all_button = gtk::Button::with_label(tr("Restore all automatic"));

        // Apps-page widgets
        let running_group = adw::PreferencesGroup::builder()
            .title(tr("Detected running workloads"))
            .description(tr(
                "Broad game and compatibility-layer matches; detection alone never changes the active mode",
            ))
            .build();
        let running_placeholder = adw::ActionRow::builder()
            .title(tr("No matching processes"))
            .subtitle(tr(
                "Launch a game, Wine/Proton application, emulator, or Steam process.",
            ))
            .build();
        let apps_group = adw::PreferencesGroup::builder()
            .title(tr("Application rules"))
            .description(tr(
                "Persistent global rules that pin a mode for matching processes",
            ))
            .build();
        let apps_placeholder = adw::ActionRow::builder()
            .title(tr("No application rules"))
            .subtitle(tr("Add a rule below to pin a mode for a matching process."))
            .build();
        let rule_exe_entry = adw::EntryRow::builder()
            .title(tr("Executable path (optional)"))
            .build();
        let rule_comm_entry = adw::EntryRow::builder()
            .title(tr("Process-name regex (optional)"))
            .build();
        let rule_mode_dropdown = gtk::DropDown::from_strings(&[]);

        // Logs-page widget
        let log_buffer = gtk::TextBuffer::new(None);
        log_buffer.set_text(&format!(
            "{}\n",
            tr("Press Refresh to load the latest uperf-linux.service journal.")
        ));

        let overlay = adw::ToastOverlay::new();

        let ui = Rc::new(Self {
            window,
            overlay,
            connection_row,
            service_button,
            service_action_running: Cell::new(false),
            state_row,
            health_row,
            health_issues_group,
            health_issue_rows: RefCell::new(Vec::new()),
            profile_row,
            scene_row,
            mode_box,
            mode_buttons: RefCell::new(BTreeMap::new()),
            syncing_modes: Cell::new(false),
            thermal_group,
            thermal_row,
            thermal_bar,
            focus_group,
            focus_row,
            focus_icon,
            focus_button,
            focus_action: Cell::new(FocusAction::None),
            focus_holder_row,
            focus_command_row,
            focus_command_label,
            workload_group,
            workload_row,
            scheduler_row,
            cgroup_row,
            clear_workload_button,
            pid_entry,
            load_group,
            load_rows: RefCell::new(BTreeMap::new()),
            freq_group,
            freq_dynamic_rows: RefCell::new(Vec::new()),
            freq_rows: RefCell::new(BTreeMap::new()),
            override_group,
            override_dynamic_rows: RefCell::new(Vec::new()),
            target_status: RefCell::new(BTreeMap::new()),
            override_ids: RefCell::new(Vec::new()),
            restore_all_button,
            running_group,
            running_placeholder,
            running_rows: RefCell::new(Vec::new()),
            apps_group,
            apps_placeholder,
            app_rows: RefCell::new(Vec::new()),
            rule_exe_entry,
            rule_comm_entry,
            rule_mode_dropdown,
            log_buffer,
            capabilities: RefCell::new(Capabilities::default()),
            status: RefCell::new(DaemonStatus::default()),
            reporter: Cell::new(ReporterState::default()),
            rules: RefCell::new(Vec::new()),
            workloads: RefCell::new(Vec::new()),
            mode_ids: RefCell::new(Vec::new()),
            connected: Cell::new(false),
            daemon_controls: RefCell::new(Vec::new()),
            pending_status: RefCell::new(None),
            pending_telemetry: RefCell::new(None),
            runtime_refresh_scheduled: Cell::new(false),
            commands,
            reporter_commands,
        });
        {
            let action_ui = ui.clone();
            ui.service_button
                .connect_clicked(move |_| action_ui.enable_and_start_service());
        }
        {
            let action_ui = ui.clone();
            ui.focus_button
                .connect_clicked(move |_| action_ui.run_focus_action());
        }
        ui.assemble();
        ui
    }

    #[allow(clippy::too_many_lines)]
    fn assemble(self: &Rc<Self>) {
        let view_stack = adw::ViewStack::new();
        let dashboard = self.build_dashboard_page();
        let apps = self.build_apps_page();
        let frequency = self.build_frequency_page();
        let settings = self.build_settings_page();
        let logs = self.build_logs_page();
        for (page, name, title, icon) in [
            (
                &dashboard,
                "dashboard",
                tr("Dashboard"),
                "speedometer-symbolic",
            ),
            (&apps, "apps", tr("Apps"), "applications-games-symbolic"),
            (
                &frequency,
                "frequency",
                tr("Frequency limits"),
                "power-profile-performance-symbolic",
            ),
            (
                &settings,
                "settings",
                tr("Settings"),
                "emblem-system-symbolic",
            ),
            (&logs, "logs", tr("Logs"), "text-x-generic-symbolic"),
        ] {
            let stack_page = view_stack.add_titled(page, Some(name), title);
            stack_page.set_icon_name(Some(icon));
        }

        let header = adw::HeaderBar::new();
        let switcher = adw::ViewSwitcher::builder()
            .stack(&view_stack)
            .policy(adw::ViewSwitcherPolicy::Wide)
            .build();
        header.set_title_widget(Some(&switcher));

        let switcher_bar = adw::ViewSwitcherBar::builder().stack(&view_stack).build();

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.add_bottom_bar(&switcher_bar);
        toolbar.set_content(Some(&view_stack));

        self.overlay.set_child(Some(&toolbar));
        self.window.set_content(Some(&self.overlay));

        // Narrow layout: hide the header switcher, reveal the bottom bar.
        let breakpoint =
            adw::Breakpoint::new(adw::BreakpointCondition::parse("max-width: 700px").unwrap());
        breakpoint.add_setter(&switcher, "visible", Some(&false.into()));
        breakpoint.add_setter(&switcher_bar, "reveal", Some(&true.into()));
        self.window.add_breakpoint(breakpoint);
    }

    fn present(&self) {
        self.window.present();
    }

    fn build_dashboard_page(self: &Rc<Self>) -> adw::PreferencesPage {
        let page = new_prefs_page(tr("Dashboard"), "speedometer-symbolic");

        // Focus leads the page: the product promise is that scheduling follows
        // the application in use, so its state and its holder come first.
        //
        // This is the one group that is deliberately *not* gated on a
        // capability. When the daemon does not advertise focus support the card
        // is the only place that can say why, which is exactly the failure users
        // hit.
        self.focus_group.add(&self.focus_row);
        self.focus_group.add(&self.focus_holder_row);
        self.focus_group.add(&self.focus_command_row);
        page.add(&self.focus_group);

        // Effective workload: what focus (or an explicit pick) actually produced.
        self.workload_group.add(&self.workload_row);
        self.workload_group.add(&self.scheduler_row);
        self.workload_group.add(&self.cgroup_row);
        let workload_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        workload_buttons.set_halign(gtk::Align::End);
        workload_buttons.set_margin_top(8);
        workload_buttons.append(&self.clear_workload_button);
        self.workload_group.add(&workload_buttons);
        self.add_daemon_control(&self.workload_group);
        page.add(&self.workload_group);

        {
            let ui = self.clone();
            self.clear_workload_button.connect_clicked(move |_| {
                let target = clear_target_for_workload(&ui.status.borrow().active_workload);
                if let Some(target) = target {
                    ui.send(ClientCommand::ClearWorkload(target));
                }
            });
        }

        let mode_group = adw::PreferencesGroup::builder()
            .title(tr("Power mode"))
            .description(tr("Modes are advertised by the running daemon"))
            .build();
        mode_group.add(&self.mode_box);
        self.add_daemon_control(&mode_group);
        page.add(&mode_group);

        let overview = adw::PreferencesGroup::builder()
            .title(tr("Status"))
            .description(tr("Observed state reported by org.uperflinux.Daemon2"))
            .build();
        overview.add(&self.connection_row);
        overview.add(&self.state_row);
        overview.add(&self.health_row);
        overview.add(&self.profile_row);
        overview.add(&self.scene_row);
        page.add(&overview);
        page.add(&self.health_issues_group);

        page.add(&self.freq_group);
        page.add(&self.load_group);

        self.thermal_group.add(&self.thermal_row);
        self.thermal_bar.set_hexpand(true);
        self.thermal_bar.set_margin_top(6);
        self.thermal_bar.set_margin_bottom(6);
        self.thermal_group.add(&self.thermal_bar);
        page.add(&self.thermal_group);

        page
    }

    fn build_apps_page(self: &Rc<Self>) -> adw::PreferencesPage {
        let page = new_prefs_page(tr("Apps"), "applications-games-symbolic");
        self.running_group.add(&self.running_placeholder);
        self.add_daemon_control(&self.running_group);
        page.add(&self.running_group);

        page.add(&self.build_manual_workload_group());

        self.apps_group.add(&self.apps_placeholder);
        page.add(&self.apps_group);

        let add_group = adw::PreferencesGroup::builder()
            .title(tr("Add rule"))
            .description(tr("Match by executable path, process-name regex, or both"))
            .build();
        add_group.add(&self.rule_exe_entry);
        add_group.add(&self.rule_comm_entry);
        let mode_row = adw::ActionRow::builder().title(tr("Mode")).build();
        self.rule_mode_dropdown.set_valign(gtk::Align::Center);
        mode_row.add_suffix(&self.rule_mode_dropdown);
        add_group.add(&mode_row);

        let add_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        add_buttons.set_halign(gtk::Align::End);
        add_buttons.set_margin_top(8);
        let add_button = gtk::Button::with_label(tr("Add rule"));
        add_button.add_css_class("suggested-action");
        add_buttons.append(&add_button);
        add_group.add(&add_buttons);
        self.add_daemon_control(&self.apps_group);
        self.add_daemon_control(&add_group);
        page.add(&add_group);

        {
            let ui = self.clone();
            add_button.connect_clicked(move |_| ui.submit_new_rule());
        }
        page
    }

    /// Hand-typed PID selection, collapsed and off the first screen.
    ///
    /// It used to be the dashboard's primary action, which taught the wrong
    /// model: an explicit pick *suppresses* focus rather than complementing it.
    /// It stays available because it is the only way to steer a process the
    /// compositor never focuses, such as a headless build.
    fn build_manual_workload_group(self: &Rc<Self>) -> adw::PreferencesGroup {
        let group = adw::PreferencesGroup::builder()
            .title(tr("Manual selection"))
            .description(tr(
                "An explicit selection overrides focus until you clear it again",
            ))
            .build();
        let expander = adw::ExpanderRow::builder()
            .title(tr("Select a workload by PID"))
            .subtitle(tr(
                "The daemon resolves and verifies the start time and UID itself",
            ))
            .build();
        let set_workload = gtk::Button::with_label(tr("Set"));
        set_workload.add_css_class("suggested-action");
        set_workload.set_valign(gtk::Align::Center);
        // Nothing to submit until a PID is typed, so the primary action stays
        // insensitive instead of answering with a parse-error toast.
        set_workload.set_sensitive(false);
        self.pid_entry.add_suffix(&set_workload);
        expander.add_row(&self.pid_entry);
        group.add(&expander);
        self.add_daemon_control(&group);

        {
            let button = set_workload.clone();
            self.pid_entry.connect_changed(move |entry| {
                button.set_sensitive(!entry.text().trim().is_empty());
            });
        }
        {
            let ui = self.clone();
            set_workload.connect_clicked(move |_| ui.submit_manual_workload());
        }
        {
            let ui = self.clone();
            self.pid_entry
                .connect_entry_activated(move |_| ui.submit_manual_workload());
        }
        group
    }

    fn build_frequency_page(self: &Rc<Self>) -> adw::PreferencesPage {
        let page = new_prefs_page(tr("Frequency limits"), "power-profile-performance-symbolic");
        page.add(&self.override_group);

        let buttons_group = adw::PreferencesGroup::new();
        let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        hbox.set_halign(gtk::Align::Center);
        self.restore_all_button.add_css_class("pill");
        self.restore_all_button.set_sensitive(false);
        hbox.append(&self.restore_all_button);
        buttons_group.add(&hbox);
        self.add_daemon_control(&self.override_group);
        self.add_daemon_control(&buttons_group);
        page.add(&buttons_group);

        {
            let ui = self.clone();
            self.restore_all_button.connect_clicked(move |_| {
                let ids = ui.override_ids.borrow().clone();
                if ids.is_empty() {
                    ui.toast(tr("No overridable targets"));
                } else {
                    ui.send(ClientCommand::ClearAllFrequency(ids));
                }
            });
        }
        page
    }

    fn build_settings_page(self: &Rc<Self>) -> adw::PreferencesPage {
        let page = new_prefs_page(tr("Settings"), "emblem-system-symbolic");
        let group = adw::PreferencesGroup::builder()
            .title(tr("Daemon configuration"))
            .build();
        let row = adw::ActionRow::builder()
            .title(tr("Configuration reload"))
            .subtitle(tr(
                "Edit the config with administrator privileges, then reload it here.",
            ))
            .build();
        let reload = gtk::Button::with_label(tr("Reload"));
        reload.set_valign(gtk::Align::Center);
        row.add_suffix(&reload);
        group.add(&row);
        self.add_daemon_control(&group);
        page.add(&group);

        {
            let ui = self.clone();
            reload.connect_clicked(move |_| ui.send(ClientCommand::ReloadConfig));
        }

        let language_group = adw::PreferencesGroup::builder()
            .title(tr("Language"))
            .build();
        let language_row = adw::ActionRow::builder()
            .title(tr("Language"))
            .subtitle(tr(
                "Language changes take effect after restarting the application",
            ))
            .build();
        let language_labels = [
            tr("Follow system language"),
            tr("English"),
            tr("Simplified Chinese"),
        ];
        let language_dropdown = gtk::DropDown::from_strings(&language_labels);
        language_dropdown.set_selected(language_choice().index());
        language_dropdown.set_valign(gtk::Align::Center);
        language_row.add_suffix(&language_dropdown);
        language_group.add(&language_row);
        page.add(&language_group);

        {
            let ui = self.clone();
            language_dropdown.connect_selected_notify(move |dropdown| {
                let Some(choice) = LanguageChoice::from_index(dropdown.selected()) else {
                    return;
                };
                if choice == language_choice() {
                    return;
                }
                match save_language_choice(choice) {
                    Ok(()) => ui.toast(tr("Language saved. Restart Uperf Linux to apply it.")),
                    Err(error) => ui.toast(&format!(
                        "{}: {error}",
                        tr("Unable to save the language preference")
                    )),
                }
            });
        }
        page
    }

    fn build_logs_page(self: &Rc<Self>) -> adw::PreferencesPage {
        let page = new_prefs_page(tr("Logs"), "text-x-generic-symbolic");
        let group = adw::PreferencesGroup::builder()
            .title(tr("Service journal"))
            .build();

        let scroller = gtk::ScrolledWindow::new();
        scroller.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
        scroller.set_size_request(-1, 320);
        scroller.add_css_class("card");
        let log_view = gtk::TextView::with_buffer(&self.log_buffer);
        log_view.set_editable(false);
        log_view.set_monospace(true);
        log_view.set_wrap_mode(gtk::WrapMode::WordChar);
        log_view.set_margin_top(6);
        log_view.set_margin_bottom(6);
        log_view.set_margin_start(6);
        log_view.set_margin_end(6);
        scroller.set_child(Some(&log_view));
        group.add(&scroller);

        let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        hbox.set_halign(gtk::Align::Center);
        hbox.set_margin_top(8);
        let refresh = gtk::Button::with_label(tr("Refresh"));
        refresh.add_css_class("pill");
        let clear = gtk::Button::with_label(tr("Clear"));
        clear.add_css_class("pill");
        hbox.append(&refresh);
        hbox.append(&clear);
        group.add(&hbox);
        page.add(&group);

        {
            let ui = self.clone();
            refresh.connect_clicked(move |_| ui.refresh_logs());
        }
        {
            let buffer = self.log_buffer.clone();
            clear.connect_clicked(move |_| buffer.set_text(""));
        }
        page
    }

    fn add_daemon_control<W>(&self, widget: &W)
    where
        W: IsA<gtk::Widget> + Clone,
    {
        let widget = widget.clone().upcast::<gtk::Widget>();
        widget.set_sensitive(self.connected.get());
        self.daemon_controls.borrow_mut().push(widget);
    }

    fn set_connected(&self, connected: bool) {
        self.connected.set(connected);
        if !connected {
            self.pending_status.borrow_mut().take();
            self.pending_telemetry.borrow_mut().take();
        }
        for widget in self.daemon_controls.borrow().iter() {
            widget.set_sensitive(connected);
        }
        self.service_button.set_visible(!connected);
        self.service_button
            .set_sensitive(!connected && !self.service_action_running.get());
        // The card's daemon half changes meaning the moment the link does, and no
        // status arrives while disconnected to redraw it.
        self.update_focus_card(&self.focus_view());
    }

    fn update_connection(&self, state: ConnectionState) {
        match state {
            ConnectionState::Connecting => {
                self.set_connected(false);
                self.connection_row.set_subtitle(tr("Connecting…"));
            }
            ConnectionState::Connected => {
                self.set_connected(true);
                self.connection_row.set_subtitle(tr("Connected"));
            }
            ConnectionState::Reconnecting { delay, reason } => {
                self.set_connected(false);
                self.connection_row.set_subtitle(&format!(
                    "{} · {} {} · {reason}",
                    tr("Disconnected"),
                    tr("retrying in"),
                    format_retry_delay(delay)
                ));
            }
            ConnectionState::Unavailable(message) => {
                self.set_connected(false);
                self.connection_row
                    .set_subtitle(&format!("{} · {message}", tr("Unavailable")));
            }
        }
    }

    fn enable_and_start_service(&self) {
        if self.service_action_running.replace(true) {
            return;
        }
        self.service_button.set_sensitive(false);
        self.service_button.set_label(tr("Enabling…"));
        if let Err(error) = self.commands.try_send(ClientCommand::EnableAndStartService) {
            self.service_action_running.set(false);
            self.service_button.set_sensitive(true);
            self.service_button.set_label(tr("Enable & Start"));
            self.toast(&format!(
                "{}: {error}",
                tr("Unable to request service activation")
            ));
        }
    }

    fn submit_manual_workload(self: &Rc<Self>) {
        match parse_workload(self.pid_entry.text().as_str()) {
            Ok(request) => self.send(ClientCommand::SetWorkload(request)),
            Err(message) => self.toast(&translate_known(&message)),
        }
    }

    /// Run whichever single action the focus card is currently offering.
    ///
    /// Enabling the reporter talks to the session bus rather than the daemon, so
    /// it must not be gated on the daemon connection: a user whose daemon is
    /// down can still fix the reporter half of the problem.
    fn run_focus_action(&self) {
        match self.focus_action.get() {
            FocusAction::None => {}
            FocusAction::EnableReporter => {
                if let Err(error) = self.reporter_commands.try_send(ReporterCommand::Enable) {
                    self.toast(&format!(
                        "{}: {error}",
                        tr("Unable to enable the focus reporter")
                    ));
                }
            }
            FocusAction::ClearExplicit => {
                self.send(ClientCommand::ClearWorkload(WorkloadClearTarget::Explicit));
            }
        }
    }

    fn send(&self, command: ClientCommand) {
        if !self.connected.get() {
            self.toast(tr("The daemon is disconnected; wait for it to reconnect"));
            return;
        }
        if let Err(error) = self.commands.try_send(command) {
            eprintln!("cannot send GUI command: {error}");
        }
    }

    fn toast(&self, message: &str) {
        self.overlay
            .add_toast(adw::Toast::builder().title(message).timeout(5).build());
    }

    fn handle(self: &Rc<Self>, event: UiEvent) {
        match event {
            UiEvent::Snapshot {
                capabilities,
                status,
                rules,
                workloads,
            } => {
                *self.capabilities.borrow_mut() = capabilities;
                *self.status.borrow_mut() = status;
                *self.rules.borrow_mut() = rules;
                *self.workloads.borrow_mut() = workloads;
                self.rebuild_capability_widgets();
                self.update_status_widgets();
                self.rebuild_app_rules();
                self.rebuild_running_workloads();
            }
            UiEvent::Status(status) => {
                self.queue_runtime_update(Some(status), None);
            }
            UiEvent::Capabilities(capabilities) => {
                *self.capabilities.borrow_mut() = capabilities;
                self.rebuild_capability_widgets();
                self.update_status_widgets();
            }
            UiEvent::Telemetry(telemetry) => self.queue_runtime_update(None, Some(telemetry)),
            UiEvent::AppRules(rules) => {
                *self.rules.borrow_mut() = rules;
                self.rebuild_app_rules();
            }
            UiEvent::RunningWorkloads(workloads) => {
                *self.workloads.borrow_mut() = workloads;
                self.rebuild_running_workloads();
            }
            UiEvent::Notice(message) => self.toast(&message),
            UiEvent::Connection(state) => self.update_connection(state),
            UiEvent::RequestError { kind, message } => {
                self.toast(&format!("{}: {message}", kind.label()));
            }
            UiEvent::ServiceActivationStarted => {
                self.service_action_running.set(true);
                self.service_button.set_sensitive(false);
                self.service_button.set_label(tr("Enabling…"));
            }
            UiEvent::Reporter(reporter) => {
                if self.reporter.replace(reporter) != reporter {
                    self.update_status_widgets();
                }
            }
            UiEvent::ServiceActivationFinished(result) => {
                self.service_action_running.set(false);
                self.service_button.set_label(tr("Enable & Start"));
                self.service_button.set_sensitive(!self.connected.get());
                match result {
                    Ok(()) => self.toast(tr("Service started and enabled for boot")),
                    Err(message) => self.toast(&format!(
                        "{}: {message}",
                        tr("Service activation was cancelled or denied")
                    )),
                }
            }
        }
    }

    /// Rebuild all widgets whose shape depends on daemon-advertised capabilities:
    /// the mode toggle group, the per-target frequency labels, and the manual
    /// override rows.
    #[allow(clippy::too_many_lines)]
    fn rebuild_capability_widgets(self: &Rc<Self>) {
        let capabilities = self.capabilities.borrow().clone();
        let status = self.status.borrow().clone();
        let view = ViewModel::from_api(&capabilities, &status, self.reporter.get());

        // Mode toggle group.
        while let Some(child) = self.mode_box.first_child() {
            self.mode_box.remove(&child);
        }
        self.mode_buttons.borrow_mut().clear();
        self.mode_ids.borrow_mut().clear();
        let mut first: Option<gtk::ToggleButton> = None;
        for mode in &view.modes {
            let button = gtk::ToggleButton::with_label(&mode.label);
            button.set_tooltip_text(Some(&mode.description));
            button.set_hexpand(true);
            if let Some(anchor) = &first {
                button.set_group(Some(anchor));
            } else {
                first = Some(button.clone());
            }
            button.set_active(mode.selected);
            {
                let ui = self.clone();
                let id = mode.id.clone();
                button.connect_toggled(move |button| {
                    if ui.syncing_modes.get() || !button.is_active() {
                        return;
                    }
                    ui.send(ClientCommand::SetMode(id.clone()));
                });
            }
            self.mode_box.append(&button);
            self.mode_buttons
                .borrow_mut()
                .insert(mode.id.clone(), button);
            self.mode_ids.borrow_mut().push(mode.id.clone());
        }
        self.sync_mode_dropdown();

        // Dashboard per-target frequency labels.
        for row in self.freq_dynamic_rows.borrow_mut().drain(..) {
            self.freq_group.remove(&row);
        }
        self.freq_rows.borrow_mut().clear();
        for target in &view.targets {
            let row = adw::ActionRow::builder()
                .title(&target.capability.label)
                .build();
            let label = value_label();
            row.add_suffix(&label);
            self.freq_group.add(&row);
            self.freq_dynamic_rows.borrow_mut().push(row);
            self.freq_rows
                .borrow_mut()
                .insert(target.capability.id.clone(), label);
        }
        self.freq_group.set_visible(!view.targets.is_empty());

        // Frequency-page override rows.
        for row in self.override_dynamic_rows.borrow_mut().drain(..) {
            self.override_group.remove(&row);
        }
        self.target_status.borrow_mut().clear();
        self.override_ids.borrow_mut().clear();
        for target in &view.targets {
            self.add_override_row(target);
        }
        self.override_group.set_visible(!view.targets.is_empty());
    }

    #[allow(clippy::too_many_lines)]
    fn add_override_row(self: &Rc<Self>, target: &TargetView) {
        let capability = &target.capability;
        let display_name = target_display_name(capability);
        let row = adw::ExpanderRow::builder()
            .title(&display_name)
            .subtitle(target_status_text(target.status.as_ref()))
            .build();
        row.set_tooltip_text(Some(&format!("{} · {}", capability.id, capability.kind)));
        let mut restore_button = None;

        if capability.can_override && !target.choices_hz.is_empty() {
            self.override_ids.borrow_mut().push(capability.id.clone());
            let labels: Vec<String> = target
                .choices_hz
                .iter()
                .map(|frequency| format_frequency(*frequency))
                .collect();
            let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            let minimum = gtk::DropDown::from_strings(&label_refs);
            let maximum = gtk::DropDown::from_strings(&label_refs);
            minimum.set_tooltip_text(Some(tr("Minimum frequency")));
            maximum.set_tooltip_text(Some(tr("Maximum frequency")));

            let (selected_minimum, selected_maximum) =
                draft_frequency_bounds(capability, target.status.as_ref());
            minimum.set_selected(closest_index(&target.choices_hz, selected_minimum));
            maximum.set_selected(closest_index(&target.choices_hz, selected_maximum));

            let minimum_row = adw::ActionRow::builder()
                .title(tr("Minimum allowed frequency"))
                .subtitle(tr(
                    "The kernel will not select a lower frequency while active",
                ))
                .build();
            minimum.set_valign(gtk::Align::Center);
            minimum_row.add_suffix(&minimum);
            minimum_row.set_activatable_widget(Some(&minimum));
            row.add_row(&minimum_row);

            let maximum_row = adw::ActionRow::builder()
                .title(tr("Maximum allowed frequency"))
                .subtitle(tr(
                    "The kernel will not select a higher frequency while active",
                ))
                .build();
            maximum.set_valign(gtk::Align::Center);
            maximum_row.add_suffix(&maximum);
            maximum_row.set_activatable_widget(Some(&maximum));
            row.add_row(&maximum_row);

            let actions = adw::ActionRow::builder()
                .title(tr("New manual limits"))
                .subtitle(tr("Changes take effect only after you apply them"))
                .build();
            let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            buttons.set_valign(gtk::Align::Center);
            let restore = gtk::Button::with_label(tr("Restore automatic"));
            restore.set_sensitive(
                target
                    .status
                    .as_ref()
                    .is_some_and(|status| status.override_active),
            );
            let apply = gtk::Button::with_label(tr("Apply limits…"));
            apply.add_css_class("suggested-action");
            buttons.append(&restore);
            buttons.append(&apply);
            actions.add_suffix(&buttons);
            row.add_row(&actions);

            {
                let ui = self.clone();
                let target_id = capability.id.clone();
                restore.connect_clicked(move |_| {
                    ui.send(ClientCommand::ClearFrequency(target_id.clone()));
                });
            }
            {
                let ui = self.clone();
                let choices = target.choices_hz.clone();
                let capability = capability.clone();
                let display_name = display_name.clone();
                apply.connect_clicked(move |_| {
                    let minimum_index = usize::try_from(minimum.selected()).unwrap_or(usize::MAX);
                    let maximum_index = usize::try_from(maximum.selected()).unwrap_or(usize::MAX);
                    let Some(minimum_hz) = choices.get(minimum_index).copied() else {
                        ui.toast(&translate_known("Select a minimum frequency"));
                        return;
                    };
                    let Some(maximum_hz) = choices.get(maximum_index).copied() else {
                        ui.toast(&translate_known("Select a maximum frequency"));
                        return;
                    };
                    let request = match frequency_override(&capability, minimum_hz, maximum_hz) {
                        Ok(request) => request,
                        Err(message) => {
                            ui.toast(&translate_known(&message));
                            return;
                        }
                    };
                    ui.confirm_frequency(&display_name, request);
                });
            }
            restore_button = Some(restore);
        } else {
            row.set_enable_expansion(false);
        }
        self.target_status.borrow_mut().insert(
            capability.id.clone(),
            FrequencyOverrideCard {
                row: row.clone(),
                restore: restore_button,
            },
        );
        self.override_group.add(&row);
        self.override_dynamic_rows.borrow_mut().push(row);
    }

    fn sync_mode_dropdown(&self) {
        let ids = self.mode_ids.borrow();
        let capabilities = self.capabilities.borrow();
        let labels: Vec<String> = capabilities
            .modes
            .iter()
            .map(|mode| localized_mode_label(&mode.id, &mode.display_name))
            .collect();
        let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let model = gtk::StringList::new(&label_refs);
        self.rule_mode_dropdown.set_model(Some(&model));
        if !ids.is_empty() {
            self.rule_mode_dropdown.set_selected(0);
        }
    }

    fn update_status_widgets(&self) {
        let capabilities = self.capabilities.borrow().clone();
        let status = self.status.borrow().clone();
        let view = ViewModel::from_api(&capabilities, &status, self.reporter.get());

        self.state_row.set_subtitle(&view.daemon_state);
        self.health_row.set_subtitle(&view.health.summary);
        self.rebuild_health_issues(&view.health.issues);
        self.profile_row.set_subtitle(&view.profile);
        self.scene_row.set_subtitle(&view.scene);
        self.update_focus_card(&self.focus_view());

        // Keep the mode toggle group in sync without re-triggering commands.
        self.syncing_modes.set(true);
        for (mode_id, button) in self.mode_buttons.borrow().iter() {
            button.set_active(*mode_id == status.mode);
        }
        self.syncing_modes.set(false);

        for target in &view.targets {
            let text = target_status_text(target.status.as_ref());
            if let Some(card) = self.target_status.borrow().get(&target.capability.id) {
                card.row.set_subtitle(&text);
                if let Some(restore) = &card.restore {
                    restore.set_sensitive(
                        self.connected.get()
                            && target
                                .status
                                .as_ref()
                                .is_some_and(|status| status.override_active),
                    );
                }
            }
            if let Some(label) = self.freq_rows.borrow().get(&target.capability.id) {
                label.set_text(&text);
            }
        }
        self.restore_all_button.set_sensitive(
            self.connected.get()
                && view.targets.iter().any(|target| {
                    target
                        .status
                        .as_ref()
                        .is_some_and(|status| status.override_active)
                }),
        );

        if let Some(thermal) = view.thermal {
            self.thermal_group.set_visible(true);
            self.thermal_row.set_title(&thermal.temperature);
            self.thermal_row
                .set_subtitle(&format!("{} · {}", thermal.state, thermal.detail));
            self.thermal_bar.set_fraction(thermal_fraction(
                status.thermal.max_temperature_millicelsius,
            ));
        } else {
            self.thermal_group.set_visible(false);
        }

        if let Some(workload) = view.workload {
            self.workload_group.set_visible(true);
            let active = workload.active;
            self.update_clear_workload_button(&active);
            if active.present {
                let mut details = vec![
                    active.name,
                    format!("PID {}", active.identity.pid),
                    localized_protocol_value(&active.effective_mode),
                ];
                if !active.source.is_empty() {
                    details.push(format!(
                        "{}: {}",
                        tr("Source"),
                        localized_protocol_value(&active.source)
                    ));
                }
                self.workload_row.set_subtitle(&details.join(" · "));
            } else {
                self.workload_row.set_subtitle(tr("None"));
            }
        } else {
            self.workload_group.set_visible(false);
            self.clear_workload_button.set_sensitive(false);
        }
    }

    /// The focus card's current content.
    ///
    /// While the daemon is unreachable its capabilities are empty, which is
    /// indistinguishable from a daemon that switched focus off, so the card falls
    /// back to the reporter half rather than blaming a policy key.
    fn focus_view(&self) -> FocusView {
        if self.connected.get() {
            ViewModel::from_api(
                &self.capabilities.borrow(),
                &self.status.borrow(),
                self.reporter.get(),
            )
            .focus
        } else {
            FocusView::disconnected(self.reporter.get())
        }
    }

    /// Render the focus card from the view model, which has already decided both
    /// the wording and the single action; nothing here re-derives intent.
    fn update_focus_card(&self, focus: &FocusView) {
        self.focus_row.set_title(&focus.summary);
        self.focus_row.set_subtitle(&focus.detail);
        self.focus_icon.set_icon_name(Some(focus_icon(focus.state)));
        // Only a working focus path gets the accent colour, so "looks enabled but
        // is not" cannot be mistaken for "working" at a glance.
        for (class, wanted) in [
            ("success", focus.state == FocusState::Following),
            ("warning", focus_is_obstructed(focus.state)),
        ] {
            if wanted {
                self.focus_icon.add_css_class(class);
            } else {
                self.focus_icon.remove_css_class(class);
            }
        }

        if let Some(holder) = &focus.holder {
            self.focus_holder_row.set_visible(true);
            self.focus_holder_row.set_subtitle(holder);
        } else {
            self.focus_holder_row.set_visible(false);
        }

        let action = focus.action();
        self.focus_action.set(action);
        match action {
            FocusAction::None => self.focus_button.set_visible(false),
            FocusAction::EnableReporter => {
                self.focus_button.set_visible(true);
                self.focus_button.set_label(tr("Turn on"));
                self.focus_button.add_css_class("suggested-action");
                // Session-bus work, so it stays usable while the daemon is down.
                self.focus_button.set_sensitive(true);
            }
            FocusAction::ClearExplicit => {
                self.focus_button.set_visible(true);
                self.focus_button.set_label(tr("Follow focus again"));
                self.focus_button.remove_css_class("suggested-action");
                self.focus_button.set_sensitive(self.connected.get());
            }
        }

        // Show the command whenever one exists: the button covers the happy path,
        // the text covers a broken or absent `gnome-extensions` D-Bus surface.
        if let Some(command) = &focus.command {
            self.focus_command_row.set_visible(true);
            self.focus_command_label.set_text(command);
        } else {
            self.focus_command_row.set_visible(false);
        }
    }

    fn update_clear_workload_button(&self, active: &ActiveWorkload) {
        let target = clear_target_for_workload(active);
        self.clear_workload_button.set_sensitive(target.is_some());
        self.clear_workload_button.set_label(match target {
            Some(WorkloadClearTarget::Explicit) => tr("Clear explicit workload"),
            Some(WorkloadClearTarget::Focus) => tr("Clear focused workload"),
            None => tr("Clear active workload"),
        });
    }

    fn rebuild_health_issues(&self, issues: &[view_model::HealthIssueView]) {
        for row in self.health_issue_rows.borrow_mut().drain(..) {
            self.health_issues_group.remove(&row);
        }
        self.health_issues_group.set_visible(!issues.is_empty());
        for issue in issues {
            let row = adw::ActionRow::builder()
                .title(&issue.message)
                .subtitle(&issue.detail)
                .build();
            self.health_issues_group.add(&row);
            self.health_issue_rows.borrow_mut().push(row);
        }
    }

    /// Coalesce high-rate daemon events into one latest-value GUI repaint.
    ///
    /// Status and telemetry are separate D-Bus contracts and may arrive in
    /// either order. Applying status first and telemetry second preserves the
    /// newest high-rate temperature/frequency observations without redrawing
    /// unrelated cards for every signal.
    fn queue_runtime_update(
        self: &Rc<Self>,
        status: Option<DaemonStatus>,
        telemetry: Option<TelemetrySnapshot>,
    ) {
        if let Some(status) = status {
            *self.pending_status.borrow_mut() = Some(status);
        }
        if let Some(telemetry) = telemetry {
            *self.pending_telemetry.borrow_mut() = Some(telemetry);
        }
        if self.runtime_refresh_scheduled.replace(true) {
            return;
        }
        let ui = self.clone();
        glib::timeout_add_local_once(GUI_REFRESH_INTERVAL, move || {
            ui.runtime_refresh_scheduled.set(false);
            ui.apply_runtime_update();
        });
    }

    fn apply_runtime_update(&self) {
        let pending_status = self.pending_status.borrow_mut().take();
        let pending_telemetry = self.pending_telemetry.borrow_mut().take();
        if pending_status.is_none() && pending_telemetry.is_none() {
            return;
        }
        if let Some(status) = pending_status {
            *self.status.borrow_mut() = status;
        }
        if let Some(telemetry) = pending_telemetry {
            self.rebuild_cpu_loads(&telemetry.cpu_loads);
            let mut status = self.status.borrow_mut();
            status.thermal = telemetry.thermal;
            status.frequencies = telemetry.frequencies;
        }
        self.update_status_widgets();
    }

    /// Update per-CPU utilization rows keyed by sparse kernel CPU IDs.
    fn rebuild_cpu_loads(&self, loads: &[uperf_api::CpuLoad]) {
        let mut rows = self.load_rows.borrow_mut();
        let live_cpus = loads
            .iter()
            .map(|load| load.cpu_id)
            .collect::<BTreeSet<_>>();
        let stale_cpus = rows
            .keys()
            .filter(|cpu| !live_cpus.contains(cpu))
            .copied()
            .collect::<Vec<_>>();
        for cpu in stale_cpus {
            if let Some(load_row) = rows.remove(&cpu) {
                self.load_group.remove(&load_row.row);
            }
        }
        for load in loads {
            let load_row = rows.entry(load.cpu_id).or_insert_with(|| {
                let row = adw::ActionRow::builder()
                    .title(format!("CPU {}", load.cpu_id))
                    .build();
                let value = value_label();
                row.add_suffix(&value);
                self.load_group.add(&row);
                CpuLoadRow { row, value }
            });
            load_row
                .value
                .set_text(&format!("{:.0} %", cpu_load_percent(*load)));
        }
        self.load_group.set_visible(!rows.is_empty());
    }

    fn confirm_frequency(self: &Rc<Self>, display_name: &str, request: FrequencyOverride) {
        let dialog = gtk::AlertDialog::builder()
            .modal(true)
            .message(tr("Apply manual frequency limits?"))
            .detail(format!(
                "{}: {} – {}. {}",
                display_name,
                format_frequency(request.min_hz),
                format_frequency(request.max_hz),
                tr("The kernel still selects the actual frequency, while thermal and hardware limits remain authoritative.")
            ))
            .buttons([tr("Cancel"), tr("Apply limits")])
            .cancel_button(0)
            .default_button(0)
            .build();
        let ui = self.clone();
        glib::spawn_future_local(async move {
            if dialog.choose_future(Some(&ui.window)).await == Ok(1) {
                ui.send(ClientCommand::SetFrequency(request));
            }
        });
    }

    #[allow(clippy::too_many_lines)]
    fn rebuild_app_rules(self: &Rc<Self>) {
        for row in self.app_rows.borrow_mut().drain(..) {
            self.apps_group.remove(&row);
        }

        let rules = self.rules.borrow().clone();
        self.apps_placeholder.set_visible(rules.is_empty());

        let mode_ids = self.mode_ids.borrow().clone();
        let mode_labels: Vec<String> = {
            let capabilities = self.capabilities.borrow();
            capabilities
                .modes
                .iter()
                .map(|mode| localized_mode_label(&mode.id, &mode.display_name))
                .collect()
        };
        let label_refs: Vec<&str> = mode_labels.iter().map(String::as_str).collect();

        for rule in &rules {
            let matcher = match (&rule.executable, &rule.comm_regex) {
                (Some(exe), Some(comm)) => format!("{exe} · /{comm}/"),
                (Some(exe), None) => exe.clone(),
                (None, Some(comm)) => format!("/{comm}/"),
                (None, None) => tr("any process").to_owned(),
            };
            let row = adw::ActionRow::builder()
                .title(&rule.id)
                .subtitle(format!("{matcher} · {} {}", tr("priority"), rule.priority))
                .build();
            row.add_prefix(&gtk::Image::from_icon_name("applications-games-symbolic"));

            let controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            controls.set_valign(gtk::Align::Center);

            let dropdown = gtk::DropDown::from_strings(&label_refs);
            if let Some(index) = mode_ids.iter().position(|id| *id == rule.mode)
                && let Ok(index) = u32::try_from(index)
            {
                dropdown.set_selected(index);
            }
            {
                let ui = self.clone();
                let mode_ids = mode_ids.clone();
                let rule = rule.clone();
                dropdown.connect_selected_notify(move |dropdown| {
                    let index = usize::try_from(dropdown.selected()).unwrap_or(usize::MAX);
                    let Some(mode) = mode_ids.get(index) else {
                        return;
                    };
                    if *mode == rule.mode {
                        return;
                    }
                    let mut updated = rule.clone();
                    updated.mode.clone_from(mode);
                    ui.send(ClientCommand::SetAppRule(updated));
                });
            }
            controls.append(&dropdown);

            let enabled = gtk::Switch::new();
            enabled.set_active(rule.enabled);
            enabled.set_valign(gtk::Align::Center);
            enabled.set_tooltip_text(Some(tr("Enable rule")));
            {
                let ui = self.clone();
                let rule = rule.clone();
                enabled.connect_state_set(move |_, state| {
                    if state != rule.enabled {
                        let mut updated = rule.clone();
                        updated.enabled = state;
                        ui.send(ClientCommand::SetAppRule(updated));
                    }
                    glib::Propagation::Proceed
                });
            }
            controls.append(&enabled);

            let remove = gtk::Button::from_icon_name("user-trash-symbolic");
            remove.add_css_class("flat");
            remove.set_tooltip_text(Some(tr("Remove rule")));
            {
                let ui = self.clone();
                let id = rule.id.clone();
                remove.connect_clicked(move |_| {
                    ui.send(ClientCommand::RemoveAppRule(id.clone()));
                });
            }
            controls.append(&remove);

            row.add_suffix(&controls);
            self.apps_group.add(&row);
            self.app_rows.borrow_mut().push(row.upcast());
        }
    }

    fn rebuild_running_workloads(self: &Rc<Self>) {
        for row in self.running_rows.borrow_mut().drain(..) {
            self.running_group.remove(&row);
        }

        let workloads = self.workloads.borrow().clone();
        self.running_placeholder.set_visible(workloads.is_empty());
        self.running_group.set_visible(
            self.capabilities
                .borrow()
                .supports(feature::RUNNING_WORKLOADS)
                || !workloads.is_empty(),
        );

        let active = workloads.iter().find(|workload| workload.active);
        if let Some(workload) = active {
            let scheduler = &workload.scheduler;
            self.scheduler_row
                .set_subtitle(&scheduler_status_text(scheduler));
            self.cgroup_row.set_subtitle(&cgroup_status_text(scheduler));
        } else {
            self.scheduler_row.set_subtitle(tr("No active workload"));
            self.cgroup_row.set_subtitle(tr("No active workload"));
        }

        for workload in workloads {
            let row = adw::ActionRow::builder()
                .title(&workload.name)
                .subtitle(running_workload_subtitle(&workload))
                .build();
            row.add_prefix(&gtk::Image::from_icon_name("applications-games-symbolic"));

            let activate = if workload.active {
                let button = gtk::Button::with_label(tr("Active"));
                button.set_sensitive(false);
                button
            } else {
                let button = gtk::Button::with_label(tr("Use"));
                button.add_css_class("suggested-action");
                let ui = self.clone();
                let pid = workload.identity.pid;
                button.connect_clicked(move |_| {
                    ui.send(ClientCommand::SetWorkload(workload_request(pid)));
                });
                button
            };
            activate.set_valign(gtk::Align::Center);
            row.add_suffix(&activate);
            self.running_group.add(&row);
            self.running_rows.borrow_mut().push(row.upcast());
        }
    }

    fn submit_new_rule(self: &Rc<Self>) {
        let executable = non_empty(self.rule_exe_entry.text().as_str());
        let comm_regex = non_empty(self.rule_comm_entry.text().as_str());
        if executable.is_none() && comm_regex.is_none() {
            self.toast(&translate_known(
                "Provide an executable path or a process-name regex",
            ));
            return;
        }
        let mode_index = usize::try_from(self.rule_mode_dropdown.selected()).unwrap_or(usize::MAX);
        let Some(mode) = self.mode_ids.borrow().get(mode_index).cloned() else {
            self.toast(&translate_known("Select a mode for the rule"));
            return;
        };
        let rule = AppRule {
            id: generate_rule_id(&self.rules.borrow()),
            enabled: true,
            owner_uid: u32::MAX,
            executable,
            comm_regex,
            mode,
            priority: 10,
        };
        self.send(ClientCommand::SetAppRule(rule));
        self.rule_exe_entry.set_text("");
        self.rule_comm_entry.set_text("");
    }

    fn refresh_logs(self: &Rc<Self>) {
        let buffer = self.log_buffer.clone();
        let launcher = gtk::gio::SubprocessLauncher::new(
            gtk::gio::SubprocessFlags::STDOUT_PIPE | gtk::gio::SubprocessFlags::STDERR_MERGE,
        );
        let process = launcher.spawn(&[
            std::ffi::OsStr::new("journalctl"),
            std::ffi::OsStr::new("-u"),
            std::ffi::OsStr::new(SERVICE_UNIT),
            std::ffi::OsStr::new("-n"),
            std::ffi::OsStr::new("200"),
            std::ffi::OsStr::new("--no-pager"),
        ]);
        let process = match process {
            Ok(process) => process,
            Err(error) => {
                buffer.set_text(&format!("{}: {error}", tr("Unable to start journalctl")));
                return;
            }
        };
        glib::spawn_future_local(async move {
            match process.communicate_utf8_future(None).await {
                Ok((Some(output), _)) if !output.is_empty() => buffer.set_text(&output),
                Ok(_) => buffer.set_text(tr("(journal is empty)")),
                Err(error) => {
                    buffer.set_text(&format!("{}: {error}", tr("Unable to read journal")));
                }
            }
        });
    }
}

fn parse_workload(pid: &str) -> Result<WorkloadRequest, String> {
    let pid = pid
        .trim()
        .parse::<u32>()
        .map_err(|_| "PID must be a positive integer")?;
    if pid == 0 {
        return Err("PID must be non-zero".into());
    }
    Ok(workload_request(pid))
}

fn workload_request(pid: u32) -> WorkloadRequest {
    WorkloadRequest {
        pid,
        mode: String::new(),
        reason: "selected in uperf-gui".into(),
    }
}

/// Icon for one focus state, using only names from the freedesktop symbolic set
/// that GNOME ships, so no icon falls back to a missing-image placeholder.
const fn focus_icon(state: FocusState) -> &'static str {
    match state {
        FocusState::Following => "emblem-ok-symbolic",
        FocusState::Overridden => "media-playback-pause-symbolic",
        FocusState::Rejected => "dialog-warning-symbolic",
        FocusState::Waiting => "content-loading-symbolic",
        FocusState::Unsupported => "action-unavailable-symbolic",
    }
}

/// Whether the state is one of the "looks enabled but is not" cases that need to
/// read as a problem rather than as a neutral wait.
const fn focus_is_obstructed(state: FocusState) -> bool {
    matches!(
        state,
        FocusState::Unsupported | FocusState::Rejected | FocusState::Overridden
    )
}

fn clear_target_for_workload(active: &ActiveWorkload) -> Option<WorkloadClearTarget> {
    if !active.present {
        return None;
    }
    match active.source.as_str() {
        "explicit" => Some(WorkloadClearTarget::Explicit),
        "focus" => Some(WorkloadClearTarget::Focus),
        _ => None,
    }
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Produce a short unique rule ID that satisfies the daemon's identifier rules
/// (ASCII alphanumeric plus `-_.`, first byte alphanumeric, <= 64 bytes).
fn generate_rule_id(existing: &[AppRule]) -> String {
    for index in 1..=u32::MAX {
        let candidate = format!("gui.rule{index}");
        if existing.iter().all(|rule| rule.id != candidate) {
            return candidate;
        }
    }
    "gui.rule".to_owned()
}

fn target_status_text(status: Option<&FrequencyStatus>) -> String {
    status.map_or_else(
        || tr("No fresh state").into(),
        |status| {
            let management = if status.override_active {
                tr("Manual limit active")
            } else {
                tr("Automatic control")
            };
            if status.observed_available && !status.stale {
                return format!(
                    "{management} · {} {} – {}",
                    tr("Current allowed range"),
                    format_frequency(status.observed_min_hz),
                    format_frequency(status.observed_max_hz),
                );
            }
            if status.applied_verified {
                return format!(
                    "{management} · {} {} – {} · {}",
                    tr("Last confirmed range"),
                    format_frequency(status.applied_min_hz),
                    format_frequency(status.applied_max_hz),
                    tr("stale"),
                );
            }
            tr("State unavailable · not managed").to_owned()
        },
    )
}

fn draft_frequency_bounds(
    target: &TargetCapability,
    status: Option<&FrequencyStatus>,
) -> (u64, u64) {
    status
        .filter(|status| status.override_active && status.desired_available)
        .map_or((target.minimum_hz, target.maximum_hz), |status| {
            (status.desired_min_hz, status.desired_max_hz)
        })
}

fn target_display_name(target: &TargetCapability) -> String {
    if target.kind == "cpufreq" && !target.cpus.is_empty() {
        return format!("CPU {}", compact_cpu_list(&target.cpus));
    }
    if target.id == "gpu" {
        return "GPU".to_owned();
    }
    target.label.clone()
}

fn compact_cpu_list(cpus: &[u32]) -> String {
    let mut cpus = cpus.to_vec();
    cpus.sort_unstable();
    cpus.dedup();
    let mut ranges = Vec::new();
    let mut start = cpus[0];
    let mut end = start;
    for cpu in cpus.into_iter().skip(1) {
        if cpu == end.saturating_add(1) {
            end = cpu;
            continue;
        }
        ranges.push(format_cpu_range(start, end));
        start = cpu;
        end = cpu;
    }
    ranges.push(format_cpu_range(start, end));
    ranges.join(", ")
}

fn format_cpu_range(start: u32, end: u32) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}–{end}")
    }
}

fn format_frequency(frequency_hz: u64) -> String {
    // Integer arithmetic keeps exact hertz and avoids lossy float casts: GHz is
    // shown to two decimals (hundredths of a GHz = 10 MHz), MHz to whole units.
    if frequency_hz >= 1_000_000_000 {
        let centi_ghz = frequency_hz / 10_000_000;
        format!("{}.{:02} GHz", centi_ghz / 100, centi_ghz % 100)
    } else if frequency_hz >= 1_000_000 {
        format!("{} MHz", frequency_hz / 1_000_000)
    } else {
        format!("{frequency_hz} Hz")
    }
}

/// Map a temperature in millidegrees Celsius onto a 40–100 °C progress bar,
/// matching the fork's dashboard gauge.
fn thermal_fraction(millicelsius: i32) -> f64 {
    if millicelsius <= 0 {
        return 0.0;
    }
    let celsius = f64::from(millicelsius) / 1_000.0;
    ((celsius - 40.0) / 60.0).clamp(0.0, 1.0)
}

fn closest_index(choices: &[u64], requested: u64) -> u32 {
    choices
        .iter()
        .enumerate()
        .min_by_key(|(_, choice)| choice.abs_diff(requested))
        .and_then(|(index, _)| u32::try_from(index).ok())
        .unwrap_or(gtk::INVALID_LIST_POSITION)
}

fn format_retry_delay(delay: Duration) -> String {
    if delay < Duration::from_secs(1) {
        format!("{} {}", delay.as_millis(), tr("ms"))
    } else {
        format!("{} {}", delay.as_secs(), tr("s"))
    }
}

fn running_workload_subtitle(workload: &RunningWorkload) -> String {
    let source = if workload.matched_pattern == "active" {
        tr("explicit active workload").to_owned()
    } else {
        format!("{} {}", tr("matched"), workload.matched_pattern)
    };
    let mut details = vec![format!("PID {}", workload.identity.pid), source];
    if workload.active {
        details.push(tr("active").to_owned());
        let scheduler = scheduler_status_text(&workload.scheduler);
        if scheduler != tr("No active workload") {
            details.push(scheduler);
        }
    }
    details.join(" · ")
}

fn scheduler_status_text(status: &SchedulerStatus) -> String {
    if !status.enabled {
        return tr("Disabled by policy").to_owned();
    }
    if status.matched_rule.is_empty() {
        return if status.warning.is_empty() {
            tr("Pending or no matching scheduler rule").to_owned()
        } else {
            format!("{} · {}", tr("No applied rule"), status.warning)
        };
    }
    let mut text = format!(
        "{} {} · {}/{} {}",
        tr("Rule"),
        status.matched_rule,
        status.applied_tasks,
        status.managed_tasks,
        tr("tasks applied")
    );
    if !status.warning.is_empty() {
        text.push_str(" · ");
        text.push_str(&status.warning);
    }
    text
}

fn cgroup_status_text(status: &SchedulerStatus) -> String {
    if !status.enabled {
        return tr("Disabled by policy").to_owned();
    }
    if status.systemd_unit.is_empty() {
        return if status.cgroup_class.is_empty() {
            tr("No dedicated unit selected").to_owned()
        } else {
            format!(
                "{} {} · {}",
                tr("Class"),
                status.cgroup_class,
                tr("no dedicated unit")
            )
        };
    }
    let state = if status.cgroup_applied {
        tr("applied")
    } else {
        tr("not applied")
    };
    if status.cgroup_class.is_empty() {
        format!("{} · {state}", status.systemd_unit)
    } else {
        format!(
            "{} {} · {} · {state}",
            tr("Class"),
            status.cgroup_class,
            status.systemd_unit
        )
    }
}

fn classify_client_error(error: &ClientError) -> ErrorDisposition {
    match error {
        ClientError::Transport(_) => ErrorDisposition::ConnectionLost,
        ClientError::IncompatibleApi { .. } => {
            ErrorDisposition::Request(RequestErrorKind::IncompatibleApi)
        }
        ClientError::InvalidRequest(_) => {
            ErrorDisposition::Request(RequestErrorKind::InvalidRequest)
        }
        ClientError::Remote { name, .. } if is_connection_error_name(name) => {
            ErrorDisposition::ConnectionLost
        }
        ClientError::Remote { name, .. } if name.ends_with(".NotAuthorized") => {
            ErrorDisposition::Request(RequestErrorKind::NotAuthorized)
        }
        ClientError::Remote { name, .. }
            if name.ends_with(".InvalidArgument") || name.ends_with(".ValidationFailed") =>
        {
            ErrorDisposition::Request(RequestErrorKind::InvalidRequest)
        }
        ClientError::Remote { .. } => ErrorDisposition::Request(RequestErrorKind::Rejected),
    }
}

fn is_connection_error_name(name: &str) -> bool {
    matches!(
        name,
        "org.freedesktop.DBus.Error.ServiceUnknown"
            | "org.freedesktop.DBus.Error.NameHasNoOwner"
            | "org.freedesktop.DBus.Error.NoReply"
            | "org.freedesktop.DBus.Error.Disconnected"
            | "org.freedesktop.DBus.Error.TimedOut"
    )
}

async fn emit(sender: &Sender<UiEvent>, event: UiEvent) -> bool {
    sender.send(event).await.is_ok()
}

async fn enable_and_start_system_service(events: &Sender<UiEvent>) -> bool {
    if !emit(events, UiEvent::ServiceActivationStarted).await {
        return false;
    }
    let result = async {
        let connection = zbus::Connection::system().await?;
        let manager = zbus::Proxy::new(
            &connection,
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
        )
        .await?;

        let changes: Option<UnitFileChanges> = manager
            .call_with_flags(
                "EnableUnitFiles",
                zbus::proxy::MethodFlags::AllowInteractiveAuth.into(),
                &(vec![SERVICE_UNIT], false, false),
            )
            .await?;
        if changes.is_none() {
            return Err(zbus::Error::Failure(
                "systemd returned no EnableUnitFiles reply".into(),
            ));
        }

        let job: Option<zbus::zvariant::OwnedObjectPath> = manager
            .call_with_flags(
                "StartUnit",
                zbus::proxy::MethodFlags::AllowInteractiveAuth.into(),
                &(SERVICE_UNIT, "replace"),
            )
            .await?;
        if job.is_none() {
            return Err(zbus::Error::Failure(
                "systemd returned no StartUnit reply".into(),
            ));
        }

        let unit_path: zbus::zvariant::OwnedObjectPath =
            manager.call("GetUnit", &(SERVICE_UNIT)).await?;
        let unit = zbus::Proxy::new(
            &connection,
            "org.freedesktop.systemd1",
            unit_path.as_str(),
            "org.freedesktop.systemd1.Unit",
        )
        .await?;
        for _ in 0..50 {
            let state: String = unit.get_property("ActiveState").await?;
            match state.as_str() {
                "active" => return Ok::<(), zbus::Error>(()),
                "failed" => {
                    return Err(zbus::Error::Failure(
                        "uperf-linux.service failed to start".into(),
                    ));
                }
                _ => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
        Err(zbus::Error::Failure(
            "timed out waiting for uperf-linux.service to become active".into(),
        ))
    }
    .await
    .map_err(|error| error.to_string());

    let succeeded = result.is_ok();
    emit(events, UiEvent::ServiceActivationFinished(result)).await;
    succeeded
}

async fn report_client_error(events: &Sender<UiEvent>, error: ClientError) -> Result<(), String> {
    let disposition = classify_client_error(&error);
    let message = error.to_string();
    match disposition {
        ErrorDisposition::ConnectionLost => Err(message),
        ErrorDisposition::Request(kind) => {
            emit(events, UiEvent::RequestError { kind, message }).await;
            Ok(())
        }
    }
}

async fn refresh_status(client: &DaemonClient, events: &Sender<UiEvent>) -> Result<(), String> {
    match client.status().await {
        Ok(status) => {
            emit(events, UiEvent::Status(status)).await;
            Ok(())
        }
        Err(error) => report_client_error(events, error).await,
    }
}

async fn refresh_app_rules(client: &DaemonClient, events: &Sender<UiEvent>) -> Result<(), String> {
    match client.app_rules().await {
        Ok(rules) => {
            emit(events, UiEvent::AppRules(rules)).await;
            Ok(())
        }
        Err(error) => report_client_error(events, error).await,
    }
}

async fn refresh_running_workloads(
    client: &DaemonClient,
    events: &Sender<UiEvent>,
) -> Result<(), String> {
    match client.running_workloads().await {
        Ok(workloads) => {
            emit(events, UiEvent::RunningWorkloads(workloads)).await;
            Ok(())
        }
        Err(error) => report_client_error(events, error).await,
    }
}

async fn handle_command(
    client: &DaemonClient,
    events: &Sender<UiEvent>,
    command: ClientCommand,
) -> Result<(), String> {
    let command = match command {
        ClientCommand::EnableAndStartService => {
            enable_and_start_system_service(events).await;
            return Ok(());
        }
        command => command,
    };
    let mut refresh_rules = false;
    let mut refresh_workloads = false;
    let result = match command {
        ClientCommand::SetMode(mode) => client.set_mode(&mode).await.map(|receipt| receipt.message),
        ClientCommand::SetFrequency(request) => client
            .set_frequency_overrides(vec![request])
            .await
            .map(|receipt| receipt.message),
        ClientCommand::ClearFrequency(target_id) => client
            .clear_frequency_overrides(vec![target_id])
            .await
            .map(|receipt| receipt.message),
        ClientCommand::ClearAllFrequency(target_ids) => client
            .clear_frequency_overrides(target_ids)
            .await
            .map(|receipt| receipt.message),
        ClientCommand::SetWorkload(request) => {
            refresh_workloads = true;
            client
                .set_active_workload(request)
                .await
                .map(|receipt| receipt.message)
        }
        ClientCommand::ClearWorkload(target) => {
            refresh_workloads = true;
            match target {
                WorkloadClearTarget::Explicit => client.clear_active_workload().await,
                WorkloadClearTarget::Focus => client.clear_foreground_process().await,
            }
            .map(|receipt| receipt.message)
        }
        ClientCommand::SetAppRule(rule) => {
            refresh_rules = true;
            client
                .set_app_rule(rule)
                .await
                .map(|receipt| receipt.message)
        }
        ClientCommand::RemoveAppRule(rule_id) => {
            refresh_rules = true;
            client
                .remove_app_rule(&rule_id)
                .await
                .map(|receipt| receipt.message)
        }
        ClientCommand::ReloadConfig => client.reload_config().await.map(|report| report.message),
        ClientCommand::EnableAndStartService => unreachable!("handled before daemon commands"),
    };
    match result {
        Ok(message) => {
            emit(events, UiEvent::Notice(message)).await;
            refresh_status(client, events).await?;
            if refresh_rules {
                refresh_app_rules(client, events).await?;
            }
            if refresh_workloads {
                refresh_running_workloads(client, events).await?;
            }
            Ok(())
        }
        Err(error) => {
            report_client_error(events, error).await?;
            refresh_status(client, events).await?;
            if refresh_rules {
                refresh_app_rules(client, events).await?;
            }
            if refresh_workloads {
                refresh_running_workloads(client, events).await?;
            }
            Ok(())
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn client_session(
    commands: &Receiver<ClientCommand>,
    events: &Sender<UiEvent>,
    connected: &mut bool,
) -> Result<(), String> {
    let client = DaemonClient::system()
        .await
        .map_err(|error| error.to_string())?;
    let (capabilities, status) = match tokio::try_join!(client.capabilities(), client.status()) {
        Ok(snapshot) => snapshot,
        Err(error) => match classify_client_error(&error) {
            ErrorDisposition::ConnectionLost => return Err(error.to_string()),
            ErrorDisposition::Request(kind) => {
                let message = error.to_string();
                emit(
                    events,
                    UiEvent::RequestError {
                        kind,
                        message: message.clone(),
                    },
                )
                .await;
                emit(
                    events,
                    UiEvent::Connection(ConnectionState::Unavailable(message)),
                )
                .await;
                return Ok(());
            }
        },
    };
    let rules = match client.app_rules().await {
        Ok(rules) => rules,
        Err(error) => {
            report_client_error(events, error).await?;
            Vec::new()
        }
    };
    let workloads = if capabilities.supports(feature::RUNNING_WORKLOADS) {
        match client.running_workloads().await {
            Ok(workloads) => workloads,
            Err(error) => {
                report_client_error(events, error).await?;
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    let proxy = client.proxy().await.map_err(|error| error.to_string())?;
    let mut state_signals = proxy
        .receive_state_changed()
        .await
        .map_err(|error| error.to_string())?;
    let mut capability_signals = proxy
        .receive_capabilities_changed()
        .await
        .map_err(|error| error.to_string())?;
    let mut health_signals = proxy
        .receive_health_changed()
        .await
        .map_err(|error| error.to_string())?;
    let mut telemetry_signals = proxy
        .receive_telemetry_updated()
        .await
        .map_err(|error| error.to_string())?;
    let mut workload_signals = proxy
        .receive_running_workloads_changed()
        .await
        .map_err(|error| error.to_string())?;
    let mut mode_properties = proxy.receive_mode_changed().await;
    let mut owner_changes = proxy
        .inner()
        .receive_owner_changed()
        .await
        .map_err(|error| error.to_string())?;

    if !emit(
        events,
        UiEvent::Snapshot {
            capabilities,
            status,
            rules,
            workloads,
        },
    )
    .await
    {
        return Ok(());
    }
    *connected = true;
    if !emit(events, UiEvent::Connection(ConnectionState::Connected)).await {
        return Ok(());
    }

    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Ok(command) => handle_command(&client, events, command).await?,
                Err(_) => return Ok(()),
            },
            signal = state_signals.next() => {
                if signal.is_none() {
                    return Err("state signal stream ended".into());
                }
                refresh_status(&client, events).await?;
            },
            signal = capability_signals.next() => {
                if signal.is_none() {
                    return Err("capability signal stream ended".into());
                }
                match client.capabilities().await {
                    Ok(capabilities) => {
                        emit(events, UiEvent::Capabilities(capabilities)).await;
                    }
                    Err(error) => {
                        report_client_error(events, error).await?;
                    }
                }
                refresh_app_rules(&client, events).await?;
                refresh_running_workloads(&client, events).await?;
            },
            signal = health_signals.next() => {
                if signal.is_none() {
                    return Err("health signal stream ended".into());
                }
                refresh_status(&client, events).await?;
            },
            signal = telemetry_signals.next() => {
                let Some(signal) = signal else {
                    return Err("telemetry signal stream ended".into());
                };
                match signal.args() {
                    Ok(arguments) => {
                        emit(events, UiEvent::Telemetry(arguments.snapshot().clone())).await;
                    }
                    Err(error) => {
                        emit(events, UiEvent::RequestError {
                            kind: RequestErrorKind::Rejected,
                            message: format!("invalid telemetry signal: {error}"),
                        }).await;
                    }
                }
            },
            signal = workload_signals.next() => {
                if signal.is_none() {
                    return Err("running-workload signal stream ended".into());
                }
                refresh_running_workloads(&client, events).await?;
            },
            property = mode_properties.next() => {
                if property.is_none() {
                    return Err("property change stream ended".into());
                }
                refresh_status(&client, events).await?;
            },
            owner = owner_changes.next() => {
                return match owner {
                    Some(Some(_)) => Err("daemon D-Bus owner changed".into()),
                    Some(None) => Err("daemon left the system bus".into()),
                    None => Err("daemon owner monitor ended".into()),
                };
            },
        }
    }
}

async fn wait_before_retry(
    commands: &Receiver<ClientCommand>,
    events: &Sender<UiEvent>,
    delay: Duration,
) -> bool {
    let timer = tokio::time::sleep(delay);
    tokio::pin!(timer);
    loop {
        tokio::select! {
            () = &mut timer => return true,
            command = commands.recv() => match command {
                Ok(ClientCommand::EnableAndStartService) => {
                    if enable_and_start_system_service(events).await {
                        return true;
                    }
                }
                Ok(_) => {
                    if !emit(events, UiEvent::RequestError {
                        kind: RequestErrorKind::Rejected,
                        message: tr("daemon disconnected before the command was sent").into(),
                    }).await {
                        return false;
                    }
                }
                Err(_) => return false,
            },
        }
    }
}

async fn client_supervisor(commands: Receiver<ClientCommand>, events: Sender<UiEvent>) {
    if !emit(&events, UiEvent::Connection(ConnectionState::Connecting)).await {
        return;
    }
    let mut backoff = ReconnectBackoff::new();
    loop {
        let mut connected = false;
        match client_session(&commands, &events, &mut connected).await {
            Ok(()) => return,
            Err(reason) => {
                if connected {
                    backoff.reset();
                }
                let delay = backoff.next_delay();
                if !emit(
                    &events,
                    UiEvent::Connection(ConnectionState::Reconnecting { delay, reason }),
                )
                .await
                {
                    return;
                }
                if !wait_before_retry(&commands, &events, delay).await {
                    return;
                }
            }
        }
    }
}

/// Track the bundled reporter on the session bus for as long as the GUI runs.
///
/// Three things move the state: GNOME Shell's own `ExtensionStateChanged`, the
/// user pressing the card's button, and a slow timer that only matters while the
/// state is one the user might be fixing by hand outside the GUI.
async fn reporter_supervisor(commands: Receiver<ReporterCommand>, events: Sender<UiEvent>) {
    // No session bus at all: leave the state Unknown so the card offers no
    // GNOME-specific advice it cannot back up.
    let Ok(connection) = zbus::Connection::session().await else {
        return;
    };
    let mut state = focus_reporter::probe(&connection).await;
    if !emit(&events, UiEvent::Reporter(state)).await {
        return;
    }
    let mut changes = focus_reporter::watch(&connection).await;

    loop {
        let recheck = tokio::time::sleep(REPORTER_RECHECK_INTERVAL);
        tokio::select! {
            command = commands.recv() => match command {
                Ok(ReporterCommand::Enable) => {
                    if let Err(error) = focus_reporter::enable(&connection).await
                        && !emit(&events, UiEvent::Notice(format!(
                            "{}: {error}", tr("Unable to enable the focus reporter"),
                        ))).await
                    {
                        return;
                    }
                }
                Err(_) => return,
            },
            signal = next_reporter_signal(changes.as_mut()) => {
                if signal.is_none() {
                    // The shell went away; re-subscribing also re-resolves the
                    // name, and the periodic re-probe keeps the state honest.
                    changes = focus_reporter::watch(&connection).await;
                }
            },
            () = recheck, if reporter_state_is_actionable(state) => {},
        }

        let observed = focus_reporter::probe(&connection).await;
        if observed != state {
            state = observed;
            if !emit(&events, UiEvent::Reporter(state)).await {
                return;
            }
        }
    }
}

/// Await the next reporter signal, or park forever when there is no shell to
/// listen to, so `select!` keeps polling its other branches.
async fn next_reporter_signal(
    changes: Option<&mut zbus::proxy::SignalStream<'static>>,
) -> Option<zbus::Message> {
    match changes {
        Some(stream) => stream.next().await,
        None => std::future::pending().await,
    }
}

/// Whether a state is worth re-polling on a timer. A working or absent reporter
/// changes only through the shell, which signals; the in-between states are the
/// ones a user fixes from a terminal while looking at this window.
const fn reporter_state_is_actionable(state: ReporterState) -> bool {
    matches!(
        state,
        ReporterState::Disabled | ReporterState::Missing | ReporterState::Unknown
    )
}

fn start_reporter_watch(commands: Receiver<ReporterCommand>, events: &Sender<UiEvent>) {
    let thread_events = events.clone();
    let result = thread::Builder::new()
        .name("uperf-session".into())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                // Without this thread the focus card simply stays at Unknown,
                // which is the same as running outside GNOME. Nothing else in
                // the GUI depends on it, so there is no failure to report.
                return;
            };
            runtime.block_on(reporter_supervisor(commands, thread_events));
        });
    if let Err(error) = result {
        eprintln!("cannot start the focus reporter watch: {error}");
    }
}

fn start_client(commands: Receiver<ClientCommand>, events: &Sender<UiEvent>) {
    let thread_events = events.clone();
    let result = thread::Builder::new()
        .name("uperf-dbus".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ =
                        thread_events.try_send(UiEvent::Connection(ConnectionState::Unavailable(
                            format!("cannot start D-Bus runtime: {error}"),
                        )));
                    return;
                }
            };
            runtime.block_on(client_supervisor(commands, thread_events));
        });
    if let Err(error) = result {
        let _ = events.try_send(UiEvent::Connection(ConnectionState::Unavailable(format!(
            "cannot start D-Bus client thread: {error}"
        ))));
    }
}

fn build_application(application: &adw::Application) {
    let (commands_tx, commands_rx) = async_channel::unbounded();
    let (reporter_tx, reporter_rx) = async_channel::unbounded();
    let (events_tx, events_rx) = async_channel::unbounded();
    let ui = Ui::new(application, commands_tx, reporter_tx);
    ui.present();
    start_client(commands_rx, &events_tx);
    start_reporter_watch(reporter_rx, &events_tx);
    glib::spawn_future_local(async move {
        while let Ok(event) = events_rx.recv().await {
            ui.handle(event);
        }
    });
}

fn main() -> glib::ExitCode {
    i18n::initialize();
    let application = adw::Application::builder()
        .application_id("org.uperflinux.Gui")
        .build();
    application.connect_activate(build_application);
    application.run()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        ErrorDisposition, FocusState, ReconnectBackoff, ReporterState, RequestErrorKind,
        WorkloadClearTarget, cgroup_status_text, classify_client_error, clear_target_for_workload,
        compact_cpu_list, draft_frequency_bounds, focus_icon, focus_is_obstructed,
        format_frequency, generate_rule_id, parse_workload, reporter_state_is_actionable,
        running_workload_subtitle, scheduler_status_text, target_display_name, thermal_fraction,
    };
    use uperf_api::{
        ActiveWorkload, ApiVersion, AppRule, ClientError, FrequencyStatus, RunningWorkload,
        SchedulerStatus, TargetCapability, WorkloadIdentity,
    };

    #[test]
    fn workload_request_contains_only_a_pid_identity_input() {
        let request = parse_workload("42").expect("valid workload");
        assert_eq!(request.pid, 42);
        assert!(parse_workload("0").is_err());
    }

    #[test]
    fn clearing_the_visible_workload_uses_its_published_source() {
        assert_eq!(
            clear_target_for_workload(&ActiveWorkload {
                present: true,
                source: "explicit".into(),
                ..ActiveWorkload::default()
            }),
            Some(WorkloadClearTarget::Explicit)
        );
        assert_eq!(
            clear_target_for_workload(&ActiveWorkload {
                present: true,
                source: "focus".into(),
                ..ActiveWorkload::default()
            }),
            Some(WorkloadClearTarget::Focus)
        );
        assert_eq!(
            clear_target_for_workload(&ActiveWorkload {
                present: true,
                source: "future-source".into(),
                ..ActiveWorkload::default()
            }),
            None,
            "unknown sources must not be guessed"
        );
        assert_eq!(
            clear_target_for_workload(&ActiveWorkload {
                source: "focus".into(),
                ..ActiveWorkload::default()
            }),
            None,
            "an absent workload has nothing to clear"
        );
    }

    #[test]
    fn only_a_live_focus_lease_reads_as_success() {
        assert!(!focus_is_obstructed(FocusState::Following));
        assert!(
            !focus_is_obstructed(FocusState::Waiting),
            "waiting for the first report is normal, not a fault"
        );
        for state in [
            FocusState::Unsupported,
            FocusState::Rejected,
            FocusState::Overridden,
        ] {
            assert!(
                focus_is_obstructed(state),
                "{state:?} looks enabled but is not steering anything"
            );
        }
    }

    #[test]
    fn every_focus_state_has_its_own_icon() {
        let icons = [
            FocusState::Unsupported,
            FocusState::Following,
            FocusState::Overridden,
            FocusState::Rejected,
            FocusState::Waiting,
        ]
        .map(focus_icon);
        let mut unique = icons.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            unique.len(),
            icons.len(),
            "states must stay distinguishable"
        );
    }

    #[test]
    fn only_unresolved_reporter_states_are_polled() {
        assert!(!reporter_state_is_actionable(ReporterState::Enabled));
        for state in [
            ReporterState::Disabled,
            ReporterState::Missing,
            ReporterState::Unknown,
        ] {
            assert!(
                reporter_state_is_actionable(state),
                "{state:?} can change without a shell signal reaching us"
            );
        }
    }

    #[test]
    fn frequency_labels_scale_units() {
        assert_eq!(format_frequency(500), "500 Hz");
        assert_eq!(format_frequency(2_803_200_000), "2.80 GHz");
    }

    #[test]
    fn target_names_compact_sparse_cpu_ranges() {
        let target = TargetCapability {
            id: "cpu.example".into(),
            kind: "cpufreq".into(),
            label: "verbose fallback".into(),
            cpus: vec![5, 2, 1, 2, 4],
            ..TargetCapability::default()
        };
        assert_eq!(compact_cpu_list(&target.cpus), "1–2, 4–5");
        assert_eq!(target_display_name(&target), "CPU 1–2, 4–5");
    }

    #[test]
    fn frequency_draft_never_treats_missing_desired_state_as_zero_hertz() {
        let target = TargetCapability {
            minimum_hz: 300_000_000,
            maximum_hz: 1_800_000_000,
            ..TargetCapability::default()
        };
        let unavailable = FrequencyStatus {
            override_active: true,
            desired_available: false,
            desired_min_hz: 0,
            desired_max_hz: 0,
            ..FrequencyStatus::default()
        };
        assert_eq!(
            draft_frequency_bounds(&target, Some(&unavailable)),
            (target.minimum_hz, target.maximum_hz)
        );

        let active = FrequencyStatus {
            override_active: true,
            desired_available: true,
            desired_min_hz: 600_000_000,
            desired_max_hz: 1_200_000_000,
            ..FrequencyStatus::default()
        };
        assert_eq!(
            draft_frequency_bounds(&target, Some(&active)),
            (600_000_000, 1_200_000_000)
        );
    }

    #[test]
    fn thermal_fraction_clamps_to_the_gauge_range() {
        assert!(thermal_fraction(0).abs() < f64::EPSILON);
        assert!(thermal_fraction(40_000).abs() < f64::EPSILON);
        assert!((thermal_fraction(100_000) - 1.0).abs() < f64::EPSILON);
        assert!((thermal_fraction(70_000) - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn generated_rule_ids_avoid_collisions() {
        let existing = vec![AppRule {
            id: "gui.rule1".into(),
            ..AppRule::default()
        }];
        assert_eq!(generate_rule_id(&existing), "gui.rule2");
        assert_eq!(generate_rule_id(&[]), "gui.rule1");
    }

    #[test]
    fn active_candidate_formats_scheduler_and_cgroup_readback() {
        let scheduler = SchedulerStatus {
            enabled: true,
            matched_rule: "game".into(),
            managed_tasks: 4,
            applied_tasks: 3,
            cgroup_class: "foreground".into(),
            systemd_unit: "app-game.scope".into(),
            cgroup_applied: true,
            warning: String::new(),
        };
        let workload = RunningWorkload {
            identity: WorkloadIdentity {
                pid: 42,
                start_time_ticks: 100,
                uid: 1000,
            },
            name: "wine64".into(),
            matched_pattern: "wine".into(),
            active: true,
            scheduler: scheduler.clone(),
        };

        assert!(running_workload_subtitle(&workload).contains("PID 42 · matched wine · active"));
        assert_eq!(
            scheduler_status_text(&scheduler),
            "Rule game · 3/4 tasks applied"
        );
        assert_eq!(
            cgroup_status_text(&scheduler),
            "Class foreground · app-game.scope · applied"
        );
    }

    #[test]
    fn reconnect_backoff_is_exponential_bounded_and_resettable() {
        let mut backoff = ReconnectBackoff::new();
        let delays: Vec<_> = (0..7).map(|_| backoff.next_delay()).collect();
        assert_eq!(
            delays,
            vec![
                Duration::from_millis(250),
                Duration::from_millis(500),
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
                Duration::from_secs(8),
            ]
        );
        backoff.reset();
        assert_eq!(backoff.next_delay(), Duration::from_millis(250));
    }

    #[test]
    fn daemon_absence_is_a_connection_failure() {
        let error = ClientError::Remote {
            name: "org.freedesktop.DBus.Error.ServiceUnknown".into(),
            message: "name has no owner".into(),
        };
        assert_eq!(
            classify_client_error(&error),
            ErrorDisposition::ConnectionLost
        );
    }

    #[test]
    fn permission_and_request_errors_do_not_masquerade_as_disconnects() {
        let denied = ClientError::Remote {
            name: "org.uperflinux.Daemon2.Error.NotAuthorized".into(),
            message: "authorization required".into(),
        };
        assert_eq!(
            classify_client_error(&denied),
            ErrorDisposition::Request(RequestErrorKind::NotAuthorized)
        );

        let invalid = ClientError::InvalidRequest("bad mode".into());
        assert_eq!(
            classify_client_error(&invalid),
            ErrorDisposition::Request(RequestErrorKind::InvalidRequest)
        );

        let incompatible = ClientError::IncompatibleApi {
            client: ApiVersion::CURRENT,
            server: ApiVersion {
                major: ApiVersion::CURRENT.major + 1,
                minor: 0,
            },
        };
        assert_eq!(
            classify_client_error(&incompatible),
            ErrorDisposition::Request(RequestErrorKind::IncompatibleApi)
        );
    }
}

mod view_model;

use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
    thread,
    time::Duration,
};

use adw::prelude::*;
use async_channel::{Receiver, Sender};
use futures_util::StreamExt;
use gtk::glib;
use uperf_api::{
    AppRule, Capabilities, ClientError, DaemonClient, DaemonStatus, FrequencyOverride,
    FrequencyStatus, RunningWorkload, SchedulerStatus, TelemetrySnapshot, WorkloadRequest, feature,
};
use view_model::{TargetView, ViewModel, cpu_load_percent, frequency_override};

const SERVICE_UNIT: &str = "uperf-linux.service";
const INITIAL_RECONNECT_DELAY: Duration = Duration::from_millis(250);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(8);

#[derive(Debug)]
enum ClientCommand {
    SetMode(String),
    SetFrequency(FrequencyOverride),
    ClearFrequency(String),
    ClearAllFrequency(Vec<String>),
    SetWorkload(WorkloadRequest),
    ClearWorkload,
    SetAppRule(AppRule),
    RemoveAppRule(String),
    ReloadConfig,
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
    const fn label(self) -> &'static str {
        match self {
            Self::NotAuthorized => "Not authorized",
            Self::InvalidRequest => "Invalid request",
            Self::IncompatibleApi => "Incompatible API",
            Self::Rejected => "Request rejected",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ErrorDisposition {
    ConnectionLost,
    Request(RequestErrorKind),
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
    state_row: adw::ActionRow,
    health_row: adw::ActionRow,
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

    // Dashboard: workload / scheduler
    workload_group: adw::PreferencesGroup,
    workload_row: adw::ActionRow,
    scheduler_row: adw::ActionRow,
    cgroup_row: adw::ActionRow,
    pid_entry: adw::EntryRow,

    // Dashboard: per-CPU utilization (rebuilt as CPU IDs are discovered)
    load_group: adw::PreferencesGroup,
    load_rows: RefCell<BTreeMap<u32, adw::ActionRow>>,

    // Dashboard: per-target frequency (rebuilt from capabilities)
    freq_group: adw::PreferencesGroup,
    freq_rows: RefCell<BTreeMap<String, gtk::Label>>,

    // Frequency page (rebuilt from capabilities)
    override_group: adw::PreferencesGroup,
    target_status: RefCell<BTreeMap<String, gtk::Label>>,
    override_ids: RefCell<Vec<String>>,

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
    rules: RefCell<Vec<AppRule>>,
    workloads: RefCell<Vec<RunningWorkload>>,
    mode_ids: RefCell<Vec<String>>,
    connected: Cell<bool>,
    daemon_controls: RefCell<Vec<gtk::Widget>>,
    commands: Sender<ClientCommand>,
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
    fn new(application: &adw::Application, commands: Sender<ClientCommand>) -> Rc<Self> {
        let window = adw::ApplicationWindow::builder()
            .application(application)
            .title("Uperf Linux")
            .default_width(520)
            .default_height(760)
            .build();

        // Dashboard widgets
        let connection_row = status_row("Connection");
        connection_row.set_subtitle("Connecting…");
        let state_row = status_row("Lifecycle");
        let health_row = status_row("Health");
        let profile_row = status_row("Effective profile");
        let scene_row = status_row("Dominant scene");
        let mode_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        mode_box.add_css_class("linked");
        mode_box.set_margin_top(4);
        mode_box.set_margin_bottom(4);
        let thermal_group = adw::PreferencesGroup::builder()
            .title("Thermal safety")
            .description("Safety state is authoritative; manual settings cannot bypass it")
            .build();
        let thermal_row = status_row("Temperature");
        let thermal_bar = gtk::ProgressBar::new();
        let workload_group = adw::PreferencesGroup::builder()
            .title("Active workload")
            .description("Enter a PID; the daemon resolves and verifies its start time and UID")
            .build();
        let workload_row = status_row("Selection");
        let scheduler_row = status_row("Task scheduler");
        let cgroup_row = status_row("Systemd cgroup");
        let pid_entry = adw::EntryRow::builder().title("PID").build();
        let load_group = adw::PreferencesGroup::builder()
            .title("CPU utilization")
            .description("Per-CPU load reported by daemon telemetry")
            .build();
        let freq_group = adw::PreferencesGroup::builder()
            .title("Cluster frequency")
            .build();

        // Frequency-page widgets
        let override_group = adw::PreferencesGroup::builder()
            .title("Manual frequency override")
            .description(
                "Manual bounds are transactional, read back by the daemon, and constrained by thermal safety",
            )
            .build();

        // Apps-page widgets
        let running_group = adw::PreferencesGroup::builder()
            .title("Detected running workloads")
            .description(
                "Broad game and compatibility-layer matches; detection alone never changes the active mode",
            )
            .build();
        let running_placeholder = adw::ActionRow::builder()
            .title("No matching processes")
            .subtitle("Launch a game, Wine/Proton application, emulator, or Steam process.")
            .build();
        let apps_group = adw::PreferencesGroup::builder()
            .title("Application rules")
            .description("Persistent global rules that pin a mode for matching processes")
            .build();
        let apps_placeholder = adw::ActionRow::builder()
            .title("No application rules")
            .subtitle("Add a rule below to pin a mode for a matching process.")
            .build();
        let rule_exe_entry = adw::EntryRow::builder()
            .title("Executable path (optional)")
            .build();
        let rule_comm_entry = adw::EntryRow::builder()
            .title("Process-name regex (optional)")
            .build();
        let rule_mode_dropdown = gtk::DropDown::from_strings(&[]);

        // Logs-page widget
        let log_buffer = gtk::TextBuffer::new(None);
        log_buffer.set_text(&format!(
            "Press Refresh to load the latest {SERVICE_UNIT} journal.\n"
        ));

        let overlay = adw::ToastOverlay::new();

        let ui = Rc::new(Self {
            window,
            overlay,
            connection_row,
            state_row,
            health_row,
            profile_row,
            scene_row,
            mode_box,
            mode_buttons: RefCell::new(BTreeMap::new()),
            syncing_modes: Cell::new(false),
            thermal_group,
            thermal_row,
            thermal_bar,
            workload_group,
            workload_row,
            scheduler_row,
            cgroup_row,
            pid_entry,
            load_group,
            load_rows: RefCell::new(BTreeMap::new()),
            freq_group,
            freq_rows: RefCell::new(BTreeMap::new()),
            override_group,
            target_status: RefCell::new(BTreeMap::new()),
            override_ids: RefCell::new(Vec::new()),
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
            rules: RefCell::new(Vec::new()),
            workloads: RefCell::new(Vec::new()),
            mode_ids: RefCell::new(Vec::new()),
            connected: Cell::new(false),
            daemon_controls: RefCell::new(Vec::new()),
            commands,
        });
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
            (&dashboard, "dashboard", "Dashboard", "speedometer-symbolic"),
            (&apps, "apps", "Apps", "applications-games-symbolic"),
            (
                &frequency,
                "frequency",
                "Frequency",
                "power-profile-performance-symbolic",
            ),
            (&settings, "settings", "Settings", "emblem-system-symbolic"),
            (&logs, "logs", "Logs", "text-x-generic-symbolic"),
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
            adw::Breakpoint::new(adw::BreakpointCondition::parse("max-width: 500px").unwrap());
        breakpoint.add_setter(&switcher, "visible", Some(&false.into()));
        breakpoint.add_setter(&switcher_bar, "reveal", Some(&true.into()));
        self.window.add_breakpoint(breakpoint);
    }

    fn present(&self) {
        self.window.present();
    }

    fn build_dashboard_page(self: &Rc<Self>) -> adw::PreferencesPage {
        let page = new_prefs_page("Dashboard", "speedometer-symbolic");

        let mode_group = adw::PreferencesGroup::builder()
            .title("Power mode")
            .description("Modes are advertised by the running daemon")
            .build();
        mode_group.add(&self.mode_box);
        self.add_daemon_control(&mode_group);
        page.add(&mode_group);

        let overview = adw::PreferencesGroup::builder()
            .title("Status")
            .description("Observed state reported by org.uperflinux.Daemon1")
            .build();
        overview.add(&self.connection_row);
        overview.add(&self.state_row);
        overview.add(&self.health_row);
        overview.add(&self.profile_row);
        overview.add(&self.scene_row);
        page.add(&overview);

        // Active workload / scheduler card.
        self.workload_group.add(&self.workload_row);
        self.workload_group.add(&self.scheduler_row);
        self.workload_group.add(&self.cgroup_row);
        self.workload_group.add(&self.pid_entry);
        let workload_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        workload_buttons.set_halign(gtk::Align::End);
        workload_buttons.set_margin_top(8);
        let clear_workload = gtk::Button::with_label("Clear active workload");
        let set_workload = gtk::Button::with_label("Set active workload");
        set_workload.add_css_class("suggested-action");
        workload_buttons.append(&clear_workload);
        workload_buttons.append(&set_workload);
        self.workload_group.add(&workload_buttons);
        self.add_daemon_control(&self.workload_group);
        page.add(&self.workload_group);

        {
            let ui = self.clone();
            set_workload.connect_clicked(move |_| {
                match parse_workload(ui.pid_entry.text().as_str()) {
                    Ok(request) => ui.send(ClientCommand::SetWorkload(request)),
                    Err(message) => ui.toast(&message),
                }
            });
        }
        {
            let ui = self.clone();
            clear_workload.connect_clicked(move |_| ui.send(ClientCommand::ClearWorkload));
        }

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
        let page = new_prefs_page("Apps", "applications-games-symbolic");
        self.running_group.add(&self.running_placeholder);
        self.add_daemon_control(&self.running_group);
        page.add(&self.running_group);

        self.apps_group.add(&self.apps_placeholder);
        page.add(&self.apps_group);

        let add_group = adw::PreferencesGroup::builder()
            .title("Add rule")
            .description("Match by executable path, process-name regex, or both")
            .build();
        add_group.add(&self.rule_exe_entry);
        add_group.add(&self.rule_comm_entry);
        let mode_row = adw::ActionRow::builder().title("Mode").build();
        self.rule_mode_dropdown.set_valign(gtk::Align::Center);
        mode_row.add_suffix(&self.rule_mode_dropdown);
        add_group.add(&mode_row);

        let add_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        add_buttons.set_halign(gtk::Align::End);
        add_buttons.set_margin_top(8);
        let add_button = gtk::Button::with_label("Add rule");
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

    fn build_frequency_page(self: &Rc<Self>) -> adw::PreferencesPage {
        let page = new_prefs_page("Frequency", "power-profile-performance-symbolic");
        page.add(&self.override_group);

        let buttons_group = adw::PreferencesGroup::new();
        let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        hbox.set_halign(gtk::Align::Center);
        let release_all = gtk::Button::with_label("Release all");
        release_all.add_css_class("pill");
        hbox.append(&release_all);
        buttons_group.add(&hbox);
        self.add_daemon_control(&self.override_group);
        self.add_daemon_control(&buttons_group);
        page.add(&buttons_group);

        {
            let ui = self.clone();
            release_all.connect_clicked(move |_| {
                let ids = ui.override_ids.borrow().clone();
                if ids.is_empty() {
                    ui.toast("No overridable targets");
                } else {
                    ui.send(ClientCommand::ClearAllFrequency(ids));
                }
            });
        }
        page
    }

    fn build_settings_page(self: &Rc<Self>) -> adw::PreferencesPage {
        let page = new_prefs_page("Settings", "emblem-system-symbolic");
        let group = adw::PreferencesGroup::builder()
            .title("Daemon configuration")
            .build();
        let row = adw::ActionRow::builder()
            .title("Configuration reload")
            .subtitle("Edit the config with administrator privileges, then reload it here.")
            .build();
        let reload = gtk::Button::with_label("Reload");
        reload.set_valign(gtk::Align::Center);
        row.add_suffix(&reload);
        group.add(&row);
        self.add_daemon_control(&group);
        page.add(&group);

        {
            let ui = self.clone();
            reload.connect_clicked(move |_| ui.send(ClientCommand::ReloadConfig));
        }
        page
    }

    fn build_logs_page(self: &Rc<Self>) -> adw::PreferencesPage {
        let page = new_prefs_page("Logs", "text-x-generic-symbolic");
        let group = adw::PreferencesGroup::builder()
            .title("Service journal")
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
        let refresh = gtk::Button::with_label("Refresh");
        refresh.add_css_class("pill");
        let clear = gtk::Button::with_label("Clear");
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
        for widget in self.daemon_controls.borrow().iter() {
            widget.set_sensitive(connected);
        }
    }

    fn update_connection(&self, state: ConnectionState) {
        match state {
            ConnectionState::Connecting => {
                self.set_connected(false);
                self.connection_row.set_subtitle("Connecting…");
            }
            ConnectionState::Connected => {
                self.set_connected(true);
                self.connection_row.set_subtitle("Connected");
            }
            ConnectionState::Reconnecting { delay, reason } => {
                self.set_connected(false);
                self.connection_row.set_subtitle(&format!(
                    "Disconnected · retrying in {} · {reason}",
                    format_retry_delay(delay)
                ));
            }
            ConnectionState::Unavailable(message) => {
                self.set_connected(false);
                self.connection_row
                    .set_subtitle(&format!("Unavailable · {message}"));
            }
        }
    }

    fn send(&self, command: ClientCommand) {
        if !self.connected.get() {
            self.toast("The daemon is disconnected; wait for it to reconnect");
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
                *self.status.borrow_mut() = status;
                self.update_status_widgets();
            }
            UiEvent::Capabilities(capabilities) => {
                *self.capabilities.borrow_mut() = capabilities;
                self.rebuild_capability_widgets();
                self.update_status_widgets();
            }
            UiEvent::Telemetry(telemetry) => self.update_telemetry(&telemetry),
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
        }
    }

    /// Rebuild all widgets whose shape depends on daemon-advertised capabilities:
    /// the mode toggle group, the per-target frequency labels, and the manual
    /// override rows.
    #[allow(clippy::too_many_lines)]
    fn rebuild_capability_widgets(self: &Rc<Self>) {
        let capabilities = self.capabilities.borrow().clone();
        let status = self.status.borrow().clone();
        let view = ViewModel::from_api(&capabilities, &status);

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
        while let Some(child) = self.freq_group.first_child() {
            self.freq_group.remove(&child);
        }
        self.freq_rows.borrow_mut().clear();
        for target in &view.targets {
            let row = adw::ActionRow::builder()
                .title(&target.capability.label)
                .build();
            let label = value_label();
            row.add_suffix(&label);
            self.freq_group.add(&row);
            self.freq_rows
                .borrow_mut()
                .insert(target.capability.id.clone(), label);
        }
        self.freq_group.set_visible(!view.targets.is_empty());

        // Frequency-page override rows.
        while let Some(child) = self.override_group.first_child() {
            self.override_group.remove(&child);
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
        let cpus = if capability.cpus.is_empty() {
            String::new()
        } else {
            format!(
                " · CPUs {}",
                capability
                    .cpus
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let row = adw::ActionRow::builder()
            .title(&capability.label)
            .subtitle(format!("{} · {}{cpus}", capability.id, capability.kind))
            .build();
        let status_label = value_label();
        row.add_suffix(&status_label);
        self.target_status
            .borrow_mut()
            .insert(capability.id.clone(), status_label);

        if capability.can_override && !target.choices_hz.is_empty() {
            self.override_ids.borrow_mut().push(capability.id.clone());
            let controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            controls.set_valign(gtk::Align::Center);
            let labels: Vec<String> = target
                .choices_hz
                .iter()
                .map(|frequency| format_frequency(*frequency))
                .collect();
            let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            let minimum = gtk::DropDown::from_strings(&label_refs);
            let maximum = gtk::DropDown::from_strings(&label_refs);
            minimum.set_tooltip_text(Some("Minimum frequency"));
            maximum.set_tooltip_text(Some("Maximum frequency"));

            let selected_minimum = target
                .status
                .as_ref()
                .map_or(capability.minimum_hz, |status| status.desired_min_hz);
            let selected_maximum = target
                .status
                .as_ref()
                .map_or(capability.maximum_hz, |status| status.desired_max_hz);
            minimum.set_selected(closest_index(&target.choices_hz, selected_minimum));
            maximum.set_selected(closest_index(&target.choices_hz, selected_maximum));

            let clear = gtk::Button::with_label("Clear");
            let apply = gtk::Button::with_label("Apply…");
            apply.add_css_class("destructive-action");
            controls.append(&minimum);
            controls.append(&maximum);
            controls.append(&clear);
            controls.append(&apply);
            row.add_suffix(&controls);

            {
                let ui = self.clone();
                let target_id = capability.id.clone();
                clear.connect_clicked(move |_| {
                    ui.send(ClientCommand::ClearFrequency(target_id.clone()));
                });
            }
            {
                let ui = self.clone();
                let choices = target.choices_hz.clone();
                let capability = capability.clone();
                apply.connect_clicked(move |_| {
                    let minimum_index = usize::try_from(minimum.selected()).unwrap_or(usize::MAX);
                    let maximum_index = usize::try_from(maximum.selected()).unwrap_or(usize::MAX);
                    let Some(minimum_hz) = choices.get(minimum_index).copied() else {
                        ui.toast("Select a minimum frequency");
                        return;
                    };
                    let Some(maximum_hz) = choices.get(maximum_index).copied() else {
                        ui.toast("Select a maximum frequency");
                        return;
                    };
                    let request = match frequency_override(&capability, minimum_hz, maximum_hz) {
                        Ok(request) => request,
                        Err(message) => {
                            ui.toast(&message);
                            return;
                        }
                    };
                    ui.confirm_frequency(request);
                });
            }
        }
        self.override_group.add(&row);
    }

    fn sync_mode_dropdown(&self) {
        let ids = self.mode_ids.borrow();
        let capabilities = self.capabilities.borrow();
        let labels: Vec<&str> = capabilities
            .modes
            .iter()
            .map(|mode| mode.display_name.as_str())
            .collect();
        let model = gtk::StringList::new(&labels);
        self.rule_mode_dropdown.set_model(Some(&model));
        if !ids.is_empty() {
            self.rule_mode_dropdown.set_selected(0);
        }
    }

    fn update_status_widgets(&self) {
        let capabilities = self.capabilities.borrow().clone();
        let status = self.status.borrow().clone();
        let view = ViewModel::from_api(&capabilities, &status);

        self.state_row.set_subtitle(&view.daemon_state);
        self.health_row.set_subtitle(&view.health);
        self.profile_row.set_subtitle(&view.profile);
        self.scene_row.set_subtitle(&view.scene);

        // Keep the mode toggle group in sync without re-triggering commands.
        self.syncing_modes.set(true);
        for (mode_id, button) in self.mode_buttons.borrow().iter() {
            button.set_active(*mode_id == status.mode);
        }
        self.syncing_modes.set(false);

        for target in &view.targets {
            let text = target_status_text(target.status.as_ref());
            if let Some(label) = self.target_status.borrow().get(&target.capability.id) {
                label.set_text(&text);
            }
            if let Some(label) = self.freq_rows.borrow().get(&target.capability.id) {
                label.set_text(&text);
            }
        }

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
            if active.present {
                self.workload_row.set_subtitle(&format!(
                    "{} · PID {} · {}",
                    active.name, active.identity.pid, active.effective_mode
                ));
                self.pid_entry.set_text(&active.identity.pid.to_string());
            } else {
                self.workload_row.set_subtitle("None");
            }
        } else {
            self.workload_group.set_visible(false);
        }
    }

    fn update_telemetry(&self, telemetry: &TelemetrySnapshot) {
        self.rebuild_cpu_loads(&telemetry.cpu_loads);
        let mut status = self.status.borrow_mut();
        status.thermal = telemetry.thermal.clone();
        status.frequencies.clone_from(&telemetry.frequencies);
        drop(status);
        self.update_status_widgets();
    }

    /// Rebuild the per-CPU utilization rows keyed by the sparse kernel CPU IDs
    /// the daemon reports, adding rows as new CPUs appear.
    fn rebuild_cpu_loads(&self, loads: &[uperf_api::CpuLoad]) {
        let mut rows = self.load_rows.borrow_mut();
        for load in loads {
            let row = rows.entry(load.cpu_id).or_insert_with(|| {
                let row = adw::ActionRow::builder()
                    .title(format!("CPU {}", load.cpu_id))
                    .build();
                let label = value_label();
                row.add_suffix(&label);
                self.load_group.add(&row);
                row
            });
            if let Some(label) = row.last_child().and_downcast::<gtk::Label>() {
                label.set_text(&format!("{:.0} %", cpu_load_percent(*load)));
            }
        }
        self.load_group.set_visible(!rows.is_empty());
    }

    fn confirm_frequency(self: &Rc<Self>, request: FrequencyOverride) {
        let dialog = gtk::AlertDialog::builder()
            .modal(true)
            .message("Apply privileged frequency limits?")
            .detail(format!(
                "{}: {} – {}. Thermal and hardware limits remain authoritative.",
                request.target_id,
                format_frequency(request.min_hz),
                format_frequency(request.max_hz)
            ))
            .buttons(["Cancel", "Apply"])
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
                .map(|mode| mode.display_name.clone())
                .collect()
        };
        let label_refs: Vec<&str> = mode_labels.iter().map(String::as_str).collect();

        for rule in &rules {
            let matcher = match (&rule.executable, &rule.comm_regex) {
                (Some(exe), Some(comm)) => format!("{exe} · /{comm}/"),
                (Some(exe), None) => exe.clone(),
                (None, Some(comm)) => format!("/{comm}/"),
                (None, None) => "any process".to_owned(),
            };
            let row = adw::ActionRow::builder()
                .title(&rule.id)
                .subtitle(format!("{matcher} · priority {}", rule.priority))
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
            enabled.set_tooltip_text(Some("Enable rule"));
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
            remove.set_tooltip_text(Some("Remove rule"));
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
            self.scheduler_row.set_subtitle("No active workload");
            self.cgroup_row.set_subtitle("No active workload");
        }

        for workload in workloads {
            let row = adw::ActionRow::builder()
                .title(&workload.name)
                .subtitle(running_workload_subtitle(&workload))
                .build();
            row.add_prefix(&gtk::Image::from_icon_name("applications-games-symbolic"));

            let activate = if workload.active {
                let button = gtk::Button::with_label("Active");
                button.set_sensitive(false);
                button
            } else {
                let button = gtk::Button::with_label("Use");
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
            self.toast("Provide an executable path or a process-name regex");
            return;
        }
        let mode_index = usize::try_from(self.rule_mode_dropdown.selected()).unwrap_or(usize::MAX);
        let Some(mode) = self.mode_ids.borrow().get(mode_index).cloned() else {
            self.toast("Select a mode for the rule");
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
                buffer.set_text(&format!("Unable to start journalctl: {error}"));
                return;
            }
        };
        glib::spawn_future_local(async move {
            match process.communicate_utf8_future(None).await {
                Ok((Some(output), _)) if !output.is_empty() => buffer.set_text(&output),
                Ok(_) => buffer.set_text("(journal is empty)"),
                Err(error) => buffer.set_text(&format!("Unable to read journal: {error}")),
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
        || "No fresh state".into(),
        |status| {
            if !status.applied_verified {
                return if status.observed_available {
                    format!(
                        "{} – {} observed · not managed{}",
                        format_frequency(status.observed_min_hz),
                        format_frequency(status.observed_max_hz),
                        if status.stale { " · stale" } else { "" },
                    )
                } else {
                    "State unavailable · not managed".to_owned()
                };
            }
            let stale = if status.stale { " · stale" } else { "" };
            let overridden = if status.override_active {
                " · override"
            } else {
                ""
            };
            format!(
                "{} – {} applied{overridden}{stale}",
                format_frequency(status.applied_min_hz),
                format_frequency(status.applied_max_hz)
            )
        },
    )
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
        format!("{} ms", delay.as_millis())
    } else {
        format!("{} s", delay.as_secs())
    }
}

fn running_workload_subtitle(workload: &RunningWorkload) -> String {
    let source = if workload.matched_pattern == "active" {
        "explicit active workload".to_owned()
    } else {
        format!("matched {}", workload.matched_pattern)
    };
    let mut details = vec![format!("PID {}", workload.identity.pid), source];
    if workload.active {
        details.push("active".to_owned());
        let scheduler = scheduler_status_text(&workload.scheduler);
        if scheduler != "No active workload" {
            details.push(scheduler);
        }
    }
    details.join(" · ")
}

fn scheduler_status_text(status: &SchedulerStatus) -> String {
    if !status.enabled {
        return "Disabled by policy".to_owned();
    }
    if status.matched_rule.is_empty() {
        return if status.warning.is_empty() {
            "Pending or no matching scheduler rule".to_owned()
        } else {
            format!("No applied rule · {}", status.warning)
        };
    }
    let mut text = format!(
        "Rule {} · {}/{} tasks applied",
        status.matched_rule, status.applied_tasks, status.managed_tasks
    );
    if !status.warning.is_empty() {
        text.push_str(" · ");
        text.push_str(&status.warning);
    }
    text
}

fn cgroup_status_text(status: &SchedulerStatus) -> String {
    if !status.enabled {
        return "Disabled by policy".to_owned();
    }
    if status.systemd_unit.is_empty() {
        return if status.cgroup_class.is_empty() {
            "No dedicated unit selected".to_owned()
        } else {
            format!("Class {} · no dedicated unit", status.cgroup_class)
        };
    }
    let state = if status.cgroup_applied {
        "applied"
    } else {
        "not applied"
    };
    if status.cgroup_class.is_empty() {
        format!("{} · {state}", status.systemd_unit)
    } else {
        format!(
            "Class {} · {} · {state}",
            status.cgroup_class, status.systemd_unit
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
        ClientCommand::ClearWorkload => {
            refresh_workloads = true;
            client
                .clear_active_workload()
                .await
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
                Ok(_) => {
                    if !emit(events, UiEvent::RequestError {
                        kind: RequestErrorKind::Rejected,
                        message: "daemon disconnected before the command was sent".into(),
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
    let (events_tx, events_rx) = async_channel::unbounded();
    let ui = Ui::new(application, commands_tx);
    ui.present();
    start_client(commands_rx, &events_tx);
    glib::spawn_future_local(async move {
        while let Ok(event) = events_rx.recv().await {
            ui.handle(event);
        }
    });
}

fn main() -> glib::ExitCode {
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
        ErrorDisposition, ReconnectBackoff, RequestErrorKind, cgroup_status_text,
        classify_client_error, format_frequency, generate_rule_id, parse_workload,
        running_workload_subtitle, scheduler_status_text, thermal_fraction,
    };
    use uperf_api::{
        ApiVersion, AppRule, ClientError, RunningWorkload, SchedulerStatus, WorkloadIdentity,
    };

    #[test]
    fn workload_request_contains_only_a_pid_identity_input() {
        let request = parse_workload("42").expect("valid workload");
        assert_eq!(request.pid, 42);
        assert!(parse_workload("0").is_err());
    }

    #[test]
    fn frequency_labels_scale_units() {
        assert_eq!(format_frequency(500), "500 Hz");
        assert_eq!(format_frequency(2_803_200_000), "2.80 GHz");
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
            name: "org.uperflinux.Daemon1.Error.NotAuthorized".into(),
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

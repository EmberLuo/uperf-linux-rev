mod view_model;

use std::{cell::RefCell, collections::BTreeMap, rc::Rc, thread};

use adw::prelude::*;
use async_channel::{Receiver, Sender};
use futures_util::StreamExt;
use gtk::glib;
use uperf_api::{
    Capabilities, DaemonClient, DaemonStatus, FrequencyOverride, FrequencyStatus,
    TelemetrySnapshot, WorkloadRequest,
};
use view_model::{TargetView, ViewModel, frequency_override};

#[derive(Debug)]
enum ClientCommand {
    SetMode(String),
    SetFrequency(FrequencyOverride),
    ClearFrequency(String),
    SetWorkload(WorkloadRequest),
    ClearWorkload,
}

#[derive(Debug)]
enum UiEvent {
    Snapshot {
        capabilities: Capabilities,
        status: DaemonStatus,
    },
    Status(DaemonStatus),
    Capabilities(Capabilities),
    Telemetry(TelemetrySnapshot),
    Notice(String),
    Error(String),
}

struct Ui {
    window: adw::ApplicationWindow,
    page: adw::PreferencesPage,
    overlay: adw::ToastOverlay,
    state_row: adw::ActionRow,
    health_row: adw::ActionRow,
    profile_row: adw::ActionRow,
    scene_row: adw::ActionRow,
    thermal_group: adw::PreferencesGroup,
    thermal_row: adw::ActionRow,
    workload_group: adw::PreferencesGroup,
    workload_row: adw::ActionRow,
    pid_entry: adw::EntryRow,
    mode_group: RefCell<Option<adw::PreferencesGroup>>,
    target_group: RefCell<Option<adw::PreferencesGroup>>,
    mode_buttons: RefCell<BTreeMap<String, gtk::Button>>,
    target_status: RefCell<BTreeMap<String, gtk::Label>>,
    capabilities: RefCell<Capabilities>,
    status: RefCell<DaemonStatus>,
    commands: Sender<ClientCommand>,
}

impl Ui {
    #[allow(clippy::too_many_lines)]
    fn new(application: &adw::Application, commands: Sender<ClientCommand>) -> Rc<Self> {
        let window = adw::ApplicationWindow::builder()
            .application(application)
            .title("Uperf Linux")
            .default_width(860)
            .default_height(720)
            .build();

        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&adw::WindowTitle::new(
            "Uperf Linux",
            "Capability-driven system performance control",
        )));

        let page = adw::PreferencesPage::new();
        page.set_vexpand(true);
        let overview_group = adw::PreferencesGroup::builder()
            .title("Daemon")
            .description("Observed state reported by org.uperflinux.Daemon1")
            .build();
        let state_row = status_row("Lifecycle");
        let health_row = status_row("Health");
        let profile_row = status_row("Effective profile");
        let scene_row = status_row("Dominant scene");
        overview_group.add(&state_row);
        overview_group.add(&health_row);
        overview_group.add(&profile_row);
        overview_group.add(&scene_row);
        page.add(&overview_group);

        let thermal_group = adw::PreferencesGroup::builder()
            .title("Thermal safety")
            .description("Safety state is authoritative; manual settings cannot bypass it")
            .build();
        let thermal_row = status_row("Temperature");
        thermal_group.add(&thermal_row);
        page.add(&thermal_group);

        let workload_group = adw::PreferencesGroup::builder()
            .title("Active workload")
            .description("Enter a PID; the daemon resolves and verifies its start time and UID")
            .build();
        let workload_row = status_row("Selection");
        let pid_entry = adw::EntryRow::builder().title("PID").build();
        workload_group.add(&workload_row);
        workload_group.add(&pid_entry);

        let workload_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        workload_buttons.set_halign(gtk::Align::End);
        workload_buttons.set_margin_top(8);
        let clear_workload = gtk::Button::with_label("Clear active workload");
        let set_workload = gtk::Button::with_label("Set active workload");
        set_workload.add_css_class("suggested-action");
        workload_buttons.append(&clear_workload);
        workload_buttons.append(&set_workload);
        workload_group.add(&workload_buttons);
        page.add(&workload_group);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&header);
        content.append(&page);
        let overlay = adw::ToastOverlay::new();
        overlay.set_child(Some(&content));
        window.set_content(Some(&overlay));

        {
            let sender = commands.clone();
            let overlay = overlay.clone();
            let pid_entry = pid_entry.clone();
            set_workload.connect_clicked(move |_| {
                let parsed = parse_workload(pid_entry.text().as_str());
                match parsed {
                    Ok(request) => send_command(&sender, ClientCommand::SetWorkload(request)),
                    Err(message) => show_toast(&overlay, &message),
                }
            });
        }
        {
            let sender = commands.clone();
            clear_workload.connect_clicked(move |_| {
                send_command(&sender, ClientCommand::ClearWorkload);
            });
        }

        Rc::new(Self {
            window,
            page,
            overlay,
            state_row,
            health_row,
            profile_row,
            scene_row,
            thermal_group,
            thermal_row,
            workload_group,
            workload_row,
            pid_entry,
            mode_group: RefCell::new(None),
            target_group: RefCell::new(None),
            mode_buttons: RefCell::new(BTreeMap::new()),
            target_status: RefCell::new(BTreeMap::new()),
            capabilities: RefCell::new(Capabilities::default()),
            status: RefCell::new(DaemonStatus::default()),
            commands,
        })
    }

    fn present(&self) {
        self.window.present();
    }

    fn handle(&self, event: UiEvent) {
        match event {
            UiEvent::Snapshot {
                capabilities,
                status,
            } => {
                *self.capabilities.borrow_mut() = capabilities;
                *self.status.borrow_mut() = status;
                self.rebuild_capability_widgets();
                self.update_status_widgets();
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
            UiEvent::Notice(message) => show_toast(&self.overlay, &message),
            UiEvent::Error(message) => {
                self.health_row
                    .set_subtitle(&format!("Unavailable: {message}"));
                show_toast(&self.overlay, &message);
            }
        }
    }

    fn rebuild_capability_widgets(&self) {
        if let Some(group) = self.mode_group.borrow_mut().take() {
            self.page.remove(&group);
        }
        if let Some(group) = self.target_group.borrow_mut().take() {
            self.page.remove(&group);
        }
        self.mode_buttons.borrow_mut().clear();
        self.target_status.borrow_mut().clear();

        let capabilities = self.capabilities.borrow();
        let status = self.status.borrow();
        let view = ViewModel::from_api(&capabilities, &status);

        let mode_group = adw::PreferencesGroup::builder()
            .title("Mode")
            .description("Modes are advertised by the running daemon")
            .build();
        let mode_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        mode_box.set_homogeneous(true);
        for mode in &view.modes {
            let button = gtk::Button::with_label(&mode.label);
            button.set_tooltip_text(Some(&mode.description));
            if mode.selected {
                button.add_css_class("suggested-action");
            }
            let sender = self.commands.clone();
            let id = mode.id.clone();
            button.connect_clicked(move |_| {
                send_command(&sender, ClientCommand::SetMode(id.clone()));
            });
            mode_box.append(&button);
            self.mode_buttons
                .borrow_mut()
                .insert(mode.id.clone(), button);
        }
        mode_group.add(&mode_box);
        self.page.add(&mode_group);
        *self.mode_group.borrow_mut() = Some(mode_group);

        let target_group = adw::PreferencesGroup::builder()
            .title("Frequency targets")
            .description(
                "Manual bounds are transactional, read back by the daemon, and constrained by thermal safety",
            )
            .build();
        for target in &view.targets {
            self.add_target_row(&target_group, target);
        }
        target_group.set_visible(!view.targets.is_empty());
        self.page.add(&target_group);
        *self.target_group.borrow_mut() = Some(target_group);
    }

    #[allow(clippy::too_many_lines)]
    fn add_target_row(&self, group: &adw::PreferencesGroup, target: &TargetView) {
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
        let status_label = gtk::Label::new(None);
        status_label.add_css_class("dim-label");
        status_label.set_xalign(1.0);
        row.add_suffix(&status_label);
        self.target_status
            .borrow_mut()
            .insert(capability.id.clone(), status_label);

        if capability.can_override && !target.choices_hz.is_empty() {
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
                let sender = self.commands.clone();
                let target_id = capability.id.clone();
                clear.connect_clicked(move |_| {
                    send_command(&sender, ClientCommand::ClearFrequency(target_id.clone()));
                });
            }
            {
                let sender = self.commands.clone();
                let overlay = self.overlay.clone();
                let window = self.window.clone();
                let choices = target.choices_hz.clone();
                let capability = capability.clone();
                apply.connect_clicked(move |_| {
                    let minimum_index = usize::try_from(minimum.selected()).unwrap_or(usize::MAX);
                    let maximum_index = usize::try_from(maximum.selected()).unwrap_or(usize::MAX);
                    let Some(minimum_hz) = choices.get(minimum_index).copied() else {
                        show_toast(&overlay, "Select a minimum frequency");
                        return;
                    };
                    let Some(maximum_hz) = choices.get(maximum_index).copied() else {
                        show_toast(&overlay, "Select a maximum frequency");
                        return;
                    };
                    let request = match frequency_override(&capability, minimum_hz, maximum_hz) {
                        Ok(request) => request,
                        Err(message) => {
                            show_toast(&overlay, &message);
                            return;
                        }
                    };
                    confirm_frequency(&window, sender.clone(), request);
                });
            }
        }
        group.add(&row);
    }

    fn update_status_widgets(&self) {
        let capabilities = self.capabilities.borrow();
        let status = self.status.borrow();
        let view = ViewModel::from_api(&capabilities, &status);

        self.state_row.set_subtitle(&view.daemon_state);
        self.health_row.set_subtitle(&view.health);
        self.profile_row.set_subtitle(&view.profile);
        self.scene_row.set_subtitle(&view.scene);

        for (mode_id, button) in self.mode_buttons.borrow().iter() {
            if *mode_id == status.mode {
                button.add_css_class("suggested-action");
            } else {
                button.remove_css_class("suggested-action");
            }
        }
        for target in &view.targets {
            if let Some(label) = self.target_status.borrow().get(&target.capability.id) {
                label.set_text(&target_status_text(target.status.as_ref()));
            }
        }

        if let Some(thermal) = view.thermal {
            self.thermal_group.set_visible(true);
            self.thermal_row.set_title(&thermal.temperature);
            self.thermal_row
                .set_subtitle(&format!("{} · {}", thermal.state, thermal.detail));
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
        if self.thermal_group.is_visible() {
            let mut status = self.status.borrow_mut();
            status.thermal = telemetry.thermal.clone();
            status.frequencies.clone_from(&telemetry.frequencies);
            drop(status);
            self.update_status_widgets();
        } else {
            for frequency in &telemetry.frequencies {
                self.update_frequency_label(frequency);
            }
        }
    }

    fn update_frequency_label(&self, frequency: &FrequencyStatus) {
        if let Some(label) = self.target_status.borrow().get(&frequency.target_id) {
            label.set_text(&frequency_status_text(frequency));
        }
    }
}

fn status_row(title: &str) -> adw::ActionRow {
    adw::ActionRow::builder().title(title).build()
}

fn parse_workload(pid: &str) -> Result<WorkloadRequest, String> {
    let pid = pid
        .trim()
        .parse::<u32>()
        .map_err(|_| "PID must be a positive integer")?;
    if pid == 0 {
        return Err("PID must be non-zero".into());
    }
    Ok(WorkloadRequest {
        pid,
        mode: String::new(),
        reason: "selected in uperf-gui".into(),
    })
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

fn frequency_status_text(status: &FrequencyStatus) -> String {
    target_status_text(Some(status))
}

fn format_frequency(frequency_hz: u64) -> String {
    format!("{frequency_hz} Hz")
}

fn closest_index(choices: &[u64], requested: u64) -> u32 {
    choices
        .iter()
        .enumerate()
        .min_by_key(|(_, choice)| choice.abs_diff(requested))
        .and_then(|(index, _)| u32::try_from(index).ok())
        .unwrap_or(gtk::INVALID_LIST_POSITION)
}

fn show_toast(overlay: &adw::ToastOverlay, message: &str) {
    overlay.add_toast(adw::Toast::builder().title(message).timeout(5).build());
}

fn send_command(sender: &Sender<ClientCommand>, command: ClientCommand) {
    if let Err(error) = sender.try_send(command) {
        eprintln!("cannot send GUI command: {error}");
    }
}

fn confirm_frequency(
    window: &adw::ApplicationWindow,
    sender: Sender<ClientCommand>,
    request: FrequencyOverride,
) {
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
    let window = window.clone();
    glib::spawn_future_local(async move {
        if dialog.choose_future(Some(&window)).await == Ok(1) {
            send_command(&sender, ClientCommand::SetFrequency(request));
        }
    });
}

async fn emit(sender: &Sender<UiEvent>, event: UiEvent) -> bool {
    sender.send(event).await.is_ok()
}

async fn refresh_status(client: &DaemonClient, events: &Sender<UiEvent>) {
    match client.status().await {
        Ok(status) => {
            emit(events, UiEvent::Status(status)).await;
        }
        Err(error) => {
            emit(events, UiEvent::Error(error.to_string())).await;
        }
    }
}

async fn handle_command(client: &DaemonClient, events: &Sender<UiEvent>, command: ClientCommand) {
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
        ClientCommand::SetWorkload(request) => client
            .set_active_workload(request)
            .await
            .map(|receipt| receipt.message),
        ClientCommand::ClearWorkload => client
            .clear_active_workload()
            .await
            .map(|receipt| receipt.message),
    };
    match result {
        Ok(message) => {
            emit(events, UiEvent::Notice(message)).await;
            refresh_status(client, events).await;
        }
        Err(error) => {
            emit(events, UiEvent::Error(error.to_string())).await;
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn client_session(
    commands: Receiver<ClientCommand>,
    events: Sender<UiEvent>,
) -> Result<(), String> {
    let client = DaemonClient::system()
        .await
        .map_err(|error| error.to_string())?;
    let (capabilities, status) = tokio::try_join!(client.capabilities(), client.status())
        .map_err(|error| error.to_string())?;
    if !emit(
        &events,
        UiEvent::Snapshot {
            capabilities,
            status,
        },
    )
    .await
    {
        return Ok(());
    }

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
    let mut mode_properties = proxy.receive_mode_changed().await;

    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Ok(command) => handle_command(&client, &events, command).await,
                Err(_) => return Ok(()),
            },
            signal = state_signals.next() => {
                if signal.is_none() {
                    return Err("state signal stream ended".into());
                }
                refresh_status(&client, &events).await;
            },
            signal = capability_signals.next() => {
                if signal.is_none() {
                    return Err("capability signal stream ended".into());
                }
                match client.capabilities().await {
                    Ok(capabilities) => {
                        emit(&events, UiEvent::Capabilities(capabilities)).await;
                    }
                    Err(error) => {
                        emit(&events, UiEvent::Error(error.to_string())).await;
                    }
                }
            },
            signal = health_signals.next() => {
                if signal.is_none() {
                    return Err("health signal stream ended".into());
                }
                refresh_status(&client, &events).await;
            },
            signal = telemetry_signals.next() => {
                let Some(signal) = signal else {
                    return Err("telemetry signal stream ended".into());
                };
                match signal.args() {
                    Ok(arguments) => {
                        emit(&events, UiEvent::Telemetry(arguments.snapshot().clone())).await;
                    }
                    Err(error) => {
                        emit(&events, UiEvent::Error(error.to_string())).await;
                    }
                }
            },
            property = mode_properties.next() => {
                if property.is_none() {
                    return Err("property change stream ended".into());
                }
                refresh_status(&client, &events).await;
            },
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
                    let _ = thread_events.try_send(UiEvent::Error(format!(
                        "cannot start D-Bus runtime: {error}"
                    )));
                    return;
                }
            };
            if let Err(error) = runtime.block_on(client_session(commands, thread_events.clone())) {
                let _ = thread_events.try_send(UiEvent::Error(error));
            }
        });
    if let Err(error) = result {
        let _ = events.try_send(UiEvent::Error(format!(
            "cannot start D-Bus client thread: {error}"
        )));
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
    use super::{format_frequency, parse_workload};

    #[test]
    fn workload_request_contains_only_a_pid_identity_input() {
        let request = parse_workload("42").expect("valid workload");
        assert_eq!(request.pid, 42);
        assert!(parse_workload("0").is_err());
    }

    #[test]
    fn frequency_labels_preserve_exact_hertz() {
        assert_eq!(format_frequency(1_001), "1001 Hz");
        assert_ne!(format_frequency(1_001), format_frequency(1_002));
    }
}

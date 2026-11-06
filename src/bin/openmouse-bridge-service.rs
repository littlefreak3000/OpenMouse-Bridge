//! OpenMouse Bridge as a Windows service.
//!
//! The tray app lives in a signed-in user's session, so it is gone while the
//! screen is locked on some setups, before anyone signs in, and after sign-out.
//! This runs the same Bridge core (the loopback API, native HID, profiles and
//! the device-state keepalive) as a LocalSystem service that starts with
//! Windows and keeps replaying the last device state through every lock,
//! unlock, resume, sign-in and sign-out.
//!
//! ```text
//! openmouse-bridge-service install     register the service (run elevated)
//! openmouse-bridge-service uninstall   stop and remove it
//! openmouse-bridge-service run         run in this console, for debugging
//! ```
//!
//! It owns port 17846, so the tray app and the service cannot run together.

#[cfg(not(windows))]
fn main() {
    eprintln!("openmouse-bridge-service is a Windows service and only runs on Windows.");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    windows_service_host::main()
}

#[cfg(windows)]
mod windows_service_host {
    use std::{
        env,
        ffi::OsString,
        net::{IpAddr, Ipv4Addr, SocketAddr},
        path::{Path, PathBuf},
        process::ExitCode,
        sync::{Arc, atomic::AtomicBool, mpsc},
        time::Duration,
    };

    use anyhow::{Context, Result, bail};
    use openmouse_bridge::{
        BRIDGE_PORT, api, config,
        service::{BridgeService, Trigger},
    };
    use tokio::{net::TcpListener, sync::oneshot};
    use tracing_appender::{
        non_blocking::WorkerGuard,
        rolling::{RollingFileAppender, Rotation},
    };
    use tracing_subscriber::{EnvFilter, fmt::writer::MakeWriterExt};
    use windows_service::{
        define_windows_service,
        service::{
            DisplayState, PowerBroadcastSetting, PowerEventParam, ServiceAccess, ServiceAction,
            ServiceActionType, ServiceControl, ServiceControlAccept, ServiceErrorControl,
            ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod, ServiceInfo,
            ServiceStartType, ServiceState, ServiceStatus, ServiceType, SessionChangeReason,
        },
        service_control_handler::{self, ServiceControlHandlerResult, ServiceStatusHandle},
        service_dispatcher,
        service_manager::{ServiceManager, ServiceManagerAccess},
    };

    const SERVICE_NAME: &str = "OpenMouseBridge";
    const DISPLAY_NAME: &str = "OpenMouse Bridge";
    const DESCRIPTION: &str = "Keeps OpenMouse device settings and lighting applied through screen lock, sign-out and sleep.";
    const SERVICE_ARGUMENT: &str = "--service";
    const START_TIMEOUT: Duration = Duration::from_secs(30);

    define_windows_service!(ffi_service_main, service_main);

    pub fn main() -> ExitCode {
        let arguments: Vec<String> = env::args().skip(1).collect();
        let outcome = match arguments.first().map(String::as_str) {
            Some(SERVICE_ARGUMENT) => run_under_scm(),
            Some("install") => install(),
            Some("uninstall") => uninstall(),
            Some("run") => run_in_console(),
            Some("--version" | "version") => {
                println!(
                    "openmouse-bridge-service {}",
                    openmouse_bridge::BRIDGE_VERSION
                );
                Ok(())
            }
            _ => {
                eprintln!(
                    "usage: openmouse-bridge-service <install|uninstall|run>\n\n  install    register the service so it starts with Windows (run elevated)\n  uninstall  stop and remove the service\n  run        run in this console instead of as a service, for debugging"
                );
                return ExitCode::from(2);
            }
        };
        match outcome {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("openmouse-bridge-service: {error:#}");
                ExitCode::FAILURE
            }
        }
    }

    // ---- Paths and logging ------------------------------------------------

    /// `%ProgramData%\OpenMouse\Bridge`, shared by every account and by the
    /// service, which has no user profile worth the name.
    fn data_directory() -> PathBuf {
        env::var_os("ProgramData")
            .map_or_else(|| PathBuf::from(r"C:\ProgramData"), PathBuf::from)
            .join("OpenMouse")
            .join("Bridge")
    }

    /// Points Bridge's config (and so its logs and device list) at the shared
    /// directory unless the caller already chose a place.
    fn use_machine_config() {
        if env::var_os("OPENMOUSE_BRIDGE_CONFIG").is_none() {
            // SAFETY: called first thing in `main`'s call tree, before any thread is started.
            unsafe {
                env::set_var(
                    "OPENMOUSE_BRIDGE_CONFIG",
                    data_directory().join("config.json"),
                );
            }
        }
    }

    fn init_logging(to_console: bool) -> Option<WorkerGuard> {
        let filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "openmouse_bridge=info,tower_http=info".into());
        let subscriber = tracing_subscriber::fmt().with_env_filter(filter);
        let appender = config::log_dir().and_then(|directory| {
            std::fs::create_dir_all(&directory)
                .with_context(|| format!("could not create {}", directory.display()))?;
            RollingFileAppender::builder()
                .rotation(Rotation::DAILY)
                .filename_prefix("openmouse-bridge-service")
                .filename_suffix("log")
                .max_log_files(7)
                .build(&directory)
                .context("could not open the service log file")
        });
        match appender {
            Ok(appender) => {
                let (writer, guard) = tracing_appender::non_blocking(appender);
                if to_console {
                    subscriber.with_writer(std::io::stdout.and(writer)).init();
                } else {
                    subscriber.with_ansi(false).with_writer(writer).init();
                }
                Some(guard)
            }
            Err(error) => {
                subscriber.init();
                tracing::warn!(%error, "file logging is unavailable");
                None
            }
        }
    }

    // ---- The Bridge core --------------------------------------------------

    /// Starts the Bridge core and serves its API until `shutdown` fires.
    /// `ready` receives the service once the port is bound.
    async fn run_core(
        shutdown: oneshot::Receiver<()>,
        ready: mpsc::Sender<BridgeService>,
    ) -> Result<()> {
        // The catalog refresh is skipped: the service never needs the network
        // to start, and the last catalog stays cached in the config.
        let (mut bridge_config, path) = config::load_or_create()?;
        // A service has no user session to relaunch an updater into.
        bridge_config.automatic_updates = false;
        let origins = bridge_config.allowed_origins.clone();
        let service = BridgeService::new(bridge_config, path.clone());
        // Nobody is at the service's desktop to read a notification.
        service.on_notice(|notice| {
            tracing::info!(title = %notice.title, detail = %notice.detail, "notice");
        });
        service.start_game_monitor(Arc::new(AtomicBool::new(true)));
        service.start_battery_monitor();
        service.start_reassert_monitor();

        // The control panel only looks on BRIDGE_PORT; the override is for
        // testing a second instance beside a running one.
        let port = env::var("OPENMOUSE_BRIDGE_PORT")
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(BRIDGE_PORT);
        let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
        let listener = TcpListener::bind(address).await.with_context(|| {
            format!("could not bind http://{address}; is the tray app or another Bridge running?")
        })?;
        tracing::info!(%address, config = %path.display(), "OpenMouse Bridge service is ready");
        let _ = ready.send(service.clone());
        axum::serve(listener, api::router(service, &origins))
            .with_graceful_shutdown(async {
                let _ = shutdown.await;
            })
            .await?;
        Ok(())
    }

    // ---- Console mode -----------------------------------------------------

    fn run_in_console() -> Result<()> {
        use_machine_config();
        let _log_guard = init_logging(true);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("could not create the async runtime")?;
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (ready_tx, _ready_rx) = mpsc::channel();
        runtime.spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            let _ = shutdown_tx.send(());
        });
        println!("Running in the console; press Ctrl-C to stop.");
        runtime.block_on(run_core(shutdown_rx, ready_tx))
    }

    // ---- Service mode -----------------------------------------------------

    fn run_under_scm() -> Result<()> {
        use_machine_config();
        service_dispatcher::start(SERVICE_NAME, ffi_service_main)
            .context("could not connect to the service control manager (run `install`, then start the service)")
    }

    enum Event {
        Trigger(Trigger),
        Stop,
    }

    fn service_main(_arguments: Vec<OsString>) {
        let _log_guard = init_logging(false);
        if let Err(error) = run_service() {
            tracing::error!(error = %format!("{error:#}"), "the service stopped with an error");
        }
    }

    fn trigger_for(reason: SessionChangeReason) -> Trigger {
        match reason {
            SessionChangeReason::SessionLock => Trigger::SessionLock,
            SessionChangeReason::SessionUnlock => Trigger::SessionUnlock,
            SessionChangeReason::SessionLogon => Trigger::SessionLogon,
            SessionChangeReason::SessionLogoff => Trigger::SessionLogoff,
            _ => Trigger::SessionChange,
        }
    }

    /// Asks Windows to tell this service when the monitor turns off and on. A
    /// service receives that as a power event only after subscribing, and the
    /// `windows-service` crate does not wrap the subscription, so it is made here
    /// with the service's own status handle. Without it the monitor-off option
    /// would never fire, so a failure is logged.
    fn subscribe_to_display_changes(handle: &ServiceStatusHandle) {
        use windows_sys::{
            Win32::{
                Foundation::HANDLE, System::Power::RegisterPowerSettingNotification,
                UI::WindowsAndMessaging::DEVICE_NOTIFY_SERVICE_HANDLE,
            },
            core::GUID,
        };
        // GUID_CONSOLE_DISPLAY_STATE: 0 off, 1 on, 2 dimmed.
        const CONSOLE_DISPLAY_STATE: GUID = GUID {
            data1: 0x6FE6_9556,
            data2: 0x704A,
            data3: 0x47A0,
            data4: [0x8F, 0x24, 0xC2, 0x8D, 0x93, 0x6F, 0xDA, 0x47],
        };
        // SAFETY: `ServiceStatusHandle` is a `Copy` newtype around the
        // `SERVICE_STATUS_HANDLE` the control manager gave this service; the
        // transmute requires both to be the same size, and the value is only handed
        // back to the Windows API that issued it.
        let raw = unsafe { std::mem::transmute::<ServiceStatusHandle, usize>(*handle) };
        // SAFETY: a valid service status handle, a static GUID, and the flag that
        // says the recipient is a service.
        let subscription = unsafe {
            RegisterPowerSettingNotification(
                raw as HANDLE,
                &CONSOLE_DISPLAY_STATE,
                DEVICE_NOTIFY_SERVICE_HANDLE,
            )
        };
        if subscription == 0 {
            tracing::warn!(
                error = %std::io::Error::last_os_error(),
                "could not subscribe to monitor on/off events; the monitor-off option will not fire"
            );
        } else {
            tracing::info!("subscribed to monitor on/off events");
        }
    }

    fn report(
        handle: &ServiceStatusHandle,
        state: ServiceState,
        accepted: ServiceControlAccept,
        exit_code: ServiceExitCode,
        checkpoint: u32,
    ) {
        let pending = matches!(
            state,
            ServiceState::StartPending | ServiceState::StopPending
        );
        let status = ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accepted,
            exit_code,
            checkpoint,
            wait_hint: if pending {
                START_TIMEOUT
            } else {
                Duration::ZERO
            },
            process_id: None,
        };
        if let Err(error) = handle.set_service_status(status) {
            tracing::warn!(%error, ?state, "could not report the service state");
        }
    }

    fn run_service() -> Result<()> {
        let (events_tx, events) = mpsc::channel::<Event>();
        let handler_events = events_tx.clone();
        let handle =
            service_control_handler::register(SERVICE_NAME, move |control| match control {
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    let _ = handler_events.send(Event::Stop);
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::SessionChange(change) => {
                    let _ = handler_events.send(Event::Trigger(trigger_for(change.reason)));
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::PowerEvent(
                    PowerEventParam::ResumeAutomatic
                    | PowerEventParam::ResumeSuspend
                    | PowerEventParam::ResumeCritical,
                ) => {
                    let _ = handler_events.send(Event::Trigger(Trigger::Resume));
                    ServiceControlHandlerResult::NoError
                }
                // About to sleep: lets the lights go off first when that option is on.
                ServiceControl::PowerEvent(PowerEventParam::Suspend) => {
                    let _ = handler_events.send(Event::Trigger(Trigger::Suspend));
                    ServiceControlHandlerResult::NoError
                }
                // The monitor went off or came on (the console display state).
                ServiceControl::PowerEvent(PowerEventParam::PowerSettingChange(
                    PowerBroadcastSetting::ConsoleDisplayState(state),
                )) => {
                    match state {
                        DisplayState::Off => {
                            let _ = handler_events.send(Event::Trigger(Trigger::DisplayOff));
                        }
                        DisplayState::On => {
                            let _ = handler_events.send(Event::Trigger(Trigger::DisplayOn));
                        }
                        // Dimming leaves the lights as they are.
                        DisplayState::Dimmed => {}
                    }
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::PowerEvent(_) | ServiceControl::Interrogate => {
                    ServiceControlHandlerResult::NoError
                }
                _ => ServiceControlHandlerResult::NotImplemented,
            })
            .context("could not register the service control handler")?;
        subscribe_to_display_changes(&handle);

        report(
            &handle,
            ServiceState::StartPending,
            ServiceControlAccept::empty(),
            ServiceExitCode::Win32(0),
            1,
        );
        let started = start_core();
        let (runtime, shutdown, core, service) = match started {
            Ok(started) => started,
            Err(error) => {
                report(
                    &handle,
                    ServiceState::Stopped,
                    ServiceControlAccept::empty(),
                    ServiceExitCode::ServiceSpecific(1),
                    0,
                );
                return Err(error);
            }
        };
        report(
            &handle,
            ServiceState::Running,
            ServiceControlAccept::STOP
                | ServiceControlAccept::SHUTDOWN
                | ServiceControlAccept::SESSION_CHANGE
                | ServiceControlAccept::POWER_EVENT,
            ServiceExitCode::Win32(0),
            0,
        );

        let mut failure = None;
        loop {
            match events.recv_timeout(Duration::from_secs(1)) {
                Ok(Event::Trigger(trigger)) => {
                    tracing::info!(?trigger, "session or power change");
                    service.request_reassert(trigger);
                }
                Ok(Event::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if core.is_finished() {
                failure = Some(anyhow::anyhow!("the Bridge core stopped on its own"));
                break;
            }
        }

        report(
            &handle,
            ServiceState::StopPending,
            ServiceControlAccept::empty(),
            ServiceExitCode::Win32(0),
            1,
        );
        let _ = shutdown.send(());
        match runtime.block_on(core) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => failure = Some(error),
            Err(error) => failure = Some(anyhow::anyhow!("the Bridge core panicked: {error}")),
        }
        report(
            &handle,
            ServiceState::Stopped,
            ServiceControlAccept::empty(),
            if failure.is_some() {
                ServiceExitCode::ServiceSpecific(1)
            } else {
                ServiceExitCode::Win32(0)
            },
            0,
        );
        failure.map_or(Ok(()), Err)
    }

    type Started = (
        tokio::runtime::Runtime,
        oneshot::Sender<()>,
        tokio::task::JoinHandle<Result<()>>,
        BridgeService,
    );

    /// Builds the runtime and starts the core, returning once it is serving.
    fn start_core() -> Result<Started> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("could not create the async runtime")?;
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let core = runtime.spawn(run_core(shutdown_rx, ready_tx));
        match ready_rx.recv_timeout(START_TIMEOUT) {
            Ok(service) => Ok((runtime, shutdown_tx, core, service)),
            Err(_) if core.is_finished() => match runtime.block_on(core) {
                Ok(Err(error)) => Err(error),
                Ok(Ok(())) => bail!("the Bridge core ended before it started serving"),
                Err(error) => bail!("the Bridge core panicked while starting: {error}"),
            },
            Err(_) => bail!("the Bridge core did not start within {START_TIMEOUT:?}"),
        }
    }

    // ---- Install and uninstall --------------------------------------------

    fn open_manager(access: ServiceManagerAccess) -> Result<ServiceManager> {
        ServiceManager::local_computer(None::<&str>, access).context(
            "could not open the service control manager; run this from an elevated (administrator) prompt",
        )
    }

    /// Limits the data directory to SYSTEM and administrators for writing, and
    /// lets other accounts read it, so a standard user cannot rewrite what a
    /// SYSTEM service replays onto the hardware.
    fn secure_directory(directory: &Path) -> Result<()> {
        std::fs::create_dir_all(directory)
            .with_context(|| format!("could not create {}", directory.display()))?;
        let status = std::process::Command::new("icacls")
            .arg(directory)
            .args([
                "/inheritance:r",
                "/grant:r",
                "*S-1-5-18:(OI)(CI)F",
                "*S-1-5-32-544:(OI)(CI)F",
                "*S-1-5-32-545:(OI)(CI)RX",
            ])
            .stdout(std::process::Stdio::null())
            .status()
            .context("could not run icacls")?;
        if !status.success() {
            bail!("icacls could not secure {}", directory.display());
        }
        Ok(())
    }

    /// The signed-in user's tray-app config (profiles, custom games, the cached
    /// game catalog), which `install` brings across the first time so the
    /// service starts with what the tray app already had.
    fn config_path_for_user() -> Result<PathBuf> {
        // `config_path` honours the override, so ask with it unset.
        let saved = env::var_os("OPENMOUSE_BRIDGE_CONFIG");
        // SAFETY: install runs single-threaded before any runtime exists.
        unsafe { env::remove_var("OPENMOUSE_BRIDGE_CONFIG") };
        let path = config::config_path();
        if let Some(saved) = saved {
            // SAFETY: as above.
            unsafe { env::set_var("OPENMOUSE_BRIDGE_CONFIG", saved) };
        }
        path
    }

    fn install() -> Result<()> {
        let executable = env::current_exe().context("could not locate this executable")?;
        let directory = data_directory();
        let target = directory.join("config.json");
        let user_config = config_path_for_user();
        secure_directory(&directory)?;
        if !target.exists()
            && let Ok(source) = &user_config
            && source.is_file()
        {
            match std::fs::copy(source, &target) {
                Ok(_) => println!("Imported your existing settings from {}.", source.display()),
                Err(error) => eprintln!("Could not import {}: {error}", source.display()),
            }
        }

        let manager =
            open_manager(ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)?;
        let info = ServiceInfo {
            name: OsString::from(SERVICE_NAME),
            display_name: OsString::from(DISPLAY_NAME),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: executable,
            launch_arguments: vec![OsString::from(SERVICE_ARGUMENT)],
            dependencies: vec![],
            account_name: None, // LocalSystem
            account_password: None,
        };
        let service = manager
            .create_service(
                &info,
                ServiceAccess::CHANGE_CONFIG | ServiceAccess::START | ServiceAccess::QUERY_STATUS,
            )
            .context(
                "could not create the service (is it already installed? run `uninstall` first)",
            )?;
        service
            .set_description(DESCRIPTION)
            .context("could not set the service description")?;
        // Come back after a crash, which matters more for a keepalive than for most services.
        service
            .update_failure_actions(ServiceFailureActions {
                reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 60 * 60)),
                reboot_msg: None,
                command: None,
                actions: Some(vec![
                    ServiceAction {
                        action_type: ServiceActionType::Restart,
                        delay: Duration::from_secs(5),
                    },
                    ServiceAction {
                        action_type: ServiceActionType::Restart,
                        delay: Duration::from_secs(30),
                    },
                    ServiceAction {
                        action_type: ServiceActionType::Restart,
                        delay: Duration::from_secs(60),
                    },
                ]),
            })
            .context("could not set the service recovery actions")?;
        println!("Installed the {DISPLAY_NAME} service (starts with Windows).");
        println!("Settings and logs: {}", directory.display());
        println!("Start it now with:  sc start {SERVICE_NAME}");
        println!("Stop the tray app first: both use port {BRIDGE_PORT}.");
        Ok(())
    }

    fn uninstall() -> Result<()> {
        let manager = open_manager(ServiceManagerAccess::CONNECT)?;
        let service = manager
            .open_service(
                SERVICE_NAME,
                ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
            )
            .context("the service is not installed")?;
        if service.query_status()?.current_state != ServiceState::Stopped {
            let _ = service.stop();
            for _ in 0..30 {
                if service.query_status()?.current_state == ServiceState::Stopped {
                    break;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        service.delete().context("could not delete the service")?;
        println!(
            "Removed the {DISPLAY_NAME} service. Its settings in {} were kept.",
            data_directory().display()
        );
        Ok(())
    }
}

//! Windows service integration (SCM).
//!
//! The binary must talk to the Service Control Manager itself — `sc create`
//! pointing at a plain console program starts, then fails with error 1053
//! ("did not respond in a timely fashion"). `windows-service` provides that
//! handshake; `--service` selects this path.

use std::ffi::OsString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use windows_service::{
    define_windows_service,
    service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
};

use crate::{collect::Collector, config::Config, runtime};

/// Set from `main` before the dispatcher starts, because the SCM hands us no
/// argv of our own.
pub static CONFIG_PATH: OnceLock<String> = OnceLock::new();

const SERVICE_NAME: &str = "ytmon-agent";
const DEFAULT_CONFIG: &str = r"C:\Program Files\ytmon\agent.toml";

define_windows_service!(ffi_service_main, service_main);

pub fn run() -> windows_service::Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
}

fn status(state: ServiceState) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::from_secs(10),
        process_id: None,
    }
}

fn service_main(_arguments: Vec<OsString>) {
    if let Err(e) = run_service() {
        // No console in this mode; the log file is the only record.
        tracing::error!(error = %e, "service stopped with error");
    }
}

fn run_service() -> Result<(), Box<dyn std::error::Error>> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_handler = stop.clone();

    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        match control_event {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                stop_for_handler.store(true, Ordering::SeqCst);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;
    status_handle.set_service_status(status(ServiceState::Running))?;

    let config_path = CONFIG_PATH
        .get()
        .cloned()
        .unwrap_or_else(|| DEFAULT_CONFIG.to_string());

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let result = rt.block_on(async move {
        let cfg = Config::load(&config_path)?;
        let host = cfg.resolved_host();
        let col = Collector::new(cfg.mounts.clone());
        runtime::run_until(cfg, col, host, stop).await
    });

    status_handle.set_service_status(status(ServiceState::Stopped))?;
    result.map_err(|e| e.into())
}

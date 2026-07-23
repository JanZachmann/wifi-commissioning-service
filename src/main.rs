//! WiFi Commissioning Service - Main Entry Point

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
#[cfg(feature = "ble")]
use std::sync::atomic::Ordering;

use clap::Parser;
use tracing::{Instrument, error, info, info_span};
use tracing_subscriber::{fmt::format::FmtSpan, layer::SubscriberExt, util::SubscriberInitExt};
use wifi_commissioning_service::{
    backend::WifiCtrlBackend,
    config::{CliArgs, Settings},
    core::service::WifiCommissioningService,
};

#[cfg(feature = "ble")]
use std::time::Duration;
#[cfg(feature = "ble")]
use tracing::warn;
#[cfg(feature = "ble")]
use wifi_commissioning_service::transport::ble::BleAdapter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,wifi_commissioning=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer().with_span_events(FmtSpan::CLOSE))
        .init();

    info!(
        version = env!("CARGO_PKG_VERSION"),
        git_ref = env!("GIT_SHORT_REF"),
        "Starting WiFi commissioning service"
    );

    // Parse CLI arguments and resolve settings
    let args = CliArgs::parse();
    info!(?args);
    let settings: Settings = args.into();
    info!(?settings);

    if let Err(e) = validate_config(
        cfg!(feature = "ble"),
        settings.enable_ble,
        settings.enable_unix_socket,
        settings.ble_secret.is_some(),
    ) {
        error!("{}", e.log_message());
        return Err(e.reason().into());
    }

    // Create WiFi backend
    let (backend, station_handle) = WifiCtrlBackend::new(settings.interface.clone()).await?;
    let backend = Arc::new(backend);
    info!(
        "WiFi backend initialized for interface: {}",
        settings.interface
    );

    // Create WiFi commissioning service
    let secret = settings
        .ble_secret
        .clone()
        .unwrap_or_else(|| "default".to_string());
    let service = Arc::new(WifiCommissioningService::new(backend, secret));
    info!("WiFi commissioning service created");

    // Start configured transports — any task exiting is fatal
    let mut transport_tasks = tokio::task::JoinSet::new();

    let ble_live = Arc::new(AtomicBool::new(false));

    // Start Unix socket transport (actix-web REST API over UDS)
    if settings.enable_unix_socket {
        info!("Starting Unix socket transport on {}", settings.socket_path);

        let handle = start_unix_socket_transport(
            &settings.socket_path,
            &settings.interface,
            ble_live.clone(),
            service.clone(),
        )?;
        transport_tasks.spawn(async move {
            handle
                .await
                .map_err(|e| format!("Unix socket task panicked: {e}"))?
        });
    }

    // Start BLE transport
    #[cfg(feature = "ble")]
    if settings.enable_ble {
        info!("Starting BLE transport");

        match start_ble_transport(service.clone(), settings.ble_name.clone()).await {
            Ok(handle) => {
                ble_live.store(true, Ordering::Relaxed);
                transport_tasks.spawn(async move {
                    handle
                        .await
                        .map_err(|e| format!("BLE task panicked: {e}"))?
                        .map_err(|e| format!("BLE adapter: {e}"))
                });
            }
            Err(e) => {
                error!("Failed to start BLE transport: {}", e);
                if !settings.enable_unix_socket {
                    return Err(e);
                }
            }
        }
    }

    info!("Service started successfully");

    // Notify systemd that the service is ready
    #[cfg(feature = "systemd")]
    sd_notify::notify(&[sd_notify::NotifyState::Ready])?;

    // Wait for shutdown signal or fatal task failure
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Received SIGINT (Ctrl+C), shutting down gracefully");
        }
        _ = shutdown_signal() => {
            info!("Received SIGTERM, shutting down gracefully");
        }
        result = station_handle => {
            match result {
                Ok(Err(e)) => {
                    error!("wpa_supplicant station runtime failed: {e}");
                    return Err(e.into());
                }
                Err(e) => {
                    error!("wpa_supplicant station task panicked: {e}");
                    return Err(e.into());
                }
                Ok(Ok(())) => {
                    error!("wpa_supplicant station exited unexpectedly");
                    return Err("wpa_supplicant station exited".into());
                }
            }
        }
        Some(result) = transport_tasks.join_next() => {
            match result {
                Ok(Err(e)) => {
                    error!("Transport failed: {e}");
                    return Err(e.into());
                }
                Err(e) => {
                    error!("Transport task panicked: {e}");
                    return Err(e.into());
                }
                Ok(Ok(())) => {
                    error!("Transport exited unexpectedly");
                    return Err("transport exited".into());
                }
            }
        }
    }

    info!("Shutting down...");
    Ok(())
}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigterm = signal(SignalKind::terminate()).expect("Failed to register SIGTERM handler");

    sigterm.recv().await;
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    // On non-Unix platforms, just wait forever
    std::future::pending::<()>().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enable_ble_on_no_ble_build_is_rejected() {
        assert_eq!(
            validate_config(false, true, true, true),
            Err(ConfigError::BleRequestedButNotCompiled)
        );
    }

    #[test]
    fn enable_ble_on_ble_build_is_ok() {
        assert_eq!(validate_config(true, true, true, true), Ok(()));
    }

    #[test]
    fn no_transport_is_rejected_regardless_of_ble_feature() {
        assert_eq!(
            validate_config(true, false, false, false),
            Err(ConfigError::NoTransportEnabled)
        );
        assert_eq!(
            validate_config(false, false, false, false),
            Err(ConfigError::NoTransportEnabled)
        );
    }

    #[test]
    fn unix_socket_only_needs_no_ble_feature_or_secret() {
        assert_eq!(validate_config(false, false, true, false), Ok(()));
        assert_eq!(validate_config(true, false, true, false), Ok(()));
    }

    #[test]
    fn ble_without_secret_is_rejected() {
        assert_eq!(
            validate_config(true, true, true, false),
            Err(ConfigError::BleSecretMissing)
        );
    }

    #[test]
    fn ble_only_with_secret_is_ok() {
        assert_eq!(validate_config(true, true, false, true), Ok(()));
    }

    #[test]
    fn config_error_messages_match_their_variant() {
        assert_eq!(
            ConfigError::BleRequestedButNotCompiled.log_message(),
            "--enable-ble was given but this build has no `ble` feature"
        );
        assert_eq!(
            ConfigError::BleRequestedButNotCompiled.reason(),
            "BLE support not compiled in"
        );
        assert_eq!(
            ConfigError::NoTransportEnabled.log_message(),
            "At least one transport (BLE or Unix socket) must be enabled"
        );
        assert_eq!(
            ConfigError::NoTransportEnabled.reason(),
            "No transport enabled"
        );
        assert_eq!(
            ConfigError::BleSecretMissing.log_message(),
            "BLE transport requires --ble-secret"
        );
        assert_eq!(
            ConfigError::BleSecretMissing.reason(),
            "BLE secret not provided"
        );
    }

    #[cfg(feature = "ble")]
    #[tokio::test(start_paused = true)]
    async fn retry_returns_ok_without_sleeping() {
        let calls = std::cell::Cell::new(0u32);
        let start = tokio::time::Instant::now();

        let result: Result<u32, &str> = retry(5, std::time::Duration::from_secs(2), || {
            calls.set(calls.get() + 1);
            std::future::ready(Ok(42))
        })
        .await;

        assert_eq!(result, Ok(42));
        assert_eq!(calls.get(), 1);
        assert_eq!(start.elapsed(), std::time::Duration::ZERO);
    }

    #[cfg(feature = "ble")]
    #[tokio::test(start_paused = true)]
    async fn retry_exhausts_all_attempts_then_errors() {
        const MAX: u32 = 5;
        const DELAY: std::time::Duration = std::time::Duration::from_secs(2);
        let calls = std::cell::Cell::new(0u32);
        let start = tokio::time::Instant::now();

        let result: Result<(), &str> = retry(MAX, DELAY, || {
            calls.set(calls.get() + 1);
            std::future::ready(Err("boom"))
        })
        .await;

        assert_eq!(result, Err("boom"));
        assert_eq!(calls.get(), MAX);
        assert_eq!(start.elapsed(), DELAY * (MAX - 1));
    }

    #[cfg(feature = "ble")]
    #[tokio::test(start_paused = true)]
    async fn retry_stops_after_first_success() {
        const DELAY: std::time::Duration = std::time::Duration::from_secs(2);
        let calls = std::cell::Cell::new(0u32);
        let start = tokio::time::Instant::now();

        let result: Result<u32, &str> = retry(5, DELAY, || {
            let n = calls.get() + 1;
            calls.set(n);
            std::future::ready(if n < 3 { Err("boom") } else { Ok(n) })
        })
        .await;

        assert_eq!(result, Ok(3));
        assert_eq!(calls.get(), 3);
        assert_eq!(start.elapsed(), DELAY * 2);
    }
}

/// Invalid transport/feature combination detected before startup.
#[derive(Debug, PartialEq, Eq)]
enum ConfigError {
    BleRequestedButNotCompiled,
    NoTransportEnabled,
    BleSecretMissing,
}

impl ConfigError {
    fn log_message(&self) -> &'static str {
        match self {
            Self::BleRequestedButNotCompiled => {
                "--enable-ble was given but this build has no `ble` feature"
            }
            Self::NoTransportEnabled => {
                "At least one transport (BLE or Unix socket) must be enabled"
            }
            Self::BleSecretMissing => "BLE transport requires --ble-secret",
        }
    }

    fn reason(&self) -> &'static str {
        match self {
            Self::BleRequestedButNotCompiled => "BLE support not compiled in",
            Self::NoTransportEnabled => "No transport enabled",
            Self::BleSecretMissing => "BLE secret not provided",
        }
    }
}

/// Reject transport/feature combinations that cannot run. `ble_compiled` is
/// `cfg!(feature = "ble")` at the call site, passed in so the check is testable
/// in both build configurations.
fn validate_config(
    ble_compiled: bool,
    enable_ble: bool,
    enable_unix_socket: bool,
    ble_secret_present: bool,
) -> Result<(), ConfigError> {
    if enable_ble && !ble_compiled {
        return Err(ConfigError::BleRequestedButNotCompiled);
    }
    if !enable_ble && !enable_unix_socket {
        return Err(ConfigError::NoTransportEnabled);
    }
    if enable_ble && !ble_secret_present {
        return Err(ConfigError::BleSecretMissing);
    }
    Ok(())
}

fn start_unix_socket_transport(
    socket_path: &str,
    interface_name: &str,
    ble_live: Arc<AtomicBool>,
    service: Arc<WifiCommissioningService<WifiCtrlBackend>>,
) -> Result<tokio::task::JoinHandle<Result<(), String>>, Box<dyn std::error::Error>> {
    use wifi_commissioning_service::transport::unix_socket::server;

    let srv = server::create(
        socket_path,
        interface_name,
        ble_live,
        service.scanner.clone(),
        service.connector.clone(),
        service.net_management.clone(),
    )?;

    let span = info_span!("unix_socket_transport");
    let handle = tokio::spawn(
        async move { srv.await.map_err(|e| format!("Unix socket server: {e}")) }.instrument(span),
    );

    Ok(handle)
}

/// Max attempts to bring up the BLE adapter at startup.
///
/// At cold boot bluetoothd auto-powers the default adapter. Our set_powered
/// call can race that power-on and get org.bluez.Error.Busy. Retrying lets the
/// adapter settle before we fall back to Unix-socket-only.
#[cfg(feature = "ble")]
const BLE_START_MAX_ATTEMPTS: u32 = 5;
#[cfg(feature = "ble")]
const BLE_START_RETRY_DELAY: Duration = Duration::from_secs(2);

/// Retry an async operation up to `max_attempts` times, sleeping `delay`
/// between attempts. Returns the last error once attempts are exhausted.
#[cfg(feature = "ble")]
async fn retry<T, E, F, Fut>(max_attempts: u32, delay: Duration, mut op: F) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    let mut attempt = 1;
    loop {
        match op().await {
            Ok(value) => return Ok(value),
            Err(e) if attempt < max_attempts => {
                warn!(
                    attempt,
                    max_attempts,
                    "operation failed, retrying in {}s: {e}",
                    delay.as_secs(),
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(feature = "ble")]
async fn bring_up_ble_adapter(
    service: Arc<WifiCommissioningService<WifiCtrlBackend>>,
    ble_name: String,
) -> Result<BleAdapter<WifiCtrlBackend>, bluer::Error> {
    use wifi_commissioning_service::transport::ble::GattServer;

    let mut adapter = BleAdapter::new(ble_name).await?;
    let session = adapter.session();

    let gatt_server = Arc::new(GattServer::new(service, session));
    adapter.start(gatt_server).await?;

    Ok(adapter)
}

#[cfg(feature = "ble")]
async fn start_ble_transport(
    service: Arc<WifiCommissioningService<WifiCtrlBackend>>,
    ble_name: String,
) -> Result<tokio::task::JoinHandle<Result<(), bluer::Error>>, Box<dyn std::error::Error>> {
    let adapter = retry(BLE_START_MAX_ATTEMPTS, BLE_START_RETRY_DELAY, || {
        bring_up_ble_adapter(service.clone(), ble_name.clone())
    })
    .await?;

    let span = info_span!("ble_transport");
    let handle = tokio::spawn(async move { adapter.run_event_loop().await }.instrument(span));

    Ok(handle)
}

use std::sync::Once;

use chrono::Utc;
use serde_json::json;
use tracing::info;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

static TRACING_INIT: Once = Once::new();

pub(super) fn record_phase_latency_ms(phase: &'static str, elapsed_ms: f64, outcome: &'static str) {
    tracing::debug!(phase, outcome, elapsed_ms, "dispatch phase latency");
}

pub(super) fn init_telemetry() {
    TRACING_INIT.call_once(|| {
        let env_filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
        let fmt_layer = tracing_subscriber::fmt::layer().with_target(true).compact();

        let _ = tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt_layer)
            .try_init();
    });
}

pub(super) fn log_line(event: &str, payload: serde_json::Value) {
    info!(event_name = event, payload = %payload, "orchd log event");
    let line = json!({
        "ts": Utc::now().to_rfc3339(),
        "event": event,
        "data": payload,
    });
    println!("{line}");
}

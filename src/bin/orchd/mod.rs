// orchd module index.
//
// Policy: each submodule import gets a 1-2 line indexing comment explaining:
// - what the submodule does
// - when you should read it (vs. safely skip it)

// Legacy monolith (transitional). Read only while shattering it into focused modules.
mod legacy;

// CLI flags/subcommands for orchd. Read when changing invocation or dispatch mode/backends.
mod cli;

// Home/tilde expansion and path normalization. Read when fixing path handling or sandbox IO roots.
mod paths;

// orchd dispatch configuration parsing/types. Read when changing prompt templates, roles, or directives.
mod dispatch_config;

// Dispatch errors + mapping to control-plane orchd runtime state. Read when adjusting failure semantics.
mod errors;

// Shared in-memory types (webhook payloads, decision records, app state). Read when wiring event->dispatch flow.
mod state;

// Webhook signature verification + directive parsing/decision. Read when changing how comments trigger dispatch.
mod webhook;

// SQLite schema + queries + dispatch state transitions. Read when changing locking, queues, or persistence.
mod db;

// tmux integration: naming, window/pane liveness probing, and run-script generation. Read when debugging operator UX.
mod tmux;

// Logging + tracing initialization. Read when debugging orchd logs or adding new observability.
mod telemetry;

pub fn run_entry() -> anyhow::Result<()> {
    legacy::run_entry()
}

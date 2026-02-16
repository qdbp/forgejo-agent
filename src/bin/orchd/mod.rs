// orchd module index.
//
// Policy: each submodule import gets a 1-2 line indexing comment explaining:
// - what the submodule does
// - when you should read it (vs. safely skip it)

// orchd entrypoint wiring (CLI parse, server runtime, subcommands). Read when changing startup.
mod entry;

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

// Issue/label/comment projection into Forgejo (API + forgejoctl). Read when changing how orchd reflects state back to issues.
mod projection;

// Webhook signature verification + directive parsing/decision. Read when changing how comments trigger dispatch.
mod webhook;

// Canonical tokens for directives/decisions/event-types. Read when adjusting directive grammar or DB literals.
mod lexicon;

// Versioned SQLite migrations. Read when changing schema shape or adding backwards-compatible upgrades.
mod migrations;

// SQLite schema + queries + dispatch state transitions. Read when changing locking, queues, or persistence.
mod db;

// Dispatch pipeline (plan/materialize/launch) + stale-dispatch healing. Read when changing orchestration semantics.
mod dispatch;

// Repo/worktree management + git plumbing (with token injection). Read when touching checkouts, worktrees, or locks.
mod repo;

// forgejoctl subprocess wrapper. Read when debugging control-plane side effects (labels/state/comments) from orchd.
mod forgejoctl_cmd;

// Dispatch finalization (db terminal transitions, landing/autoland/PR). Read when changing completion semantics.
mod finalize;

// Run script generation for codex offline execution + finalize handoff. Read when changing dispatch launch payload.
mod run_script;

// Operator-facing issue subcommands (postmortem session resume). Read when extending orchd obs/issue CLI surface.
mod issue;

// Logging + tracing initialization. Read when debugging orchd logs or adding new observability.
mod telemetry;

// Desktop notification loop for dispatch lifecycle transitions. Read when changing operator alerts.
mod notifier;

// Axum server (webhook ingress) + background loops. Read when changing runtime wiring or webhook handling.
mod server;

pub fn run_entry() -> anyhow::Result<()> {
    entry::run_entry()
}

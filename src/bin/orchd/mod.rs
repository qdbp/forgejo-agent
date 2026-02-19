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

// Live dispatch-config snapshot + hot-reload. Read when removing restart requirements or hardening durability.
mod dispatch_config_live;

// Reading material (DocPlan) config + render. Read when changing doc inclusion policy or prompt bloat controls.
mod reading_material;

// Dispatch errors + mapping to control-plane orchd runtime state. Read when adjusting failure semantics.
mod errors;

// Minimal `{{token}}` prompt templating utilities. Read when changing template syntax or rendering rules.
mod template;

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

// Failure inquisition automation (spawn `audit-failure` tickets on harness failures). Read when changing auto-audit behavior.
mod inquisition;

// Dispatch finalization (db terminal transitions + PR landing). Read when changing completion semantics.
mod finalize;

// Typed dispatch runner used by backends (replaces generated run.sh). Read when changing codex execution/finalize boundary.
mod run_dispatch;

// Observability issue subcommands (`orchd obs issue sessions|list|resume`). Read when extending issue-level session inspection/resume.
mod issue;

// Role inventory/check/add subcommands for dispatch identity hygiene. Read when onboarding roles or debugging role drift.
mod role;

// Timer scheduler entrypoints (`orchd schedule list|tick`). Read when changing periodic dispatch behavior or context reuse policy.
mod schedule;

// Prompt inspection utilities (preview rendered prompt + DocPlan). Read when debugging prompt composition.
mod prompt;

// Timer-session observability (`orchd obs timer sessions|list|resume`). Read when changing timer-level resume interlocks.
mod timer;

// Logging + tracing initialization. Read when debugging orchd logs or adding new observability.
mod telemetry;

// Desktop notification loop for dispatch lifecycle and codex replies. Read when changing operator alerts.
mod notifier;

// Push-triggered deploy queue + Rust-native deploy worker lane (`orchd deploy worker`). Read when changing autodeploy behavior.
mod deploy;

// Axum server (webhook ingress) + background loops. Read when changing runtime wiring or webhook handling.
mod server;

pub fn run_entry() -> anyhow::Result<()> {
    entry::run_entry()
}

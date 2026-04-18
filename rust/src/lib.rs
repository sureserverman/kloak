//! kloak — Rust reimplementation of the keystroke / mouse timing anonymizer.
//!
//! Stages 3 onward populate this crate with real logic. Stage 3 adds the
//! pure-logic modules (config, keys, escape combo parsing, scroll tick
//! accumulator, jitter scheduler, typed event model). Stage 4 adds the
//! uinput output backend. Stage 5 wires libinput ingest and the poll loop.
//!
//! See `docs/plans/2026-04-18-kloak-rust-rewrite-plan.md`.

#![forbid(unsafe_op_in_unsafe_fn)]
#![warn(missing_debug_implementations)]

pub mod config;
pub mod escape;
pub mod event;
pub mod keys;
pub mod queue;
pub mod scroll;

#[cfg(target_os = "linux")]
pub mod uinput;

#[cfg(target_os = "linux")]
pub mod time_src;

#[cfg(target_os = "linux")]
pub mod urandom;

#[cfg(target_os = "linux")]
pub mod libinput_ctx;

#[cfg(target_os = "linux")]
pub mod hotplug;

#[cfg(target_os = "linux")]
pub mod translate;

pub use config::{Config, ParseOutcome};
pub use escape::EscCombo;
pub use event::InputPacket;
pub use queue::{RandBetween, ScheduledPacket, Scheduler};

#[cfg(target_os = "linux")]
pub use uinput::UInput;

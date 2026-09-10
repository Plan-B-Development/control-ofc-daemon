//! `control-ofc-tray` — a StatusNotifierItem client for the Control-OFC daemon.
//!
//! # What this is, and what it is deliberately not
//!
//! The tray is a **read-mostly API client** (DEC-352). It calls `GET /status`,
//! `GET /profiles`, `POST /profile/activate` and `POST /profile/deactivate`,
//! and nothing else. It holds no lease, writes no PWM, evaluates no curve, and
//! reads no hardware. The daemon has no knowledge of it and no dependency on
//! it: killing the tray, or never starting it, changes nothing about fan
//! control.
//!
//! It lives in its own workspace crate specifically so that boundary is
//! enforced by the crate graph rather than by convention — there is no path
//! from here into the daemon's internals.
//!
//! # Why it costs nothing when idle
//!
//! There is no timer and no polling loop. Plasma calls `AboutToShow` on every
//! context-menu open, which ksni surfaces as
//! [`ksni::Tray::menu_about_to_show`]; the tray reads the daemon there and
//! nowhere else. An idle tray issues zero requests, runs zero timers, and its
//! threads sit blocked in epoll.

pub mod client;
pub mod launch;
pub mod menu;
pub mod single_instance;

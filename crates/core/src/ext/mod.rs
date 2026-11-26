//! Per-lane extension points (`SPEC-contracts.md` M8).
//!
//! After W0a, `fsm-core` is frozen: a lane that needs a shared helper or
//! type adds it to its own `ext/<lane>.rs` and nowhere else in this crate.
//! Lanes must not add variants to [`crate::FsmError`]. There is no `w5.rs`.

pub mod w0b;
pub mod w0c;
pub mod w0d;
pub mod w1a;
pub mod w1b;
pub mod w1c;
pub mod w1d;
pub mod w2a;
pub mod w2b;
pub mod w2c;
pub mod w3a;
pub mod w3b;
pub mod w3c;
pub mod w4a;
pub mod w4b;
pub mod w6a;
pub mod w7;

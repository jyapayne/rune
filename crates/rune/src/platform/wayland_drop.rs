//! Native Wayland drag and drop via `wl_data_device`: file drops onto windows
//! (`wl_data_offer`) and drags out of them (`wl_data_source` with `start_drag`).
//!
//! The drop state machine, RFC 2483 URI-list decoder, surface routing table, and bounded
//! pipe-read loop live in `machine`, and the drag source's offers, payloads, icon and session
//! machine in `source`, so they compile and run unit tests on every platform; the Wayland wire
//! binding and reader thread live in `wire` on desktop Unix targets.

pub(crate) mod machine;
#[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
mod pipe;
pub(crate) mod source;
#[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
mod wire;

#[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
pub(crate) use wire::{DropTarget, install, start_drag, take};

#[cfg(test)]
#[path = "wayland_drop/tests.rs"]
mod tests;
#[cfg(test)]
#[path = "wayland_drop/source_tests.rs"]
mod source_tests;

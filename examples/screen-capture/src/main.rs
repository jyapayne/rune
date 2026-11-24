//! Captures a Wayland compositor's outputs and windows with `rune::capture`: one picture to a
//! PNG, a timed benchmark of continuous captures, or a window of live previews.

#![forbid(unsafe_code)]

#[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
mod bench;
#[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
mod cli;
#[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
mod view;

#[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
fn main() {
    cli::main();
}

#[cfg(not(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android"))))]
fn main() {
    eprintln!("screen-capture runs on Wayland compositors only");
}

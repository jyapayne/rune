//! What the macOS panel and an overlay on a window of its own (X11, Windows) report as
//! `nonactivating_keys`, kept cross-platform so it compiles and runs tests on every host. As
//! with a layer-shell surface, a policy that takes keys (`KeyPolicy::takes_keys`) lets the
//! window take them when `OverlayHandle::request_keys` asks or the user clicks it, and showing
//! alone never does; `KeyPolicy::None` never takes them. Only layer-shell tells
//! `KeyPolicy::Exclusive` from `OnDemand`.

use crate::{KeyPolicy, OverlaySupport};

/// What an overlay with `policy` reports as `OverlayCapabilities::nonactivating_keys`.
/// `focuser`: the platform piece that focuses it is there (X11's own connection, which a server
/// without the SHAPE extension doesn't get; always there for the macOS panel and on Windows).
#[cfg_attr(not(any(target_os = "windows", all(unix, not(target_os = "ios"), not(target_os = "android")))), allow(dead_code))]
pub(crate) fn keys_support(policy: KeyPolicy, focuser: bool) -> OverlaySupport {
    if policy.takes_keys() && focuser { OverlaySupport::Supported } else { OverlaySupport::Unsupported }
}

#[cfg(test)]
#[path = "keys_tests.rs"]
mod tests;

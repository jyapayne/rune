use super::keys_support;
use crate::{KeyPolicy, OverlayInput, OverlaySupport};

#[test]
fn only_a_policy_that_takes_keys_makes_a_window_that_can() {
    assert!(!KeyPolicy::None.takes_keys(), "never activates, as today");
    assert!(KeyPolicy::OnDemand.takes_keys());
    assert!(KeyPolicy::Exclusive.takes_keys());
    assert!(!KeyPolicy::default().takes_keys(), "an overlay takes no keys unless it asks");
    assert_eq!(keys_support(KeyPolicy::None, true), OverlaySupport::Unsupported);
    for policy in [KeyPolicy::OnDemand, KeyPolicy::Exclusive] {
        assert_eq!(keys_support(policy, true), OverlaySupport::Supported, "{policy:?}");
        assert_eq!(keys_support(policy, false), OverlaySupport::Unsupported, "no X11 connection to focus it with, {policy:?}");
    }
}

/// `KeyPolicy` means the same on every backend that can take keys: one that takes them may,
/// `None` never does. Only XWayland, whose override-redirect window never takes focus, refuses
/// them, and only layer-shell tells `Exclusive` from `OnDemand`: it needs no version 4.
#[test]
fn key_policy_agrees_across_backends() {
    use crate::platform::layer_shell_machine::keyboard::supports_keys as layer;
    use crate::overlay::xwayland::xwayland_allowed;
    for policy in [KeyPolicy::None, KeyPolicy::OnDemand, KeyPolicy::Exclusive] {
        let wants = policy.takes_keys();
        assert_eq!(keys_support(policy, true) == OverlaySupport::Supported, wants, "macOS panel, X11 and Windows windows, {policy:?}");
        assert_eq!(layer(policy, 4, true), wants, "layer-shell, {policy:?}");
        assert_eq!(xwayland_allowed(OverlayInput::None, policy, true), !wants, "XWayland, {policy:?}");
        assert!(!xwayland_allowed(OverlayInput::All, policy, false), "XWayland takes the pointer only by opt-in, {policy:?}");
    }
    assert!(layer(KeyPolicy::Exclusive, 1, true), "exclusive interactivity is in every layer-shell version");
    assert!(!layer(KeyPolicy::OnDemand, 3, true), "on-demand interactivity came with version 4");
}

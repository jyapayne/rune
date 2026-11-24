//! A layer surface against a mock compositor on a socket pair: what `show` and `hide` put on
//! the wire, and that no call waits for the compositor.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use wayland_client::{Connection, Proxy};
use winit::raw_window_handle::{RawDisplayHandle, WaylandDisplayHandle};

use super::super::{Input, LayerSurface};
use crate::el::Align;
use crate::overlay::{Placement, ScreenEdge};
use crate::platform::layer_shell_machine::{keyboard, mapping};
use crate::{KeyPolicy, OverlayOptions};

fn push_u32(buf: &mut Vec<u8>, v: u32) { buf.extend_from_slice(&v.to_ne_bytes()); }

fn push_str(buf: &mut Vec<u8>, s: &str) {
    push_u32(buf, (s.len() + 1) as u32);
    buf.extend_from_slice(s.as_bytes());
    buf.push(0);
    while buf.len() % 4 != 0 { buf.push(0); }
}

fn send(stream: &mut UnixStream, sender: u32, opcode: u16, payload: &[u8]) -> std::io::Result<()> {
    let total = 8 + payload.len();
    let mut msg = Vec::with_capacity(total);
    push_u32(&mut msg, sender);
    push_u32(&mut msg, ((total as u32) << 16) | u32::from(opcode));
    msg.extend_from_slice(payload);
    stream.write_all(&msg)
}

fn u32_at(buf: &[u8], at: usize) -> u32 { u32::from_ne_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]]) }

/// What the mock saw, in order, and the ids of the objects the test sends events from.
#[derive(Default)]
struct Seen {
    requests: Vec<String>,
    layer: u32,
    grab: u32,
}

/// A compositor that knows a layer surface's rules as wlroots keeps them: the first commit of
/// a role is answered with a configure, and a null buffer puts the role back where it began.
/// It has `hyprland_focus_grab_manager_v1` too, and no seat.
fn mock_compositor(mut server: UnixStream, seen: Arc<Mutex<Seen>>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let (mut registry, mut compositor, mut shell, mut grabs) = (0u32, 0u32, 0u32, 0u32);
        let (mut surface, mut layer, mut grab) = (0u32, 0u32, 0u32);
        let (mut initialised, mut null_attached, mut serial) = (false, false, 0u32);
        let mut header = [0u8; 8];
        while server.read_exact(&mut header).is_ok() {
            let (sender, word) = (u32_at(&header, 0), u32_at(&header, 4));
            let (size, opcode) = ((word >> 16) as usize, (word & 0xffff) as u16);
            if size < 8 { break; }
            let mut payload = vec![0u8; size - 8];
            if server.read_exact(&mut payload).is_err() { break; }
            let note = |what: String| seen.lock().unwrap().requests.push(what);
            if sender == 1 && opcode == 1 {
                registry = u32_at(&payload, 0);
                for (name, interface, version) in [(1, "wl_compositor", 6), (2, "zwlr_layer_shell_v1", 4), (3, "hyprland_focus_grab_manager_v1", 1)] {
                    let mut global = Vec::new();
                    push_u32(&mut global, name);
                    push_str(&mut global, interface);
                    push_u32(&mut global, version);
                    if send(&mut server, registry, 0, &global).is_err() { return; }
                }
            } else if sender == 1 && opcode == 0 {
                if surface != 0 { note("sync".into()); }
                if send(&mut server, u32_at(&payload, 0), 0, &0u32.to_ne_bytes()).is_err() { return; }
            } else if sender == registry && opcode == 0 {
                let name = u32_at(&payload, 0);
                let padded = (u32_at(&payload, 4) as usize + 3) & !3;
                let id = u32_at(&payload, 8 + padded + 4);
                match name {
                    1 => compositor = id,
                    2 => shell = id,
                    _ => grabs = id,
                }
            } else if sender == compositor && opcode == 0 {
                surface = u32_at(&payload, 0);
                note("create_surface".into());
            } else if sender == shell && opcode == 0 {
                layer = u32_at(&payload, 0);
                seen.lock().unwrap().layer = layer;
                note("get_layer_surface".into());
            } else if sender == grabs && grabs != 0 && opcode == 0 {
                grab = u32_at(&payload, 0);
                seen.lock().unwrap().grab = grab;
                note("create_grab".into());
            } else if sender == grab && grab != 0 {
                match opcode {
                    0 => note(format!("grab add_surface {}", if u32_at(&payload, 0) == surface { "ours" } else { "another" })),
                    2 => note("grab commit".into()),
                    3 => {
                        note("grab destroy".into());
                        grab = 0;
                    }
                    _ => {}
                }
            } else if sender == layer && layer != 0 {
                match opcode {
                    0 => note(format!("set_size {} {}", u32_at(&payload, 0), u32_at(&payload, 4))),
                    4 => note(format!("set_keyboard_interactivity {}", u32_at(&payload, 0))),
                    6 => note(format!("ack {}", u32_at(&payload, 0))),
                    _ => {}
                }
            } else if sender == surface && surface != 0 {
                match opcode {
                    1 => {
                        null_attached = u32_at(&payload, 0) == 0;
                        note(if null_attached { "attach nil" } else { "attach buffer" }.into());
                    }
                    6 if std::mem::take(&mut null_attached) => {
                        note("commit unmap".into());
                        initialised = false;
                    }
                    6 => {
                        note("commit".into());
                        if !std::mem::replace(&mut initialised, true) {
                            serial += 1;
                            let mut configure = Vec::new();
                            for v in [serial, 200, 100] { push_u32(&mut configure, v); }
                            if send(&mut server, layer, 0, &configure).is_err() { return; }
                        }
                    }
                    _ => {}
                }
            }
        }
    })
}

/// One read of the display socket, as winit's loop does for every queue on the connection.
fn read_once(host: &Connection) {
    let guard = host.prepare_read().or_else(|| {
        host.backend().dispatch_inner_queue().ok();
        host.prepare_read()
    });
    if let Some(guard) = guard { let _ = guard.read(); }
}

/// Turns the loop, hidden or shown, until `done`; the mock answers from its own thread.
fn turn_until(host: &Connection, layer: &mut LayerSurface, shown: bool, done: impl Fn(&LayerSurface) -> bool) {
    for _ in 0..400 {
        read_once(host);
        if shown { layer.dispatch(); } else { layer.drain(); }
        if done(layer) { return; }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("the mock compositor never answered; the surface is {:?}", layer.state.mapping);
}

/// What the mock saw since the last call. A roundtrip first, so it has read every request
/// sent so far; the roundtrip's own `sync` is the last thing it saw.
fn requests(host: &Connection, seen: &Mutex<Seen>) -> Vec<String> {
    host.roundtrip().expect("a roundtrip with the mock compositor");
    let mut requests = std::mem::take(&mut seen.lock().unwrap().requests);
    assert_eq!(requests.pop().as_deref(), Some("sync"));
    requests
}

fn display(host: &Connection) -> RawDisplayHandle {
    RawDisplayHandle::Wayland(WaylandDisplayHandle::new(NonNull::new(host.display().id().as_ptr().cast()).unwrap()))
}

const AT: Placement = Placement { size: (200.0, 100.0), edge: ScreenEdge::Top, margin: 0.0, align: Align::Start, inset: 0.0 };

#[test]
fn a_kept_surface_unmaps_on_hide_and_maps_again_without_a_new_surface_or_a_wait() -> std::io::Result<()> {
    let (client, server) = UnixStream::pair()?;
    let mut server_tx = server.try_clone()?;
    let seen = Arc::new(Mutex::new(Seen::default()));
    let compositor = mock_compositor(server, seen.clone());
    let Ok(host) = Connection::from_socket(client) else { return Ok(()); };

    // Made: the role state is committed and the configure asked for, not waited for.
    let mut layer = LayerSurface::new(display(&host), "kept", AT, false, &OverlayOptions::default(), None).expect("a layer surface");
    assert!(matches!(layer.state.mapping, mapping::State::AwaitingConfigure { .. }), "new returns before the configure");
    assert!(!layer.frame_ready(), "nothing draws before the configure");
    assert!(layer.configure_deadline().is_some());
    turn_until(&host, &mut layer, false, |l| l.frame_ready());
    assert_eq!(layer.configure_deadline(), None);
    assert_eq!(requests(&host, &seen), ["create_surface", "get_layer_surface", "set_size 200 100", "set_keyboard_interactivity 0", "commit", "ack 1"]);
    assert_eq!(layer.dispatch().size, Some((200, 100)), "the configured size waits for a shown turn");

    // Shown and drawn: wgpu's present attaches the buffer, which the mock never sees.
    layer.show(AT);
    layer.request_frame();
    layer.presented();
    assert!(requests(&host, &seen).is_empty(), "a configured surface shows by drawing, and one that takes no keys grabs nothing");
    assert!(!layer.frame_ready(), "its frame callback is outstanding");

    // Hidden: a null buffer and a sync behind it. A configure the compositor sent before the
    // unmap reached it is already on the socket, and must not be acked.
    let layer_id = seen.lock().unwrap().layer;
    let mut stale = Vec::new();
    for v in [77, 200, 100] { push_u32(&mut stale, v); }
    send(&mut server_tx, layer_id, 0, &stale)?;
    layer.hide();
    assert!(layer.state.frame.ready() && layer.state.frame_callback.is_none(), "the unmapped surface waits on no frame callback");
    assert!(!layer.frame_ready() && layer.is_live());
    turn_until(&host, &mut layer, false, |l| l.state.mapping == mapping::State::Unmapped);
    assert_eq!(requests(&host, &seen), ["attach nil", "commit unmap", "sync"]);
    // Unmapped, a placement change commits nothing: a commit would ask for a configure.
    let at = Placement { size: (240.0, 120.0), ..AT };
    layer.place(at);
    layer.commit();

    // Shown again: the role state whole, one commit, then the configure, on the same objects.
    layer.show(at);
    assert!(matches!(layer.state.mapping, mapping::State::AwaitingConfigure { .. }));
    assert!(!layer.frame_ready());
    turn_until(&host, &mut layer, true, |l| l.frame_ready());
    assert_eq!(requests(&host, &seen), ["set_size 240 120", "set_size 240 120", "set_keyboard_interactivity 0", "commit", "ack 2"]);
    layer.presented();

    // A show that follows its hide at once commits only after the unmap's fence answers.
    layer.hide();
    layer.show(at);
    assert!(matches!(layer.state.mapping, mapping::State::Unmapping { remap: true, .. }));
    turn_until(&host, &mut layer, true, |l| l.frame_ready());
    assert_eq!(requests(&host, &seen), ["attach nil", "commit unmap", "sync", "set_size 240 120", "set_keyboard_interactivity 0", "commit", "ack 3"]);

    drop(layer);
    drop(host);
    drop(server_tx);
    compositor.join().unwrap();
    Ok(())
}

#[test]
fn an_exclusive_surface_asks_for_the_keys_as_it_shows_and_guards_itself_with_a_focus_grab() -> std::io::Result<()> {
    let (client, server) = UnixStream::pair()?;
    let mut server_tx = server.try_clone()?;
    let seen = Arc::new(Mutex::new(Seen::default()));
    let compositor = mock_compositor(server, seen.clone());
    let Ok(host) = Connection::from_socket(client) else { return Ok(()); };
    let options = OverlayOptions { keys: KeyPolicy::Exclusive, ..Default::default() };

    // Made hidden (a prewarm): no keys asked for, and no grab on a surface with no buffer.
    let mut layer = LayerSurface::new(display(&host), "launcher", AT, true, &options, None).expect("a layer surface");
    turn_until(&host, &mut layer, false, |l| l.frame_ready());
    assert_eq!(requests(&host, &seen), ["create_surface", "get_layer_surface", "set_size 200 100", "set_keyboard_interactivity 0", "commit", "ack 1"]);

    // Shown: exclusive interactivity rides the first buffer's commit, and the grab follows it.
    layer.show(AT);
    assert_eq!(requests(&host, &seen), ["set_keyboard_interactivity 1"]);
    layer.presented();
    assert_eq!(requests(&host, &seen), ["create_grab", "grab add_surface ours", "grab commit"]);
    layer.presented();
    assert!(requests(&host, &seen).is_empty(), "one grab for one show");

    // The keyboard enters, then the user presses outside: the compositor clears the grab. The
    // overlay hears one blur, and the keys still come until it hides.
    layer.state.step_keyboard_focus(keyboard::FocusEvent::EnterOurs);
    assert!(matches!(layer.dispatch().input[..], [Input::Focus(true)]));
    let grab = seen.lock().unwrap().grab;
    send(&mut server_tx, grab, 0, &[])?;
    let mut input = Vec::new();
    for _ in 0..400 {
        read_once(&host);
        input.extend(layer.dispatch().input);
        if !input.is_empty() { break; }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(matches!(input[..], [Input::Focus(false)]), "a cleared grab is a blur");
    assert!(layer.has_keyboard_focus(), "the surface still holds the keyboard");
    assert_eq!(requests(&host, &seen), ["grab destroy"], "cleared, the grab is inert and goes");

    // Hidden: the keys are given up before the null buffer. This compositor, like Hyprland
    // with nothing else to focus, sends no leave: focus is its to take, and it hasn't.
    layer.hide();
    turn_until(&host, &mut layer, false, |l| l.state.mapping == mapping::State::Unmapped);
    assert!(layer.has_keyboard_focus());
    assert_eq!(requests(&host, &seen), ["set_keyboard_interactivity 0", "attach nil", "commit unmap", "sync"]);
    // Shown again: the keys asked for anew and a new grab with the first buffer. No enter
    // comes either, so mapping is what gives the dismissed overlay its focus back.
    layer.show(AT);
    turn_until(&host, &mut layer, true, |l| l.frame_ready());
    assert_eq!(requests(&host, &seen), ["set_keyboard_interactivity 1", "set_size 200 100", "set_keyboard_interactivity 1", "commit", "ack 2"]);
    layer.presented();
    assert!(matches!(layer.dispatch().input[..], [Input::Focus(true)]), "mapped again, it has the focus");
    assert_eq!(requests(&host, &seen), ["create_grab", "grab add_surface ours", "grab commit"]);
    // Hidden while the grab holds, the grab goes with the unmap.
    layer.hide();
    turn_until(&host, &mut layer, false, |l| l.state.mapping == mapping::State::Unmapped);
    assert_eq!(requests(&host, &seen), ["set_keyboard_interactivity 0", "grab destroy", "attach nil", "commit unmap", "sync"]);

    drop(layer);
    drop(host);
    drop(server_tx);
    compositor.join().unwrap();
    Ok(())
}

#[test]
fn a_surface_the_compositor_never_configures_gives_up_at_its_deadline() -> std::io::Result<()> {
    let (client, server) = UnixStream::pair()?;
    let seen = Arc::new(Mutex::new(Seen::default()));
    let compositor = mock_compositor(server, seen.clone());
    let Ok(host) = Connection::from_socket(client) else { return Ok(()); };
    let mut layer = LayerSurface::new(display(&host), "silent", AT, false, &OverlayOptions::default(), None).expect("a layer surface");

    // Its configure is on the socket, and nothing reads it: the wake timer's turn passes the
    // deadline as an event.
    let mapping::State::AwaitingConfigure { attempt, due } = layer.state.mapping else { panic!("{:?}", layer.state.mapping) };
    assert_eq!(layer.configure_deadline(), Some(due));
    assert_eq!(layer.drain(), None, "not due yet");
    layer.state.step_mapping(mapping::Event::Timeout { attempt, now: due });
    layer.carry_out();
    assert_eq!(layer.drain(), Some(mapping::Gone::Unanswered));
    assert!(!layer.is_live() && !layer.frame_ready() && layer.configure_deadline().is_none());
    assert_eq!(layer.dispatch().gone, Some(mapping::Gone::Unanswered));

    drop(layer);
    drop(host);
    compositor.join().unwrap();
    Ok(())
}

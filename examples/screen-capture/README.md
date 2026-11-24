# Screen capture

Captures a Wayland compositor's outputs and windows with `rune::capture`: one picture to a PNG,
a timed benchmark of continuous captures, or a window of live previews. Windows are named by
their Hyprland address, as `hyprctl clients -j` shows it.

```sh
cargo run -p screen-capture -- info                           # protocols, outputs, windows
cargo run -p screen-capture -- shot output DP-2 target/dp-2.png
cargo run -p screen-capture -- shot window 0x55d1c0de4ab0 target/window.png
cargo run -p screen-capture --profile perf -- bench output DP-2 10 10
cargo run -p screen-capture --profile perf -- bench windows 4 10   # every listed window
cargo run -p screen-capture --profile perf -- bench shots DP-2 30
cargo run -p screen-capture -- view                           # live previews in a window
cargo run -p screen-capture -- --legacy info                  # wlr-screencopy + hyprland-toplevel-export
```

```rust
let capturer = Capturer::connect()?;
let screen = capturer.shot_blocking(Target::output("DP-2"), Duration::from_secs(1))?;

let preview = NativeSurface::new();
let stream = capturer.stream(Target::window(address), preview.clone(), 4.0);
stream.set_shown(true);   // only while the preview is on screen
el().native_surface(&preview, SurfaceFit::Fit)
```

## What Hyprland 0.57.2 offers

`ext-image-copy-capture-v1` with `ext-output-image-capture-source-manager-v1` and
`ext-foreign-toplevel-image-capture-source-manager-v1`, `ext-foreign-toplevel-list-v1`,
`hyprland-toplevel-mapping-v1` (a listed window's address), and the older
`wlr-screencopy-unstable-v1` (v3) and `hyprland-toplevel-export-v1` (v2). `rune::capture` uses the
`ext` protocols and falls back on the older two (`--legacy` forces them). Its sessions take
`wl_shm` buffers in `XRGB8888`/`ARGB8888` (and their B, G, R twins).

- A session's first frame comes on the compositor's next render, about one refresh: 16.7 ms
  median for a 1920×1080 headless output at 60 Hz, busy or still, either protocol.
- Later frames come only once the content has changed. Hyprland's unit is the output's render:
  a window's frame can come because something else on its output changed, but a still output
  sends nothing.
- A window on a hidden workspace is captured (Hyprland renders it for the capture) as long as
  its workspace's monitor renders: one that doesn't (a nested compositor's window that isn't
  drawn) leaves the capture unanswered.
- `hyprland-toplevel-export-v1` leaves a frame of a window that doesn't exist, or has closed,
  unanswered. `rune::capture` checks windows against the list where it can, and ends a stream
  whose window the list says closed.

## Measured

Hyprland 0.57.2 nested in a desktop session, a 1920×1080 headless output at scale 2, `--profile
perf`, AMD Ryzen 7 8745HS. Sources animate continuously (`vkcube`), so every allowed frame comes.
Latency runs from the compositor's presentation time of the frame's content to the frame's push
into its `NativeSurface`; CPU is this process's (all of it on the capture thread) and the
compositor's extra over the same scene without capture. Not counted: drawing the frames in a
view (the upload of each frame to the GPU).

| | Frames | Latency median / p95 | Read and convert median / p95 | CPU (capture) | CPU (compositor, extra) |
|---|---|---|---|---|---|
| One 1920×1080 output, cap 10 fps, 10 s | 8.6 fps | 4.0 / 7.8 ms | 1.9 / 3.4 ms | 1.8 % of a core | 2.4 % |
| Eight windows (1600×900 and 1836×916), cap 4 fps, 10 s | 3.8 fps each | 6.8 / 9.8 ms | 1.9 / 2.9 ms | 6.1 % | 2.6 % |

A stream asks for its next frame one interval after the last arrived, and the compositor answers
on its next render after that, so a busy source delivers a little under the cap (8.6 for 10).

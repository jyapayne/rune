//! A window of live previews: the first output (or the one named) at 10 fps, and every listed
//! window at 4 fps, each a `NativeSurface` a capture stream keeps current.

use std::sync::OnceLock;

use rune::capture::{Capturer, Stream, Target};
use rune::{AppSpec, Cx, El, NativeSurface, SurfaceFit, Waker, col, el, row, text};

static ARGS: OnceLock<(bool, Option<String>)> = OnceLock::new();

struct Previews {
    tiles: Vec<Tile>,
}

struct Tile {
    label: String,
    surface: NativeSurface,
    stream: Stream,
}

pub(crate) fn run(legacy: bool, output: Option<String>) {
    let _ = ARGS.set((legacy, output));
    rune::run(AppSpec { setup: Some(setup), ..AppSpec::new("Screen capture", (980.0, 660.0), state, view) });
}

fn tile(capturer: &Capturer, target: Target, label: String, fps: f32) -> Tile {
    let surface = NativeSurface::new();
    let stream = capturer.stream(target, surface.clone(), fps);
    Tile { label, surface, stream }
}

fn state() -> Previews {
    let (legacy, output) = ARGS.get().cloned().unwrap_or_default();
    let capturer = crate::cli::connect(legacy);
    let mut tiles = Vec::new();
    if let Some(name) = output.or_else(|| capturer.outputs().into_iter().next()) {
        tiles.push(tile(&capturer, Target::output(name.clone()), format!("{name} · 10 fps"), 10.0));
    }
    for window in capturer.windows() {
        tiles.push(tile(&capturer, Target::window(window.address), format!("{} {} · 4 fps", window.app_id, window.address), 4.0));
    }
    Previews { tiles }
}

fn setup(s: &mut Previews, wake: Waker) {
    for tile in &s.tiles {
        tile.surface.set_waker(wake.clone());
        tile.stream.set_shown(true);
    }
}

fn view(s: &Previews, _: &mut Cx<Previews>) -> El {
    col().key("previews").gap(12.0).padding(12.0).children(s.tiles.chunks(3).enumerate().map(|(r, tiles)| {
        row().key(format!("row-{r}")).gap(12.0).children(tiles.iter().enumerate().map(|(c, tile)| {
            col().key(format!("tile-{r}-{c}")).gap(4.0).children([
                el().w(300.0).h(169.0).radius(8.0).native_surface(&tile.surface, SurfaceFit::Fit),
                text(tile.label.clone()),
            ])
        }))
    }))
}

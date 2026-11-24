use std::path::Path;
use std::process::exit;
use std::time::{Duration, Instant};

use rune::capture::{Capturer, Target, WindowAddress};

const USAGE: &str = "usage: screen-capture [--legacy] <command>
  info                                  protocols, outputs and windows
  shot output <NAME> <PNG> [--cursor]   one picture of an output
  shot window <ADDRESS> <PNG>           one picture of a window (hyprctl clients -j's address)
  bench output <NAME> <FPS> <SECONDS>   stream an output, then print frame rate, latency and CPU
  bench shots <NAME> <COUNT>            one picture of an output COUNT times, then print how long each took
  bench windows <FPS> <SECONDS> [ADDRESS...]  stream these windows (every listed one by default) the same way
  view [<OUTPUT>]                       a window of live previews: the output at 10 fps, windows at 4
--legacy uses wlr-screencopy and hyprland-toplevel-export even where ext-image-copy-capture is offered.";

pub(crate) fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let legacy = take_flag(&mut args, "--legacy");
    let cursor = take_flag(&mut args, "--cursor");
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    if let ["view", rest @ ..] = args.as_slice() {
        return crate::view::run(legacy, rest.first().map(|name| name.to_string()));
    }
    let capturer = connect(legacy);
    match args.as_slice() {
        ["info"] => info(&capturer),
        ["shot", "output", name, png] => {
            let target = if cursor { Target::output(*name).with_cursor() } else { Target::output(*name) };
            shot(&capturer, target, png);
        }
        ["shot", "window", address, png] => shot(&capturer, Target::window(address_or_exit(address)), png),
        ["bench", "output", name, fps, seconds] => crate::bench::run(&capturer, vec![Target::output(*name)], number(fps), number(seconds)),
        ["bench", "shots", name, count] => crate::bench::shots(&capturer, Target::output(*name), number(count) as usize),
        ["bench", "windows", fps, seconds, addresses @ ..] => {
            let targets = if addresses.is_empty() {
                capturer.windows().into_iter().map(|window| Target::window(window.address)).collect()
            } else {
                addresses.iter().map(|address| Target::window(address_or_exit(address))).collect()
            };
            crate::bench::run(&capturer, targets, number(fps), number(seconds));
        }
        _ => {
            eprintln!("{USAGE}");
            exit(2);
        }
    }
}

fn take_flag(args: &mut Vec<String>, flag: &str) -> bool {
    let before = args.len();
    args.retain(|arg| arg != flag);
    args.len() != before
}

pub(crate) fn connect(legacy: bool) -> Capturer {
    let connected = if legacy { Capturer::connect_legacy() } else { Capturer::connect() };
    connected.unwrap_or_else(|e| {
        eprintln!("screen-capture: {e}");
        exit(1);
    })
}

fn number(text: &str) -> f32 {
    text.parse().unwrap_or_else(|_| {
        eprintln!("screen-capture: {text} isn't a number");
        exit(2);
    })
}

fn address_or_exit(text: &str) -> WindowAddress {
    WindowAddress::parse(text).unwrap_or_else(|| {
        eprintln!("screen-capture: {text} isn't a window address");
        exit(2);
    })
}

fn info(capturer: &Capturer) {
    let protocols = capturer.protocols();
    println!("protocols: {protocols:?}");
    println!("outputs captured with {:?}, windows with {:?}", protocols.outputs(), protocols.windows());
    for output in capturer.outputs() {
        println!("output {output}");
    }
    for window in capturer.windows() {
        println!("window {} {:?} {:?}", window.address, window.app_id, window.title);
    }
}

fn shot(capturer: &Capturer, target: Target, png: &str) {
    let start = Instant::now();
    match capturer.shot_blocking(target, Duration::from_secs(5)) {
        Ok(still) => {
            let took = start.elapsed();
            rune::render::write_png(Path::new(png), still.width(), still.height(), &still.to_rgba()).unwrap_or_else(|e| {
                eprintln!("screen-capture: {png}: {e}");
                exit(1);
            });
            println!("{}x{} in {:.1} ms -> {png}", still.width(), still.height(), took.as_secs_f64() * 1e3);
        }
        Err(e) => {
            eprintln!("screen-capture: {e}");
            exit(1);
        }
    }
}

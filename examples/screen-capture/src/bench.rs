//! Streams targets for a while, then prints what they delivered and what it cost: frames and
//! rate per stream, latency from the compositor's presentation to the push and the read and
//! conversion time (median and p95 over every frame), and the CPU this process and its capture
//! thread used. What it leaves out: drawing the frames (GPU upload and presentation in a view),
//! and the compositor's own copies (read the compositor's CPU time beside it).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rune::NativeSurface;
use rune::capture::{Capturer, Target};

#[derive(Default)]
struct Samples {
    frames: Vec<usize>,
    latency: Vec<Duration>,
    copy: Vec<Duration>,
}

pub(crate) fn run(capturer: &Capturer, targets: Vec<Target>, fps: f32, seconds: f32) {
    if targets.is_empty() {
        eprintln!("screen-capture: nothing to stream");
        std::process::exit(1);
    }
    let samples = Arc::new(Mutex::new(Samples { frames: vec![0; targets.len()], ..Samples::default() }));
    let mut streams = Vec::new();
    let cpu_before = (cpu_ticks("self"), capture_thread().map(|tid| cpu_ticks(&format!("self/task/{tid}"))));
    let start = Instant::now();
    for (i, target) in targets.iter().enumerate() {
        let surface = NativeSurface::new();
        let stream = Arc::new(capturer.stream(target.clone(), surface.clone(), fps));
        let (weak, samples) = (Arc::downgrade(&stream), samples.clone());
        surface.set_waker(Arc::new(move || {
            let Some(stream) = weak.upgrade() else { return };
            let stats = stream.stats();
            let mut samples = samples.lock().unwrap();
            samples.frames[i] += 1;
            samples.latency.extend(stats.latency);
            samples.copy.extend(stats.copy);
        }));
        stream.set_shown(true);
        streams.push(stream);
    }
    std::thread::sleep(Duration::from_secs_f32(seconds));
    let wall = start.elapsed().as_secs_f64();
    let thread = capture_thread();
    let cpu_after = (cpu_ticks("self"), thread.map(|tid| cpu_ticks(&format!("self/task/{tid}"))));
    let samples = samples.lock().unwrap();
    for (i, target) in targets.iter().enumerate() {
        let ended = streams[i].stats().ended.map(|why| format!(" (ended: {why})")).unwrap_or_default();
        println!("{target:?}: {} frames, {:.2} fps{ended}", samples.frames[i], samples.frames[i] as f64 / wall);
    }
    println!("latency, presentation to push: {}", spread(&samples.latency));
    println!("read and convert: {}", spread(&samples.copy));
    // USER_HZ, 100 on Linux.
    let percent = |ticks: u64| ticks as f64 / 100.0 / wall * 100.0;
    println!("process CPU: {:.1}% of one core over {wall:.1} s", percent(cpu_after.0 - cpu_before.0));
    if let (Some(before), Some(after)) = (cpu_before.1, cpu_after.1) {
        println!("capture thread CPU: {:.1}% of one core", percent(after - before));
    }
}

/// One picture of `target`, `count` times in a row on one connection: from asking to holding it.
pub(crate) fn shots(capturer: &Capturer, target: Target, count: usize) {
    let mut took = Vec::with_capacity(count);
    for _ in 0..count {
        let start = Instant::now();
        if let Err(e) = capturer.shot_blocking(target.clone(), Duration::from_secs(5)) {
            eprintln!("screen-capture: {e}");
            std::process::exit(1);
        }
        took.push(start.elapsed());
    }
    println!("{target:?}: one picture in {}", spread(&took));
}

fn spread(values: &[Duration]) -> String {
    if values.is_empty() {
        return "none".into();
    }
    let mut sorted = values.to_vec();
    sorted.sort();
    let at = |q: f64| sorted[((sorted.len() - 1) as f64 * q).round() as usize].as_secs_f64() * 1e3;
    format!("median {:.2} ms, p95 {:.2} ms, max {:.2} ms over {} frames", at(0.5), at(0.95), at(1.0), sorted.len())
}

/// User and system CPU ticks of `/proc/<which>/stat`.
fn cpu_ticks(which: &str) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{which}/stat")).unwrap_or_default();
    // Fields after the parenthesised name: state is the 3rd field, utime the 14th, stime the 15th.
    let rest = stat.rsplit_once(')').map_or("", |(_, rest)| rest);
    let fields: Vec<u64> = rest.split_whitespace().skip(11).take(2).filter_map(|v| v.parse().ok()).collect();
    fields.iter().sum()
}

/// The capture thread's id, by its name.
fn capture_thread() -> Option<String> {
    std::fs::read_dir("/proc/self/task").ok()?.flatten().find_map(|task| {
        let comm = std::fs::read_to_string(task.path().join("comm")).ok()?;
        (comm.trim() == "rune-capture").then(|| task.file_name().to_string_lossy().into_owned())
    })
}

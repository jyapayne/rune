use wayland_client::protocol::wl_output::Transform;
use wayland_client::protocol::wl_shm::Format;

use super::{Layout, Source, Turn, pick, upright};

/// A `w`×`h` buffer whose pixel `(x, y)` is B=x, G=y, R=7, A=255, rows `stride` bytes apart
/// with the padding filled with 0xEE, as a compositor writes it in `Layout::Bgra`.
fn numbered(w: u32, h: u32, stride: u32) -> Vec<u8> {
    let mut bytes = vec![0xEE; (stride * h) as usize];
    for y in 0..h {
        for x in 0..w {
            let at = (y * stride + x * 4) as usize;
            bytes[at..at + 4].copy_from_slice(&[x as u8, y as u8, 7, 255]);
        }
    }
    bytes
}

fn source(w: u32, h: u32, stride: u32, turn: Turn, y_invert: bool) -> Source {
    Source { width: w, height: h, stride, layout: Layout::Bgra, turn, y_invert }
}

/// The (x, y) each upright pixel came from, read back from `numbered`'s B and G bytes.
fn origins(out: &[u8], w: u32) -> Vec<Vec<(u8, u8)>> {
    out.chunks_exact(w as usize * 4).map(|row| row.as_chunks::<4>().0.iter().map(|p| (p[0], p[1])).collect()).collect()
}

#[test]
fn a_packed_upright_buffer_is_kept_and_only_its_fourth_byte_is_fixed() {
    let mut bytes = numbered(3, 2, 12);
    for pixel in bytes.as_chunks_mut::<4>().0 {
        pixel[3] = 0x42; // XRGB8888's unused byte holds anything
    }
    let before = bytes.as_ptr();
    let src = Source { layout: Layout::Bgrx, ..source(3, 2, 12, Turn::Normal, false) };
    let out = upright(&src, bytes).unwrap();
    assert_eq!(out.as_ptr(), before, "no copy for a buffer that is already the picture");
    assert_eq!(out.len(), 24);
    assert!(out.as_chunks::<4>().0.iter().all(|p| p[3] == 255), "an unused byte reads as opaque");
    assert_eq!(origins(&out, 3), [[(0, 0), (1, 0), (2, 0)], [(0, 1), (1, 1), (2, 1)]]);
}

#[test]
fn rows_padded_by_the_compositor_are_packed() {
    let out = upright(&source(3, 2, 16, Turn::Normal, false), numbered(3, 2, 16)).unwrap();
    assert_eq!(out.len(), 3 * 2 * 4);
    assert!(!out.contains(&0xEE), "no padding byte reaches the picture");
    assert_eq!(origins(&out, 3), [[(0, 0), (1, 0), (2, 0)], [(0, 1), (1, 1), (2, 1)]]);
}

#[test]
fn a_y_inverted_buffer_comes_out_top_row_first() {
    let out = upright(&source(2, 3, 8, Turn::Normal, true), numbered(2, 3, 8)).unwrap();
    assert_eq!(origins(&out, 2), [[(0, 2), (1, 2)], [(0, 1), (1, 1)], [(0, 0), (1, 0)]]);
}

#[test]
fn every_output_transform_is_undone() {
    // An upright 3×2 picture numbered by its own (x, y), turned by each transform the way a
    // compositor turns it into the buffer, comes back to the same numbering.
    let upright_picture: Vec<Vec<(u8, u8)>> = (0..2).map(|y| (0..3).map(|x| (x, y)).collect()).collect();
    for turn in [Turn::Normal, Turn::Rotate90, Turn::Rotate180, Turn::Rotate270, Turn::Flipped, Turn::Flipped90, Turn::Flipped180, Turn::Flipped270] {
        let (bw, bh) = if matches!(turn, Turn::Rotate90 | Turn::Rotate270 | Turn::Flipped90 | Turn::Flipped270) { (2, 3) } else { (3, 2) };
        let mut buffer = vec![0u8; bw * bh * 4];
        for (y, row) in upright_picture.iter().enumerate() {
            for (x, &(px, py)) in row.iter().enumerate() {
                let [bx, by] = turn.buffer_at([x as i64, y as i64], [3, 2]);
                assert!((0..bw as i64).contains(&bx) && (0..bh as i64).contains(&by), "{turn:?} keeps ({x}, {y}) inside the buffer");
                let at = (by as usize * bw + bx as usize) * 4;
                buffer[at..at + 4].copy_from_slice(&[px, py, 0, 255]);
            }
        }
        let src = source(bw as u32, bh as u32, bw as u32 * 4, turn, false);
        assert_eq!(src.upright_size(), [3, 2], "{turn:?}");
        assert_eq!(origins(&upright(&src, buffer).unwrap(), 3), upright_picture, "{turn:?}");
    }
}

#[test]
fn a_quarter_turn_moves_the_top_left_corner_where_wl_output_says() {
    // wl_output's 90 is counter-clockwise: the picture's top-left corner lands bottom-left.
    assert_eq!(Turn::Rotate90.buffer_at([0, 0], [3, 2]), [0, 2]);
    assert_eq!(Turn::Rotate270.buffer_at([0, 0], [3, 2]), [1, 0]);
    assert_eq!(Turn::Flipped.buffer_at([0, 0], [3, 2]), [2, 0]);
    assert_eq!(Turn::of(Transform::_90), Turn::Rotate90);
    assert_eq!(Turn::of(Transform::Flipped270), Turn::Flipped270);
}

#[test]
fn premultiplied_alpha_comes_out_straight() {
    let src = Source { layout: Layout::Bgra, ..source(3, 1, 12, Turn::Normal, false) };
    let out = upright(&src, vec![50, 100, 128, 128, 9, 9, 9, 0, 1, 2, 3, 255]).unwrap();
    assert_eq!(&out[0..4], &[100, 199, 255, 128], "half-covered colour doubles back");
    assert_eq!(&out[4..8], &[0, 0, 0, 0], "fully transparent is black");
    assert_eq!(&out[8..12], &[1, 2, 3, 255], "opaque is untouched");
}

#[test]
fn red_and_blue_trade_places_for_the_bgr_layouts() {
    let src = Source { layout: Layout::Rgbx, ..source(1, 1, 4, Turn::Normal, false) };
    assert_eq!(upright(&src, vec![10, 20, 30, 0]).unwrap(), [30, 20, 10, 255]);
    let src = Source { layout: Layout::Rgba, ..src };
    assert_eq!(upright(&src, vec![10, 20, 30, 255]).unwrap(), [30, 20, 10, 255]);
}

#[test]
fn a_buffer_shorter_than_it_claims_is_refused() {
    assert_eq!(upright(&source(3, 2, 12, Turn::Normal, false), vec![0; 23]), None);
    assert_eq!(upright(&source(3, 2, 8, Turn::Normal, false), vec![0; 64]), None, "a row can't fit its stride");
}

#[test]
fn the_format_asked_for_needs_no_swap_and_keeps_alpha_only_where_wanted() {
    let offered = [Format::Abgr8888, Format::Xrgb8888, Format::Argb8888, Format::Rgb565];
    assert_eq!(pick(&offered, true), Some(Format::Argb8888));
    assert_eq!(pick(&offered, false), Some(Format::Xrgb8888));
    assert_eq!(pick(&[Format::Xbgr8888], true), Some(Format::Xbgr8888));
    assert_eq!(pick(&[Format::Rgb565, Format::Xrgb2101010], false), None, "nothing this reads");
    assert_eq!(Layout::of(Format::Xbgr8888), Some(Layout::Rgbx));
}

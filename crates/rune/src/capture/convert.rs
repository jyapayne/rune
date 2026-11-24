//! A captured buffer's bytes turned into the picture a view shows: B, G, R, A bytes with
//! straight alpha, rows packed, the top row first and upright. Compositors write shm buffers
//! with their own row stride, in one of four 8-bit layouts (alpha premultiplied, or an unused
//! fourth byte), possibly upside down (`wlr-screencopy`'s `y_invert`) or turned the way the
//! output is (`ext-image-copy-capture`'s `transform`).

use wayland_client::protocol::wl_output::Transform;
use wayland_client::protocol::wl_shm::Format;

/// The byte order of a buffer's pixels and what its fourth byte means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Layout {
    /// `XRGB8888`: B, G, R, unused.
    Bgrx,
    /// `ARGB8888`: B, G, R, A, premultiplied.
    Bgra,
    /// `XBGR8888`: R, G, B, unused.
    Rgbx,
    /// `ABGR8888`: R, G, B, A, premultiplied.
    Rgba,
}

impl Layout {
    pub(crate) fn of(format: Format) -> Option<Layout> {
        match format {
            Format::Xrgb8888 => Some(Layout::Bgrx),
            Format::Argb8888 => Some(Layout::Bgra),
            Format::Xbgr8888 => Some(Layout::Rgbx),
            Format::Abgr8888 => Some(Layout::Rgba),
            _ => None,
        }
    }
}

/// The format to ask for among those the compositor `offered`: one this module reads, B, G, R
/// order first (no swap), with alpha first when the picture keeps it (a window with
/// translucent parts) and without when it doesn't (an output, opaque).
pub(crate) fn pick(offered: &[Format], alpha: bool) -> Option<Format> {
    let order = if alpha {
        [Format::Argb8888, Format::Xrgb8888, Format::Abgr8888, Format::Xbgr8888]
    } else {
        [Format::Xrgb8888, Format::Argb8888, Format::Xbgr8888, Format::Abgr8888]
    };
    order.into_iter().find(|format| offered.contains(format))
}

/// The rotation or flip a compositor applied to a buffer's contents (`wl_output.transform`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Turn {
    #[default]
    Normal,
    /// 90° counter-clockwise.
    Rotate90,
    Rotate180,
    Rotate270,
    /// Mirrored left to right.
    Flipped,
    Flipped90,
    Flipped180,
    Flipped270,
}

impl Turn {
    pub(crate) fn of(transform: Transform) -> Turn {
        match transform {
            Transform::_90 => Turn::Rotate90,
            Transform::_180 => Turn::Rotate180,
            Transform::_270 => Turn::Rotate270,
            Transform::Flipped => Turn::Flipped,
            Transform::Flipped90 => Turn::Flipped90,
            Transform::Flipped180 => Turn::Flipped180,
            Transform::Flipped270 => Turn::Flipped270,
            _ => Turn::Normal,
        }
    }

    fn swaps(self) -> bool {
        matches!(self, Turn::Rotate90 | Turn::Rotate270 | Turn::Flipped90 | Turn::Flipped270)
    }

    /// Where the upright pixel `(x, y)` of a `w`×`h` picture lies in the buffer.
    fn buffer_at(self, [x, y]: [i64; 2], [w, h]: [i64; 2]) -> [i64; 2] {
        match self {
            Turn::Normal => [x, y],
            Turn::Rotate90 => [y, w - 1 - x],
            Turn::Rotate180 => [w - 1 - x, h - 1 - y],
            Turn::Rotate270 => [h - 1 - y, x],
            Turn::Flipped => [w - 1 - x, y],
            Turn::Flipped90 => [y, x],
            Turn::Flipped180 => [x, h - 1 - y],
            Turn::Flipped270 => [h - 1 - y, w - 1 - x],
        }
    }
}

/// How a captured buffer holds its picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Source {
    /// The buffer's size, as allocated.
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) stride: u32,
    pub(crate) layout: Layout,
    pub(crate) turn: Turn,
    /// The buffer's rows run bottom to top.
    pub(crate) y_invert: bool,
}

impl Source {
    /// The bytes the buffer spans.
    pub(crate) fn len(&self) -> usize {
        self.stride as usize * self.height as usize
    }

    /// The upright picture's size.
    pub(crate) fn upright_size(&self) -> [u32; 2] {
        if self.turn.swaps() { [self.height, self.width] } else { [self.width, self.height] }
    }

    /// Where the buffer's pixel `(x, y)` starts, in bytes.
    fn offset(&self, [x, y]: [i64; 2]) -> i64 {
        let row = if self.y_invert { i64::from(self.height) - 1 - y } else { y };
        row * i64::from(self.stride) + x * 4
    }
}

/// The upright picture in `bytes` (the buffer `source` describes), as packed straight-alpha
/// B, G, R, A rows. `bytes` is reused when the buffer is already upright and packed. None when
/// `bytes` is shorter than the buffer or the stride can't hold a row.
pub(crate) fn upright(source: &Source, mut bytes: Vec<u8>) -> Option<Vec<u8>> {
    if bytes.len() < source.len() || (source.stride as usize) < source.width as usize * 4 {
        return None;
    }
    let [w, h] = source.upright_size();
    let packed = source.stride == source.width * 4;
    if source.turn == Turn::Normal && !source.y_invert && packed {
        bytes.truncate(source.len());
        fix(source.layout, &mut bytes);
        return Some(bytes);
    }
    // Each upright pixel is an affine step through the buffer: the first, and one to the right
    // and one down from it.
    let size = [i64::from(w), i64::from(h)];
    let at = |p: [i64; 2]| source.offset(source.turn.buffer_at(p, size));
    let origin = at([0, 0]);
    let (dx, dy) = (at([1, 0]) - origin, at([0, 1]) - origin);
    let mut out = vec![0u8; w as usize * h as usize * 4];
    for (y, row) in out.chunks_exact_mut(w as usize * 4).enumerate() {
        let start = origin + y as i64 * dy;
        if dx == 4 {
            let start = start as usize;
            row.copy_from_slice(&bytes[start..start + row.len()]);
        } else {
            for (x, pixel) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let at = (start + x as i64 * dx) as usize;
                pixel.copy_from_slice(&bytes[at..at + 4]);
            }
        }
    }
    fix(source.layout, &mut out);
    Some(out)
}

/// Packed pixels in `layout` to straight-alpha B, G, R, A in place.
/// One loop per layout, so the opaque ones (every output) stay a plain store per pixel.
fn fix(layout: Layout, pixels: &mut [u8]) {
    let pixels = pixels.as_chunks_mut::<4>().0.iter_mut();
    match layout {
        Layout::Bgrx => pixels.for_each(|pixel| pixel[3] = 255),
        Layout::Rgbx => pixels.for_each(|pixel| {
            pixel.swap(0, 2);
            pixel[3] = 255;
        }),
        Layout::Bgra => pixels.for_each(unpremultiply),
        Layout::Rgba => pixels.for_each(|pixel| {
            pixel.swap(0, 2);
            unpremultiply(pixel);
        }),
    }
}

fn unpremultiply(pixel: &mut [u8; 4]) {
    match pixel[3] {
        255 => {}
        0 => pixel[..3].fill(0),
        a => {
            let a = u32::from(a);
            for c in &mut pixel[..3] {
                *c = ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
}

#[cfg(test)]
#[path = "convert_tests.rs"]
mod tests;

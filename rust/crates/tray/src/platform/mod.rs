//! The native parts: the icon, the menu, the window, single instance.

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(not(windows))]
mod unix;
#[cfg(not(windows))]
pub use unix::*;

/// RGBA pixels of the frame closest to `want` px in a 32-bit .ico (the files build-icons.mjs writes).
#[cfg_attr(windows, allow(dead_code))]
pub fn ico_rgba(bytes: &[u8], want: u32) -> Option<(Vec<u8>, u32)> {
    let u16le = |o: usize| bytes.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let u32le = |o: usize| bytes.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    if u16le(2)? != 1 {
        return None;
    }
    let n = u16le(4)? as usize;
    let mut best: Option<(u32, usize, usize)> = None;
    for i in 0..n {
        let e = 6 + i * 16;
        let size = match *bytes.get(e)? {
            0 => 256,
            s => s as u32,
        };
        let (len, off) = (u32le(e + 8)? as usize, u32le(e + 12)? as usize);
        let png = bytes.get(off..off + 4) == Some(&b"\x89PNG"[..]);
        if png {
            continue;
        }
        let better = best.is_none_or(|(s, _, _)| (size as i64 - want as i64).abs() < (s as i64 - want as i64).abs());
        if better {
            best = Some((size, off, len));
        }
    }
    let (size, off, _) = best?;
    let header = u32le(off)? as usize;
    if u16le(off + 14)? != 32 {
        return None;
    }
    let s = size as usize;
    let mut rgba = vec![0u8; s * s * 4];
    for y in 0..s {
        for x in 0..s {
            let src = off + header + ((s - 1 - y) * s + x) * 4;
            let px = bytes.get(src..src + 4)?;
            let dst = (y * s + x) * 4;
            rgba[dst..dst + 4].copy_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
    }
    Some((rgba, size))
}

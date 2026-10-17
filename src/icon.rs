//! The icon a display entry shows in the app (its window strip's badge, the Apps drawer): a white
//! monitor on a dark rounded square, 64×64, drawn here as a PNG.

/// 64×64 RGBA PNG.
pub fn display_icon() -> Vec<u8> {
    const N: usize = 64;
    let mut px = vec![[0u8; 4]; N * N];
    let inside_rounded = |x: f64, y: f64, x0: f64, y0: f64, x1: f64, y1: f64, r: f64| {
        let cx = x.clamp(x0 + r, x1 - r);
        let cy = y.clamp(y0 + r, y1 - r);
        x >= x0 && x <= x1 && y >= y0 && y <= y1 && (x - cx).hypot(y - cy) <= r
    };
    for y in 0..N {
        for x in 0..N {
            let (fx, fy) = (x as f64 + 0.5, y as f64 + 0.5);
            if !inside_rounded(fx, fy, 0.0, 0.0, 64.0, 64.0, 14.0) {
                continue;
            }
            let screen_outer = inside_rounded(fx, fy, 12.0, 15.0, 52.0, 42.0, 3.0);
            let screen_inner = inside_rounded(fx, fy, 16.0, 19.0, 48.0, 38.0, 1.0);
            let stand = (29.0..35.0).contains(&fx) && (42.0..48.0).contains(&fy);
            let base = inside_rounded(fx, fy, 22.0, 47.0, 42.0, 51.0, 2.0);
            px[y * N + x] = if (screen_outer && !screen_inner) || stand || base { [255, 255, 255, 255] } else { [0x3A, 0x3D, 0x44, 255] };
        }
    }
    png(N as u32, N as u32, &px)
}

/// An RGBA PNG with stored (uncompressed) deflate blocks: a few kilobytes for an icon.
fn png(w: u32, h: u32, px: &[[u8; 4]]) -> Vec<u8> {
    let mut raw = Vec::with_capacity((w as usize * 4 + 1) * h as usize);
    for row in px.chunks(w as usize) {
        raw.push(0); // no filter
        raw.extend(row.iter().flatten());
    }
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65535).collect();
    for (i, b) in blocks.iter().enumerate() {
        z.push((i + 1 == blocks.len()) as u8);
        z.extend((b.len() as u16).to_le_bytes());
        z.extend((!(b.len() as u16)).to_le_bytes());
        z.extend_from_slice(b);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &c in &raw {
        a = (a + c as u32) % 65521;
        b = (b + a) % 65521;
    }
    z.extend(((b << 16) | a).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend(w.to_be_bytes());
    ihdr.extend(h.to_be_bytes());
    ihdr.extend([8, 6, 0, 0, 0]); // 8-bit RGBA
    for (kind, data) in [(b"IHDR", ihdr), (b"IDAT", z), (b"IEND", Vec::new())] {
        out.extend((data.len() as u32).to_be_bytes());
        let mut chunk = kind.to_vec();
        chunk.extend(&data);
        out.extend(&chunk);
        out.extend(crc32(&chunk).to_be_bytes());
    }
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_png() {
        assert_eq!(crc32(b"IEND"), 0xAE42_6082);
        let p = display_icon();
        assert!(p.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(&p[12..16], b"IHDR");
        assert_eq!(u32::from_be_bytes(p[16..20].try_into().unwrap()), 64);
        assert!(p.ends_with(&[0xAE, 0x42, 0x60, 0x82]));
    }
}

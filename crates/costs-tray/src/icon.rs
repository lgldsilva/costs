//! Ícone com o valor desenhado em RGBA (Windows e Linux só mostram ícone).
// macOS usa `set_title`; lá estas funções só rodam nos testes.
#![cfg_attr(target_os = "macos", allow(dead_code))]

/// Lado do ícone em pixels.
pub const SIZE: u32 = 32;
const GLYPH_W: u32 = 3;
const GLYPH_H: u32 = 5;
const GAP: u32 = 1;
const BG: [u8; 4] = [0x20, 0x20, 0x28, 0xff];
const FG: [u8; 4] = [0xff, 0xff, 0xff, 0xff];

/// Fonte 3x5 mínima: uma string de 15 bits (linha a linha) por caractere.
fn glyph(c: char) -> &'static str {
    match c {
        '0' => "111101101101111",
        '1' => "010110010010111",
        '2' => "111001111100111",
        '3' => "111001111001111",
        '4' => "101101111001001",
        '5' => "111100111001111",
        '6' => "111100111101111",
        '7' => "111001001001001",
        '8' => "111101111101111",
        '9' => "111101111001111",
        '.' => "000000000000010",
        'k' => "101101110101101",
        '-' => "000000111000000",
        _ => "111001011000010", // '?'
    }
}

fn put(buf: &mut [u8], x: u32, y: u32, rgba: [u8; 4]) {
    let i = ((y * SIZE + x) * 4) as usize;
    buf[i..i + 4].copy_from_slice(&rgba);
}

fn paint_glyph(buf: &mut [u8], x0: u32, y0: u32, ch: char, scale: u32) {
    let bits = glyph(ch);
    for (bit, on) in bits.bytes().enumerate() {
        if on != b'1' {
            continue;
        }
        let (col, row) = (bit as u32 % GLYPH_W, bit as u32 / GLYPH_W);
        for dy in 0..scale {
            for dx in 0..scale {
                let (x, y) = (x0 + col * scale + dx, y0 + row * scale + dy);
                if x < SIZE && y < SIZE {
                    put(buf, x, y, FG);
                }
            }
        }
    }
}

/// Renderiza `text` (centralizado, ampliado ao máximo que cabe) em RGBA `SIZE`x`SIZE`.
pub fn render(text: &str) -> Vec<u8> {
    let mut buf: Vec<u8> = BG.iter().copied().cycle().take((SIZE * SIZE * 4) as usize).collect();
    let n = text.chars().count() as u32;
    if n == 0 {
        return buf;
    }
    let units = n * (GLYPH_W + GAP) - GAP;
    let scale = (SIZE / units).clamp(1, SIZE / GLYPH_H);
    let x0 = SIZE.saturating_sub(units * scale) / 2;
    let y0 = SIZE.saturating_sub(GLYPH_H * scale) / 2;
    for (ci, ch) in text.chars().enumerate() {
        let gx = x0 + ci as u32 * (GLYPH_W + GAP) * scale;
        paint_glyph(&mut buf, gx, y0, ch, scale);
    }
    buf
}

/// RGBA -> ARGB32 (formato do StatusNotifierItem).
pub fn to_argb(rgba: &[u8]) -> Vec<u8> {
    rgba.as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| [p[3], p[0], p[1], p[2]])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_expected_dimensions() {
        let buf = render("123");
        assert_eq!(buf.len(), (SIZE * SIZE * 4) as usize);
    }

    #[test]
    fn draws_foreground_pixels() {
        let buf = render("12.3");
        assert!(buf.as_chunks::<4>().0.contains(&FG));
        assert!(buf.as_chunks::<4>().0.contains(&BG));
    }

    #[test]
    fn empty_text_is_background_only_and_unknown_glyph_draws() {
        assert!(render("").as_chunks::<4>().0.iter().all(|p| *p == BG));
        assert!(render("x").as_chunks::<4>().0.contains(&FG));
    }

    #[test]
    fn long_text_does_not_panic() {
        assert_eq!(render("12345678901234567890").len(), (SIZE * SIZE * 4) as usize);
    }

    #[test]
    fn argb_moves_alpha_first() {
        assert_eq!(to_argb(&[1, 2, 3, 4]), vec![4, 1, 2, 3]);
    }
}

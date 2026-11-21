use std::fmt::Write;

const LEVELS: usize = 4;
const PALETTE: usize = LEVELS * LEVELS * LEVELS;

pub fn encode(argb: &[u8], width: usize, height: usize, stride: usize) -> String {
    // Square pixels and a transparent background. Color indices are private to the image
    // on terminals with private registers and are redefined in every frame.
    let mut output = format!("\x1bP0;1;0q\"1;1;{width};{height}");
    let mut defined = [false; PALETTE];
    let mut columns = vec![[0_u8; PALETTE]; width];
    for top in (0..height).step_by(6) {
        if top != 0 {
            output.push('-');
        }
        columns.fill([0; PALETTE]);
        let mut ends = [0; PALETTE];
        for y in top..(top + 6).min(height) {
            let row = &argb[y * stride..y * stride + width * 4];
            for (x, pixel) in row.as_chunks::<4>().0.iter().enumerate() {
                if let Some(color) = color_index(*pixel) {
                    columns[x][color] |= 1 << (y - top);
                    ends[color] = x + 1;
                }
            }
        }
        for (color, end) in ends.into_iter().enumerate().filter(|&(_, end)| end != 0) {
            if defined[color] {
                let _ = write!(output, "#{color}");
            } else {
                let percent = |value| (value % LEVELS) * 100 / (LEVELS - 1);
                let (r, g, b) = (
                    percent(color / (LEVELS * LEVELS)),
                    percent(color / LEVELS),
                    percent(color),
                );
                let _ = write!(output, "#{color};2;{r};{g};{b}");
                defined[color] = true;
            }
            let mut remaining = &columns[..end];
            while let Some(first) = remaining.first() {
                let mask = first[color];
                let count = remaining
                    .iter()
                    .take_while(|column| column[color] == mask)
                    .count();
                let mark = char::from(b'?' + mask);
                if count >= 4 {
                    let _ = write!(output, "!{count}{mark}");
                } else {
                    output.extend(std::iter::repeat_n(mark, count));
                }
                remaining = &remaining[count..];
            }
            output.push('$');
        }
    }
    output.push_str("\x1b\\");
    output
}

fn color_index(bytes: [u8; 4]) -> Option<usize> {
    // ARGB32 is native-endian with premultiplied alpha. Sixel has binary transparency.
    let [alpha, r, g, b] = u32::from_ne_bytes(bytes).to_be_bytes().map(usize::from);
    if alpha < 128 {
        return None;
    }
    let channel = |value| (value * (LEVELS - 1) + alpha / 2) / alpha;
    Some((channel(r) * LEVELS + channel(g)) * LEVELS + channel(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_sparse_colored_pixels_in_six_row_bands() {
        let mut pixels = [0; 3 * 7 * 4];
        pixels[4..8].copy_from_slice(&0xffff0000_u32.to_ne_bytes());
        pixels[72..76].copy_from_slice(&0xff00ff00_u32.to_ne_bytes());
        assert_eq!(
            encode(&pixels, 3, 7, 12),
            "\x1bP0;1;0q\"1;1;3;7#48;2;100;0;0?@$-#12;2;0;100;0@$\x1b\\"
        );
    }
}

//! A wrapped lyrics document in terminal-cell coordinates.
use crate::lyrics::Lyrics;
use std::{ffi::c_char, ops::Range};

#[link(name = "unistring")]
unsafe extern "C" {
    fn u8_grapheme_next(start: *const u8, end: *const u8) -> *const u8;
    fn u8_width(text: *const u8, length: usize, encoding: *const c_char) -> i32;
}

pub struct Graphemes<'a> {
    text: &'a str,
    byte: usize,
}

pub fn graphemes(text: &str) -> Graphemes<'_> {
    Graphemes { text, byte: 0 }
}

impl<'a> Iterator for Graphemes<'a> {
    type Item = (usize, &'a str, usize);

    fn next(&mut self) -> Option<Self::Item> {
        let remaining = self.text.get(self.byte..).filter(|s| !s.is_empty())?;
        let start = remaining.as_ptr();
        // Both pointers bound the same UTF-8 slice. libunistring returns its next cluster boundary.
        let end = unsafe { u8_grapheme_next(start, start.add(remaining.len())) };
        let len = if end.is_null() {
            remaining.len()
        } else {
            (unsafe { end.offset_from(start) }) as usize
        };
        let cluster = &remaining[..len];
        let width = if cluster.chars().any(char::is_control) {
            if cluster == "\t" { 1 } else { 0 }
        } else {
            let raw = unsafe { u8_width(start, len, c"UTF-8".as_ptr()) };
            // Joined emoji, regional indicators and modifiers occupy one glyph on Kitty.
            if cluster.contains(['\u{fe0f}', '\u{20e3}']) {
                2
            } else {
                raw.clamp(0, 2) as usize
            }
        };
        let byte = self.byte;
        self.byte += len;
        Some((byte, cluster, width))
    }
}

#[derive(Clone, Debug)]
pub struct Block {
    pub top: usize,
    pub rows: Vec<Range<usize>>,
}

pub struct Layout {
    pub padding: usize,
    pub blocks: Vec<Block>,
}

fn wrap(text: &str, width: usize) -> Vec<Range<usize>> {
    let mut rows = Vec::new();
    let mut start = 0;
    let mut columns = 0;
    let mut break_at = None;
    let mut tail_columns = 0;
    for (byte, cluster, cells) in graphemes(text) {
        if cluster.contains('\n') {
            rows.push(start..byte);
            start = byte + cluster.len();
            columns = 0;
            break_at = None;
            tail_columns = 0;
            continue;
        }
        if cluster.chars().all(char::is_whitespace) {
            if columns == 0 {
                start = byte + cluster.len();
                continue;
            }
            if break_at.is_none() || tail_columns != 0 {
                break_at = Some((byte, byte + cluster.len()));
            } else if let Some((_, next)) = &mut break_at {
                *next = byte + cluster.len();
            }
            tail_columns = 0;
        } else if columns + cells > width && columns > 0 {
            if let Some((end, next)) = break_at.take().filter(|_| tail_columns + cells <= width) {
                rows.push(start..end);
                start = next;
                columns = tail_columns;
            } else {
                rows.push(start..byte);
                start = byte;
                columns = 0;
            }
            tail_columns = 0;
        } else if break_at.is_some() {
            tail_columns += cells;
        }
        columns += cells;
    }
    rows.push(start..text.len());
    rows
}

impl Layout {
    pub fn new(lyrics: &Lyrics, width: usize, padding: usize, spacing: usize) -> Self {
        let padding = padding.min(width.saturating_sub(2) / 2);
        let content = width
            .saturating_sub(padding + usize::from(width > 1))
            .max(1);
        let mut top = 0;
        let blocks = lyrics
            .lines
            .iter()
            .map(|line| {
                let rows = wrap(&line.text, content);
                let block = Block { top, rows };
                top += block.rows.len() + spacing;
                block
            })
            .collect();
        Self { padding, blocks }
    }

    pub fn focus(&self, index: usize, height: usize, ratio: f64) -> f64 {
        let block = &self.blocks[index];
        let center = ((block.rows.len().saturating_sub(1)) as f64 / 2.0).min(height as f64 * 0.2);
        block.top as f64 + center - (height.saturating_sub(1)) as f64 * ratio
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lyrics::lrc;

    #[test]
    fn clusters_wrap_without_splitting_and_resize_recalculates_heights() {
        let lyric = lrc("[00:01]Я e\u{301} 👩‍🎤 спів\n[00:02]Далі");
        let narrow = Layout::new(&lyric, 12, 4, 2);
        let wide = Layout::new(&lyric, 40, 4, 2);
        assert_eq!(narrow.padding, 4);
        assert_eq!(narrow.blocks[0].rows.len(), 2);
        assert_eq!(narrow.blocks[1].top, 4);
        assert_eq!(wide.blocks[1].top, 3);
        assert_eq!(
            &lyric.lines[0].text[narrow.blocks[0].rows[0].clone()],
            "Я e\u{301} 👩‍🎤"
        );
        assert_eq!(graphemes("👩‍🎤").count(), 1);
        assert_eq!(graphemes("👩‍🎤").next().unwrap().2, 2);
        assert_ne!(narrow.focus(1, 20, 0.45), wide.focus(1, 20, 0.45));
    }
}

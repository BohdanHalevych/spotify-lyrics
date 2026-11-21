use crate::{
    config::{Config, Easing},
    layout::{Layout, graphemes},
    lyrics::Lyrics,
    motion::{Color, Scroll, Tween, WordState, word_state},
    provider::Notice,
};
use std::{collections::BTreeMap, fmt::Write, time::Instant};

#[derive(Clone, Copy)]
pub enum Colors {
    True,
    Indexed,
    Plain,
}

impl Colors {
    pub fn detect(graphics: bool) -> Self {
        if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
            return Self::Plain;
        }
        let term = std::env::var("TERM").unwrap_or_default();
        let color = std::env::var("COLORTERM").unwrap_or_default();
        if graphics
            || term.contains("kitty")
            || term.contains("direct")
            || matches!(color.as_str(), "truecolor" | "24bit")
        {
            Self::True
        } else if term.contains("256color") {
            Self::Indexed
        } else {
            Self::Plain
        }
    }

    fn write(self, dest: &mut String, Color(r, g, b): Color, bold: bool, underline: bool) {
        dest.push_str("\x1b[0");
        if bold {
            dest.push_str(";1");
        }
        if underline {
            dest.push_str(";4");
        }
        match self {
            Self::True => {
                let _ = write!(dest, ";38;2;{r};{g};{b}");
            }
            Self::Indexed => {
                let cube = |v| (u16::from(v) * 5 + 127) / 255;
                let code = 16 + 36 * cube(r) + 6 * cube(g) + cube(b);
                let _ = write!(dest, ";38;5;{code}");
            }
            Self::Plain => {}
        }
        dest.push('m');
    }
}

const ACTIVE: Color = Color(247, 235, 255);
const VIOLET: Color = Color(207, 177, 255);
const PREVIOUS: Color = Color(161, 132, 199);
const UPCOMING: Color = Color(149, 133, 181);
const DISTANT: Color = Color(113, 100, 141);
const MINT: Color = Color(125, 246, 218);

pub struct View {
    lyrics: Option<Lyrics>,
    layout: Option<Layout>,
    size: (usize, usize),
    position: i64,
    current: Option<usize>,
    focus: Option<usize>,
    scroll: Scroll,
    highlights: BTreeMap<usize, Tween>,
    notice: Option<Notice>,
}

impl View {
    pub fn new(now: Instant) -> Self {
        Self {
            lyrics: None,
            layout: None,
            size: (0, 0),
            position: 0,
            current: None,
            focus: None,
            scroll: Scroll::new(0.0, now),
            highlights: BTreeMap::new(),
            notice: None,
        }
    }

    pub fn set_lyrics(&mut self, lyrics: Option<Lyrics>, now: Instant) {
        *self = Self::new(now);
        self.lyrics = lyrics;
    }

    pub fn set_notice(&mut self, notice: Option<Notice>) {
        self.notice = notice;
    }

    pub fn update(
        &mut self,
        cfg: &Config,
        size: (usize, usize),
        position: Option<i64>,
        now: Instant,
        jump: bool,
    ) {
        let (Some(lyrics), Some(position)) = (&self.lyrics, position) else {
            self.current = None;
            self.highlights.clear();
            self.focus = None;
            return;
        };
        self.position = position
            .saturating_add(cfg.offset)
            .saturating_add(lyrics.offset);
        let active = lyrics.index(self.position);
        let focus = active
            .or_else(|| {
                lyrics
                    .lines
                    .partition_point(|line| line.start <= self.position)
                    .checked_sub(1)
            })
            .or_else(|| (!lyrics.lines.is_empty()).then_some(0));
        let resized = self.size != size || self.layout.is_none();
        if self.size.0 != size.0 || self.layout.is_none() {
            self.layout = Some(Layout::new(lyrics, size.0, cfg.padding, cfg.spacing));
        }
        self.size = size;
        if let (Some(focus), Some(layout)) = (focus, &self.layout) {
            let target = layout.focus(focus, size.1, cfg.focus);
            if resized || self.focus.is_none() || !cfg.animated || cfg.scroll_ms == 0 {
                self.scroll = Scroll::new(target, now);
            } else {
                self.scroll.advance(now);
                if jump {
                    let delta = self.scroll.current_scroll_offset - target;
                    if delta.abs() > size.1 as f64 * 0.8 {
                        self.scroll = Scroll::new(target + delta.signum() * 2.0, now);
                    }
                    self.scroll.retarget(target, cfg.scroll_ms.min(240));
                } else if self.focus != Some(focus) {
                    self.scroll.retarget(target, cfg.scroll_ms);
                }
                self.scroll.present();
            }
        }
        if active != self.current {
            let duration = if !cfg.animated {
                0
            } else if jump {
                cfg.color_ms.min(160)
            } else {
                cfg.color_ms
            };
            if let Some(previous) = self
                .current
                .and_then(|index| self.highlights.get_mut(&index))
            {
                previous.retarget(0.0, now, duration, Easing::Out);
            }
            if let Some(index) = active {
                self.highlights
                    .entry(index)
                    .or_insert_with(|| Tween::fixed(0.0, now))
                    .retarget(1.0, now, duration, cfg.easing);
            }
            self.current = active;
        }
        self.highlights
            .retain(|&index, tween| Some(index) == active || tween.running(now));
        self.focus = focus;
    }

    pub fn needs_frames(&self, now: Instant, playing: bool) -> bool {
        (playing && self.focus.is_some())
            || self.scroll.running()
            || self.highlights.values().any(|tween| tween.running(now))
    }

    pub fn render(
        &self,
        size: (usize, usize),
        colors: Colors,
        cfg: &Config,
        now: Instant,
    ) -> Vec<String> {
        self.render_rows(size, colors, cfg, now, None)
    }

    pub fn render_pixels(
        &self,
        size: (usize, usize),
        colors: Colors,
        cfg: &Config,
        now: Instant,
        cell_height: usize,
    ) -> (Vec<String>, usize) {
        let scroll = self.scroll.current_scroll_offset;
        let pixels = (scroll * cell_height as f64).round() as isize;
        let origin = pixels.div_euclid(cell_height as isize);
        let shift = pixels.rem_euclid(cell_height as isize) as usize;
        (
            self.render_rows(size, colors, cfg, now, Some((origin, shift))),
            shift,
        )
    }

    fn render_rows(
        &self,
        size: (usize, usize),
        colors: Colors,
        cfg: &Config,
        now: Instant,
        pixels: Option<(isize, usize)>,
    ) -> Vec<String> {
        let mut rows = vec![String::new(); size.1];
        let (Some(lyrics), Some(layout), Some(focus)) = (&self.lyrics, &self.layout, self.focus)
        else {
            let notice = match self.notice {
                Some(Notice::Missing) => "Oops, we didn't find it",
                Some(Notice::Instrumental) => "♪",
                None => &cfg.fallback,
            };
            if size.0 > 1 && !notice.is_empty() && size.1 > 0 {
                let row = ((size.1 - 1) as f64 * cfg.focus).round() as usize;
                let padding = cfg.padding.min((size.0 - 2) / 2);
                let mut width = size.0 - padding - 1;
                rows[row].push_str(&" ".repeat(padding));
                for (_, piece, cells) in graphemes(notice) {
                    if cells > width {
                        break;
                    }
                    width -= cells;
                    rows[row].push_str(piece);
                }
            }
            return rows;
        };
        let scroll = self.scroll.current_scroll_offset;
        let (origin, scroll) = if let Some((origin, shift)) = pixels {
            if shift != 0 {
                rows.push(String::new());
            }
            (origin, scroll)
        } else {
            let origin = self.scroll.row();
            (
                origin,
                scroll.clamp(origin as f64 - 0.5, origin as f64 + 0.5),
            )
        };
        let focus_row = scroll + size.1.saturating_sub(1) as f64 * cfg.focus;
        let start = layout
            .blocks
            .partition_point(|block| (block.top + block.rows.len()) as isize <= origin);
        let padding = " ".repeat(layout.padding);
        for (index, block) in layout.blocks.iter().enumerate().skip(start) {
            if block.top as isize - origin >= rows.len() as isize {
                break;
            }
            if (!cfg.previous && index < focus)
                || (!cfg.upcoming && index > focus)
                || (self.current.is_none()
                    && index == focus
                    && self.position < lyrics.lines[index].start
                    && !cfg.upcoming)
            {
                continue;
            }
            let line = &lyrics.lines[index];
            let emphasis = self
                .highlights
                .get(&index)
                .map_or(0.0, |tween| tween.value(now));
            let middle = block.top as f64 + block.rows.len().saturating_sub(1) as f64 / 2.0;
            let distance = (middle - focus_row) / (cfg.spacing + 1) as f64;
            let base = PREVIOUS
                .mix(UPCOMING, distance + 0.5)
                .mix(DISTANT, (distance.abs() * 0.13).min(0.75));
            for (row_index, span) in block.rows.iter().enumerate() {
                let y = (block.top + row_index) as isize - origin;
                if y < 0 || y >= rows.len() as isize {
                    continue;
                }
                let text = if row_index == 0 && line.text.trim().is_empty() {
                    "♪"
                } else {
                    line.text[span.clone()].trim_end()
                };
                if text.is_empty() {
                    continue;
                }
                let out = &mut rows[y as usize];
                out.push_str(&padding);
                let mut last = None;
                let mut width = size
                    .0
                    .saturating_sub(layout.padding + usize::from(size.0 > 1));
                for (byte, glyph, cells) in graphemes(text) {
                    if cells == 0 {
                        continue;
                    }
                    if cells > width {
                        break;
                    }
                    width -= cells;
                    let byte = byte + span.start;
                    let progress = byte as f64 / line.text.len().max(1) as f64;
                    let bright = if cfg.gradient {
                        ACTIVE.mix(VIOLET, progress * 0.55)
                    } else {
                        ACTIVE
                    };
                    let mut color = base.mix(bright, emphasis);
                    let mut underline = false;
                    if cfg.words && self.current == Some(index) && !line.words.is_empty() {
                        let i = line.words.partition_point(|word| word.bytes.end <= byte);
                        if line
                            .words
                            .get(i)
                            .is_some_and(|word| word.bytes.contains(&byte))
                        {
                            match word_state(line, i, self.position, cfg.animated, cfg.pulse) {
                                WordState::Upcoming => color = color.mix(VIOLET, 0.4 * emphasis),
                                WordState::Complete => color = color.mix(MINT, 0.25 * emphasis),
                                WordState::Active(amount) => {
                                    underline = matches!(colors, Colors::Plain);
                                    color = color.mix(MINT, (0.12 + amount * 0.88) * emphasis);
                                }
                            }
                        }
                    }
                    let style = (color, emphasis > 0.5, underline);
                    if last != Some(style) {
                        colors.write(out, style.0, style.1, style.2);
                        last = Some(style);
                    }
                    if glyph == "\t" {
                        out.push(' ');
                    } else {
                        out.push_str(glyph);
                    }
                }
            }
        }
        if self.current.is_none()
            && self.position >= 0
            && layout.padding < size.0
            && let Some((_, row)) = rows
                .iter_mut()
                .take(size.1)
                .enumerate()
                .filter(|(_, row)| row.is_empty())
                .min_by_key(|(index, _)| {
                    index.abs_diff((size.1.saturating_sub(1) as f64 * cfg.focus).round() as usize)
                })
        {
            row.push_str(&padding);
            colors.write(row, UPCOMING, false, false);
            row.push('♪');
        }
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lyrics::lrc;

    #[test]
    fn musical_breaks_and_confirmed_misses_have_distinct_markers() {
        let now = Instant::now();
        let cfg = Config::default();
        let mut view = View::new(now);
        let size = (40, 16);
        view.set_lyrics(Some(lrc("[00:01]First\n[00:02]\n[00:05]Next")), now);
        view.update(&cfg, size, Some(3000), now, false);
        assert!(
            view.render(size, Colors::Plain, &cfg, now)
                .iter()
                .any(|row| row.contains('♪'))
        );

        view.set_lyrics(None, now);
        view.set_notice(Some(Notice::Missing));
        assert!(
            view.render(size, Colors::Plain, &cfg, now)
                .iter()
                .any(|row| row.contains("Oops, we didn't find it"))
        );
        view.set_lyrics(None, now);
        assert!(
            view.render(size, Colors::Plain, &cfg, now)
                .iter()
                .all(String::is_empty)
        );
    }

    #[test]
    fn wrapped_blocks_share_the_left_edge_and_default_background() {
        let now = Instant::now();
        let mut view = View::new(now);
        view.set_lyrics(
            Some(lrc(
                "[00:01]Перший\n[00:02]Дуже довгий рядок для переносу\n[00:03]Далі",
            )),
            now,
        );
        let cfg = Config {
            animated: false,
            ..Config::default()
        };
        view.update(&cfg, (24, 17), Some(2100), now, false);
        let painted: Vec<_> = view
            .render((24, 17), Colors::True, &cfg, now)
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect();
        assert!(painted.len() >= 4);
        assert!(
            painted
                .iter()
                .all(|s| s.starts_with("    \x1b") && !s.contains(";48;"))
        );
        assert!(painted.iter().any(|s| s.contains("Перший")));
        assert!(painted.iter().any(|s| s.contains("Далі")));
        view.update(&cfg, (60, 17), Some(2100), now, false);
        assert_eq!(view.layout.unwrap().blocks[1].rows.len(), 1);
    }

    #[test]
    fn pixel_view_moves_inside_a_row_before_the_text_view_moves() {
        let now = Instant::now();
        let mut view = View::new(now);
        let cfg = Config {
            gradient: false,
            ..Config::default()
        };
        let size = (36, 12);
        view.set_lyrics(Some(lrc("[00:01]First\n[00:02]Second")), now);
        view.update(&cfg, size, Some(1000), now, false);
        view.update(&cfg, size, Some(2000), now, false);
        let row = view.scroll.row();
        let (_, first_shift) = view.render_pixels(size, Colors::True, &cfg, now, 20);
        let next = now + std::time::Duration::from_millis(17);
        view.update(&cfg, size, Some(2017), next, false);
        let (rows, second_shift) = view.render_pixels(size, Colors::True, &cfg, next, 20);
        assert_eq!(view.scroll.row(), row);
        assert_eq!(second_shift, (first_shift + 4) % 20);
        assert!(rows.iter().any(|row| row.contains("First")));
    }

    #[test]
    fn seeks_retarget_one_document_and_track_reset_drops_old_rows() {
        let now = Instant::now();
        let mut view = View::new(now);
        let cfg = Config::default();
        view.set_lyrics(Some(lrc("[00:01]Old\n[00:02]Next\n[00:03]Last")), now);
        view.update(&cfg, (36, 12), Some(1000), now, false);
        view.update(&cfg, (36, 12), Some(2000), now, false);
        let before = view.scroll.current_scroll_offset;
        view.update(&cfg, (36, 12), Some(3000), now, true);
        assert_eq!(view.scroll.current_scroll_offset, before);
        view.set_lyrics(None, now);
        assert!(
            view.render((36, 12), Colors::Plain, &cfg, now)
                .iter()
                .all(String::is_empty)
        );
    }
}

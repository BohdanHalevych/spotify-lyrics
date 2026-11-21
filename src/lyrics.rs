use crate::data::Document;
use std::{collections::BTreeMap, ops::Range};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Word {
    pub bytes: Range<usize>,
    pub start: i64,
    pub end: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub text: String,
    pub start: i64,
    pub end: Option<i64>,
    pub words: Vec<Word>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Lyrics {
    pub lines: Vec<Line>,
    pub tags: BTreeMap<String, String>,
    pub offset: i64,
}

impl Lyrics {
    pub fn index(&self, position: i64) -> Option<usize> {
        let count = self.lines.partition_point(|line| line.start <= position);
        count
            .checked_sub(1)
            .filter(|&i| {
                // Overlapping vocals can remain active after a more recent line ends.
                self.lines[i].end.is_none_or(|end| position < end)
            })
            .or_else(|| {
                (0..count)
                    .rev()
                    .find(|&i| self.lines[i].end.is_some_and(|end| position < end))
            })
    }

    pub fn valid(&self, limit: i64) -> bool {
        self.lines.iter().any(|l| !l.text.trim().is_empty())
            && self.lines.iter().all(|l| {
                l.start <= limit
                    && l.end.is_none_or(|end| end <= limit)
                    && l.words
                        .iter()
                        .all(|w| w.start <= limit && w.end.is_none_or(|end| end <= limit))
            })
    }
}

fn stamp(text: &str) -> Option<i64> {
    let (minutes, seconds) = text.split_once(':')?;
    let (seconds, fraction) = seconds.split_once(['.', ':']).unwrap_or((seconds, ""));
    if minutes.is_empty()
        || minutes.len() > 6
        || seconds.len() != 2
        || fraction.len() > 3
        || ![minutes, seconds, fraction]
            .into_iter()
            .all(|s| s.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let seconds: i64 = seconds.parse().ok()?;
    if seconds >= 60 {
        return None;
    }
    let millis = fraction.parse::<i64>().unwrap_or(0) * 10_i64.pow(3 - fraction.len() as u32);
    Some(minutes.parse::<i64>().ok()? * 60_000 + seconds * 1000 + millis)
}

fn enhanced(raw: &str, start: i64) -> Line {
    let mut text = String::new();
    let mut markers = Vec::new();
    let mut tail = raw;
    while let Some(open) = tail.find('<') {
        text.push_str(&tail[..open]);
        tail = &tail[open + 1..];
        if let Some((value, rest)) = tail.split_once('>')
            && let Some(time) = stamp(value)
        {
            markers.push((text.len(), time));
            tail = rest;
            continue;
        }
        text.push('<');
    }
    text.push_str(tail);
    let mut words = Vec::new();
    let mut end = None;
    if markers
        .first()
        .is_some_and(|(i, _)| text[..*i].trim().is_empty())
        && markers.iter().all(|(_, t)| *t >= start)
        && markers.windows(2).all(|pair| pair[0].1 <= pair[1].1)
    {
        for (i, &(byte, time)) in markers.iter().enumerate() {
            let next = markers.get(i + 1).map_or(text.len(), |m| m.0);
            if !text[byte..next].trim().is_empty() {
                words.push(Word {
                    bytes: byte..next,
                    start: time,
                    end: None,
                });
            } else if i + 1 == markers.len()
                && let Some(last) = words.last_mut()
            {
                last.end = Some(time);
                end = Some(time);
            }
        }
    }
    Line {
        text,
        start,
        end,
        words,
    }
}

pub fn lrc(input: &str) -> Lyrics {
    let mut lyrics = Lyrics::default();
    for raw in input.trim_start_matches('\u{feff}').lines() {
        let mut body = raw.trim();
        let mut starts = Vec::new();
        while let Some((tag, rest)) = body.strip_prefix('[').and_then(|s| s.split_once(']')) {
            if let Some(time) = stamp(tag) {
                starts.push(time);
                body = rest.trim_start();
            } else {
                if starts.is_empty()
                    && let Some((key, value)) = tag.split_once(':')
                {
                    let key = key.to_ascii_lowercase();
                    let value = value.trim();
                    if key == "offset" {
                        if let Ok(offset) = value.parse::<i32>() {
                            lyrics.offset = i64::from(offset);
                        }
                    } else {
                        lyrics.tags.insert(key, value.to_owned());
                    }
                    body = rest.trim_start();
                    continue;
                }
                break;
            }
        }
        if let Some((&first, repeats)) = starts.split_first() {
            let line = enhanced(body, first);
            lyrics.lines.extend(repeats.iter().map(|&start| Line {
                text: line.text.clone(),
                start,
                end: None,
                words: Vec::new(),
            }));
            lyrics.lines.push(line);
        }
    }
    lyrics.lines.sort_by_key(|line| line.start);
    lyrics.lines.dedup_by(|later, earlier| {
        if later.start != earlier.start || later.text != earlier.text {
            return false;
        }
        if earlier.words.is_empty() && !later.words.is_empty() {
            std::mem::swap(later, earlier);
        }
        true
    });
    lyrics
}

pub fn lyricsfile(input: &str) -> Option<Lyrics> {
    let yaml = Document::parse(input, false)?;
    let root = yaml.root();
    if root.get("version").text()? != "1.0" {
        return None;
    }
    let number = |value: Option<&str>| {
        value?
            .parse::<i64>()
            .ok()
            .filter(|v| (0..=86_400_000).contains(v))
    };
    let mut lyrics = Lyrics::default();
    let meta = root.get("metadata");
    for (field, tag) in [("artist", "ar"), ("title", "ti"), ("album", "al")] {
        if let Some(value) = meta.get(field).text() {
            lyrics.tags.insert(tag.to_owned(), value.to_owned());
        }
    }
    for row in root.get("lines").items().take(5001) {
        let start = number(row.get("start_ms").text())?;
        let end = row
            .get("end_ms")
            .text()
            .map(|value| number(Some(value)).filter(|&v| v >= start).ok_or(()))
            .transpose()
            .ok()?;
        let text = row.get("text").text()?.to_owned();
        let mut words = Vec::new();
        let mut byte = 0;
        let mut previous = start;
        let mut valid = true;
        for item in row.get("words").items().take(1025) {
            let segment = item.get("text").text().unwrap_or_default();
            let time = number(item.get("start_ms").text());
            let raw_end = item.get("end_ms").text();
            let finish = number(raw_end);
            if let Some(time) = time.filter(|&t| t >= previous && end.is_none_or(|v| t <= v))
                && !segment.is_empty()
                && text[byte..].starts_with(segment)
                && (raw_end.is_none()
                    || finish.is_some_and(|v| v >= time && end.is_none_or(|e| v <= e)))
            {
                words.push(Word {
                    bytes: byte..byte + segment.len(),
                    start: time,
                    end: finish,
                });
                byte += segment.len();
                previous = time;
            } else {
                valid = false;
                break;
            }
        }
        if !valid || byte != text.len() || words.len() > 1024 {
            words.clear();
        }
        lyrics.lines.push(Line {
            text,
            start,
            end,
            words,
        });
    }
    if lyrics.lines.len() > 5000 {
        return None;
    }
    lyrics.lines.sort_by_key(|line| line.start);
    Some(lyrics)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enhanced_cyrillic_and_repeat() {
        let doc = lrc("[offset:+125]\n[00:01][00:07]<00:01>Привіт, <00:02>світе!<00:03>");
        assert_eq!(doc.offset, 125);
        let line = &doc.lines[0];
        assert_eq!(&line.text[line.words[0].bytes.clone()], "Привіт, ");
        assert_eq!(line.words[1].end, Some(3000));
        assert_eq!(doc.index(2999), Some(0));
        assert_eq!(doc.index(3000), None);
        assert!(doc.lines[1].words.is_empty());
        assert_eq!(doc.index(7000), Some(1));
    }

    #[test]
    fn draft_word_format_and_fallback() {
        let raw = "version: '1.0'\nlines:\n - text: 'Та так'\n   start_ms: 1\n   words:\n    - {text: 'Та ', start_ms: 1, end_ms: 3}\n    - {text: так, start_ms: 3}\n";
        assert_eq!(lyricsfile(raw).unwrap().lines[0].words.len(), 2);
        assert!(
            lyricsfile(&raw.replace("text: так", "text: ні"))
                .unwrap()
                .lines[0]
                .words
                .is_empty()
        );
    }
}

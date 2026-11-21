use crate::{
    config::save_private,
    data::{Document, Node},
    lyrics::{self, Lyrics},
    playback::Track,
    process,
};
use std::{
    fs::File,
    io::{self, Read},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone)]
pub struct Provider {
    pub local: PathBuf,
    pub cache: PathBuf,
    pub cache_enabled: bool,
    pub offline: bool,
    pub contact: Option<String>,
    pub throttle: Arc<Mutex<Option<(Instant, bool)>>>,
}

pub struct Load {
    pub lyrics: Option<Lyrics>,
    pub message: String,
    pub notice: Option<Notice>,
}

#[derive(Clone, Copy)]
pub enum Notice {
    Missing,
    Instrumental,
}

pub struct Request {
    cancelled: Arc<AtomicBool>,
    pub generation: u64,
    pub result: Receiver<Load>,
}

impl Drop for Request {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

fn read(path: &Path) -> io::Result<String> {
    let mut text = String::new();
    File::open(path)?
        .take(2_000_001)
        .read_to_string(&mut text)?;
    if text.len() > 2_000_000 {
        return Err(io::Error::other("lyrics file is too large"));
    }
    Ok(text)
}

fn normal(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn tags_match(lyrics: &Lyrics, track: &Track) -> bool {
    [
        ("ar", &track.artist),
        ("ti", &track.title),
        ("al", &track.album),
    ]
    .into_iter()
    .all(|(tag, expected)| {
        expected.is_empty()
            || lyrics
                .tags
                .get(tag)
                .is_none_or(|value| normal(value) == normal(expected))
    })
}

fn encoded(s: &str) -> String {
    let mut result = String::new();
    for byte in s.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            result.push(char::from(byte));
        } else {
            use std::fmt::Write;
            let _ = write!(result, "%{byte:02X}");
        }
    }
    result
}

fn filenames(track: &Track) -> Vec<String> {
    let mut names = Vec::new();
    let id = track
        .id
        .strip_prefix("spotify:track:")
        .or_else(|| track.id.strip_prefix("/com/spotify/track/"));
    if let Some(id) =
        id.filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric()))
    {
        names.push(format!("spotify-{id}.lrc"));
    }
    if !track.artist.is_empty() && !track.title.is_empty() {
        let stem = format!("{} - {}", track.artist, track.title)
            .replace('%', "%25")
            .replace('/', "%2F")
            .replace('\\', "%5C")
            .replace('\0', "%00");
        if stem.len() <= 250 {
            names.push(format!("{stem}.lrc"));
        }
    }
    names
}

enum Match {
    Timed(Lyrics),
    Instrumental,
}

fn match_record(node: Node<'_>, track: &Track) -> Option<Match> {
    let duration = track.duration?;
    let actual = node.get("duration").text()?.parse::<f64>().ok()? * 1000.0;
    if !actual.is_finite() || actual <= 0.0 || (actual - duration as f64).abs() > 2000.0 {
        return None;
    }
    for (field, expected) in [
        ("artistName", &track.artist),
        ("trackName", &track.title),
        ("albumName", &track.album),
    ] {
        if !expected.is_empty() && normal(node.get(field).text()?) != normal(expected) {
            return None;
        }
    }
    let candidates = [
        (
            "lyricsfile",
            lyrics::lyricsfile as fn(&str) -> Option<Lyrics>,
        ),
        ("syncedLyrics", |text| Some(lyrics::lrc(text))),
    ];
    for (field, parse) in candidates {
        let Some(lyrics) = node.get(field).text().and_then(parse) else {
            continue;
        };
        if lyrics.valid(duration + 2000) && tags_match(&lyrics, track) {
            return Some(Match::Timed(lyrics));
        }
    }
    (node.get("instrumental").text() == Some("true")).then_some(Match::Instrumental)
}

fn decode(body: &str, track: &Track) -> Result<Option<Match>, String> {
    let doc = Document::parse(body, true).ok_or("LRCLIB response is not valid JSON")?;
    if !body.trim_start().starts_with('[') {
        return Ok(match_record(doc.root(), track));
    }
    let mut chosen = None;
    let mut instrumental = false;
    for candidate in doc
        .root()
        .items()
        .take(30)
        .filter_map(|n| match_record(n, track))
    {
        match candidate {
            Match::Timed(lyrics) => {
                if chosen.as_ref().is_some_and(|prev: &Lyrics| {
                    prev.lines != lyrics.lines || prev.offset != lyrics.offset
                }) {
                    return Err("Several LRCLIB recordings match with conflicting lyrics".into());
                }
                chosen = Some(lyrics);
            }
            Match::Instrumental => instrumental = true,
        }
    }
    if instrumental && chosen.is_some() {
        return Err("LRCLIB candidates disagree on whether the track is instrumental".into());
    }
    Ok(chosen
        .map(Match::Timed)
        .or(instrumental.then_some(Match::Instrumental)))
}

fn describe(value: Match, source: &str) -> Load {
    match value {
        Match::Timed(lyrics) => Load {
            lyrics: Some(lyrics),
            message: format!("Synced lyrics: {source}"),
            notice: None,
        },
        Match::Instrumental => Load {
            lyrics: None,
            message: format!("Instrumental: {source}"),
            notice: Some(Notice::Instrumental),
        },
    }
}

fn cache_key(track: &Track) -> String {
    // Stable FNV-1a key. Every cache hit is independently checked against the track.
    let mut hash = 0xcbf29ce484222325_u64;
    for field in [
        &track.id,
        &track.artist,
        &track.title,
        &track.album,
        &track.duration.unwrap_or(0).to_string(),
    ] {
        for byte in field.bytes().chain([0xff]) {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
        }
    }
    format!("{hash:016x}.json")
}

fn legacy_cache_path(cache: &Path, track: &Track, stop: &AtomicBool) -> Option<PathBuf> {
    let legacy = cache.parent()?.join("lrclib");
    if !legacy.is_dir() {
        return None;
    }
    // Match the Python v2 cache signature, including Unicode and JSON separators.
    fn quote(text: &str) -> String {
        use std::fmt::Write;
        let mut result = String::from("\"");
        for ch in text.chars() {
            match ch {
                '"' => result.push_str("\\\""),
                '\\' => result.push_str("\\\\"),
                '\n' => result.push_str("\\n"),
                '\r' => result.push_str("\\r"),
                '\t' => result.push_str("\\t"),
                '\u{8}' => result.push_str("\\b"),
                '\u{c}' => result.push_str("\\f"),
                c if c < ' ' => {
                    let _ = write!(result, "\\u{:04x}", u32::from(c));
                }
                c => result.push(c),
            }
        }
        result.push('"');
        result
    }
    let duration = track
        .duration
        .map_or_else(|| "null".to_owned(), |ms| ms.to_string());
    let signature = format!(
        "{{\"album\": {}, \"artist\": {}, \"duration_ms\": {duration}, \"identity\": {}, \"title\": {}}}",
        quote(&track.album),
        quote(&track.artist),
        quote(&track.id),
        quote(&track.title),
    );
    let output = process::with_input(
        &mut Command::new("sha256sum"),
        stop,
        Duration::from_millis(500),
        128,
        signature.as_bytes(),
    )
    .ok()?;
    let hash = output.split_ascii_whitespace().next()?;
    (hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| legacy.join(format!("{hash}.json")))
}

fn retry_delay(header: &str) -> u64 {
    if let Ok(seconds) = header.parse::<u32>() {
        return u64::from(seconds).max(1);
    }
    // GNU date can decode the HTTP-date form of Retry-After.
    if !header.is_empty() {
        let mut command = Command::new("date");
        command.arg("--date").arg(header).arg("+%s");
        let stop = AtomicBool::new(false);
        if let Ok(raw) = process::output(&mut command, &stop, Duration::from_millis(300), 64)
            && let Ok(when) = raw.trim().parse::<u64>()
        {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            return when.saturating_sub(now).max(1);
        }
    }
    60
}

impl Provider {
    fn http(
        &self,
        endpoint: &str,
        query: &str,
        stop: &AtomicBool,
    ) -> Result<Option<String>, String> {
        let mut gate = loop {
            if stop.load(Ordering::Relaxed) {
                return Err("Lookup cancelled".into());
            }
            if let Ok(lock) = self.throttle.try_lock() {
                break lock;
            }
            thread::sleep(Duration::from_millis(20));
        };
        if let Some((until, limited)) = *gate {
            let remaining = until.saturating_duration_since(Instant::now());
            if limited && !remaining.is_zero() {
                return Err(format!(
                    "LRCLIB rate limited; retry in {} seconds",
                    remaining.as_secs() + 1
                ));
            }
            while until > Instant::now() {
                if stop.load(Ordering::Relaxed) {
                    return Err("Lookup cancelled".into());
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
        let contact = self
            .contact
            .as_ref()
            .ok_or("Run lyricsync --configure to enable LRCLIB")?;
        let agent = format!("LyricSync/{} ({contact})", env!("CARGO_PKG_VERSION"));
        let mut cmd = Command::new("curl");
        cmd.args([
            "--disable",
            "--silent",
            "--connect-timeout",
            "4",
            "--max-time",
            "7",
            "--max-filesize",
            "1999800",
            "--proto",
            "=https",
            "--write-out",
            "\n%{http_code}\n%header{retry-after}",
            "--config",
        ])
        .arg("-")
        .arg(format!("https://lrclib.net/api/{endpoint}?{query}"));
        let input = format!("user-agent = \"{agent}\"\n");
        // Send the contact on stdin so it is absent from the process arguments.
        let output = process::with_input(
            &mut cmd,
            stop,
            Duration::from_secs(8),
            2_000_000,
            input.as_bytes(),
        );
        *gate = Some((Instant::now() + Duration::from_millis(350), false));
        let output = output.map_err(|e| format!("LRCLIB request failed: {e}"))?;
        let (prefix, retry) = output.rsplit_once('\n').ok_or("Incomplete LRCLIB reply")?;
        let (body, code) = prefix.rsplit_once('\n').ok_or("Incomplete LRCLIB reply")?;
        match code {
            "200" => Ok(Some(body.to_owned())),
            "404" => Ok(None),
            "429" | "503" if code == "429" || !retry.is_empty() => {
                let delay = retry_delay(retry.trim());
                let now = Instant::now();
                let (until, delay) = now
                    .checked_add(Duration::from_secs(delay))
                    .map_or((now + Duration::from_secs(60), 60), |until| (until, delay));
                *gate = Some((until, true));
                Err(format!("LRCLIB rate limited; retry in {delay} seconds"))
            }
            _ => Err(format!("LRCLIB returned HTTP {code}")),
        }
    }

    fn online(&self, track: &Track, stop: &AtomicBool) -> Result<Option<(Match, String)>, String> {
        let duration = track.duration.filter(|d| (1000..=3_600_000).contains(d));
        if track.artist.is_empty() || duration.is_none() || track.id.starts_with("spotify:episode:")
        {
            return Err("Track metadata is incomplete for LRCLIB".into());
        }
        let mut query = format!(
            "track_name={}&artist_name={}",
            encoded(&track.title),
            encoded(&track.artist)
        );
        if !track.album.is_empty() {
            query.push_str(&format!("&album_name={}", encoded(&track.album)));
        }
        let signature = format!("{query}&duration={}", (duration.unwrap_or(0) + 500) / 1000);
        for (endpoint, params) in [("get", signature.as_str()), ("search", query.as_str())] {
            if let Some(body) = self.http(endpoint, params, stop)?
                && let Some(found) = decode(&body, track)?
            {
                return Ok(Some((found, body)));
            }
        }
        Ok(None)
    }

    pub fn load(&self, track: &Track, refresh: bool, stop: &AtomicBool) -> Load {
        let empty = |message: String| Load {
            lyrics: None,
            message,
            notice: None,
        };
        let missing = |message: &str| Load {
            notice: Some(Notice::Missing),
            ..empty(message.into())
        };
        for name in filenames(track) {
            match read(&self.local.join(name)) {
                Ok(source) => {
                    let lyrics = lyrics::lrc(&source);
                    if tags_match(&lyrics, track) && lyrics.valid(i64::MAX) {
                        return describe(Match::Timed(lyrics), "local LRC");
                    }
                    return empty(
                        "Local LRC has no valid timed lines or its metadata conflicts with Spotify"
                            .into(),
                    );
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return empty(format!("Cannot read local LRC: {e}")),
            }
        }
        let path = self.cache.join(cache_key(track));
        let cached = if self.cache_enabled {
            read(&path)
                .ok()
                .and_then(|raw| decode(&raw, track).ok().flatten())
                .or_else(|| {
                    let legacy = legacy_cache_path(&self.cache, track, stop)?;
                    let raw = read(&legacy).ok()?;
                    let matched = decode(&raw, track).ok().flatten()?;
                    if !stop.load(Ordering::Relaxed) {
                        let _ = save_private(&path, raw.as_bytes());
                    }
                    Some(matched)
                })
        } else {
            None
        };
        if (!refresh || self.offline || self.contact.is_none())
            && let Some(value) = cached
        {
            return describe(value, "LRCLIB cache");
        }
        if self.offline {
            return missing("No local or cached lyrics");
        }
        if self.contact.is_none() {
            return empty("Run lyricsync --configure to enable LRCLIB".into());
        }
        match self.online(track, stop) {
            Ok(Some((value, body))) => {
                let mut result = describe(value, "LRCLIB");
                if self.cache_enabled
                    && !stop.load(Ordering::Relaxed)
                    && let Err(e) = save_private(&path, body.as_bytes())
                {
                    result
                        .message
                        .push_str(&format!(" (cache write failed: {e})"));
                }
                result
            }
            Ok(None) => missing("No matching synchronized lyrics on LRCLIB"),
            Err(error) => {
                if let Some(value) = cached {
                    let mut result = describe(value, "LRCLIB cache");
                    result.message.push_str(&format!(" ({error})"));
                    result
                } else {
                    empty(error)
                }
            }
        }
    }

    pub fn spawn<'scope>(
        &self,
        scope: &'scope thread::Scope<'scope, '_>,
        track: Track,
        generation: u64,
        refresh: bool,
    ) -> Request {
        let (send, result) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&cancelled);
        let provider = self.clone();
        scope.spawn(move || {
            let lyrics = provider.load(&track, refresh, &stop);
            if !stop.load(Ordering::Relaxed) {
                let _ = send.send(lyrics);
            }
        });
        Request {
            cancelled,
            generation,
            result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_wrong_album_duration_and_ambiguous_candidates() {
        let track = Track {
            artist: "Автор".into(),
            title: "Назва".into(),
            album: "Альбом".into(),
            id: "a".into(),
            duration: Some(10000),
        };
        let good = r#"{"artistName":"Автор","trackName":"Назва","albumName":"Альбом","duration":10,"syncedLyrics":"[00:01]Вітаю"}"#;
        assert!(matches!(decode(good, &track), Ok(Some(Match::Timed(_)))));
        assert!(
            decode(&good.replace("Альбом", "Concert"), &track)
                .unwrap()
                .is_none()
        );
        assert!(
            decode(&good.replace(":10", ":13"), &track)
                .unwrap()
                .is_none()
        );
        assert!(
            decode(
                &format!("[{good},{}]", good.replace("Вітаю", "Луна")),
                &track
            )
            .is_err()
        );
    }
}

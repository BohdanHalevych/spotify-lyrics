use crate::process;
use std::{
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct Track {
    pub artist: String,
    pub title: String,
    pub album: String,
    pub id: String,
    pub duration: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Playing,
    Paused,
    Stopped,
}

#[derive(Clone, Debug)]
pub struct Sample {
    pub track: Track,
    pub status: Status,
    pub position: i64,
    pub at: Instant,
}

pub struct Update {
    pub sample: Option<Sample>,
    pub warning: Option<String>,
}

const FORMAT: &str = "{{status}}\x1f{{position}}\x1f{{mpris:length}}\x1f{{mpris:trackid}}\x1f{{xesam:url}}\x1f{{artist}}\x1f{{title}}\x1f{{album}}";

fn command() -> Command {
    let mut cmd = Command::new("playerctl");
    cmd.arg("--player=spotify").env("LC_ALL", "C.UTF-8");
    cmd
}

const MISSING_PLAYERCTL: &str =
    "playerctl was not found in PATH. Install the playerctl package and run lyricsync again.";

pub fn check_playerctl() -> Result<(), String> {
    process::output(
        Command::new("playerctl").arg("--version"),
        &AtomicBool::new(false),
        Duration::from_secs(2),
        4096,
    )
    .map(|_| ())
    .map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            MISSING_PLAYERCTL.into()
        } else {
            format!("Cannot run playerctl --version: {error}")
        }
    })
}

fn parse(raw: &str, at: Instant) -> Option<Sample> {
    let [status, pos, duration, id, url, artist, title, album]: [&str; 8] = raw
        .trim_end_matches('\n')
        .split('\x1f')
        .collect::<Vec<_>>()
        .try_into()
        .ok()?;
    if title.is_empty() {
        return None;
    }
    let ms = |s: &str| s.parse::<i64>().ok().map(|n| (n / 1000).max(0));
    let duration = ms(duration).filter(|n| *n > 0);
    Some(Sample {
        track: Track {
            artist: artist.into(),
            title: title.into(),
            album: album.into(),
            id: if url.starts_with("spotify:") { url } else { id }.into(),
            duration,
        },
        status: match status {
            "Playing" => Status::Playing,
            "Paused" => Status::Paused,
            "Stopped" => Status::Stopped,
            _ => return None,
        },
        position: ms(pos)?.min(duration.unwrap_or(i64::MAX)),
        at,
    })
}

pub fn sample(stop: &AtomicBool) -> Result<Option<Sample>, String> {
    process::output(
        command().args(["metadata", "--format", FORMAT]),
        stop,
        Duration::from_secs(2),
        64_000,
    )
    .map(|raw| parse(&raw, Instant::now()))
    .map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            MISSING_PLAYERCTL.into()
        } else {
            format!("Spotify MPRIS unavailable ({error}). Run playerctl --player=spotify status.")
        }
    })
}

pub fn spawn(interval: u64, stop: Arc<AtomicBool>) -> (Receiver<Update>, thread::JoinHandle<()>) {
    let (updates, receive) = mpsc::channel();
    let worker = thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            let (sample, warning) = match sample(&stop) {
                Ok(value) => (value, None),
                Err(error) => (None, Some(error)),
            };
            let delay = match sample.as_ref().map(|sample| sample.status) {
                Some(Status::Playing) => interval,
                Some(_) => interval.max(500),
                None => interval.max(1000),
            };
            if updates.send(Update { sample, warning }).is_err() {
                break;
            }
            if !stop.load(Ordering::Relaxed) {
                thread::park_timeout(Duration::from_millis(delay));
            }
        }
    });
    (receive, worker)
}

#[derive(Default)]
pub struct Clock {
    pub sample: Option<Sample>,
    pub generation: u64,
    pub jump: bool,
    floor: f64,
}

impl Clock {
    pub fn update(&mut self, next: Option<Sample>) -> bool {
        let changed = self.sample.as_ref().map(|s| &s.track) != next.as_ref().map(|s| &s.track);
        let jumped = !changed
            && self
                .sample
                .as_ref()
                .zip(next.as_ref())
                .is_some_and(|(a, b)| {
                    let elapsed = if a.status == Status::Playing {
                        b.at.saturating_duration_since(a.at).as_millis() as i64
                    } else {
                        0
                    };
                    let delta = b.position - a.position;
                    delta < -85 || (delta - elapsed).abs() > 500
                });
        self.jump = !changed && (self.jump || jumped);
        if changed {
            self.generation = self.generation.wrapping_add(1);
        }
        if changed
            || jumped
            || self.sample.as_ref().map(|s| s.status) != next.as_ref().map(|s| s.status)
        {
            self.floor = next.as_ref().map_or(0.0, |s| s.position as f64);
        }
        self.sample = next;
        changed
    }

    pub fn position(&mut self, now: Instant, lead: u64) -> Option<i64> {
        let sample = self.sample.as_ref()?;
        match sample.status {
            Status::Stopped => None,
            Status::Paused => Some(sample.position),
            Status::Playing => {
                let base = sample.position as f64;
                let estimate =
                    base + now.saturating_duration_since(sample.at).as_secs_f64() * 1000.0;
                self.floor = estimate
                    .min(base + lead as f64)
                    .max(self.floor)
                    .min(sample.track.duration.unwrap_or(i64::MAX) as f64);
                Some(self.floor as i64)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_is_bounded_and_pause_seek_track_change_reset() {
        let now = Instant::now();
        let raw = [
            "Playing",
            "1000000",
            "10000000",
            "id",
            "spotify:track:abc",
            "Артист",
            "Назва",
            "Альбом",
        ]
        .join("\x1f");
        let mut s = parse(&raw, now).unwrap();
        let mut clock = Clock::default();
        assert!(clock.update(Some(s.clone())));
        assert_eq!(
            clock.position(now + Duration::from_secs(1), 200),
            Some(1200)
        );
        s.position -= 40;
        s.at += Duration::from_millis(40);
        assert!(!clock.update(Some(s.clone())));
        assert!(!clock.jump);
        assert_eq!(clock.position(s.at, 200), Some(1200));
        s.position = 1000;
        s.status = Status::Paused;
        clock.update(Some(s.clone()));
        assert_eq!(
            clock.position(now + Duration::from_secs(1), 200),
            Some(1000)
        );
        s.position = 9000;
        assert!(!clock.update(Some(s.clone())));
        assert!(clock.jump);
        s.track.id.push('z');
        assert!(clock.update(Some(s)));
        assert!(clock.update(None));
    }
}

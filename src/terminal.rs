use crate::graphics::{Protocol, Size, cell_size};
use std::{
    ffi::c_int,
    io::{self, IsTerminal, Read, Write},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

static SIGNAL: AtomicBool = AtomicBool::new(false);

#[repr(C)]
struct Window {
    rows: u16,
    cols: u16,
    x: u16,
    y: u16,
}

#[repr(C)]
struct Poll {
    fd: c_int,
    events: i16,
    revents: i16,
}

unsafe extern "C" {
    fn ioctl(fd: c_int, request: std::ffi::c_ulong, ...) -> c_int;
    fn poll(fds: *mut Poll, count: std::ffi::c_ulong, timeout: c_int) -> c_int;
    fn signal(number: c_int, handler: extern "C" fn(c_int)) -> usize;
}

extern "C" fn terminate(_: c_int) {
    SIGNAL.store(true, Ordering::Relaxed);
}

pub fn interrupted() -> bool {
    SIGNAL.load(Ordering::Relaxed)
}

pub fn dimensions() -> ((usize, usize), (usize, usize)) {
    let mut window = Window {
        rows: 0,
        cols: 0,
        x: 0,
        y: 0,
    };
    // TIOCGWINSZ reports the grid and its pixel dimensions on Linux.
    if unsafe { ioctl(1, 0x5413, &mut window) } == 0 {
        return (
            (
                usize::from(window.cols.max(1)),
                usize::from(window.rows.max(1)),
            ),
            (usize::from(window.x), usize::from(window.y)),
        );
    }
    ((80, 24), (0, 0))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Quit,
    Plus,
    Minus,
    Refresh,
}

const GRAPHICS_QUERY: &[u8] = b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\";
const PROBE_TIMEOUT: Duration = Duration::from_millis(200);

#[derive(Default)]
pub struct Input {
    bytes: Vec<u8>,
    queued: Vec<Key>,
    graphics_reply: Option<bool>,
    cell_reply: Option<(usize, usize)>,
    attributes_reply: Option<bool>,
    eof: bool,
}

impl Input {
    pub fn probe_graphics(
        &mut self,
        initial_cell: Option<Size>,
    ) -> io::Result<Option<(Protocol, Size)>> {
        self.graphics_reply = None;
        self.cell_reply = None;
        self.attributes_reply = None;
        {
            let mut out = io::stdout().lock();
            out.write_all(GRAPHICS_QUERY)?;
            if initial_cell.is_none() {
                out.write_all(b"\x1b[16t\x1b[14t")?;
            }
            out.write_all(b"\x1b[c")?;
            out.flush()?;
        }
        let deadline = Instant::now() + PROBE_TIMEOUT;
        while !self.eof && Instant::now() < deadline {
            self.read(deadline.saturating_duration_since(Instant::now()))?;
            let keys = self.decode();
            self.queued.extend(keys);
            let cell = initial_cell.or(self.cell_reply);
            if self.graphics_reply == Some(true) {
                if let Some(cell) = cell {
                    return Ok(Some((Protocol::Kitty, cell)));
                }
            } else if let Some(sixel) = self.attributes_reply {
                if !sixel {
                    break;
                }
                if let Some(cell) = cell {
                    return Ok(Some((Protocol::Sixel, cell)));
                }
            }
        }
        Ok(None)
    }

    pub fn wait(&mut self, delay: Duration) -> io::Result<Vec<Key>> {
        if !self.queued.is_empty() {
            return Ok(std::mem::take(&mut self.queued));
        }
        self.read(delay)?;
        if self.eof {
            return Ok(vec![Key::Quit]);
        }
        Ok(self.decode())
    }

    fn read(&mut self, delay: Duration) -> io::Result<()> {
        let mut fd = Poll {
            fd: 0,
            events: 1,
            revents: 0,
        };
        let timeout = delay.as_nanos().div_ceil(1_000_000).min(c_int::MAX as u128) as c_int;
        let ready = unsafe { poll(&mut fd, 1, timeout) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        if ready > 0 {
            let mut buffer = [0_u8; 128];
            let count = io::stdin().read(&mut buffer)?;
            if count == 0 {
                self.eof = true;
            }
            self.bytes.extend_from_slice(&buffer[..count]);
        }
        Ok(())
    }

    fn decode(&mut self) -> Vec<Key> {
        let mut result = Vec::new();
        let mut n = 0;
        while n < self.bytes.len() {
            let rest = &self.bytes[n..];
            if rest[0] == 27 {
                if rest.len() < 2 {
                    break;
                }
                if rest[1] == b'_' {
                    let Some(end) = rest[2..].windows(2).position(|bytes| bytes == b"\x1b\\")
                    else {
                        if rest.len() > 1024 {
                            n += rest.len();
                        }
                        break;
                    };
                    let reply = &rest[2..end + 2];
                    if let Some((header, result)) = std::str::from_utf8(reply)
                        .ok()
                        .and_then(|reply| reply.strip_prefix('G')?.split_once(';'))
                        && header.split(',').any(|part| part == "i=31")
                    {
                        self.graphics_reply = Some(result == "OK");
                    }
                    n += end + 4;
                    continue;
                }
                if rest[1] == b'[' || rest[1] == b'O' {
                    let Some(end) = rest
                        .iter()
                        .enumerate()
                        .skip(2)
                        .find(|(_, b)| (0x40..=0x7e).contains(*b))
                    else {
                        if rest.len() > 32 {
                            n += rest.len();
                        }
                        break;
                    };
                    match end.1 {
                        b'c' if rest[1..].starts_with(b"[?") => {
                            let mut codes = rest[3..end.0].split(|byte| *byte == b';');
                            codes.next(); // The first value is the terminal class.
                            self.attributes_reply = Some(codes.any(|code| code == b"4"));
                        }
                        b't' if rest[1] == b'['
                            && (self.cell_reply.is_none() || rest[2..].starts_with(b"6;")) =>
                        {
                            self.cell_reply = cell_reply(&rest[2..end.0]).or(self.cell_reply);
                        }
                        _ => {}
                    }
                    n += end.0 + 1;
                    continue;
                }
                n += 1;
                continue;
            }
            match rest[0] {
                b'q' | b'Q' | 3 => result.push(Key::Quit),
                b'+' | b'=' => result.push(Key::Plus),
                b'-' => result.push(Key::Minus),
                b'r' | b'R' => result.push(Key::Refresh),
                _ => {}
            }
            n += 1;
        }
        self.bytes.drain(..n);
        result
    }
}

fn cell_reply(reply: &[u8]) -> Option<(usize, usize)> {
    let mut parts = std::str::from_utf8(reply).ok()?.split(';');
    let kind = parts.next()?;
    let height = parts.next()?.parse::<u16>().ok()? as usize;
    let width = parts.next()?.parse::<u16>().ok()? as usize;
    if parts.next().is_some() || width == 0 || height == 0 {
        return None;
    }
    match kind {
        "6" => Some((width, height)),
        "4" => cell_size(dimensions().0, (width, height)),
        _ => None,
    }
}

pub struct Terminal {
    saved: String,
    last: Vec<String>,
    synchronized: bool,
}

impl Terminal {
    pub fn enter(alacritty: bool) -> io::Result<Self> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(io::Error::other(
                "A terminal is required; use lyricsync --diagnose for playback status",
            ));
        }
        let output = Command::new("stty")
            .arg("-g")
            .stdin(Stdio::inherit())
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other("Cannot read terminal settings"));
        }
        let saved = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        for number in [1, 2, 15] {
            // The handler only sets an atomic flag; the main loop performs terminal cleanup.
            if unsafe { signal(number, terminate) } == usize::MAX {
                return Err(io::Error::last_os_error());
            }
        }
        let terminal = Self {
            saved,
            last: Vec::new(),
            synchronized: alacritty
                || std::env::var("TERM").is_ok_and(|term| {
                    term.contains("kitty")
                        || term.starts_with("alacritty")
                        || term.starts_with("foot")
                })
                || std::env::var_os("ALACRITTY_WINDOW_ID").is_some(),
        };
        if !Command::new("stty")
            .args(["raw", "-echo"])
            .status()?
            .success()
        {
            return Err(io::Error::other("Cannot set raw terminal mode"));
        }
        let mut out = io::stdout().lock();
        out.write_all(b"\x1b[?1049h\x1b[?25l\x1b[0m\x1b[2J")?;
        out.flush()?;
        Ok(terminal)
    }

    pub fn paint(&mut self, rows: Vec<String>) -> io::Result<()> {
        let mut frame = String::new();
        use std::fmt::Write as _;
        for (index, row) in rows.iter().enumerate() {
            if self.last.get(index) == Some(row) {
                continue;
            }
            let _ = write!(frame, "\x1b[{};1H{row}\x1b[0m\x1b[K", index + 1);
        }
        if !frame.is_empty() {
            if self.synchronized {
                frame.insert_str(0, "\x1b[?2026h");
                frame.push_str("\x1b[?2026l");
            }
            let mut out = io::stdout().lock();
            out.write_all(frame.as_bytes())?;
            out.flush()?;
        }
        self.last = rows;
        Ok(())
    }

    pub fn resized(&mut self) {
        self.last.clear();
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let mut out = io::stdout().lock();
        if self.synchronized {
            let _ = out.write_all(b"\x1b[?2026l");
        }
        let _ = out.write_all(b"\x1b[0m\x1b[?1049l\x1b[?25h");
        let _ = out.flush();
        let _ = Command::new("stty").arg(&self.saved).status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_replies_do_not_trigger_controls_or_drop_typed_keys() {
        let mut input = Input {
            bytes: b" +\x1b_Gi=31;".to_vec(),
            ..Input::default()
        };
        assert_eq!(input.decode(), [Key::Plus]);
        assert_eq!(input.graphics_reply, None);
        input.bytes.extend(b"OK\x1b\\\x1b[6;20;10t\x1b[?62;c\x1b[D");
        assert!(input.decode().is_empty());
        assert_eq!(input.graphics_reply, Some(true));
        assert_eq!(input.cell_reply, Some((10, 20)));
        assert_eq!(input.attributes_reply, Some(false));
        input.bytes.extend(b"\x1b_Gi=31;query rejected\x1b\\");
        assert!(input.decode().is_empty());
        assert_eq!(input.graphics_reply, Some(false));
    }

    #[test]
    fn ignores_split_arrow_and_space_and_decodes_viewer_keys() {
        let mut input = Input {
            bytes: b"\x1b[".to_vec(),
            ..Input::default()
        };
        assert!(input.decode().is_empty());
        input.bytes.extend(b"D+ rQ");
        assert_eq!(input.decode(), [Key::Plus, Key::Refresh, Key::Quit]);
    }
}

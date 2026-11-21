use std::{
    env, fs,
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Easing {
    InOut,
    Out,
    Spring,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub padding: usize,
    pub spacing: usize,
    pub focus: f64,
    pub previous: bool,
    pub upcoming: bool,
    pub gradient: bool,
    pub pulse: bool,
    pub animated: bool,
    pub fps: u64,
    pub scroll_ms: u64,
    pub color_ms: u64,
    pub easing: Easing,
    pub offset: i64,
    pub words: bool,
    pub poll_ms: u64,
    pub cache: bool,
    pub offline: bool,
    pub fallback: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            padding: 4,
            spacing: 2,
            focus: 0.45,
            previous: true,
            upcoming: true,
            gradient: true,
            pulse: true,
            animated: true,
            fps: 60,
            scroll_ms: 650,
            color_ms: 350,
            easing: Easing::InOut,
            offset: 0,
            words: true,
            poll_ms: 200,
            cache: true,
            offline: false,
            fallback: String::new(),
        }
    }
}

pub struct Paths {
    pub config: PathBuf,
    pub data: PathBuf,
    pub cache: PathBuf,
}

fn xdg(var: &str, default: &str) -> io::Result<PathBuf> {
    if let Some(value) = env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        return Ok(value.join("lyricsync"));
    }
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute());
    home.map(|p| p.join(default).join("lyricsync"))
        .ok_or_else(|| io::Error::other(format!("Set {var} or HOME")))
}

impl Paths {
    pub fn discover() -> io::Result<Self> {
        Ok(Self {
            config: xdg("XDG_CONFIG_HOME", ".config")?,
            data: xdg("XDG_DATA_HOME", ".local/share")?,
            cache: xdg("XDG_CACHE_HOME", ".cache")?,
        })
    }
}

fn quoted(raw: &str) -> Option<&str> {
    raw.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| raw.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .filter(|s| !s.contains(['\n', '\r', '\\']))
}

fn without_comment(line: &str) -> &str {
    let mut quote = None;
    for (index, ch) in line.char_indices() {
        match (quote, ch) {
            (None, '#') => return &line[..index],
            (None, '\'' | '"') => quote = Some(ch),
            (Some(mark), ch) if mark == ch => quote = None,
            _ => {}
        }
    }
    line
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, String> {
        match fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(format!("{}: {error}", path.display())),
        }
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let mut result = Self::default();
        let mut section = "";
        let mut seen = std::collections::HashSet::new();
        for (index, raw) in text.lines().enumerate() {
            let line = without_comment(raw).trim();
            let bad = || format!("invalid configuration at line {}", index + 1);
            if line.is_empty() {
                continue;
            }
            if let Some(table) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                section = table.trim();
                if !matches!(
                    section,
                    "appearance" | "animation" | "sync" | "spotify" | "lyrics"
                ) {
                    return Err(bad());
                }
                continue;
            }
            let (key, value) = line.split_once('=').ok_or_else(bad)?;
            let (key, value) = (key.trim(), value.trim());
            if !seen.insert((section, key)) {
                return Err(bad());
            }
            let n = || value.parse::<u64>().map_err(|_| bad());
            let b = || value.parse::<bool>().map_err(|_| bad());
            let s = || quoted(value).ok_or_else(bad);
            match (section, key) {
                ("appearance", "left_padding") => result.padding = n()?.min(1000) as usize,
                ("appearance", "line_spacing") => result.spacing = n()?.min(1000) as usize,
                ("appearance", "focus_position") => {
                    result.focus = value.parse().map_err(|_| bad())?
                }
                ("appearance", "show_previous") => result.previous = b()?,
                ("appearance", "show_upcoming") => result.upcoming = b()?,
                ("appearance", "gradient") => result.gradient = b()?,
                ("appearance", "fallback") => result.fallback = s()?.to_owned(),
                ("appearance", "theme") if s()? == "lavender" => {}
                ("animation", "enabled") => result.animated = b()?,
                ("animation", "target_fps") => result.fps = n()?,
                ("animation", "scroll_duration_ms") => result.scroll_ms = n()?,
                ("animation", "color_transition_ms") => result.color_ms = n()?,
                ("animation", "pulse") => result.pulse = b()?,
                ("animation", "easing") => {
                    result.easing = match s()? {
                        "ease_in_out_cubic" => Easing::InOut,
                        "ease_out_cubic" => Easing::Out,
                        "spring" => Easing::Spring,
                        _ => return Err(bad()),
                    }
                }
                ("sync", "offset_ms") => result.offset = value.parse().map_err(|_| bad())?,
                ("sync", "word_highlighting") => result.words = b()?,
                ("spotify", "poll_interval_ms") => result.poll_ms = n()?,
                ("spotify", "player") if s()? == "spotify" => {}
                ("lyrics", "provider") if s()? == "lrclib" => {}
                ("lyrics", "cache_enabled") => result.cache = b()?,
                ("lyrics", "offline") => result.offline = b()?,
                _ => {
                    return Err(format!(
                        "unknown or unsupported [{section}].{key} at line {}",
                        index + 1
                    ));
                }
            }
        }
        if result.padding > 32
            || result.spacing > 8
            || !(0.15..=0.85).contains(&result.focus)
            || !(15..=60).contains(&result.fps)
            || result.scroll_ms > 2000
            || result.color_ms > 2000
            || !(100..=2000).contains(&result.poll_ms)
            || result.offset.unsigned_abs() > 60_000
            || result.fallback.chars().count() > 100
            || result.fallback.chars().any(char::is_control)
        {
            return Err("configuration value outside the supported range; see README".into());
        }
        Ok(result)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TerminalChoice {
    #[default]
    Auto,
    Kitty,
    Ghostty,
    WezTerm,
    Contour,
    Rio,
    Text,
    Alacritty,
    Konsole,
    Foot,
    Xterm,
}

impl TerminalChoice {
    pub const OPTIONS: [(Self, &'static str); 11] = [
        (Self::Auto, "auto"),
        (Self::Kitty, "kitty"),
        (Self::Ghostty, "ghostty"),
        (Self::WezTerm, "wezterm"),
        (Self::Contour, "contour"),
        (Self::Rio, "rio"),
        (Self::Text, "text"),
        (Self::Alacritty, "alacritty"),
        (Self::Konsole, "konsole"),
        (Self::Foot, "foot"),
        (Self::Xterm, "xterm"),
    ];

    pub fn parse(value: &str) -> Option<Self> {
        if value.eq_ignore_ascii_case("other") {
            return Some(Self::Text);
        }
        Self::OPTIONS
            .iter()
            .enumerate()
            .find(|(index, (_, name))| {
                value.eq_ignore_ascii_case(name) || value.parse::<usize>() == Ok(index + 1)
            })
            .map(|(_, (choice, _))| *choice)
    }

    pub fn name(self) -> &'static str {
        Self::OPTIONS
            .iter()
            .find(|(choice, _)| *choice == self)
            .unwrap()
            .1
    }
}

pub struct Setup {
    pub terminal: TerminalChoice,
    pub offline: bool,
}

impl Setup {
    pub fn load(paths: &Paths) -> Result<Option<Self>, String> {
        let path = paths.config.join("setup");
        match fs::read_to_string(path) {
            Ok(contents) => {
                let mut lines = contents.lines();
                let terminal = lines.next().and_then(TerminalChoice::parse);
                let mode = lines.next();
                match (terminal, mode) {
                    (Some(terminal), Some("online" | "offline")) => Ok(Some(Self {
                        terminal,
                        offline: mode == Some("offline"),
                    })),
                    _ => {
                        Err("Invalid setup. Run `lyricsync --configure` to set it up again.".into())
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("Cannot read LyricSync setup: {error}")),
        }
    }

    pub fn save(&self, paths: &Paths) -> io::Result<()> {
        let mode = if self.offline { "offline" } else { "online" };
        let value = format!("{}\n{mode}\n", self.terminal.name());
        save_private(&paths.config.join("setup"), value.as_bytes())
    }
}

pub fn valid_contact(value: &str) -> bool {
    if value.is_empty()
        || value.len() > 254
        || !value.bytes().all(|b| b.is_ascii_graphic())
        || value.contains(['(', ')', '\\', '"', '<', '>'])
    {
        return false;
    }
    if let Some(host) = value.strip_prefix("https://") {
        return host
            .split('/')
            .next()
            .is_some_and(|h| h.contains('.') && !h.starts_with('.'));
    }
    value.split_once('@').is_some_and(|(user, domain)| {
        !user.is_empty()
            && !domain.contains('@')
            && domain.contains('.')
            && domain.split('.').all(|label| {
                !label.is_empty()
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            })
    })
}

pub fn save_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("Missing parent directory"))?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let tmp = parent.join(format!(".tmp-{}-{nonce}", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    let result = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| fs::rename(&tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tables_and_validates_ranges() {
        let cfg = Config::parse("[appearance]\nleft_padding = 3 # comment\nfallback = '# silence'\n[animation]\neasing = 'spring'\n").unwrap();
        assert_eq!(cfg.padding, 3);
        assert_eq!(cfg.fallback, "# silence");
        assert_eq!(cfg.easing, Easing::Spring);
        assert!(Config::parse("[animation]\ntarget_fps = 61").is_err());
        assert!(Config::parse("[spotify]\nplayer = 'brave'").is_err());
    }
}

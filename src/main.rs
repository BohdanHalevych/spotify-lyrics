mod config;
mod data;
mod graphics;
mod layout;
mod lyrics;
mod motion;
mod playback;
mod process;
mod provider;
mod sixel;
mod terminal;
mod view;

use config::{Config, Paths, Setup, TerminalChoice, save_private, valid_contact};
use playback::Clock;
use provider::{Provider, Request};
use std::{
    env, fs,
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use terminal::{Input, Key, Terminal};
use view::{Colors, View};

const HELP: &str = "LyricSync — Spotify-synchronized terminal lyrics

Usage: lyricsync [OPTIONS]

  --configure             Set up LRCLIB and choose your terminal
  --auth                  Update your saved LRCLIB contact email
  --diagnose              Print Spotify and lyric-source status outside the TUI
  --offline               Use local LRC and cache only
  --config PATH           Load a specific config.toml
  --lyrics-dir PATH       Override the local LRC directory
  --log-file PATH         Append playback/provider diagnostics to a private file
  --reduced-motion        Disable animated scrolling and color transitions
  --graphics              Try pixel graphics for this run
  --text                  Use the text renderer for this run
  --preview FILE          Run the same viewer with a local LRC and a demo clock
  -h, --help              Show help
  -V, --version           Print the version

Keys: Q or Ctrl+C quit · +/− adjust lyric offset ±100 ms · R refresh lyrics";

#[derive(Default)]
struct Arguments {
    help: bool,
    version: bool,
    auth: bool,
    configure: bool,
    diagnose: bool,
    offline: bool,
    reduced: bool,
    graphics: bool,
    text: bool,
    config: Option<PathBuf>,
    local: Option<PathBuf>,
    log: Option<PathBuf>,
    preview: Option<PathBuf>,
    contact: Option<String>,
}

impl Arguments {
    fn parse(mut args: impl Iterator<Item = std::ffi::OsString>) -> Result<Self, String> {
        let mut options = Self::default();
        while let Some(raw) = args.next() {
            let arg = raw.to_str().ok_or("Option name must be UTF-8")?;
            let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
            match arg {
                "--help" | "-h" => options.help = true,
                "--version" | "-V" => options.version = true,
                "--auth" => options.auth = true,
                "--configure" => options.configure = true,
                "--diagnose" => options.diagnose = true,
                "--offline" => options.offline = true,
                "--reduced-motion" => options.reduced = true,
                "--graphics" | "--kitty-graphics" => options.graphics = true,
                "--text" => options.text = true,
                "--config" => options.config = Some(value()?.into()),
                "--lyrics-dir" => options.local = Some(value()?.into()),
                "--log-file" => options.log = Some(value()?.into()),
                "--preview" => options.preview = Some(value()?.into()),
                "--lrclib-contact" => {
                    options.contact = Some(
                        value()?
                            .into_string()
                            .map_err(|_| "Invalid contact encoding")?,
                    )
                }
                _ => return Err(format!("Unknown option {arg}. See lyricsync --help.")),
            }
        }
        if options.graphics && options.text {
            return Err("Choose either --graphics or --text.".into());
        }
        Ok(options)
    }
}

fn prompt(label: &str) -> Result<String, String> {
    print!("{label}");
    io::stdout().flush().map_err(|e| e.to_string())?;
    let mut answer = String::new();
    if io::stdin()
        .read_line(&mut answer)
        .map_err(|e| e.to_string())?
        == 0
    {
        return Err("Setup cancelled.".into());
    }
    Ok(answer.trim().to_owned())
}

fn saved_contact(paths: &Paths) -> Option<String> {
    fs::read_to_string(paths.config.join("lrclib-contact"))
        .ok()
        .map(|value| value.trim().to_owned())
}

fn save_contact(paths: &Paths, email: &str) -> Result<(), String> {
    save_private(&paths.config.join("lrclib-contact"), email.as_bytes())
        .map_err(|e| format!("Cannot save LRCLIB contact: {e}"))
}

fn auth(paths: &Paths) -> Result<(), String> {
    let email = prompt("LRCLIB contact email: ")?;
    validate_email(&email)?;
    save_contact(paths, &email)?;
    println!("LRCLIB contact saved.");
    Ok(())
}

fn validate_email(email: &str) -> Result<(), String> {
    if !email.contains('@') || email.starts_with("https://") || !valid_contact(email) {
        return Err("Enter a valid email address. No contact was changed.".into());
    }
    Ok(())
}

fn configure(paths: &Paths) -> Result<(), String> {
    let previous = Setup::load(paths).ok().flatten();
    let terminal = previous
        .as_ref()
        .map_or(TerminalChoice::Auto, |setup| setup.terminal);
    println!("Terminal for LyricSync");
    println!("  1) Auto-detect  2) Kitty  3) Ghostty  4) WezTerm");
    println!("  5) Contour      6) Rio    7) Other (text)   8) Alacritty");
    println!("  9) Konsole     10) foot  11) xterm");
    let answer = prompt(&format!("Choose a terminal [{}]: ", terminal.name()))?;
    let terminal = if answer.is_empty() {
        terminal
    } else {
        TerminalChoice::parse(&answer).ok_or("Choose a terminal from 1 to 11.")?
    };
    let existing = saved_contact(paths).is_some_and(|email| validate_email(&email).is_ok());
    let kept_mode = if previous.is_some_and(|setup| setup.offline) {
        "offline"
    } else if existing {
        "saved contact"
    } else {
        ""
    };
    let label = if kept_mode.is_empty() {
        "LRCLIB contact email, or type offline: ".into()
    } else {
        format!("LRCLIB contact email, or type offline [{kept_mode}]: ")
    };
    let email = prompt(&label)?;
    let offline = if email.is_empty() && !kept_mode.is_empty() {
        kept_mode == "offline"
    } else if email.eq_ignore_ascii_case("offline") {
        true
    } else {
        validate_email(&email)?;
        save_contact(paths, &email)?;
        false
    };
    Setup { terminal, offline }
        .save(paths)
        .map_err(|e| format!("Cannot save LyricSync setup: {e}"))?;
    println!("Setup saved. Run lyricsync to start.");
    Ok(())
}

fn logger(path: Option<PathBuf>) -> io::Result<Option<fs::File>> {
    use std::os::unix::fs::OpenOptionsExt;
    path.map(|p| {
        fs::OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(p)
    })
    .transpose()
}

fn log(file: &mut Option<fs::File>, message: &str) {
    if let Some(file) = file {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let _ = writeln!(file, "{now} {}", message.escape_debug());
    }
}

struct Playback {
    updates: mpsc::Receiver<playback::Update>,
    worker: Option<std::thread::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl Playback {
    fn new(interval: u64) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let (updates, worker) = playback::spawn(interval, Arc::clone(&stop));
        Self {
            updates,
            worker: Some(worker),
            stop,
        }
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}

fn active_view(
    mut cfg: Config,
    provider: Provider,
    preview: Option<lyrics::Lyrics>,
    mut log_file: Option<fs::File>,
    try_graphics: bool,
    preference: TerminalChoice,
) -> io::Result<()> {
    std::thread::scope(|scope| {
        let playback = preview.is_none().then(|| Playback::new(cfg.poll_ms));
        let mut terminal = Terminal::enter(preference == TerminalChoice::Alacritty)?;
        let mut input = Input::default();
        let (grid, pixels) = terminal::dimensions();
        let initial_cell = graphics::cell_size(grid, pixels);
        let graphics_cell = if try_graphics {
            input.probe_graphics(initial_cell)?
        } else {
            None
        };
        let mut graphics =
            graphics_cell.map(|(protocol, cell)| (graphics::Graphics::new(protocol), cell));
        let colors = Colors::detect(graphics.is_some());
        log(
            &mut log_file,
            match graphics_cell.map(|(protocol, _)| protocol) {
                Some(graphics::Protocol::Kitty) => "Renderer: Kitty graphics protocol",
                Some(graphics::Protocol::Sixel) => "Renderer: Sixel graphics",
                None => "Renderer: terminal text",
            },
        );
        let start = Instant::now();
        let mut view = View::new(start);
        let demo = preview.is_some();
        view.set_lyrics(preview, start);
        let mut clock = Clock::default();
        let mut request: Option<Request> = None;
        let mut dimensions = (grid, pixels);
        let mut warning = None;
        let frame = Duration::from_secs_f64(1.0 / cfg.fps as f64);
        let mut deadline = start;
        let mut fast = false;

        while !terminal::interrupted() {
            let now = Instant::now();
            let mut dirty = false;
            if let Some(worker) = &playback {
                for update in worker.updates.try_iter() {
                    if update.warning != warning {
                        if let Some(warning) = &update.warning {
                            log(&mut log_file, warning);
                        }
                        warning = update.warning;
                    }
                    let previous_status = clock.sample.as_ref().map(|s| s.status);
                    if clock.update(update.sample) {
                        view.set_lyrics(None, now);
                        request = clock.sample.as_ref().map(|sample| {
                            provider.spawn(scope, sample.track.clone(), clock.generation, false)
                        });
                        dirty = true;
                    }
                    dirty |=
                        clock.jump || previous_status != clock.sample.as_ref().map(|s| s.status);
                }
            }
            if let Some(pending) = &request
                && let Ok(loaded) = pending.result.try_recv()
            {
                if pending.generation == clock.generation {
                    log(&mut log_file, &loaded.message);
                    view.set_lyrics(loaded.lyrics, now);
                    view.set_notice(loaded.notice);
                    dirty = true;
                }
                request = None;
            }
            let (size, pixels) = terminal::dimensions();
            if dimensions != (size, pixels) {
                terminal.resized();
                if let Some((_, cell)) = &mut graphics {
                    if let Some(reported) = graphics::cell_size(size, pixels) {
                        *cell = reported;
                    } else if let Some((_, reported)) = input.probe_graphics(None)? {
                        *cell = reported;
                    }
                }
                dimensions = (size, pixels);
                dirty = true;
            }
            if now >= deadline || dirty {
                let (position, playing) = if demo {
                    (Some(now.duration_since(start).as_millis() as i64), true)
                } else {
                    (
                        clock.position(now, cfg.poll_ms.min(250)),
                        clock
                            .sample
                            .as_ref()
                            .is_some_and(|s| s.status == playback::Status::Playing),
                    )
                };
                view.update(&cfg, size, position, now, std::mem::take(&mut clock.jump));
                if let Some((graphics, cell)) = &mut graphics {
                    let (rows, shift) = view.render_pixels(size, colors, &cfg, now, cell.1);
                    graphics.paint(size, *cell, rows, shift)?;
                } else {
                    terminal.paint(view.render(size, colors, &cfg, now))?;
                }
                let animate = view.needs_frames(now, playing);
                let finished = Instant::now();
                deadline = if animate {
                    motion::next_frame(if fast { deadline } else { now }, finished, frame)
                } else {
                    finished + Duration::from_millis(120)
                };
                fast = animate;
            }
            for key in input.wait(deadline.saturating_duration_since(Instant::now()))? {
                match key {
                    Key::Quit => return Ok(()),
                    Key::Plus | Key::Minus => {
                        cfg.offset = (cfg.offset + if key == Key::Plus { 100 } else { -100 })
                            .clamp(-60_000, 60_000);
                        clock.jump = true;
                        deadline = Instant::now();
                    }
                    Key::Refresh => {
                        if let Some(sample) = &clock.sample {
                            request = Some(provider.spawn(
                                scope,
                                sample.track.clone(),
                                clock.generation,
                                true,
                            ));
                        }
                    }
                }
            }
        }
        Ok(())
    })
}

fn run() -> Result<(), String> {
    let args = Arguments::parse(env::args_os().skip(1))?;
    if args.help {
        println!("{HELP}");
        return Ok(());
    }
    if args.version {
        println!("lyricsync {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let paths = Paths::discover().map_err(|e| e.to_string())?;
    if args.configure {
        return configure(&paths);
    }
    if args.auth {
        return auth(&paths);
    }

    let config_path = args
        .config
        .as_ref()
        .cloned()
        .unwrap_or_else(|| paths.config.join("config.toml"));
    if args.config.is_some() && !config_path.is_file() {
        return Err(format!(
            "Config file does not exist: {}",
            config_path.display()
        ));
    }
    let mut cfg = Config::load(&config_path)?;
    let setup = Setup::load(&paths)?;
    if setup.is_none() && !args.diagnose && args.preview.is_none() && !args.offline && !cfg.offline
    {
        return Err("Run `lyricsync --configure` first to choose your terminal and set up LRCLIB. Use `lyricsync --offline` to start with local and cached lyrics.".into());
    }
    cfg.animated &= !args.reduced;
    let contact = args
        .contact
        .or_else(|| {
            env::var("LYRICSYNC_LRCLIB_CONTACT")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .or_else(|| saved_contact(&paths));
    let offline = cfg.offline || args.offline || setup.as_ref().is_some_and(|setup| setup.offline);
    if args.preview.is_none() && !offline && contact.as_ref().is_some_and(|s| !valid_contact(s)) {
        return Err(
            "Invalid LRCLIB contact. Run lyricsync --auth to update the saved email.".into(),
        );
    }
    let provider = Provider {
        local: args.local.unwrap_or_else(|| paths.data.join("lyrics")),
        cache: paths.cache.join("v3"),
        cache_enabled: cfg.cache,
        offline,
        contact,
        throttle: Arc::new(Mutex::new(None)),
    };
    if args.diagnose {
        println!(
            "LRCLIB: {}",
            if provider.offline {
                "offline"
            } else if provider.contact.is_some() {
                "contact configured"
            } else {
                "run lyricsync --configure to enable online lookup"
            }
        );
        let stop = AtomicBool::new(false);
        let Some(sample) = playback::sample(&stop)? else {
            return Err("Spotify has no usable track metadata or position".into());
        };
        println!(
            "Spotify: {:?}, {} ms — {} / {}",
            sample.status,
            sample.position,
            sample.track.artist.escape_debug(),
            sample.track.title.escape_debug()
        );
        println!("{}", provider.load(&sample.track, false, &stop).message);
        return Ok(());
    }
    if args.preview.is_none() && !provider.offline && provider.contact.is_none() {
        return Err(
            "LRCLIB contact is missing. Run `lyricsync --configure` or use `lyricsync --offline` for local and cached lyrics."
                .into(),
        );
    }
    let preview = args
        .preview
        .map(|path| {
            let raw = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let lyrics = lyrics::lrc(&raw);
            if !lyrics.valid(i64::MAX) {
                return Err("Preview needs a timed LRC file".to_owned());
            }
            Ok(lyrics)
        })
        .transpose()?;
    if preview.is_none() {
        playback::check_playerctl()?;
    }
    let log = logger(args.log).map_err(|e| format!("Cannot open log file: {e}"))?;
    let preference = setup.map_or(TerminalChoice::Auto, |setup| setup.terminal);
    let try_graphics = !args.text
        && (args.graphics
            || !matches!(preference, TerminalChoice::Text | TerminalChoice::Alacritty));
    active_view(cfg, provider, preview, log, try_graphics, preference).map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("lyricsync: {error}");
            ExitCode::FAILURE
        }
    }
}

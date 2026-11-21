use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("lyricsync-test-{}-{id}", std::process::id()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lyricsync"));
        let path = std::env::var_os("PATH").unwrap_or_default();
        let joined = std::env::join_paths(
            std::iter::once(self.0.clone()).chain(std::env::split_paths(&path)),
        )
        .unwrap();
        command
            .current_dir(&self.0)
            .env("PATH", joined)
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("XDG_DATA_HOME", self.0.join("data"))
            .env("XDG_CACHE_HOME", self.0.join("cache"))
            .env_remove("LYRICSYNC_LRCLIB_CONTACT");
        command
    }

    fn script(&self, name: &str, body: &str) {
        let path = self.0.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn saved(fixture: &Fixture) -> PathBuf {
    fixture.0.join("config/lyricsync/lrclib-contact")
}

fn auth(fixture: &Fixture, email: &str) -> std::process::Output {
    answer(fixture, "--auth", email)
}

fn answer(fixture: &Fixture, option: &str, input: &str) -> std::process::Output {
    let mut child = fixture
        .command()
        .arg(option)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(child.stdin.take().unwrap(), "{input}").unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn first_online_start_requires_configure_before_opening_the_terminal() {
    let fixture = Fixture::new();
    fixture.script("playerctl", "test \"$1\" = --version");
    let output = fixture.command().output().unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("lyricsync --configure"));
    assert!(error.contains("lyricsync --offline"));

    let offline = fixture.command().arg("--offline").output().unwrap();
    assert!(
        String::from_utf8(offline.stderr)
            .unwrap()
            .contains("A terminal is required")
    );

    assert!(
        answer(&fixture, "--configure", "1\ntester@example.org")
            .status
            .success()
    );
    let configured = fixture.command().output().unwrap();
    assert!(
        String::from_utf8(configured.stderr)
            .unwrap()
            .contains("A terminal is required")
    );
}

#[test]
fn configure_saves_terminal_and_contact_and_accepts_offline() {
    let fixture = Fixture::new();
    fixture.script("playerctl", "test \"$1\" = --version");
    let setup = fixture.0.join("config/lyricsync/setup");
    let email = "tester@example.org";
    assert!(auth(&fixture, email).status.success());
    let configured = answer(&fixture, "--configure", "Ghostty\n");
    assert!(configured.status.success());
    assert_eq!(fs::read_to_string(&setup).unwrap(), "ghostty\nonline\n");
    assert_eq!(fs::read_to_string(saved(&fixture)).unwrap(), email);

    assert!(
        !answer(&fixture, "--configure", "Rio\nbad-contact")
            .status
            .success()
    );
    assert_eq!(fs::read_to_string(&setup).unwrap(), "ghostty\nonline\n");
    assert_eq!(fs::read_to_string(saved(&fixture)).unwrap(), email);

    assert!(
        answer(&fixture, "--configure", "Alacritty\noffline")
            .status
            .success()
    );
    assert_eq!(fs::read_to_string(&setup).unwrap(), "alacritty\noffline\n");
    assert_eq!(
        fs::metadata(&setup).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let output = fixture.command().output().unwrap();
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("A terminal is required")
    );
}

#[test]
fn missing_playerctl_is_reported_before_opening_the_terminal() {
    let fixture = Fixture::new();
    assert!(
        answer(&fixture, "--configure", "1\noffline")
            .status
            .success()
    );

    for args in [&[][..], &["--offline"][..], &["--diagnose"][..]] {
        let result = fixture
            .command()
            .env("PATH", &fixture.0)
            .args(args)
            .output()
            .unwrap();
        assert!(!result.status.success());
        let error = String::from_utf8(result.stderr).unwrap();
        assert!(error.contains("playerctl was not found in PATH"));
        assert!(error.contains("Install the playerctl package"));
    }

    let preview = fixture.0.join("demo.lrc");
    fs::write(&preview, "[00:01]First line\n").unwrap();
    let result = fixture
        .command()
        .env("PATH", &fixture.0)
        .arg("--preview")
        .arg(preview)
        .output()
        .unwrap();
    let error = String::from_utf8(result.stderr).unwrap();
    assert!(error.contains("A terminal is required"));
}

#[test]
fn auth_is_private_persistent_and_invalid_input_preserves_previous_contact() {
    let fixture = Fixture::new();
    let contact = "tester@example.org";
    let output = auth(&fixture, contact);
    assert!(output.status.success());
    assert!(!String::from_utf8(output.stdout).unwrap().contains(contact));
    assert_eq!(fs::read_to_string(saved(&fixture)).unwrap(), contact);
    assert_eq!(
        fs::metadata(saved(&fixture)).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(!auth(&fixture, "invalid").status.success());
    assert_eq!(fs::read_to_string(saved(&fixture)).unwrap(), contact);
}

#[test]
fn cli_help_and_version_start_from_another_directory() {
    let fixture = Fixture::new();
    let version = fixture.command().arg("--version").output().unwrap();
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8(version.stdout).unwrap().trim(),
        "lyricsync 3.0.0"
    );
    for option in ["-h", "--help"] {
        let help = fixture.command().arg(option).output().unwrap();
        assert!(help.status.success());
        let help = String::from_utf8(help.stdout).unwrap();
        assert!(help.contains("--auth"));
        assert!(help.contains("--configure"));
        assert!(help.contains("--text"));
    }
}

#[test]
fn saved_contact_reaches_provider_and_cache_works_after_restart() {
    let fixture = Fixture::new();
    assert!(auth(&fixture, "tester@example.org").status.success());
    fixture.script("playerctl", r#"
[ "$1" = "--player=spotify" ] || exit 10
printf 'Playing\0371000000\03710000000\037/track/demo\037spotify:track:demo\037Автор\037Назва\037Альбом\n'
"#);
    fixture.script("curl", r#"
read -r header
case "$header" in *tester@example.org*) ;; *) exit 6 ;; esac
printf '%s\n200\n' '{"artistName":"Автор","trackName":"Назва","albumName":"Альбом","duration":10,"syncedLyrics":"[00:01]Вітаю"}'
"#);
    let online = fixture.command().arg("--diagnose").output().unwrap();
    assert!(online.status.success());
    let text = String::from_utf8(online.stdout).unwrap();
    assert!(text.contains("contact configured"));
    assert!(text.contains("Synced lyrics: LRCLIB"));
    assert_eq!(
        fs::read_dir(fixture.0.join("cache/lyricsync/v3"))
            .unwrap()
            .count(),
        1
    );
    fs::remove_file(fixture.0.join(Path::new("curl"))).unwrap();
    let cached = fixture
        .command()
        .args(["--diagnose", "--offline"])
        .output()
        .unwrap();
    assert!(cached.status.success());
    assert!(
        String::from_utf8(cached.stdout)
            .unwrap()
            .contains("Synced lyrics: LRCLIB cache")
    );
}

#[test]
fn migrates_matching_v2_cache_without_online_access() {
    let fixture = Fixture::new();
    fixture.script("playerctl", r#"
[ "$1" = "--player=spotify" ] || exit 10
printf 'Paused\0371000000\03710000000\037/track/demo\037spotify:track:demo\037Автор\037Назва\037Альбом\n'
"#);
    let legacy = fixture.0.join("cache/lyricsync/lrclib");
    fs::create_dir_all(&legacy).unwrap();
    fs::write(
        legacy.join("ab497b291752bb935f935b2880667be8463d32671f3afdb05aacb7585a4174a3.json"),
        r#"{"artistName":"Автор","trackName":"Назва","albumName":"Альбом","duration":10,"syncedLyrics":"[00:01]Вітаю"}"#,
    ).unwrap();
    let cached = fixture
        .command()
        .args(["--diagnose", "--offline"])
        .output()
        .unwrap();
    assert!(cached.status.success());
    assert!(
        String::from_utf8(cached.stdout)
            .unwrap()
            .contains("Synced lyrics: LRCLIB cache")
    );
    assert_eq!(
        fs::read_dir(fixture.0.join("cache/lyricsync/v3"))
            .unwrap()
            .count(),
        1
    );
}

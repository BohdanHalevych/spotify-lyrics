use std::{
    io::{self, Read, Write},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

/// Drain stdout off the render thread, kill on cancellation/deadline, and reap the child.
pub fn output(
    command: &mut Command,
    stop: &AtomicBool,
    timeout: Duration,
    limit: usize,
) -> io::Result<String> {
    run(command, stop, timeout, limit, None)
}

pub fn with_input(
    command: &mut Command,
    stop: &AtomicBool,
    timeout: Duration,
    limit: usize,
    input: &[u8],
) -> io::Result<String> {
    run(command, stop, timeout, limit, Some(input))
}

fn run(
    command: &mut Command,
    stop: &AtomicBool,
    timeout: Duration,
    limit: usize,
    input: Option<&[u8]>,
) -> io::Result<String> {
    if stop.load(Ordering::Relaxed) {
        return Err(io::ErrorKind::Interrupted.into());
    }
    let mut child = command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(input) = input
        && let Err(error) = child
            .stdin
            .take()
            .expect("piped child stdin")
            .write_all(input)
    {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    let stdout = child.stdout.take().expect("piped child stdout");
    thread::scope(|scope| {
        let reader = scope.spawn(move || {
            let mut bytes = Vec::new();
            stdout.take(limit as u64 + 1).read_to_end(&mut bytes)?;
            if bytes.len() > limit {
                return Err(io::Error::other("response exceeds size limit"));
            }
            String::from_utf8(bytes).map_err(io::Error::other)
        });
        let start = Instant::now();
        loop {
            if stop.load(Ordering::Relaxed) || start.elapsed() >= timeout {
                let _ = child.kill();
                let _ = child.wait();
                return Err(if stop.load(Ordering::Relaxed) {
                    io::ErrorKind::Interrupted
                } else {
                    io::ErrorKind::TimedOut
                }
                .into());
            }
            match child.try_wait() {
                Ok(Some(status)) if status.success() => {
                    return reader
                        .join()
                        .map_err(|_| io::Error::other("child reader stopped"))?;
                }
                Ok(Some(status)) => {
                    return Err(io::Error::other(format!("command exited with {status}")));
                }
                Ok(None) => thread::sleep(Duration::from_millis(8)),
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error);
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancels_an_in_flight_child() {
        let stop = AtomicBool::new(false);
        thread::scope(|scope| {
            let start = Instant::now();
            scope.spawn(|| {
                thread::sleep(Duration::from_millis(60));
                stop.store(true, Ordering::Relaxed);
            });
            let error = output(
                Command::new("sleep").arg("5"),
                &stop,
                Duration::from_secs(6),
                100,
            )
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::Interrupted);
            assert!(start.elapsed() < Duration::from_secs(2));
        });
    }
}

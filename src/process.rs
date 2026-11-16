use std::{
    io::{self, BufRead, BufReader, Read},
    process::{Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use command_group::{CommandGroup, GroupChild};

const MAX_STDOUT_CAPTURE: usize = 2 * 1024 * 1024;
const MAX_STDERR_CAPTURE: usize = 256 * 1024;

#[derive(Debug)]
pub struct CapturedOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[cfg(test)]
fn run_bounded(command: &mut Command, timeout: Duration) -> Result<CapturedOutput> {
    run_bounded_cancellable(command, timeout, None)
}

pub fn run_bounded_cancellable(
    command: &mut Command,
    timeout: Duration,
    cancellation: Option<&AtomicBool>,
) -> Result<CapturedOutput> {
    run_bounded_cancellable_inner(command, timeout, cancellation, None)
}

fn run_bounded_cancellable_inner(
    command: &mut Command,
    timeout: Duration,
    cancellation: Option<&AtomicBool>,
    on_progress: Option<Arc<dyn Fn(u64) + Send + Sync>>,
) -> Result<CapturedOutput> {
    let mut child: GroupChild = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .group_spawn()
        .context("failed to start external media tool")?;

    let stdout = child
        .inner()
        .stdout
        .take()
        .ok_or_else(|| anyhow!("could not capture media tool stdout"))?;
    let stderr = child
        .inner()
        .stderr
        .take()
        .ok_or_else(|| anyhow!("could not capture media tool stderr"))?;
    let stdout_reader = thread::spawn(move || match on_progress {
        Some(callback) => drain_progress(stdout, MAX_STDOUT_CAPTURE, callback),
        None => drain_capped(stdout, MAX_STDOUT_CAPTURE),
    });
    let stderr_reader = thread::spawn(move || drain_capped(stderr, MAX_STDERR_CAPTURE));

    let start = Instant::now();
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .context("failed while waiting for media tool")?
        {
            break status;
        }
        if cancellation.is_some_and(|token| token.load(Ordering::Acquire)) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            bail!("media tool was cancelled");
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            bail!("media tool exceeded its time limit after {:?}", timeout);
        }
        thread::sleep(Duration::from_millis(25));
    };

    let stdout = stdout_reader
        .join()
        .map_err(|_| anyhow!("stdout reader thread panicked"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| anyhow!("stderr reader thread panicked"))??;

    Ok(CapturedOutput {
        status,
        stdout,
        stderr,
    })
}

pub(crate) fn run_checked_cancellable_with_progress(
    command: &mut Command,
    timeout: Duration,
    label: &str,
    cancellation: Option<&AtomicBool>,
    on_progress: Arc<dyn Fn(u64) + Send + Sync>,
) -> Result<CapturedOutput> {
    let output = run_bounded_cancellable_inner(command, timeout, cancellation, Some(on_progress))
        .with_context(|| format!("{label} failed"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{label} exited with {}: {}", output.status, stderr.trim());
    }
    Ok(output)
}

pub fn run_checked_cancellable(
    command: &mut Command,
    timeout: Duration,
    label: &str,
    cancellation: Option<&AtomicBool>,
) -> Result<CapturedOutput> {
    let output = run_bounded_cancellable(command, timeout, cancellation)
        .with_context(|| format!("{label} failed"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{label} exited with {}: {}", output.status, stderr.trim());
    }
    Ok(output)
}

fn drain_capped(mut reader: impl Read, cap: usize) -> io::Result<Vec<u8>> {
    let mut captured = Vec::with_capacity(cap.min(16 * 1024));
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let remaining = cap.saturating_sub(captured.len());
        captured.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    Ok(captured)
}

fn drain_progress(
    reader: impl Read,
    cap: usize,
    on_progress: Arc<dyn Fn(u64) + Send + Sync>,
) -> io::Result<Vec<u8>> {
    let mut reader = BufReader::new(reader);
    let mut captured = Vec::with_capacity(cap.min(16 * 1024));
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        let remaining = cap.saturating_sub(captured.len());
        captured.extend_from_slice(&line[..read.min(remaining)]);

        if let Some(frame) = line
            .strip_prefix(b"frame=")
            .and_then(|value| std::str::from_utf8(value).ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
        {
            on_progress(frame);
        }
    }
    Ok(captured)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn progress_reader_reports_frames_and_preserves_output() {
        let progress = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&progress);
        let input = b"frame=12\nout_time_us=500000\nprogress=continue\nframe=24\nprogress=end\n";
        let output = drain_progress(
            &input[..],
            1024,
            Arc::new(move |frame| {
                observed.lock().unwrap().push(frame);
            }),
        )
        .unwrap();

        assert_eq!(output, input);
        assert_eq!(*progress.lock().unwrap(), [12, 24]);
    }

    #[test]
    fn run_bounded_kills_a_process_after_its_deadline() {
        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new("ping.exe");
            command.args(["-n", "3", "127.0.0.1"]);
            command
        };
        #[cfg(unix)]
        let mut command = {
            let mut command = Command::new("sh");
            command.args(["-c", "sleep 30 & wait"]);
            command
        };

        let started = Instant::now();
        let result = run_bounded(&mut command, Duration::from_millis(100));
        assert!(result.is_err());
        assert!(format!("{:#}", result.unwrap_err()).contains("time limit"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn run_bounded_kills_a_process_when_cancelled() {
        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new("ping.exe");
            command.args(["-n", "30", "127.0.0.1"]);
            command
        };
        #[cfg(unix)]
        let mut command = {
            let mut command = Command::new("sh");
            command.args(["-c", "sleep 30 & wait"]);
            command
        };

        let cancellation = Arc::new(AtomicBool::new(false));
        let cancel_after_delay = Arc::clone(&cancellation);
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            cancel_after_delay.store(true, Ordering::Release);
        });
        let started = Instant::now();
        let result =
            run_bounded_cancellable(&mut command, Duration::from_secs(10), Some(&cancellation));
        canceller.join().unwrap();
        assert!(result.is_err());
        assert!(format!("{:#}", result.unwrap_err()).contains("cancelled"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}

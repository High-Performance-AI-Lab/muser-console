//! Where a reading comes from.
//!
//! The real source runs `powermetrics`; the recorded source replays checked-in
//! text. Both are variants of one enum rather than a `dyn` trait so the async
//! sample method needs no boxing, and so the exporter cannot be *accidentally*
//! wired to the test double: constructing that variant is explicit and only
//! tests do it.

use std::collections::VecDeque;
use std::io::ErrorKind;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::ChildStderr;
use tokio::sync::watch;

/// Why a scrape got no reading. The `detail` is for the exporter's own stderr
/// log; only [`SourceError::category`] — which quotes nothing the exporter did
/// not write itself — ever reaches an HTTP response body.
#[derive(Clone, Debug)]
pub enum SourceError {
    /// The process could not be started at all (not installed, not permitted).
    Spawn(String),
    /// Started, then an I/O error while collecting its output.
    Io(String),
    /// Ran and exited non-zero. On any machine that is not root this is the
    /// expected outcome: `powermetrics` refuses with
    /// "powermetrics must be invoked as the superuser" and exit status 1.
    Exit { status: String, stderr: String },
    /// No completed frame arrived inside a bounded wait. A stalled sampler is
    /// killed by its supervisor; an HTTP timeout leaves a healthy one running.
    Timeout(Duration),
    /// Produced bytes that are not UTF-8. Lossy decoding could corrupt a
    /// digit, so the sample is discarded instead.
    NonUtf8,
}

/// One completed source frame, carrying its own time rather than acquiring a
/// new timestamp when an HTTP client happens to ask for it.
#[derive(Clone, Debug)]
pub struct SourceSample {
    pub text: String,
    pub completed_at: Instant,
    pub completed_unix_s: Option<f64>,
}

impl SourceSample {
    fn now(text: String) -> SourceSample {
        SourceSample {
            text,
            completed_at: Instant::now(),
            completed_unix_s: unix_seconds(),
        }
    }
}

impl SourceError {
    /// A fixed phrase safe to publish in the exposition: it names the failure
    /// class and nothing the exporter read from the environment.
    pub fn category(&self) -> &'static str {
        match self {
            SourceError::Spawn(_) => "the powermetrics process could not be started",
            SourceError::Io(_) => "reading the powermetrics output failed",
            SourceError::Exit { .. } => "the powermetrics process exited non-zero",
            SourceError::Timeout(_) => "powermetrics did not produce a completed frame in time",
            SourceError::NonUtf8 => "the powermetrics output was not UTF-8",
        }
    }

    /// The full reason, for the exporter's stderr log only.
    pub fn detail(&self) -> String {
        match self {
            SourceError::Spawn(error) => format!("spawn failed: {error}"),
            SourceError::Io(error) => format!("io error: {error}"),
            // `status` already reads "exit status: N".
            SourceError::Exit { status, stderr } if stderr.is_empty() => status.clone(),
            SourceError::Exit { status, stderr } => format!("{status} — {stderr}"),
            SourceError::Timeout(budget) => {
                format!("no completed frame within {:.3} s", budget.as_secs_f64())
            }
            SourceError::NonUtf8 => "output was not UTF-8".to_owned(),
        }
    }
}

/// The `powermetrics` invocation.
///
/// `powermetrics` requires root. This exporter **never** escalates: no `sudo`,
/// no setuid helper, no launchd shim. Run it as root and it reads the
/// counters; run it as anyone else and every scrape reports
/// `muser_agent_up 0`, which is the true statement about a process that cannot
/// see them.
#[derive(Clone, Debug)]
pub struct Powermetrics {
    /// `-i`: the width of each frame produced by the persistent sampler.
    pub sample_ms: u64,
    /// How long one scrape waits for a new completed frame.
    pub budget: Duration,
    worker: Arc<SamplerWorker>,
}

#[derive(Clone, Debug)]
struct Published {
    generation: u64,
    outcome: Option<Result<SourceSample, SourceError>>,
}

#[derive(Debug)]
struct SamplerWorker {
    started: AtomicBool,
    generation: AtomicU64,
    served_generation: AtomicU64,
    published: watch::Sender<Published>,
}

impl Powermetrics {
    /// Default sample window. `powermetrics` itself defaults to 5000 ms, which
    /// is far longer than a 1 s scrape tick can wait.
    pub const DEFAULT_SAMPLE_MS: u64 = 200;

    /// Absolute path, never a PATH search. This process is meant to run as
    /// root, and resolving a bare program name through an inherited PATH
    /// would let anyone who can write an earlier directory choose what root
    /// executes.
    pub const PROGRAM: &'static str = "/usr/bin/powermetrics";

    /// Ceiling on how long one HTTP scrape waits for a newly completed frame.
    /// The console gives an agent scrape 950 ms, so this remains inside the
    /// one-second history tick and leaves 50 ms for loopback and parsing.
    pub const MAX_BUDGET: Duration = Duration::from_millis(900);

    /// A persistent sampler that stops producing is killed and restarted. The
    /// first frame on this machine takes about a second because initialization
    /// dominates; steady-state frames still arrive at `sample_ms` cadence.
    const RESTART_DELAY: Duration = Duration::from_secs(1);
    const STALL_ALLOWANCE: Duration = Duration::from_secs(3);
    const MAX_FRAME_BYTES: usize = 1024 * 1024;

    pub fn new(sample_ms: u64) -> Powermetrics {
        let (published, _) = watch::channel(Published {
            generation: 0,
            outcome: None,
        });
        Powermetrics {
            sample_ms,
            budget: Powermetrics::MAX_BUDGET,
            worker: Arc::new(SamplerWorker {
                started: AtomicBool::new(false),
                generation: AtomicU64::new(0),
                served_generation: AtomicU64::new(0),
                published,
            }),
        }
    }

    fn ensure_started(&self) {
        if self
            .worker
            .started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let worker = Arc::clone(&self.worker);
        let sample_ms = self.sample_ms;
        tokio::spawn(async move {
            worker.supervise(sample_ms).await;
        });
    }

    async fn sample(&self) -> Result<SourceSample, SourceError> {
        self.ensure_started();
        let mut receiver = self.worker.published.subscribe();
        let last_served = self.worker.served_generation.load(Ordering::Acquire);
        let wait = async {
            loop {
                let published = receiver.borrow().clone();
                if let Some(outcome) = published.outcome {
                    match outcome {
                        Ok(sample) if published.generation > last_served => {
                            self.worker
                                .served_generation
                                .store(published.generation, Ordering::Release);
                            return Ok(sample);
                        }
                        Err(error) => return Err(error),
                        Ok(_) => {}
                    }
                }
                receiver.changed().await.map_err(|_| {
                    SourceError::Spawn("powermetrics sampler supervisor stopped".to_owned())
                })?;
            }
        };
        tokio::time::timeout(self.budget, wait)
            .await
            .map_err(|_| SourceError::Timeout(self.budget))?
    }
}

impl SamplerWorker {
    async fn supervise(self: Arc<Self>, sample_ms: u64) {
        loop {
            let error = self.run_once(sample_ms).await;
            self.publish(Err(error));
            tokio::time::sleep(Powermetrics::RESTART_DELAY).await;
        }
    }

    fn publish(&self, outcome: Result<SourceSample, SourceError>) {
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.published.send_replace(Published {
            generation,
            outcome: Some(outcome),
        });
    }

    async fn run_once(&self, sample_ms: u64) -> SourceError {
        let mut command = tokio::process::Command::new(Powermetrics::PROGRAM);
        command
            .arg("--buffer-size")
            .arg("1")
            .arg("--samplers")
            .arg("cpu_power,gpu_power")
            .arg("-n")
            .arg("-1")
            .arg("-i")
            .arg(sample_ms.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Runtime shutdown drops the supervisor and takes its privileged
            // child with it; an HTTP client disconnect never owns this child.
            .kill_on_drop(true);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => return SourceError::Spawn(error.to_string()),
        };
        let Some(stdout) = child.stdout.take() else {
            return SourceError::Io("powermetrics stdout was not piped".to_owned());
        };
        let Some(stderr) = child.stderr.take() else {
            return SourceError::Io("powermetrics stderr was not piped".to_owned());
        };
        let stderr_task = tokio::spawn(drain_stderr(stderr));
        let mut lines = BufReader::new(stdout).lines();
        let mut frame = FrameAssembler::default();
        let stall_budget =
            Duration::from_millis(sample_ms).saturating_add(Powermetrics::STALL_ALLOWANCE);
        loop {
            let next = match tokio::time::timeout(stall_budget, lines.next_line()).await {
                Ok(next) => next,
                Err(_) => {
                    stop_child(&mut child).await;
                    let _ = stderr_task.await;
                    return SourceError::Timeout(stall_budget);
                }
            };
            match next {
                Ok(Some(line)) => match frame.push(&line) {
                    Ok(Some(completed)) => self.publish(Ok(SourceSample::now(completed))),
                    Ok(None) => {}
                    Err(error) => {
                        stop_child(&mut child).await;
                        let _ = stderr_task.await;
                        return error;
                    }
                },
                Ok(None) => break,
                Err(error) if error.kind() == ErrorKind::InvalidData => {
                    stop_child(&mut child).await;
                    let _ = stderr_task.await;
                    return SourceError::NonUtf8;
                }
                Err(error) => {
                    stop_child(&mut child).await;
                    let _ = stderr_task.await;
                    return SourceError::Io(error.to_string());
                }
            }
        }
        let status = match child.wait().await {
            Ok(status) => status,
            Err(error) => return SourceError::Io(error.to_string()),
        };
        let stderr = stderr_task.await.unwrap_or_default();
        if status.success() {
            SourceError::Io("persistent powermetrics sampler exited".to_owned())
        } else {
            SourceError::Exit {
                status: status.to_string(),
                stderr,
            }
        }
    }
}

#[derive(Default)]
struct FrameAssembler {
    current: Option<String>,
}

impl FrameAssembler {
    fn push(&mut self, line: &str) -> Result<Option<String>, SourceError> {
        if line.starts_with("*** Sampled system activity (") {
            let completed = self.current.replace(format!("{line}\n"));
            return Ok(completed);
        }
        let Some(current) = self.current.as_mut() else {
            return Ok(None);
        };
        if current.len().saturating_add(line.len()).saturating_add(1)
            > Powermetrics::MAX_FRAME_BYTES
        {
            return Err(SourceError::Io(
                "powermetrics sample exceeded the frame limit".to_owned(),
            ));
        }
        current.push_str(line);
        current.push('\n');
        Ok(None)
    }
}

async fn drain_stderr(stderr: ChildStderr) -> String {
    let mut lines = BufReader::new(stderr).lines();
    let mut first = String::new();
    while let Ok(Some(line)) = lines.next_line().await {
        if first.is_empty() && !line.trim().is_empty() {
            first = first_line(&line);
        }
    }
    first
}

async fn stop_child(child: &mut tokio::process::Child) {
    let _ = child.kill().await;
}

fn unix_seconds() -> Option<f64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|since| since.as_secs_f64())
}

/// Trim a child's stderr down to one short line for the log. Nothing from
/// here reaches an HTTP body.
fn first_line(text: &str) -> String {
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    let line = line.trim();
    match line.char_indices().nth(200) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_owned(),
    }
}

/// A test double. It replays text that is already on disk (or a recorded
/// failure) instead of running anything, so the parser and the exposition can
/// be tested on a machine that is not root — which is every machine this is
/// developed on.
///
/// It measures nothing and must never be constructed by the binary.
#[derive(Debug)]
pub struct RecordedSource {
    responses: Mutex<VecDeque<Result<String, SourceError>>>,
    last: Mutex<Result<String, SourceError>>,
    calls: AtomicUsize,
}

impl RecordedSource {
    /// Replays `responses` in order. Once they run out the final response
    /// repeats, so a test can say "succeeds once, then fails forever".
    pub fn new(responses: Vec<Result<String, SourceError>>) -> RecordedSource {
        let last = responses
            .last()
            .cloned()
            .unwrap_or(Err(SourceError::Spawn("no recorded responses".to_owned())));
        RecordedSource {
            responses: Mutex::new(responses.into()),
            last: Mutex::new(last),
            calls: AtomicUsize::new(0),
        }
    }

    /// Always yields the same text.
    pub fn text(text: &str) -> RecordedSource {
        RecordedSource::new(vec![Ok(text.to_owned())])
    }

    /// Always fails the way a non-root `powermetrics` run fails.
    pub fn not_root() -> RecordedSource {
        RecordedSource::new(vec![Err(SourceError::Exit {
            status: "exit status: 1".to_owned(),
            stderr: "powermetrics must be invoked as the superuser".to_owned(),
        })])
    }

    /// How many times a sample was actually taken — how the cache tests prove
    /// the source was *not* re-run.
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    fn sample(&self) -> Result<SourceSample, SourceError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let next = self
            .responses
            .lock()
            .expect("recorded source lock")
            .pop_front();
        let response = match next {
            Some(response) => {
                *self.last.lock().expect("recorded source lock") = response.clone();
                response
            }
            None => self.last.lock().expect("recorded source lock").clone(),
        };
        response.map(SourceSample::now)
    }
}

/// The exporter's data source.
#[derive(Debug)]
pub enum PowerSource {
    /// The real thing.
    Powermetrics(Powermetrics),
    /// The test double. Never constructed by the binary.
    Recorded(RecordedSource),
}

impl PowerSource {
    pub fn powermetrics(sample_ms: u64) -> PowerSource {
        PowerSource::Powermetrics(Powermetrics::new(sample_ms))
    }

    pub fn recorded(source: RecordedSource) -> PowerSource {
        PowerSource::Recorded(source)
    }

    /// Take one sample. `Ok` is exactly the bytes the source produced; the
    /// caller decides what, if anything, is in them.
    pub async fn sample(&self) -> Result<SourceSample, SourceError> {
        match self {
            PowerSource::Powermetrics(command) => command.sample().await,
            PowerSource::Recorded(recorded) => recorded.sample(),
        }
    }

    /// The recorded double, when that is what this source is — lets a test
    /// read the call count without holding a second handle.
    pub fn recorded_source(&self) -> Option<&RecordedSource> {
        match self {
            PowerSource::Recorded(recorded) => Some(recorded),
            PowerSource::Powermetrics(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorded_responses_replay_in_order_then_repeat_the_last() {
        let recorded = RecordedSource::new(vec![
            Ok("first".to_owned()),
            Err(SourceError::Timeout(Duration::from_secs(2))),
        ]);
        assert_eq!(recorded.sample().expect("first response").text, "first");
        assert!(recorded.sample().is_err());
        assert!(recorded.sample().is_err(), "last response repeats");
        assert_eq!(recorded.calls(), 3);
    }

    #[test]
    fn error_categories_quote_nothing_from_the_environment() {
        let error = SourceError::Exit {
            status: "exit status: 1".to_owned(),
            stderr: "powermetrics must be invoked as the superuser".to_owned(),
        };
        assert_eq!(error.category(), "the powermetrics process exited non-zero");
        assert!(
            error.detail().contains("superuser"),
            "the full reason is available for the log"
        );
    }

    #[test]
    fn stderr_is_reduced_to_one_bounded_line() {
        assert_eq!(first_line("\n  hello \nworld\n"), "hello");
        let long = "x".repeat(500);
        let trimmed = first_line(&long);
        assert_eq!(
            trimmed.chars().count(),
            201,
            "200 characters plus an ellipsis"
        );
    }

    #[test]
    fn real_source_budget_stays_inside_the_console_tick() {
        let default = Powermetrics::new(Powermetrics::DEFAULT_SAMPLE_MS);
        assert_eq!(default.budget, Duration::from_millis(900));
        assert_eq!(Powermetrics::new(50).budget, Powermetrics::MAX_BUDGET);
        assert_eq!(
            Powermetrics::new(5_000).budget,
            Powermetrics::MAX_BUDGET,
            "operator input cannot stretch a scrape beyond one history tick"
        );
    }

    #[test]
    fn real_frame_delimiters_publish_only_completed_samples() {
        let mut frames = FrameAssembler::default();
        assert!(frames
            .push("Machine model: ignored preamble")
            .unwrap()
            .is_none());
        assert!(frames
            .push("*** Sampled system activity (first) ***")
            .unwrap()
            .is_none());
        assert!(frames.push("CPU Power: 1 W").unwrap().is_none());
        let first = frames
            .push("*** Sampled system activity (second) ***")
            .unwrap()
            .expect("the second header completes the first frame");
        assert_eq!(
            first,
            "*** Sampled system activity (first) ***\nCPU Power: 1 W\n"
        );
        assert!(
            frames.push("GPU Power: 2 W").unwrap().is_none(),
            "an in-progress final frame is never presented as complete"
        );
    }

    #[test]
    fn a_runaway_frame_is_refused_instead_of_growing_without_bound() {
        let mut frames = FrameAssembler::default();
        frames
            .push("*** Sampled system activity (first) ***")
            .unwrap();
        let line = "x".repeat(Powermetrics::MAX_FRAME_BYTES);
        assert!(matches!(frames.push(&line), Err(SourceError::Io(_))));
    }
}

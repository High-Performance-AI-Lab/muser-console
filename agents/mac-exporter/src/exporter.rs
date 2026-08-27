//! Scrape logic: one bounded cache, one reading, no invention.
//!
//! A persistent `powermetrics` process produces independent frames while HTTP
//! clients come and go. A successful reading is reused — but only for
//! [`MAX_READING_AGE`], and only with its source completion time and age
//! published alongside it. Past that age the reading is not served at all:
//! re-serving it would present a measurement of one moment as a measurement of
//! another, which is the exact failure this project is built to avoid.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::logging::log;
use crate::parse::{parse_powermetrics, Reading};
use crate::source::PowerSource;

/// How long a successful reading may still be served. Not a knob: it is part
/// of what "current" means in this exposition, and a console that tuned it
/// would change what its stored samples claim.
///
/// Deliberately shorter than the console's 1 s scrape tick. The cache exists
/// so a burst of concurrent scrapers shares one completed frame, not so a
/// single scraper can be handed the same measurement on consecutive ticks —
/// that would store one reading as several distinct 1 Hz samples, which is a
/// flat line the hardware never produced. Every scrape a second apart
/// re-measures.
pub const MAX_READING_AGE: Duration = Duration::from_millis(500);

/// The reading a scrape served, with the facts needed to say when it was
/// taken.
#[derive(Clone, Debug)]
pub struct Served {
    pub reading: Reading,
    /// Unix seconds at which `powermetrics` returned this sample — the end of
    /// its sample window. `None` only if the system clock is unreadable.
    pub completed_unix_s: Option<f64>,
    /// How old the sample was when this scrape served it. Zero-ish for a fresh
    /// run, up to [`MAX_READING_AGE`] for a reused one.
    pub age: Duration,
}

/// What one scrape produced.
#[derive(Clone, Debug)]
pub struct Scrape {
    /// True exactly when this scrape served a real reading.
    pub up: bool,
    /// Wall-clock time this scrape spent getting there.
    pub duration: Duration,
    pub served: Option<Served>,
    /// Why there was no reading, as a fixed phrase safe to publish.
    pub failure: Option<&'static str>,
}

#[derive(Clone, Debug)]
struct Cached {
    reading: Reading,
    completed_unix_s: Option<f64>,
    completed_at: Instant,
}

pub struct Exporter {
    source: PowerSource,
    host: Option<String>,
    max_age: Duration,
    cached: Mutex<Option<Cached>>,
    /// Serializes sampling so a burst of scrapes runs `powermetrics` once, not
    /// once each.
    sampling: tokio::sync::Mutex<()>,
    /// Last published up/down state, for logging transitions instead of ticks.
    last_up: Mutex<Option<bool>>,
    /// How many lines the transition logger has written. Counting them is how
    /// a test can assert "once per state change" without capturing stderr.
    logged: AtomicUsize,
}

impl Exporter {
    pub fn new(source: PowerSource, host: Option<String>) -> Exporter {
        Exporter::with_max_age(source, host, MAX_READING_AGE)
    }

    /// `max_age` is a test seam, not a configuration knob: the binary always
    /// uses [`MAX_READING_AGE`]. A zero max age makes every scrape re-measure.
    pub fn with_max_age(source: PowerSource, host: Option<String>, max_age: Duration) -> Exporter {
        Exporter {
            source,
            host,
            max_age,
            cached: Mutex::new(None),
            sampling: tokio::sync::Mutex::new(()),
            last_up: Mutex::new(None),
            logged: AtomicUsize::new(0),
        }
    }

    /// Number of up/down transitions logged so far.
    pub fn state_log_count(&self) -> usize {
        self.logged.load(Ordering::Relaxed)
    }

    pub fn host(&self) -> Option<&str> {
        self.host.as_deref()
    }

    pub fn source(&self) -> &PowerSource {
        &self.source
    }

    pub fn max_age(&self) -> Duration {
        self.max_age
    }

    /// Take (or reuse) one reading.
    pub async fn scrape(&self) -> Scrape {
        let started = Instant::now();
        if let Some(served) = self.fresh_reading() {
            // A reused reading does not change the up/down state and so logs
            // nothing; it is the same source answering.
            return Scrape {
                up: true,
                duration: started.elapsed(),
                served: Some(served),
                failure: None,
            };
        }

        let _sampling = self.sampling.lock().await;
        // Another scrape may have refreshed the cache while this one waited;
        // serving its reading is cheaper and just as true.
        if let Some(served) = self.fresh_reading() {
            return Scrape {
                up: true,
                duration: started.elapsed(),
                served: Some(served),
                failure: None,
            };
        }

        match self.source.sample().await {
            Ok(sample) => {
                let reading = parse_powermetrics(&sample.text);
                let completed_at = sample.completed_at;
                let completed_unix_s = sample.completed_unix_s;
                *self.cached.lock().expect("cache lock") = Some(Cached {
                    reading: reading.clone(),
                    completed_unix_s,
                    completed_at,
                });
                self.note_state(true, || {
                    if reading.is_empty() {
                        "powermetrics ran but printed none of the power lines this exporter \
                         recognizes; publishing no power series"
                            .to_owned()
                    } else {
                        "powermetrics reading available".to_owned()
                    }
                });
                Scrape {
                    up: true,
                    duration: started.elapsed(),
                    served: Some(Served {
                        reading,
                        completed_unix_s,
                        age: completed_at.elapsed(),
                    }),
                    failure: None,
                }
            }
            Err(error) => {
                self.note_state(false, || {
                    format!("no reading: {} (publishing nothing)", error.detail())
                });
                // The cache is deliberately left alone and deliberately not
                // consulted: it is already older than max_age or this branch
                // would not have run.
                Scrape {
                    up: false,
                    duration: started.elapsed(),
                    served: None,
                    failure: Some(error.category()),
                }
            }
        }
    }

    fn fresh_reading(&self) -> Option<Served> {
        let cached = self.cached.lock().expect("cache lock");
        let cached = cached.as_ref()?;
        let age = cached.completed_at.elapsed();
        if age > self.max_age {
            return None;
        }
        Some(Served {
            reading: cached.reading.clone(),
            completed_unix_s: cached.completed_unix_s,
            age,
        })
    }

    /// Log only when the up/down state changes, so a machine that is simply
    /// not root writes one line, not one per second forever.
    fn note_state(&self, up: bool, message: impl FnOnce() -> String) {
        let mut last = self.last_up.lock().expect("state lock");
        if *last != Some(up) {
            *last = Some(up);
            self.logged.fetch_add(1, Ordering::Relaxed);
            log(&message());
        }
    }
}

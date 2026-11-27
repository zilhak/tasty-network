//! The Ping sender of one channel (docs/design/transport.md "heartbeat와 읽기 시간 제한").

use std::io;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::channel::{Outbox, PingResult};
use crate::lock::lock;

/// When a side sends [`StreamTag::Ping`](tasty_net_frame::StreamTag::Ping).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeartbeatPolicy {
    /// Only after the sending direction was quiet for one interval (the tasty server's write
    /// thread).
    WhenIdle,
    /// Every interval, whatever else is sent (the tasty GUI and CLI clients).
    Periodic,
}

struct Stop {
    stopped: Mutex<bool>,
    cv: Condvar,
    /// Run by the thread right before it ends, for tests that hold a join in progress.
    #[cfg(test)]
    before_exit: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

/// The heartbeat thread of one channel. Dropping it stops and joins the thread; a Ping write
/// in progress is bounded by the channel's write timeout.
pub(crate) struct Heartbeat {
    stop: Arc<Stop>,
    thread: Option<JoinHandle<()>>,
}

impl Heartbeat {
    pub(crate) fn start(
        out: Arc<Outbox>,
        policy: HeartbeatPolicy,
        interval: Duration,
    ) -> io::Result<Self> {
        let stop = Arc::new(Stop {
            stopped: Mutex::new(false),
            cv: Condvar::new(),
            #[cfg(test)]
            before_exit: Mutex::new(None),
        });
        let seen = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("tasty-net-heartbeat".into())
            .spawn(move || {
                run(&out, &seen, policy, interval);
                #[cfg(test)]
                if let Some(hook) = lock(&seen.before_exit).take() {
                    hook();
                }
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }

    /// Makes the thread run `hook` right before it ends (after it saw the stop).
    #[cfg(test)]
    pub(crate) fn before_exit(&self, hook: Box<dyn FnOnce() + Send>) {
        *lock(&self.stop.before_exit) = Some(hook);
    }
}

fn run(out: &Outbox, stop: &Stop, policy: HeartbeatPolicy, interval: Duration) {
    let mut next_periodic = Instant::now() + interval;
    let mut wait = interval;
    loop {
        {
            let stopped = lock(&stop.stopped);
            let (stopped, _) = stop
                .cv
                .wait_timeout_while(stopped, wait, |s| !*s)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if *stopped {
                return;
            }
        }
        let now = Instant::now();
        let result = match policy {
            HeartbeatPolicy::WhenIdle => out.write_ping_if(|last| {
                let due = last + interval;
                (due > now).then(|| due - now)
            }),
            HeartbeatPolicy::Periodic => {
                if next_periodic > now {
                    PingResult::NotDue(next_periodic - now)
                } else {
                    next_periodic = now + interval;
                    out.write_ping_if(|_| None)
                }
            }
        };
        wait = match result {
            PingResult::Sent => interval,
            PingResult::NotDue(left) => left,
            PingResult::Stopped => return,
        };
    }
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        *lock(&self.stop.stopped) = true;
        self.stop.cv.notify_all();
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::warn!("the heartbeat thread panicked");
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::channel::{ChannelConfig, RawChannel};
    use crate::heartbeat::HeartbeatPolicy;
    use crate::memory::memory_channel_pair;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};
    use tasty_net_frame::{ReadFailure, StreamTag, read_frame};

    fn config(policy: HeartbeatPolicy, read_timeout: Duration) -> ChannelConfig {
        ChannelConfig {
            read_timeout,
            write_timeout: Duration::from_secs(5),
            heartbeat: policy,
            drain_cap: Duration::from_millis(200),
        }
    }

    /// Reads raw frames from `raw` until `deadline` and returns their tags.
    fn raw_tags_until(raw: &mut RawChannel, deadline: Instant) -> Vec<StreamTag> {
        raw.control()
            .set_read_timeout(Some(Duration::from_millis(20)))
            .expect("timeout");
        let mut tags = Vec::new();
        while Instant::now() < deadline {
            match read_frame(raw) {
                Ok(frame) => tags.push(frame.tag),
                Err(e) if ReadFailure::of_io(&e) == ReadFailure::HeartbeatTimeout => {}
                Err(e) => panic!("raw read failed: {e}"),
            }
        }
        tags
    }

    #[test]
    fn a_quiet_writer_sends_pings() {
        let (a, mut b) = memory_channel_pair();
        // 600 ms read timeout -> 100 ms interval.
        let a = a
            .framed(&config(
                HeartbeatPolicy::WhenIdle,
                Duration::from_millis(600),
            ))
            .expect("framed");
        let tags = raw_tags_until(&mut b, Instant::now() + Duration::from_millis(550));
        assert!(
            tags.iter().filter(|t| **t == StreamTag::Ping).count() >= 2,
            "{tags:?}"
        );
        drop(a);
    }

    /// Waits until `from + pause` by checking the clock and yielding, not by a timed sleep.
    /// The macOS CI runner left the writer of this test quiet for 86 to 178 ms where it
    /// meant one write every 20 ms. Its log keeps only the largest of the measured gaps, so
    /// it does not tell which gap grew; a 20 ms `thread::sleep` that wakes late is the most
    /// likely cause, and pacing by the clock does not depend on when a timed wait wakes.
    fn pace(from: Instant, pause: Duration) {
        while from.elapsed() < pause {
            std::thread::yield_now();
        }
    }

    /// How long the channel's last write stayed unchanged while the peer read, as upper
    /// bounds of three kinds of gaps, and the writes that ended while it read.
    #[derive(Debug, Clone, Copy)]
    struct Gaps {
        /// From before `framed` (which sets the last write) to the end of the first write.
        first: Duration,
        /// The longest from the start of one write to the end of the next.
        middle: Duration,
        /// From the start of the last write to when the reading ended.
        last: Duration,
        writes: usize,
    }

    impl Gaps {
        fn longest(&self) -> Duration {
            self.first.max(self.middle).max(self.last)
        }

        /// The gaps of the writes `(start, end)`, counting only the time until `read_end`:
        /// a write that ended after it does not end a gap, and the last gap ends at
        /// `min(left, read_end)` (`left` is when the writer left its loop).
        fn of(
            framed_from: Instant,
            writes: &[(Instant, Instant)],
            left: Instant,
            read_end: Instant,
        ) -> Self {
            let done: Vec<&(Instant, Instant)> =
                writes.iter().filter(|(_, end)| *end <= read_end).collect();
            let until = left.min(read_end);
            let (Some((_, first_end)), Some((last_start, _))) = (done.first(), done.last()) else {
                let first = until.saturating_duration_since(framed_from);
                return Self {
                    first,
                    middle: Duration::ZERO,
                    last: Duration::ZERO,
                    writes: 0,
                };
            };
            let middle = done
                .windows(2)
                .map(|pair| pair[1].1.saturating_duration_since(pair[0].0))
                .max()
                .unwrap_or(Duration::ZERO);
            Self {
                first: first_end.saturating_duration_since(framed_from),
                middle,
                last: until.saturating_duration_since(*last_start),
                writes: done.len(),
            }
        }
    }

    /// Writes Data every 20 ms on a channel with a 100 ms heartbeat interval while the peer
    /// reads for 450 ms. Returns the tags it read and the gaps of the writes during the
    /// reading.
    fn busy_run(policy: HeartbeatPolicy) -> (Vec<StreamTag>, Gaps) {
        let (a, mut b) = memory_channel_pair();
        let before_framed = Instant::now();
        // 600 ms read timeout -> 100 ms interval.
        let a = a
            .framed(&config(policy, Duration::from_millis(600)))
            .expect("framed");
        let read_done = Arc::new(AtomicBool::new(false));
        let done = Arc::clone(&read_done);
        let writer = a.writer;
        let busy = std::thread::spawn(move || {
            let mut writes = Vec::new();
            while !done.load(Ordering::SeqCst) {
                let start = Instant::now();
                writer.write_frame(StreamTag::Data, b"x").expect("write");
                writes.push((start, Instant::now()));
                pace(start, Duration::from_millis(20));
            }
            (writer, writes, Instant::now())
        });
        let tags = raw_tags_until(&mut b, Instant::now() + Duration::from_millis(450));
        let read_end = Instant::now();
        read_done.store(true, Ordering::SeqCst);
        let (writer, writes, left) = busy.join().expect("writer");
        drop(writer);
        (tags, Gaps::of(before_framed, &writes, left, read_end))
    }

    /// The tags of a run of `policy` in which frames flowed: every gap of the writes stayed
    /// under half the heartbeat interval. A run where the scheduler kept the writer quiet
    /// longer is not one and runs again, up to ten times.
    fn flowing_run(policy: HeartbeatPolicy) -> Vec<StreamTag> {
        const FLOWING: Duration = Duration::from_millis(50);
        const RUNS: usize = 10;
        let mut quiet_runs = Vec::new();
        for _ in 0..RUNS {
            let (tags, gaps) = busy_run(policy);
            if gaps.longest() < FLOWING {
                return tags;
            }
            quiet_runs.push((gaps.middle, gaps.first, gaps.last, gaps.writes));
        }
        panic!(
            "{policy:?}: the writer never wrote without a {FLOWING:?} gap in {RUNS} runs \
             (longest middle gap, first gap, last gap, writes): {quiet_runs:?}"
        );
    }

    /// "While frames flow" is judged by the writes' real times, for both policies. A WhenIdle
    /// Ping after a quiet interval is correct (CI once saw only four writes in 450 ms and a
    /// Ping among them), so such a run proves nothing either way; a Periodic Ping in such a
    /// run could also be an idle Ping.
    #[test]
    fn when_idle_sends_no_ping_while_frames_flow_but_periodic_does() {
        let tags = flowing_run(HeartbeatPolicy::WhenIdle);
        let pings = tags.iter().filter(|t| **t == StreamTag::Ping).count();
        assert_eq!(
            pings, 0,
            "WhenIdle sent a Ping while frames flowed: {tags:?}"
        );

        // Periodic pings while frames flow too.
        let tags = flowing_run(HeartbeatPolicy::Periodic);
        let pings = tags.iter().filter(|t| **t == StreamTag::Ping).count();
        assert!(
            pings > 0,
            "Periodic sent no Ping while frames flowed: {tags:?}"
        );
    }

    #[test]
    fn a_reader_that_hears_nothing_ends_with_heartbeat_timeout() {
        let (a, _b) = memory_channel_pair();
        let mut a = a
            .framed(&config(
                HeartbeatPolicy::WhenIdle,
                Duration::from_millis(200),
            ))
            .expect("framed");
        let started = Instant::now();
        let error = a.reader.read_frame().expect_err("timeout");
        assert_eq!(error.failure(), ReadFailure::HeartbeatTimeout);
        assert!(started.elapsed() >= Duration::from_millis(200));
    }

    #[test]
    fn pings_keep_a_reader_alive_and_are_not_returned() {
        let (a, b) = memory_channel_pair();
        // The reader's timeout is 300 ms; the peer pings every 50 ms.
        let mut a = a
            .framed(&config(
                HeartbeatPolicy::WhenIdle,
                Duration::from_millis(300),
            ))
            .expect("framed a");
        let b = b
            .framed(&config(
                HeartbeatPolicy::Periodic,
                Duration::from_millis(300),
            ))
            .expect("framed b");
        let writer = b.writer;
        let late = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(900));
            writer.write_frame(StreamTag::Data, b"late").expect("write");
            writer
        });
        let frame = a
            .reader
            .read_frame()
            .expect("the Pings kept the reader alive");
        assert_eq!(frame.tag, StreamTag::Data);
        assert_eq!(frame.payload, b"late");
        drop(late.join().expect("writer"));
    }
}

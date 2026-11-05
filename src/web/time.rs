//! `tokio::time`'s names on the browser's timers.

use std::future::Future;
use std::time::Duration;

use futures::future::{Either, select};
pub use web_time::Instant;

/// The deadline passed.
#[derive(Debug)]
pub struct Elapsed;

impl std::fmt::Display for Elapsed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("deadline has elapsed")
    }
}

impl std::error::Error for Elapsed {}

pub async fn sleep(duration: Duration) {
    gloo_timers::future::sleep(duration).await;
}

pub async fn sleep_until(deadline: Instant) {
    sleep(deadline.saturating_duration_since(Instant::now())).await;
}

pub async fn timeout<F: Future>(duration: Duration, future: F) -> Result<F::Output, Elapsed> {
    let future = std::pin::pin!(future);
    let timer = std::pin::pin!(sleep(duration));
    match select(future, timer).await {
        Either::Left((output, _)) => Ok(output),
        Either::Right(_) => Err(Elapsed),
    }
}

/// A repeating timer. The first tick is immediate, as Tokio's is.
pub struct Interval {
    period: Duration,
    next: Instant,
}

pub fn interval(period: Duration) -> Interval {
    Interval {
        period,
        next: Instant::now(),
    }
}

impl Interval {
    pub async fn tick(&mut self) {
        sleep_until(self.next).await;
        let now = Instant::now();
        // Missed ticks are skipped: a throttled tab must not fire a burst.
        self.next = (self.next + self.period).max(now);
    }
}

//! Recording voice messages from the browser's microphone. The same interface as
//! the desktop's recorder, without its threads: `microphone` hears it off the page's
//! thread, so a page busy with a history sync delays the sound but loses none of it.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use wasm_bindgen::JsValue;

use crate::backend::Waker;
use crate::platform::Instant;
use crate::web::microphone::{self, Capture};

#[derive(Default)]
struct Take {
    /// What was heard, mono, at `rate`.
    samples: RefCell<Vec<f32>>,
    rate: Cell<u32>,
    /// A loudness for each 50 ms.
    levels: RefCell<Vec<f32>>,
    /// When sound first came, which is unset while the microphone wakes.
    began: Cell<Option<Instant>>,
    failure: RefCell<Option<String>>,
    stepped: Cell<bool>,
    /// Set once nobody wants the microphone, which may still be being opened.
    stopped: Cell<bool>,
    microphone: RefCell<Option<Capture>>,
}

impl Take {
    fn hear(&self, block: &[f32], waker: &Waker) {
        if self.stopped.get() {
            return;
        }
        if self.began.get().is_none() {
            self.began.set(Some(Instant::now()));
        }
        let loudness =
            (block.iter().map(|s| s * s).sum::<f32>() / block.len().max(1) as f32).sqrt();
        self.samples.borrow_mut().extend_from_slice(block);
        self.levels.borrow_mut().push(loudness);
        if !self.stepped.get() {
            waker.wake();
        }
    }

    fn fail(&self, reason: String) {
        *self.failure.borrow_mut() = Some(reason);
        self.microphone.borrow_mut().take();
    }
}

async fn open(take: Rc<Take>, waker: Waker) -> Result<(), JsValue> {
    let heard = {
        let take = Rc::clone(&take);
        let waker = waker.clone();
        move |block: Vec<f32>| take.hear(&block, &waker)
    };
    let ended = {
        let take = Rc::clone(&take);
        move || {
            take.fail("The microphone stopped".to_owned());
            waker.wake();
        }
    };
    let capture = microphone::open(&JsValue::TRUE, heard, ended).await?;
    take.rate.set(capture.rate());
    // A recording stopped while this was opening lets the microphone go here.
    if !take.stopped.get() {
        *take.microphone.borrow_mut() = Some(capture);
    }
    Ok(())
}

/// Records from the default microphone until told to stop.
pub struct Recorder {
    take: Rc<Take>,
}

impl Recorder {
    pub fn start(waker: Waker) -> Self {
        let take = Rc::new(Take::default());
        let opening = Rc::clone(&take);
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(error) = open(Rc::clone(&opening), waker.clone()).await
                && !opening.stopped.get()
            {
                opening.fail(microphone::refusal(&error));
                waker.wake();
            }
        });
        Self { take }
    }

    /// With no threads to fake one on, the demo records for real.
    #[cfg(feature = "demo")]
    pub fn simulated(waker: Waker) -> Self {
        Self::start(waker)
    }

    /// A take already under way, for demos.
    #[cfg(feature = "demo")]
    pub fn rehearsal() -> Self {
        let take = Take::default();
        *take.levels.borrow_mut() = (0..90)
            .map(|index| 0.05 + 0.2 * ((index as f32 * 0.6).sin().abs()))
            .collect();
        take.began.set(
            Instant::now()
                .checked_sub(Duration::from_millis(4_500))
                .or_else(|| Some(Instant::now())),
        );
        take.stopped.set(true);
        Self {
            take: Rc::new(take),
        }
    }

    /// How long the take has recorded sound: zero while the microphone wakes.
    pub fn elapsed(&self) -> Duration {
        self.take
            .began
            .get()
            .map_or(Duration::ZERO, |began| began.elapsed())
    }

    /// Whether sound has started to arrive, so what is said now is recorded.
    pub fn listening(&self) -> bool {
        self.take.began.get().is_some()
    }

    /// Stops new levels from asking for a frame of their own while `stepped`.
    pub fn set_stepped(&self, stepped: bool) {
        self.take.stepped.set(stepped);
    }

    pub fn levels(&self) -> Vec<f32> {
        self.take.levels.borrow().clone()
    }

    /// Error that stopped recording early.
    pub fn failure(&self) -> Option<String> {
        self.take.failure.borrow().clone()
    }

    /// Stops and returns mono 48 kHz samples. The worklet's last blocks may still
    /// be on their way, so a take can lose up to its final tenth of a second.
    pub fn finish(self) -> Result<Vec<f32>, String> {
        if let Some(error) = self.failure() {
            return Err(error);
        }
        self.take.stopped.set(true);
        self.take.microphone.borrow_mut().take();
        let heard = self.take.samples.take();
        if heard.is_empty() {
            return Err("The microphone did not record any audio".to_owned());
        }
        Ok(crate::voice::mono_at_rate(&heard, 1, self.take.rate.get()))
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.take.stopped.set(true);
        self.take.microphone.borrow_mut().take();
    }
}

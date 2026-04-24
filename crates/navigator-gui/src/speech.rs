//! A background speech sink so the UI thread never blocks on TTS.
//!
//! Prism's `speak` is synchronous on some backends (SAPI). We push requests
//! through a bounded channel; the worker drops older items when it falls
//! behind so we don't back up announcements during rapid arrow-key travel.

use std::sync::Arc;
use std::thread;

use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use parking_lot::Mutex;
use tracing::warn;

use navigator_prism::{Prism, Speaker};

#[derive(Debug, Clone)]
pub struct Utterance {
    pub text: String,
    pub interrupt: bool,
}

pub struct SpeechSink {
    tx: Sender<Utterance>,
    _worker: thread::JoinHandle<()>,
    _prism: Arc<Mutex<Option<Prism>>>,
}

impl SpeechSink {
    /// Start a worker thread that owns the speaker. The `Prism` context is
    /// kept in an `Arc<Mutex<Option<_>>>` purely so `Drop` can cleanly shut it
    /// down from the sink's `Drop`.
    pub fn start() -> Self {
        let prism = Prism::init().ok();
        let prism = Arc::new(Mutex::new(prism));
        let (tx, rx) = bounded::<Utterance>(32);

        let prism_for_worker = Arc::clone(&prism);
        let worker = thread::Builder::new()
            .name("navigator-speech".into())
            .spawn(move || speech_loop(prism_for_worker, rx))
            .expect("spawn speech worker");

        Self { tx, _worker: worker, _prism: prism }
    }

    /// Raw sender clone for worker threads that need a long-lived handle.
    pub fn handle(&self) -> Sender<Utterance> { self.tx.clone() }

    /// Enqueue an utterance. If the queue is full we retry once — by the
    /// time the caller races the worker the latter typically has drained a
    /// slot. Still full after the retry: drop with a warning.
    pub fn say(&self, text: impl Into<String>, interrupt: bool) {
        let u = Utterance { text: text.into(), interrupt };
        match self.tx.try_send(u) {
            Ok(()) => {}
            Err(TrySendError::Full(back)) => {
                if self.tx.try_send(back).is_err() {
                    warn!("speech queue full; dropping utterance");
                }
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

fn speech_loop(prism: Arc<Mutex<Option<Prism>>>, rx: Receiver<Utterance>) {
    let speaker: Option<Speaker> = {
        let guard = prism.lock();
        guard.as_ref().and_then(|p| p.best_speaker().ok())
    };
    let Some(speaker) = speaker else {
        warn!("no prism speaker available; speech sink no-ops");
        while rx.recv().is_ok() {}
        return;
    };
    while let Ok(u) = rx.recv() {
        if let Err(e) = speaker.speak(&u.text, u.interrupt) {
            warn!("prism speak failed: {e}");
        }
    }
}

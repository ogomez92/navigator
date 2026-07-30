//! Event sounds — a background WAV player, mirroring [`crate::speech`].
//!
//! Playback goes through `PlaySoundW` (winmm) with `SND_ASYNC`. That is a
//! single process-wide channel: starting a sound stops whatever was
//! playing. For event feedback that is the behaviour you want — a delete
//! that finishes while the navigate chime is still ringing should say
//! "delete finished", not talk over itself — and it is the same
//! last-one-wins model the speech sink uses for interrupting utterances.
//!
//! Why a worker thread at all, given `SND_ASYNC` returns immediately?
//! Two reasons. `PlaySoundW` still parses the file header on the calling
//! thread, so a WAV on a cold or network drive can block for tens of
//! milliseconds — never on the UI thread. And the filename buffer has to
//! outlive the call, which is trivial to guarantee on a thread that owns
//! it and nowhere else.
//!
//! Missing or unreadable files are silently ignored (`SND_NODEFAULT`
//! suppresses the Windows default beep): a sound that vanished should not
//! turn every copy into an error chime.

#![cfg(windows)]

use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::thread;

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use tracing::warn;
use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_FILENAME, SND_NODEFAULT};
use windows::core::PCWSTR;

use navigator_config::{ConfigHandle, SoundEvent};

/// Clone-able handle to the sound worker plus the config it resolves
/// events through. Held by `AppState` and copied into every `WorkerCtx`,
/// so a worker thread can play a completion sound without reaching back
/// into `AppState`.
///
/// The worker thread lives as long as any clone does: it exits when the
/// last sender drops, which is process teardown in practice.
#[derive(Clone)]
pub struct SoundPlayer {
    tx: Sender<PathBuf>,
    config: ConfigHandle,
}

impl SoundPlayer {
    /// Spawn the playback thread and return a handle bound to `config`.
    pub fn start(config: ConfigHandle) -> Self {
        // Small queue: these are notifications, not a playlist. If events
        // arrive faster than winmm can start them the extras are worthless
        // anyway — only the last one would be audible.
        let (tx, rx) = bounded::<PathBuf>(8);
        thread::Builder::new()
            .name("navigator-sound".into())
            .spawn(move || sound_loop(rx))
            .expect("spawn sound worker");
        Self { tx, config }
    }

    /// Play the sound mapped to `ev`, if sounds are enabled and the event
    /// has a file assigned. Cheap and non-blocking; a no-op otherwise.
    pub fn play(&self, ev: SoundEvent) {
        let path = {
            let cfg = self.config.read();
            if !cfg.sounds.enabled {
                return;
            }
            match cfg.sounds.file_for(ev) {
                Some(f) => navigator_config::sound_path(f),
                None => None,
            }
        };
        if let Some(p) = path {
            self.send(p);
        }
    }

    /// Play `path` directly, bypassing the enabled flag and the event
    /// mapping. Used by Options → Sounds to audition a file the moment it
    /// is chosen — you have to be able to hear a candidate before deciding
    /// to switch sounds on.
    pub fn play_file(&self, path: PathBuf) {
        self.send(path);
    }

    /// Queue a path, dropping it if the worker is behind. Losing a
    /// notification sound is strictly better than stalling the caller —
    /// which can be the UI thread.
    fn send(&self, path: PathBuf) {
        match self.tx.try_send(path) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => warn!("sound queue full; dropping"),
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

fn sound_loop(rx: Receiver<PathBuf>) {
    // The UTF-16 filename must stay alive for the whole `PlaySoundW` call.
    // Holding the previous one across iterations costs nothing and removes
    // any question about how long winmm keeps looking at the pointer.
    let mut _held: Vec<u16> = Vec::new();
    while let Ok(path) = rx.recv() {
        // `PlaySoundW` cannot report this for us: with `SND_ASYNC` it
        // returns TRUE as soon as it has handed the request off, so a
        // file that no longer exists looks exactly like a successful
        // play. Checking here turns the otherwise silent-and-baffling
        // "my sound stopped working" into one log line naming the file.
        if !path.is_file() {
            warn!("sound file missing: {}", path.display());
            continue;
        }
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        // No `SND_NOSTOP`: a new event *should* cut off the previous one,
        // so what the user hears is always the most recent thing that
        // happened rather than whatever got in first.
        let ok = unsafe {
            PlaySoundW(
                PCWSTR(wide.as_ptr()),
                None,
                SND_FILENAME | SND_ASYNC | SND_NODEFAULT,
            )
        };
        if !ok.as_bool() {
            warn!("could not play {}", path.display());
        }
        _held = wide;
    }
    // Nothing left to queue work: stop any sound still in flight so the
    // thread doesn't outlive its own audio.
    unsafe {
        let _ = PlaySoundW(PCWSTR::null(), None, SND_ASYNC);
    }
}

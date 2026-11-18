//! Always-on dictation: one press of its shortcut opens the microphone and
//! keeps it open, typing each stretch of speech as soon as a pause ends it,
//! until the shortcut is pressed again.
//!
//! The recording never stops between stretches. Its sink cuts the speech at
//! each pause (see `CaptureSink::with_pause_segments`) and hands it to a
//! worker thread, which transcribes, post-processes and types the stretches
//! in order while the microphone keeps listening.

use crate::actions::{complete_unless_cancelled, process_transcription_output};
use crate::audio_feedback::{play_feedback_sound, play_feedback_sound_blocking, SoundType};
use crate::audio_toolkit::constants::WHISPER_SAMPLE_RATE;
use crate::audio_toolkit::{SegmentCallback, VadPolicy};
use crate::managers::audio::{recording_notice_code, AudioRecordingManager};
use crate::managers::history::HistoryManager;
use crate::managers::model::ModelManager;
use crate::managers::transcription::TranscriptionManager;
use crate::notices::{self, Notice, NoticeSource};
use crate::settings::get_settings;
use crate::tray::{set_tray_state, TrayIconState};
use crate::utils;
use log::{debug, error, info, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

pub const BINDING_ID: &str = "continuous_dictation";

/// Overlay event: `true` while a stretch is transcribed, post-processed and
/// typed, `false` once it is done.
const BUSY_EVENT: &str = "continuous-dictation-busy";

/// How often the worker checks for a cancel while waiting for speech.
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// After the microphone fails mid-session, how long to keep trying to open
/// one again before ending the session, and how long between tries. A
/// microphone that drops out usually comes back, or the system switches to
/// another, within a second or two.
const REOPEN_TIMEOUT: Duration = Duration::from_secs(5);
const REOPEN_RETRY_INTERVAL: Duration = Duration::from_secs(1);

/// The listening session: present from start until stop or cancel.
struct Session {
    segments: Sender<Vec<f32>>,
    cancel_generation: u64,
}

static SESSION: Mutex<Option<Session>> = Mutex::new(None);

/// The worker is alive: listening, or still typing what was said before stop.
static WORKER_ALIVE: AtomicBool = AtomicBool::new(false);

/// Whether always-on dictation owns the microphone or is still typing, so
/// other dictation shortcuts stay out of its way.
pub fn is_active() -> bool {
    WORKER_ALIVE.load(Ordering::SeqCst)
}

/// Starts a session, or stops the one listening.
pub fn toggle(app: &AppHandle) {
    if SESSION.lock().unwrap().is_some() {
        stop(app);
    } else if is_active() {
        debug!("Always-on dictation is still typing the last session; ignoring press");
    } else {
        start(app);
    }
}

/// Stops a listening session, if any. What was said since the last pause is
/// still typed.
pub fn stop(app: &AppHandle) {
    let Some(session) = SESSION.lock().unwrap().take() else {
        return;
    };
    info!("Stopping always-on dictation");

    let rm = Arc::clone(&app.state::<Arc<AudioRecordingManager>>());
    set_tray_state(app, TrayIconState::Transcribing);
    utils::show_transcribing_overlay(app);
    rm.remove_mute();
    play_feedback_sound(app, SoundType::Stop);

    // Stopping waits out any extra recording buffer, so keep it off the
    // shortcut thread.
    std::thread::spawn(move || {
        if let Some(samples) = rm.stop_recording(BINDING_ID, session.cancel_generation) {
            let _ = session.segments.send(samples);
        }
        // Dropping the sender lets the worker finish once it has typed the rest.
    });
}

fn start(app: &AppHandle) {
    let tm = app.state::<Arc<TranscriptionManager>>();
    let rm = Arc::clone(&app.state::<Arc<AudioRecordingManager>>());

    if rm.is_recording() {
        warn!("Not starting always-on dictation: another recording is in progress");
        return;
    }

    tm.initiate_model_load(true);
    if !tm.is_model_loaded() {
        let selected_model = get_settings(app).selected_model;
        if let Err(e) = app
            .state::<Arc<ModelManager>>()
            .get_model_path(&selected_model)
        {
            warn!("Not starting always-on dictation: no model can transcribe it ({e})");
            return;
        }
    }

    let pause_ms = get_settings(app).continuous_dictation_pause_ms();
    let (segments, received) = mpsc::channel::<Vec<f32>>();

    let readiness = match start_listening(&rm, pause_ms, &segments) {
        Ok(readiness) => readiness,
        Err(e) => {
            error!("Failed to start always-on dictation: {e:#}");
            notices::raise(
                app,
                Notice::new(recording_notice_code(&e), NoticeSource::Dictation)
                    .with_detail(format!("{e:#}")),
            );
            return;
        }
    };
    info!("Always-on dictation started (pause {pause_ms}ms)");

    let cancel_generation = rm.cancel_generation();
    *SESSION.lock().unwrap() = Some(Session {
        segments,
        cancel_generation,
    });
    WORKER_ALIVE.store(true, Ordering::SeqCst);
    set_tray_state(app, TrayIconState::Recording);
    utils::show_recording_overlay(app);

    let generation = readiness.generation();
    let app_for_ready = app.clone();
    let rm_for_ready = Arc::clone(&rm);
    std::thread::spawn(move || {
        if !readiness.wait() || !rm_for_ready.is_recording_readiness_current(generation) {
            return;
        }
        utils::emit_recording_ready(&app_for_ready);
        play_feedback_sound_blocking(&app_for_ready, SoundType::Start);
        if rm_for_ready.is_recording_readiness_current(generation) {
            rm_for_ready.apply_mute();
        }
    });

    let app = app.clone();
    if let Err(e) = std::thread::Builder::new()
        .name("continuous-dictation".into())
        .spawn(move || run_worker(app, rm, received, cancel_generation, pause_ms))
    {
        error!("Failed to start the always-on dictation worker: {e}");
        WORKER_ALIVE.store(false, Ordering::SeqCst);
    }
}

/// Starts the segmented recording that feeds `segments`. Pauses are found by
/// VAD, so it runs even when the VAD setting is off.
fn start_listening(
    rm: &AudioRecordingManager,
    pause_ms: u64,
    segments: &Sender<Vec<f32>>,
) -> anyhow::Result<crate::managers::audio::RecordingReadiness> {
    let on_segment: SegmentCallback = {
        let segments = segments.clone();
        Arc::new(move |samples| {
            let _ = segments.send(samples);
        })
    };
    rm.try_start_segmented_recording(BINDING_ID, VadPolicy::Offline, pause_ms, on_segment)
}

/// Where a session whose microphone failed stands after a try to reopen it.
enum Reopen {
    /// Listening again, or the session ended meanwhile: nothing left to do.
    Done,
    /// No microphone opened; worth another try until the timeout.
    Failed(anyhow::Error),
}

/// The microphone under this session failed (unplugged, or it dropped out),
/// so it hears nothing more: hand on what was said before the failure.
fn hand_off_audio_before_failure(rm: &AudioRecordingManager, cancel_generation: u64) {
    // A stop or cancel that got here first collects the audio itself.
    let segments = match SESSION.lock().unwrap().as_ref() {
        Some(s) if s.cancel_generation == cancel_generation => s.segments.clone(),
        _ => return,
    };
    if let Some(samples) = rm.stop_recording(BINDING_ID, cancel_generation) {
        let _ = segments.send(samples);
    }
}

/// Listens again on whichever microphone opens now: the same one if it came
/// back, or the system default the OS switched to.
fn reopen_microphone(
    app: &AppHandle,
    rm: &Arc<AudioRecordingManager>,
    pause_ms: u64,
    cancel_generation: u64,
) -> Reopen {
    // Held while the microphone opens, so a stop pressed meanwhile waits for
    // this recording and stops it, instead of leaving it open with no session.
    let session = SESSION.lock().unwrap();
    let Some(session) = session
        .as_ref()
        .filter(|s| s.cancel_generation == cancel_generation)
    else {
        return Reopen::Done;
    };
    if rm.was_cancelled_since(cancel_generation) {
        return Reopen::Done;
    }
    let readiness = match start_listening(rm, pause_ms, &session.segments) {
        Ok(readiness) => readiness,
        Err(e) => return Reopen::Failed(e),
    };
    info!("Always-on dictation is listening again after a microphone failure");

    // Reopening restored any mute; put it back once audio flows again.
    let generation = readiness.generation();
    let app = app.clone();
    let rm = Arc::clone(rm);
    std::thread::spawn(move || {
        if !readiness.wait() || !rm.is_recording_readiness_current(generation) {
            return;
        }
        utils::emit_recording_ready(&app);
        rm.apply_mute();
    });
    Reopen::Done
}

/// Types each stretch of speech in order until the session ends and its
/// last stretch is typed, or it is cancelled.
fn run_worker(
    app: AppHandle,
    rm: Arc<AudioRecordingManager>,
    received: Receiver<Vec<f32>>,
    cancel_generation: u64,
    pause_ms: u64,
) {
    // Set while the microphone has failed and is being reopened: when the
    // failure was found, and when the last try was.
    let mut reopening: Option<(Instant, Option<Instant>)> = None;
    // Shown in place of the overlay when the session ends without a microphone.
    let mut final_notice: Option<Notice> = None;

    loop {
        if rm.was_cancelled_since(cancel_generation) {
            debug!("Always-on dictation cancelled");
            break;
        }
        match received.recv_timeout(CANCEL_POLL_INTERVAL) {
            Ok(samples) => {
                // Lets the overlay show that a stretch is being worked on
                // while the microphone keeps listening.
                let _ = app.emit_to("recording_overlay", BUSY_EVENT, true);
                type_segment(&app, samples, || rm.was_cancelled_since(cancel_generation));
                let _ = app.emit_to("recording_overlay", BUSY_EVENT, false);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        // A failed microphone sends nothing more, so without this the session
        // would sit there looking like it listens, typing nothing.
        if reopening.is_none() && rm.recording_microphone_failed() {
            warn!("The microphone failed during always-on dictation; reopening it");
            hand_off_audio_before_failure(&rm, cancel_generation);
            reopening = Some((Instant::now(), None));
        }
        if let Some((failed_at, last_try)) = reopening {
            if last_try.is_some_and(|t| t.elapsed() < REOPEN_RETRY_INTERVAL) {
                continue;
            }
            match reopen_microphone(&app, &rm, pause_ms, cancel_generation) {
                Reopen::Done => reopening = None,
                Reopen::Failed(e) if failed_at.elapsed() >= REOPEN_TIMEOUT => {
                    error!("Ending always-on dictation: no microphone opened ({e:#})");
                    final_notice = Some(
                        Notice::new(recording_notice_code(&e), NoticeSource::Dictation)
                            .with_detail(format!("{e:#}")),
                    );
                    // Dropping the session's sender ends the loop once what
                    // was said before the failure is typed.
                    let mut session = SESSION.lock().unwrap();
                    if session
                        .as_ref()
                        .is_some_and(|s| s.cancel_generation == cancel_generation)
                    {
                        *session = None;
                    }
                    reopening = None;
                }
                Reopen::Failed(e) => {
                    warn!("Couldn't reopen the microphone yet: {e:#}");
                    reopening = Some((failed_at, Some(Instant::now())));
                }
            }
        }
    }

    // A cancel leaves the session behind; clear it unless a newer one began.
    {
        let mut session = SESSION.lock().unwrap();
        if session
            .as_ref()
            .is_some_and(|s| s.cancel_generation == cancel_generation)
        {
            *session = None;
        }
    }
    let tm = app.state::<Arc<TranscriptionManager>>();
    tm.maybe_unload_immediately("always-on dictation");
    match final_notice {
        Some(notice) => notices::raise(&app, notice),
        None => utils::hide_recording_overlay(&app),
    }
    set_tray_state(&app, TrayIconState::Idle);
    WORKER_ALIVE.store(false, Ordering::SeqCst);
    info!("Always-on dictation finished");
}

/// Transcribes one stretch of speech, post-processes it when post-processing
/// is on, saves it to history and types it.
fn type_segment(app: &AppHandle, mut samples: Vec<f32>, is_cancelled: impl Fn() -> bool) {
    if samples.is_empty() {
        return;
    }
    // Models need about a second of audio; pad short stretches with silence
    // as a stopped recording would be.
    let rate = WHISPER_SAMPLE_RATE as usize;
    if samples.len() < rate {
        samples.resize(rate * 5 / 4, 0.0);
    }

    let tm = app.state::<Arc<TranscriptionManager>>();
    let hm = app.state::<Arc<HistoryManager>>();

    // Reloads the model if an immediate unload dropped it after the last one.
    tm.initiate_model_load(false);

    let file_name = format!("handy-{}.wav", chrono::Utc::now().timestamp_millis());
    let wav_path = hm.recordings_dir().join(&file_name);
    let wav_saved = match crate::audio_toolkit::save_wav_file(&wav_path, &samples) {
        Ok(()) => true,
        Err(e) => {
            error!("Failed to save always-on dictation WAV: {e}");
            false
        }
    };

    let transcription = match tm.transcribe_recording(samples) {
        Ok(transcription) => transcription,
        Err(e) => {
            if !is_cancelled() {
                error!("Always-on dictation transcription failed: {e}");
                let _ = app.emit("transcription-error", e.to_string());
            }
            return;
        }
    };
    if is_cancelled() {
        return;
    }

    // The post-processing toggle decides, read per stretch so flipping it
    // mid-session takes effect on the next one.
    let settings = get_settings(app);
    let post_process = settings.post_process_enabled;
    // A post-processing request has no overall timeout, so a hung LLM would
    // keep the worker alive forever and lock out every dictation shortcut.
    // Let a cancel abandon it.
    let Some(processed) = tauri::async_runtime::block_on(complete_unless_cancelled(
        process_transcription_output(app, &transcription.text, post_process),
        &is_cancelled,
    )) else {
        debug!("Always-on dictation cancelled during post-processing");
        return;
    };
    if is_cancelled() {
        return;
    }

    if wav_saved {
        if let Err(e) = hm.save_entry(
            file_name,
            transcription.text,
            post_process,
            processed.post_processed_text,
            processed.post_process_prompt,
            transcription.language,
        ) {
            error!("Failed to save always-on dictation history entry: {e}");
        }
    }

    let mut text = processed.final_text;
    if text.trim().is_empty() {
        return;
    }
    // Keep consecutive stretches from running together.
    if !settings.append_trailing_space && !text.ends_with(char::is_whitespace) {
        text.push(' ');
    }
    if let Err(e) = utils::paste(text, app.clone()) {
        error!("Failed to paste always-on dictation: {e}");
        let _ = app.emit("paste-error", ());
    }
}

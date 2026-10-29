//! Always-on dictation: one press of its shortcut opens the microphone and
//! keeps it open, typing each stretch of speech as soon as a pause ends it,
//! until the shortcut is pressed again.
//!
//! The recording never stops between stretches. Its sink cuts the speech at
//! each pause (see `CaptureSink::with_pause_segments`) and hands it to a
//! worker thread, which transcribes, post-processes and types the stretches
//! in order while the microphone keeps listening.

use crate::actions::process_transcription_output;
use crate::audio_feedback::{play_feedback_sound, play_feedback_sound_blocking, SoundType};
use crate::audio_toolkit::constants::WHISPER_SAMPLE_RATE;
use crate::audio_toolkit::{SegmentCallback, VadPolicy};
use crate::managers::audio::{recording_error_type, AudioRecordingManager};
use crate::managers::history::HistoryManager;
use crate::managers::model::ModelManager;
use crate::managers::transcription::TranscriptionManager;
use crate::settings::get_settings;
use crate::tray::{set_tray_state, TrayIconState};
use crate::utils;
use log::{debug, error, info, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

pub const BINDING_ID: &str = "continuous_dictation";

/// How often the worker checks for a cancel while waiting for speech.
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(100);

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

    tm.initiate_model_load();
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
    let on_segment: SegmentCallback = {
        let segments = segments.clone();
        Arc::new(move |samples| {
            let _ = segments.send(samples);
        })
    };

    // Pauses are found by VAD, so it runs even when the VAD setting is off.
    let readiness = match rm.try_start_segmented_recording(
        BINDING_ID,
        VadPolicy::Offline,
        pause_ms,
        on_segment,
    ) {
        Ok(readiness) => readiness,
        Err(e) => {
            error!("Failed to start always-on dictation: {e:#}");
            let _ = app.emit(
                "recording-error",
                serde_json::json!({
                    "error_type": recording_error_type(&e),
                    "detail": format!("{e:#}"),
                }),
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
        .spawn(move || run_worker(app, rm, received, cancel_generation))
    {
        error!("Failed to start the always-on dictation worker: {e}");
        WORKER_ALIVE.store(false, Ordering::SeqCst);
    }
}

/// Types each stretch of speech in order until the session ends and its
/// last stretch is typed, or it is cancelled.
fn run_worker(
    app: AppHandle,
    rm: Arc<AudioRecordingManager>,
    received: Receiver<Vec<f32>>,
    cancel_generation: u64,
) {
    loop {
        if rm.was_cancelled_since(cancel_generation) {
            debug!("Always-on dictation cancelled");
            break;
        }
        match received.recv_timeout(CANCEL_POLL_INTERVAL) {
            Ok(samples) => {
                type_segment(&app, samples, || rm.was_cancelled_since(cancel_generation))
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
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
    utils::hide_recording_overlay(&app);
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
    tm.initiate_model_load();

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
    let processed = tauri::async_runtime::block_on(process_transcription_output(
        app,
        &transcription.text,
        post_process,
    ));
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

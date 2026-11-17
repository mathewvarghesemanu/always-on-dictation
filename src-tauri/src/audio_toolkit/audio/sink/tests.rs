use super::{AudioFrameCallback, CaptureSink, CaptureVad, LevelCallback, VadPolicy};
use crate::audio_toolkit::vad::{VadFrame, VoiceActivityDetector};
use handy_recorder::{AudioChunk, Sink};
use std::sync::{mpsc, Arc, Mutex};

/// Keeps frames whose first sample is non-zero, and records how the sink
/// configured it.
#[derive(Default)]
struct ScriptedVad {
    frame_samples: usize,
    hangover: Arc<Mutex<Option<usize>>>,
    resets: Arc<Mutex<usize>>,
}

impl VoiceActivityDetector for ScriptedVad {
    fn push_frame<'a>(&'a mut self, frame: &'a [f32]) -> anyhow::Result<VadFrame<'a>> {
        Ok(if frame[0] != 0.0 {
            VadFrame::Speech(frame)
        } else {
            VadFrame::Noise
        })
    }

    fn frame_samples(&self) -> usize {
        self.frame_samples
    }

    fn set_hangover_frames(&mut self, frames: usize) {
        *self.hangover.lock().unwrap() = Some(frames);
    }

    fn reset(&mut self) {
        *self.resets.lock().unwrap() += 1;
    }
}

const FRAME: usize = 4;

struct Harness {
    sink: CaptureSink,
    ready: mpsc::Receiver<()>,
    fed: Arc<Mutex<Vec<f32>>>,
    hangover: Arc<Mutex<Option<usize>>>,
    resets: Arc<Mutex<usize>>,
}

fn harness(policy: VadPolicy) -> Harness {
    let vad = ScriptedVad {
        frame_samples: FRAME,
        ..Default::default()
    };
    let hangover = Arc::clone(&vad.hangover);
    let resets = Arc::clone(&vad.resets);
    let fed = Arc::new(Mutex::new(Vec::new()));
    let on_audio: AudioFrameCallback = {
        let fed = Arc::clone(&fed);
        Arc::new(move |frame: &[f32]| fed.lock().unwrap().extend_from_slice(frame))
    };
    let on_levels: LevelCallback = Arc::new(|_| {});
    let (ready_tx, ready) = mpsc::channel();
    let sink = CaptureSink::new(
        CaptureVad::new(Box::new(vad), 10, 20),
        policy,
        on_levels,
        on_audio,
        ready_tx,
    );
    Harness {
        sink,
        ready,
        fed,
        hangover,
        resets,
    }
}

fn chunk(samples: &[f32], valid_frames: usize) -> AudioChunk<'_> {
    AudioChunk {
        samples,
        sample_rate: 16_000,
        channels: 1,
        valid_frames,
    }
}

#[test]
fn readiness_fires_once_on_the_first_chunk() {
    let mut h = harness(VadPolicy::Disabled);
    assert!(h.ready.try_recv().is_err());

    h.sink.process_chunk(chunk(&[0.0; FRAME], FRAME));
    h.sink.process_chunk(chunk(&[0.0; FRAME], FRAME));

    assert!(h.ready.try_recv().is_ok());
    // The sender is gone after the first chunk, so a second signal can't come.
    assert_eq!(h.ready.try_recv(), Err(mpsc::TryRecvError::Disconnected));
}

#[test]
fn readiness_waiter_wakes_when_a_recording_ends_without_audio() {
    let h = harness(VadPolicy::Offline);
    let _ = h.sink.finish();
    assert!(h.ready.recv().is_err());
}

#[test]
fn disabled_vad_keeps_every_real_sample_and_drops_padding() {
    let mut h = harness(VadPolicy::Disabled);
    h.sink.process_chunk(chunk(&[1.0, 0.0, 2.0, 0.0], FRAME));
    // Final chunk: one real frame, three of zero padding.
    h.sink.process_chunk(chunk(&[3.0, 0.0, 0.0, 0.0], 1));

    let expected = vec![1.0, 0.0, 2.0, 0.0, 3.0];
    assert_eq!(*h.fed.lock().unwrap(), expected);
    assert_eq!(h.sink.finish().samples, expected);
    assert_eq!(
        *h.resets.lock().unwrap(),
        0,
        "a disabled policy leaves VAD alone"
    );
}

#[test]
fn a_recording_of_only_zeros_is_silent_input() {
    let mut h = harness(VadPolicy::Offline);
    h.sink.process_chunk(chunk(&[0.0; FRAME], FRAME));
    h.sink.process_chunk(chunk(&[0.0; FRAME], FRAME));
    assert!(h.sink.finish().input_silent);
}

#[test]
fn a_recording_stopped_before_any_audio_is_not_silent_input() {
    // A quick tap that ends before the device delivers its first chunk says
    // nothing about the microphone.
    let h = harness(VadPolicy::Offline);
    assert!(!h.sink.finish().input_silent);
}

#[test]
fn sound_counts_even_when_vad_drops_it() {
    let mut h = harness(VadPolicy::Offline);
    // Noise to the detector (its first sample is zero), but not silence.
    h.sink.process_chunk(chunk(&[0.0, 0.01, 0.0, 0.0], FRAME));
    let captured = h.sink.finish();
    assert!(captured.samples.is_empty());
    assert!(!captured.input_silent);
}

#[test]
fn vad_keeps_speech_frames_only() {
    let mut h = harness(VadPolicy::Offline);
    h.sink.process_chunk(chunk(&[1.0; FRAME], FRAME));
    h.sink.process_chunk(chunk(&[0.0; FRAME], FRAME));
    h.sink.process_chunk(chunk(&[2.0; FRAME], FRAME));

    let expected = [[1.0; FRAME], [2.0; FRAME]].concat();
    assert_eq!(*h.fed.lock().unwrap(), expected);
    assert_eq!(h.sink.finish().samples, expected);
}

#[test]
fn each_recording_resets_the_detector_with_its_policys_hangover() {
    let offline = harness(VadPolicy::Offline);
    assert_eq!(*offline.hangover.lock().unwrap(), Some(10));
    assert_eq!(*offline.resets.lock().unwrap(), 1);

    let streaming = harness(VadPolicy::Streaming);
    assert_eq!(*streaming.hangover.lock().unwrap(), Some(20));
    assert_eq!(*streaming.resets.lock().unwrap(), 1);
}

#[test]
fn finish_returns_the_detector_for_the_next_recording() {
    let h = harness(VadPolicy::Offline);
    let captured = h.sink.finish();
    assert_eq!(captured.vad.frame_samples(), FRAME);
    assert_eq!(captured.vad.hangover_for(VadPolicy::Streaming), 20);
}

/// A sink cutting at pauses of `pause_ms`, and the segments it hands off. The
/// harness frames are 4 samples, which round up to 1 ms, and the offline
/// hangover is 10 frames, so `pause_ms` of 13 waits for 3 noise frames.
fn segmenting(pause_ms: u64) -> (CaptureSink, Arc<Mutex<Vec<Vec<f32>>>>) {
    let segments = Arc::new(Mutex::new(Vec::new()));
    let sink = harness(VadPolicy::Offline)
        .sink
        .with_pause_segments(pause_ms, {
            let segments = Arc::clone(&segments);
            Arc::new(move |segment| segments.lock().unwrap().push(segment))
        });
    (sink, segments)
}

fn speech(sink: &mut CaptureSink, value: f32) {
    sink.process_chunk(chunk(&[value; FRAME], FRAME));
}

fn noise(sink: &mut CaptureSink, frames: usize) {
    for _ in 0..frames {
        sink.process_chunk(chunk(&[0.0; FRAME], FRAME));
    }
}

#[test]
fn a_pause_after_speech_hands_off_a_segment_and_keeps_recording() {
    let (mut sink, segments) = segmenting(13);
    speech(&mut sink, 1.0);
    noise(&mut sink, 2);
    assert!(
        segments.lock().unwrap().is_empty(),
        "the pause is too short"
    );

    noise(&mut sink, 1);
    assert_eq!(*segments.lock().unwrap(), vec![vec![1.0; FRAME]]);

    speech(&mut sink, 2.0);
    noise(&mut sink, 3);
    speech(&mut sink, 3.0);
    assert_eq!(
        *segments.lock().unwrap(),
        vec![vec![1.0; FRAME], vec![2.0; FRAME]]
    );
    // Speech after the last pause stays in the recording for its stop.
    assert_eq!(sink.finish().samples, vec![3.0; FRAME]);
}

#[test]
fn speech_inside_the_pause_window_restarts_the_count() {
    let (mut sink, segments) = segmenting(13);
    speech(&mut sink, 1.0);
    noise(&mut sink, 2);
    speech(&mut sink, 2.0);
    noise(&mut sink, 2);
    assert!(segments.lock().unwrap().is_empty());

    noise(&mut sink, 1);
    let mut expected = vec![1.0; FRAME];
    expected.extend([2.0; FRAME]);
    assert_eq!(*segments.lock().unwrap(), vec![expected]);
}

#[test]
fn silence_without_speech_never_hands_off_a_segment() {
    let (mut sink, segments) = segmenting(13);
    noise(&mut sink, 50);
    assert!(segments.lock().unwrap().is_empty());
}

#[test]
fn a_pause_shorter_than_the_hangover_still_waits_one_frame() {
    let (mut sink, segments) = segmenting(0);
    speech(&mut sink, 1.0);
    assert!(segments.lock().unwrap().is_empty());
    noise(&mut sink, 1);
    assert_eq!(segments.lock().unwrap().len(), 1);
}

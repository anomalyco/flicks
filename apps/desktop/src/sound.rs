//! The intro's sound (`assets/brand/intro.wav`) on the default output through `flicks-audio`, on
//! its own thread. CoreAudio keeps it silent when the system output is muted or at zero volume.

use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use flicks_audio::{Channel, Output};
use flicks_ui::Sound;

const WAV: &[u8] = include_bytes!("../../../assets/brand/intro.wav");
/// How long the thread waits for the intro to start before letting the device go.
const WAIT: Duration = Duration::from_secs(10);
/// Audio kept queued ahead of the device: enough to ride out a busy launch, short enough that a
/// skip fades at once.
const AHEAD: Duration = Duration::from_millis(60);
/// A skip fades the sound out over this long instead of cutting it.
const FADE: f64 = 0.03;

/// Opens the output on a thread while the window opens, and returns the hook the intro calls.
pub fn start() -> Option<Box<dyn Fn(Sound)>> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new().name("intro-sound".into()).spawn(move || play(&rx)).ok()?;
    Some(Box::new(move |cue| {
        let _ = tx.send((cue, Instant::now()));
    }))
}

fn play(rx: &Receiver<(Sound, Instant)>) {
    let Some((rate, samples)) = decode(WAV) else { return };
    let Ok(mut output) = Output::open() else { return };
    let started = match rx.recv_timeout(WAIT) {
        Ok((Sound::Play, at)) => at,
        _ => return,
    };
    let layout = [Channel::L, Channel::R];
    let frames = samples.len() / 2;
    let chunk = rate as usize / 100;
    // A device that opened late starts the sound where the animation already is.
    let mut at = ((started.elapsed().as_secs_f64() * f64::from(rate)) as usize).min(frames);
    while at < frames {
        match rx.try_recv() {
            Ok((Sound::Stop, _)) | Err(TryRecvError::Disconnected) => {
                let n = ((FADE * f64::from(rate)) as usize).min(frames - at);
                let tail: Vec<f32> = samples[at * 2..(at + n) * 2]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .enumerate()
                    .flat_map(|(i, f)| {
                        let g = 1.0 - i as f32 / n.max(1) as f32;
                        [f[0] * g, f[1] * g]
                    })
                    .collect();
                let _ = output.push(at as f64 / f64::from(rate), rate, &layout, &tail);
                break;
            }
            _ => {}
        }
        if output.queued() < AHEAD {
            let n = chunk.min(frames - at);
            if output.push(at as f64 / f64::from(rate), rate, &layout, &samples[at * 2..(at + n) * 2]).is_err() {
                return;
            }
            at += n;
        } else {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    // Let the device play out what's queued before closing it.
    std::thread::sleep(output.queued() + output.latency() + Duration::from_millis(50));
}

/// The rate and interleaved stereo samples of a 16-bit PCM stereo WAV.
fn decode(wav: &[u8]) -> Option<(u32, Vec<f32>)> {
    if wav.get(..4)? != b"RIFF" || wav.get(8..12)? != b"WAVE" {
        return None;
    }
    let (mut rate, mut data) = (None, None);
    let mut at = 12usize;
    while let Some(head) = wav.get(at..at.checked_add(8)?) {
        let len = u32::from_le_bytes(head.get(4..8)?.try_into().ok()?) as usize;
        let body = wav.get(at + 8..(at + 8).checked_add(len)?)?;
        match head.get(..4)? {
            b"fmt " => {
                let u16_at = |i: usize| Some(u16::from_le_bytes(body.get(i..i + 2)?.try_into().ok()?));
                // PCM, two channels, 16 bits.
                if u16_at(0)? != 1 || u16_at(2)? != 2 || u16_at(14)? != 16 {
                    return None;
                }
                rate = Some(u32::from_le_bytes(body.get(4..8)?.try_into().ok()?));
            }
            b"data" => data = Some(body),
            _ => {}
        }
        at = at.checked_add(8 + len + len % 2)?;
    }
    let samples = data?.as_chunks::<2>().0.iter().map(|s| f32::from(i16::from_le_bytes(*s)) / 32768.0).collect();
    Some((rate.filter(|r| (8_000..=192_000).contains(r))?, samples))
}

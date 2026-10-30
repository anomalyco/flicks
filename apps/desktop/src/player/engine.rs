//! The playback thread: it owns the [`Player`] (blocking reads and decode), takes the screen's
//! commands, hands frames to the compositor through [`SLOT`], and tells the screen what changed.
//! While paused it sleeps on its channel; while playing it wakes every few milliseconds, the
//! cadence the player releases frames at.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use flicks_container::matroska::Codec;
use flicks_player::{Cue, Player, Source, State, TrackKind};
use rune::AppHandle;

use super::video::SLOT;
use super::{Shown, Track};
use crate::Flicks;

pub enum Command {
    Toggle,
    /// Seek to seconds.
    Seek(f64),
    /// Seek by seconds from where it is.
    SeekBy(f64),
    Audio(usize),
    /// A track, or `None` for off.
    Subtitle(Option<usize>),
    /// An SRT or WebVTT file to show instead of the file's own subtitles.
    Sidecar(PathBuf),
    Stop,
}

/// How often the player is asked for a frame while it plays.
const TICK: Duration = Duration::from_millis(4);
/// Progress is saved this often while playing, and on pause, stop and big seeks.
const SAVE_EVERY: Duration = Duration::from_secs(180);
/// A seek at least this long saves progress.
const BIG_SEEK: f64 = 30.0;

pub fn start(app: AppHandle<Flicks>, source: Source, resume: f64, rx: Receiver<Command>) {
    let spawned = std::thread::Builder::new().name("flicks-playback".into()).spawn(move || {
        let reporter = app.clone();
        if let Err(error) = run(app, source, resume, rx) {
            reporter.update(move |s, _| s.player_failed(error));
        }
        SLOT.clear();
    });
    if let Err(error) = spawned {
        eprintln!("flicks: couldn't start playback: {error}");
    }
}

fn run(app: AppHandle<Flicks>, source: Source, resume: f64, rx: Receiver<Command>) -> Result<(), String> {
    let mut player = Player::open(source).map_err(|e| format!("Couldn't open this file: {e}"))?;
    if resume > 0.0 && resume < player.duration() {
        player.seek(resume).map_err(|e| e.to_string())?;
    }
    player.play();
    let tracks = tracks(&player);
    let duration = player.duration();
    app.update(move |s, _| s.player_opened(tracks, duration));

    let mut last = Shown::default();
    let mut sidecar = None;
    let mut saved = Instant::now();
    loop {
        let playing = matches!(player.state(), State::Playing | State::Buffering);
        let command = if playing {
            match rx.recv_timeout(TICK) {
                Ok(command) => Some(command),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => Some(Command::Stop),
            }
        } else {
            Some(rx.recv().unwrap_or(Command::Stop))
        };
        let mut save = false;
        match command {
            None => {}
            Some(Command::Stop) => {
                send_progress(&app, &player, false);
                return Ok(());
            }
            Some(Command::Toggle) if playing => {
                player.pause().map_err(|e| e.to_string())?;
                save = true;
            }
            Some(Command::Toggle) => player.play(),
            Some(Command::Seek(to)) => save = seek(&mut player, to)?,
            Some(Command::SeekBy(by)) => {
                let to = player.position() + by;
                save = seek(&mut player, to)?;
            }
            Some(Command::Audio(id)) => player.select_audio(Some(id)).map_err(|e| e.to_string())?,
            Some(Command::Subtitle(id)) => {
                // A file's own track replaces a loaded sidecar (the player drops it).
                sidecar = None;
                player.select_subtitle(id).map_err(|e| e.to_string())?;
            }
            Some(Command::Sidecar(path)) => match read_subtitles(&path) {
                Ok(track) => {
                    player.set_subtitles(Some(track));
                    sidecar = path.file_name().map(|n| n.to_string_lossy().into_owned());
                }
                Err(error) => app.update(move |s, _| s.player_notice(error)),
            },
        }
        let frame = player.next_frame_for_display(Instant::now()).map_err(|e| format!("Playback stopped: {e}"))?;
        let new_frame = frame.is_some();
        if let Some(frame) = frame {
            SLOT.show(frame);
        }
        if player.state() == State::Ended {
            SLOT.set_playing(false);
            send_progress(&app, &player, true);
            app.update(|s, _| s.close_player());
            // Wait for the screen's Stop, so this thread ends where it always does.
            while !matches!(rx.recv(), Ok(Command::Stop) | Err(_)) {}
            return Ok(());
        }
        if save || (playing && saved.elapsed() >= SAVE_EVERY) {
            send_progress(&app, &player, false);
            saved = Instant::now();
        }
        let shown = Shown {
            second: player.position() as u64,
            playing: matches!(player.state(), State::Playing | State::Buffering),
            audio: player.selected_audio(),
            subtitle: player.selected_subtitle(),
            sidecar: sidecar.clone(),
            cues: player.active_cues().into_iter().cloned().collect::<Vec<Cue>>(),
        };
        // Before the screen hears of it, so the frame it draws already animates.
        SLOT.set_playing(shown.playing);
        // A frame shown while paused (after a seek) needs a redraw: the compositor isn't animating.
        if shown != last || (new_frame && !shown.playing) {
            let position = player.position();
            last = shown.clone();
            app.update(move |s, _| s.player_shown(shown, position));
        }
    }
}

/// Seeks, and says whether it was far enough to save progress.
fn seek(player: &mut Player, to: f64) -> Result<bool, String> {
    let from = player.position();
    let to = to.clamp(0.0, (player.duration() - 1.0).max(0.0));
    player.seek(to).map_err(|e| e.to_string())?;
    Ok((to - from).abs() >= BIG_SEEK)
}

fn send_progress(app: &AppHandle<Flicks>, player: &Player, ended: bool) {
    let (position, duration) = (player.position(), player.duration());
    let played = ended || (duration > 0.0 && position >= duration * 0.95);
    let progress = flicks_client::Progress {
        position: if played { 0 } else { (position * 1000.0) as i64 },
        played,
        updated_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64),
    };
    app.update(move |s, _| s.save_progress(progress));
}

fn tracks(player: &Player) -> Vec<Track> {
    // The player shows SRT and WebVTT; other subtitle tracks aren't offered.
    let text = |codec: &Codec| matches!(codec, Codec::SubRip | Codec::WebVtt);
    player
        .tracks()
        .iter()
        .filter(|t| t.enabled && (t.kind == TrackKind::Audio || (t.kind == TrackKind::Subtitle && text(&t.codec))))
        .map(|t| {
            let label = [t.name.as_str(), language(&t.language)].into_iter().find(|s| !s.is_empty());
            Track {
                id: t.id,
                audio: t.kind == TrackKind::Audio,
                label: label.map_or_else(|| format!("Track {}", t.id + 1), str::to_string),
            }
        })
        .collect()
}

/// A language tag as shown, `und` (undetermined) as nothing.
fn language(tag: &str) -> &str {
    match tag {
        "und" | "" => "",
        "en" | "eng" => "English",
        "fr" | "fre" | "fra" => "French",
        "de" | "ger" | "deu" => "German",
        "es" | "spa" => "Spanish",
        "it" | "ita" => "Italian",
        "ja" | "jpn" => "Japanese",
        other => other,
    }
}

fn read_subtitles(path: &std::path::Path) -> Result<flicks_subtitle::Track, String> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let too_big = std::fs::metadata(path).map(|m| m.len() > flicks_subtitle::MAX_INPUT as u64);
    if too_big.map_err(|e| format!("Couldn't read {name}: {e}"))? {
        return Err(format!("{name} is too large for subtitles."));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("Couldn't read {name}: {e}"))?;
    let vtt = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("vtt"));
    let track = if vtt { flicks_subtitle::parse_webvtt(&bytes) } else { flicks_subtitle::parse_srt(&bytes) };
    let track = track.map_err(|e| format!("Couldn't read {name}: {e}"))?;
    if track.cues().is_empty() {
        return Err(format!("{name} has no subtitles in it."));
    }
    Ok(track)
}

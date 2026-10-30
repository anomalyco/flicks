//! The playback thread: it owns the [`Player`] (blocking reads and decode), takes the screen's
//! commands, hands frames to the compositor through [`SLOT`], and tells the screen what changed.
//! While paused it sleeps on its channel; while playing it wakes every few milliseconds, the
//! cadence the player releases frames at.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use flicks_container::matroska::Codec;
use flicks_player::{Cue, Player, Source, State, TrackKind};
use rune::AppHandle;

use flicks_client::merge::Key;
use flicks_ui::Flicks;
use flicks_ui::player::{Command, Shown, Track};

use super::video::SLOT;

/// The title's key, and the server and file its progress is saved for.
pub type Saving = (Key, String, i64);

/// How often the player is asked for a frame while it plays.
const TICK: Duration = Duration::from_millis(4);
/// Progress is saved this often while playing, and on pause, stop and big seeks.
const SAVE_EVERY: Duration = Duration::from_secs(180);
/// Buffering shows once it has lasted this long.
const BUFFERING_SHOWS: Duration = Duration::from_millis(300);
/// A seek at least this long saves progress.
const BIG_SEEK: f64 = 30.0;

pub fn start(app: AppHandle<Flicks>, id: u64, source: Source, resume: f64, saving: Saving, rx: Receiver<Command>) {
    let spawned = std::thread::Builder::new().name("flicks-playback".into()).spawn(move || {
        let reporter = app.clone();
        let result = run(&Play { app, id, saving, resume }, source, rx);
        SLOT.clear(id);
        if let Err(error) = result {
            reporter.update(move |s, _| s.player_failed(id, error));
        }
    });
    if let Err(error) = spawned {
        eprintln!("flicks: couldn't start playback: {error}");
    }
}

/// Where a play reports to.
struct Play {
    app: AppHandle<Flicks>,
    id: u64,
    saving: Saving,
    resume: f64,
}

fn run(play: &Play, source: Source, rx: Receiver<Command>) -> Result<(), String> {
    let mut player = Player::open(source).map_err(|e| format!("Couldn't open this file: {e}"))?;
    let result = playing(play, &mut player, rx);
    if result.is_err() {
        send_progress(play, &player, false);
        log_stats(&player);
    }
    result
}

fn playing(play: &Play, player: &mut Player, rx: Receiver<Command>) -> Result<(), String> {
    let (app, id) = (&play.app, play.id);
    let resume = play.resume;
    if resume > 0.0 && resume < player.duration() {
        player.seek(resume).map_err(|e| e.to_string())?;
    }
    player.play();
    let tracks = tracks(player);
    let duration = player.duration();
    app.update(move |s, _| s.player_opened(id, tracks, duration));

    let mut last = Shown::default();
    let mut sidecar = None;
    let mut saved = Instant::now();
    let mut buffering: Option<Instant> = None;
    let mut clock = Clock { origin: Instant::now(), base: None };
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
        for command in coalesce(command, &rx, player.position()) {
            match command {
                Command::Stop => {
                    send_progress(play, player, false);
                    log_stats(player);
                    return Ok(());
                }
                Command::Toggle if matches!(player.state(), State::Playing | State::Buffering) => {
                    player.pause().map_err(|e| e.to_string())?;
                    save = true;
                }
                Command::Toggle => player.play(),
                Command::Seek(to) => {
                    SLOT.flush(id);
                    clock.base = None;
                    save |= seek(player, to)?;
                }
                Command::SeekBy(_) => {} // Coalesced into one absolute seek below.
                Command::Audio(track) => {
                    SLOT.flush(id);
                    clock.base = None;
                    player.select_audio(Some(track)).map_err(|e| e.to_string())?;
                }
                Command::Subtitle(track) => {
                    SLOT.flush(id);
                    clock.base = None;
                    // A file's own track replaces a loaded sidecar (the player drops it).
                    sidecar = None;
                    player.select_subtitle(track).map_err(|e| e.to_string())?;
                }
                Command::Sidecar(path) => match read_subtitles(&path) {
                    Ok(track) => {
                        player.set_subtitles(Some(track));
                        sidecar = path.file_name().map(|n| n.to_string_lossy().into_owned());
                    }
                    Err(error) => app.update(move |s, _| s.player_notice(id, error)),
                },
            }
        }
        let now = Instant::now();
        let frame = player.next_frame_for_display(now).map_err(|e| format!("Playback stopped: {e}"))?;
        let new_frame = frame.is_some();
        let playing = player.state() == State::Playing;
        let base = clock.base(now, player.position(), playing);
        if let Some(frame) = frame {
            // When the audio clock passed the frame's time: the compositor shows it on that beat.
            let due = base.and_then(|base| {
                Duration::try_from_secs_f64(base + frame.pts()).ok().and_then(|d| clock.origin.checked_add(d))
            });
            SLOT.show(id, frame, due);
        }
        if player.state() == State::Ended {
            send_progress(play, player, true);
            log_stats(player);
            app.update(move |s, _| s.player_ended(id));
            return Ok(());
        }
        if save || (playing && saved.elapsed() >= SAVE_EVERY) {
            send_progress(play, player, false);
            saved = Instant::now();
        }
        buffering = (player.state() == State::Buffering).then(|| buffering.unwrap_or_else(Instant::now));
        let shown = Shown {
            second: player.position() as u64,
            playing: matches!(player.state(), State::Playing | State::Buffering),
            buffering: buffering.is_some_and(|since| since.elapsed() >= BUFFERING_SHOWS),
            audio: player.selected_audio(),
            subtitle: player.selected_subtitle(),
            sidecar: sidecar.clone(),
            cues: player.active_cues().into_iter().cloned().collect::<Vec<Cue>>(),
        };
        // Before the screen hears of it, so the frame it draws already animates.
        let animation_changed = SLOT.set_playing(id, playing);
        // A frame shown while paused (after a seek) needs a redraw: the compositor isn't animating.
        if shown != last || animation_changed || (new_frame && !playing) {
            let position = player.position();
            last = shown.clone();
            app.update(move |s, _| s.player_shown(id, shown, position));
        }
    }
}

/// Drain each wake before doing blocking work. The latest absolute seek replaces earlier
/// seeks; relative deltas after it add up. Stop discards everything, including track changes.
fn coalesce(first: Option<Command>, rx: &Receiver<Command>, position: f64) -> Vec<Command> {
    let mut commands = Vec::new();
    let mut seek = None;
    for command in first.into_iter().chain(rx.try_iter()) {
        match command {
            Command::Stop => return vec![Command::Stop],
            Command::Seek(to) => seek = Some(to),
            Command::SeekBy(by) => seek = Some(seek.unwrap_or(position) + by),
            other => commands.push(other),
        }
    }
    if let Some(to) = seek {
        commands.push(Command::Seek(to));
    }
    commands
}

fn log_stats(player: &Player) {
    let s = player.stats();
    let decode = if player.is_hardware() { "hardware" } else { "software" };
    eprintln!(
        "flicks: stopped at {:.1} s: {} decoded, {} displayed, {} dropped, {} hardware fallbacks ({decode} decode)",
        player.position(),
        s.decoded,
        s.displayed,
        s.dropped,
        s.hardware_fallbacks
    );
}

/// The audio clock as wall time: when media time zero was (`base`, seconds after `origin`). Each
/// reading of the clock wobbles by a few milliseconds; following it slowly gives frames due
/// exactly one frame apart, so they land on the same refresh pattern throughout.
struct Clock {
    origin: Instant,
    base: Option<f64>,
}

impl Clock {
    fn base(&mut self, now: Instant, position: f64, playing: bool) -> Option<f64> {
        if !playing {
            self.base = None;
            return None;
        }
        let raw = now.saturating_duration_since(self.origin).as_secs_f64() - position;
        let base = match self.base {
            // A seek, a stall or a resume moves the clock for real: follow at once.
            Some(base) if (raw - base).abs() < 0.02 => base + (raw - base) / 64.0,
            _ => raw,
        };
        self.base = Some(base);
        Some(base)
    }
}

/// Seeks, and says whether it was far enough to save progress.
fn seek(player: &mut Player, to: f64) -> Result<bool, String> {
    let from = player.position();
    let to = to.clamp(0.0, (player.duration() - 1.0).max(0.0));
    player.seek(to).map_err(|e| e.to_string())?;
    Ok((to - from).abs() >= BIG_SEEK)
}

fn send_progress(play: &Play, player: &Player, ended: bool) {
    let (position, duration) = (player.position(), player.duration());
    let played = ended || (duration > 0.0 && position >= duration * 0.95);
    if !played && position < 5.0 && play.resume <= 0.0 {
        return;
    }
    let progress = flicks_client::Progress {
        position: if played { 0 } else { (position * 1000.0) as i64 },
        played,
        updated_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64),
    };
    let (key, server, file) = play.saving.clone();
    play.app.update(move |s, _| s.player_progress(key, server, file, progress));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_arrows_and_scrubbing_do_one_seek_per_wake_and_stop_wins() {
        let (tx, rx) = std::sync::mpsc::channel();
        for _ in 0..40 {
            tx.send(Command::SeekBy(10.0)).unwrap();
        }
        assert_eq!(coalesce(Some(Command::SeekBy(-10.0)), &rx, 20.0), [Command::Seek(410.0)]);
        for command in [Command::Seek(100.0), Command::Seek(200.0), Command::SeekBy(-10.0)] {
            tx.send(command).unwrap();
        }
        assert_eq!(coalesce(None, &rx, 0.0), [Command::Seek(190.0)]);
        for command in [Command::Audio(2), Command::Seek(30.0), Command::Stop, Command::Seek(50.0)] {
            tx.send(command).unwrap();
        }
        assert_eq!(coalesce(Some(Command::SeekBy(10.0)), &rx, 0.0), [Command::Stop]);
    }
}

use crate::playback::{Player, PlayerEvent};
use anyhow::{Context, Result, ensure};
use std::{
    path::Path,
    thread,
    time::{Duration, Instant},
};

/// Exercise the real playback library without requiring a graphical desktop.
pub fn run(path: &Path) -> Result<()> {
    ensure!(path.is_file(), "Video does not exist: {}", path.display());
    let player = Player::new()?;
    let started = Instant::now();
    player.load(path.to_str().context("Video path must be valid UTF-8")?)?;
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut frames = 0;
    let mut first_frame_ms = None;
    let mut sought = false;
    let mut loaded = false;
    let mut frames_after_seek = 0;
    let mut seek_target = 0.;
    while Instant::now() < deadline {
        for event in player.poll_events() {
            match event {
                PlayerEvent::Error(message) => anyhow::bail!(message),
                PlayerEvent::FileLoaded => loaded = true,
                _ => {}
            }
        }
        if loaded && let Some(frame) = player.render(640, 360)? {
            ensure!(frame.pixels.len() == 640 * 360 * 4, "Unexpected frame size");
            first_frame_ms.get_or_insert(started.elapsed().as_millis());
            frames += 1;
            if sought {
                frames_after_seek += 1;
            }
        }
        let state = player.snapshot();
        if frames >= 3 && state.duration > 2. && !sought {
            player.set_property("pause", "yes")?;
            ensure!(player.snapshot().paused, "Pause did not take effect");
            player.set_property("volume", "45")?;
            ensure!(
                (player.snapshot().volume - 45.).abs() < 0.1,
                "Volume change failed"
            );
            player.set_property("speed", "1.25")?;
            ensure!(
                (player.snapshot().speed - 1.25).abs() < 0.01,
                "Speed change failed"
            );
            seek_target = state.duration * 0.5;
            player.command(&["seek", &seek_target.to_string(), "absolute+exact"])?;
            // Verify the jump while paused, so natural playback cannot make a
            // broken seek appear successful. Allow asynchronous decoder work.
            let seek_deadline = Instant::now() + Duration::from_secs(2);
            let mut seek_confirmed = false;
            while Instant::now() < seek_deadline {
                player.poll_events();
                let _ = player.render(640, 360)?;
                if (player.snapshot().position - seek_target).abs() < 0.15 {
                    seek_confirmed = true;
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            ensure!(
                seek_confirmed,
                "Paused seek did not reach the requested timestamp"
            );
            player.set_property("pause", "no")?;
            sought = true;
        }
        if sought && frames_after_seek >= 3 && state.position > seek_target + 0.03 {
            println!(
                "{}",
                serde_json::json!({"result":"passed","frames":frames,"first_frame_ms":first_frame_ms,"duration":state.duration,"position_after_seek":state.position,"presentation":"software-bgra","audio":std::env::var("FLICK_AUDIO").unwrap_or_else(|_| "system".into())})
            );
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    }
    anyhow::bail!(
        "Playback smoke test timed out after {} decoded frames",
        frames
    )
}

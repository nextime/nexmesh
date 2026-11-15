// nexmesh -- multi-agent orchestration for agent CLIs across machines.
// Copyright (C) 2026 Stefy Lanza <stefy@nexlab.net>
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the GNU General
// Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version. This program is
// distributed WITHOUT ANY WARRANTY; see the LICENSE file, or
// <https://www.gnu.org/licenses/>, for the full terms.

//! The camera on the machine you are typing at.
//!
//! "Look at this" is most of what a camera is for on a fleet: a screen, a
//! whiteboard, a machine with a light on it. The browser has had this since
//! the dashboard was a page on a phone; a terminal has a camera too, and
//! nothing but a missing twenty lines was stopping it.
//!
//! Nothing is bundled. `ffmpeg` is what every machine with a webcam already
//! has, and a machine without it simply has no camera key -- said out loud
//! rather than left as a key that does nothing.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// Which device, and what can drive it.
#[derive(Debug, Clone, Default)]
pub struct Camera {
    /// `/dev/video0`, or whatever the first one is.
    pub device: Option<String>,
    pub have_ffmpeg: bool,
}

impl Camera {
    pub fn ready(&self) -> bool {
        self.device.is_some() && self.have_ffmpeg
    }

    pub fn why_not(&self) -> &'static str {
        "no camera here: nothing at /dev/video*, or ffmpeg is not installed"
    }
}

fn on_path(bin: &str) -> bool {
    std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|d| !d.is_empty())
        .any(|d| std::fs::metadata(Path::new(d).join(bin)).map(|m| m.is_file()).unwrap_or(false))
}

/// Look once. A machine does not grow a webcam while a console is open.
pub fn detect() -> Camera {
    let mut devices: Vec<String> = std::fs::read_dir("/dev")
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| n.starts_with("video"))
                .collect()
        })
        .unwrap_or_default();
    devices.sort();
    Camera {
        device: devices.first().map(|n| format!("/dev/{n}")),
        have_ffmpeg: on_path("ffmpeg"),
    }
}

/// A still, as a jpeg. The two discarded frames are the webcam's own
/// exposure settling; without them half the photos are black.
pub async fn photo(cam: &Camera, dir: &Path) -> Result<PathBuf> {
    photo_from(cam, cam.device.as_deref().unwrap_or_default(), dir).await
}

/// The same, from a camera the operator picked.
pub async fn photo_from(cam: &Camera, device: &str, dir: &Path) -> Result<PathBuf> {
    let device = if device.is_empty() {
        cam.device.clone().context("no camera on this machine")?
    } else {
        device.to_string()
    };
    if !cam.have_ffmpeg {
        bail!("ffmpeg is not installed here, and it is what drives the camera");
    }
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join(format!("photo-{}.jpg", nexmesh_proto::now_ms()));
    run(&[
        "-hide_banner", "-loglevel", "error", "-y",
        "-f", "v4l2", "-i", &device,
        "-frames:v", "1", "-vf", "select=gte(n\\,2)",
        &path.to_string_lossy(),
    ])
    .await?;
    settled(&path)
}

/// A clip of `seconds`, as mp4 -- which is what a phone, a browser and every
/// model that reads video all accept.
pub async fn clip(cam: &Camera, dir: &Path, seconds: u32) -> Result<PathBuf> {
    clip_from(cam, cam.device.as_deref().unwrap_or_default(), dir, seconds).await
}

/// The same, from a camera the operator picked.
pub async fn clip_from(cam: &Camera, device: &str, dir: &Path, seconds: u32) -> Result<PathBuf> {
    let device = if device.is_empty() {
        cam.device.clone().context("no camera on this machine")?
    } else {
        device.to_string()
    };
    if !cam.have_ffmpeg {
        bail!("ffmpeg is not installed here, and it is what drives the camera");
    }
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join(format!("clip-{}.mp4", nexmesh_proto::now_ms()));
    let secs = seconds.clamp(1, 120).to_string();
    run(&[
        "-hide_banner", "-loglevel", "error", "-y",
        "-f", "v4l2", "-i", &device,
        "-t", &secs, "-pix_fmt", "yuv420p",
        &path.to_string_lossy(),
    ])
    .await?;
    settled(&path)
}

/// Every camera this machine has, in the order the kernel numbered them.
///
/// A laptop usually has one and a USB camera makes two; `/dev/video1` is
/// often the metadata node of `/dev/video0` rather than a second camera, so
/// cycling through them is offered rather than guessed at.
pub fn devices() -> Vec<String> {
    let mut found: Vec<String> = std::fs::read_dir("/dev")
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| n.starts_with("video"))
                .collect()
        })
        .unwrap_or_default();
    found.sort_by_key(|n| n.trim_start_matches("video").parse::<u32>().unwrap_or(9999));
    found.into_iter().map(|n| format!("/dev/{n}")).collect()
}

/// A live view, as raw rgb24 frames of a fixed size on a pipe.
///
/// One ffmpeg for the whole preview rather than one per frame: starting
/// ffmpeg costs half a second, which would make a two-frame-a-second
/// preview mostly startup. The caller reads `w * h * 3` bytes per frame and
/// draws them; dropping the child stops the camera.
pub struct Preview {
    pub child: tokio::process::Child,
    pub w: usize,
    pub h: usize,
}

pub fn preview(device: &str, w: usize, h: usize) -> Result<Preview> {
    // Even numbers: the terminal draws two pixel rows per character cell,
    // and ffmpeg's scaler wants even dimensions for most pixel formats.
    let w = (w.max(16) / 2) * 2;
    let h = (h.max(8) / 2) * 2;
    let child = tokio::process::Command::new("ffmpeg")
        .args([
            "-hide_banner", "-loglevel", "error", "-nostdin",
            "-f", "v4l2", "-i", device,
            "-vf", &format!("fps=6,scale={w}:{h}"),
            "-f", "rawvideo", "-pix_fmt", "rgb24", "-",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("starting the camera preview")?;
    Ok(Preview { child, w, h })
}

/// One frame as terminal rows: two pixel rows per character, the upper half
/// block coloured as the top pixel and its background as the bottom one.
///
/// Truecolour, because a webcam in sixteen colours is a rumour of a webcam.
pub fn frame_rows(rgb: &[u8], w: usize, h: usize) -> Vec<String> {
    let mut rows = Vec::with_capacity(h / 2);
    let at = |x: usize, y: usize| -> (u8, u8, u8) {
        let i = (y * w + x) * 3;
        match rgb.get(i + 2) {
            Some(_) => (rgb[i], rgb[i + 1], rgb[i + 2]),
            None => (0, 0, 0),
        }
    };
    for y in (0..h.saturating_sub(1)).step_by(2) {
        let mut line = String::with_capacity(w * 20);
        let (mut last_top, mut last_bot) = ((300u16, 0, 0), (300u16, 0, 0));
        for x in 0..w {
            let (tr, tg, tb) = at(x, y);
            let (br, bg, bb) = at(x, y + 1);
            if (last_top.0 as u8, last_top.1, last_top.2) != (tr, tg, tb) || last_top.0 > 255 {
                line.push_str(&format!("\x1b[38;2;{tr};{tg};{tb}m"));
                last_top = (tr as u16, tg, tb);
            }
            if (last_bot.0 as u8, last_bot.1, last_bot.2) != (br, bg, bb) || last_bot.0 > 255 {
                line.push_str(&format!("\x1b[48;2;{br};{bg};{bb}m"));
                last_bot = (br as u16, bg, bb);
            }
            line.push('\u{2580}');
        }
        line.push_str("\x1b[0m");
        rows.push(line);
    }
    rows
}

/// Start recording, and hand back the process so it can be stopped when the
/// operator decides it has gone on long enough. A fixed length is a guess
/// nobody can make before the thing has happened.
pub fn record(
    cam: &Camera,
    device: &str,
    dir: &Path,
) -> Result<(std::process::Child, PathBuf)> {
    if !cam.have_ffmpeg {
        bail!("ffmpeg is not installed here, and it is what drives the camera");
    }
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join(format!("clip-{}.mp4", nexmesh_proto::now_ms()));
    // `-t 600` is a backstop, not the length: a forgotten recording should
    // not fill a disk.
    let child = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner", "-loglevel", "error", "-nostdin", "-y",
            "-f", "v4l2", "-i", device,
            "-t", "600", "-pix_fmt", "yuv420p",
            &path.to_string_lossy(),
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("starting the recording")?;
    Ok((child, path))
}

/// Stop a recording properly: ffmpeg is asked to finish, so the container
/// gets its index and the file is playable. Killed outright it is not.
pub async fn finish(mut child: std::process::Child) {
    // `q` on stdin is ffmpeg's own "stop now and close the file".
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(b"q");
        let _ = stdin.flush();
    }
    for _ in 0..60 {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Stop a recording nobody wants, and take the half-written file with it.
pub async fn abandon(child: std::process::Child, path: &Path) {
    finish(child).await;
    let _ = std::fs::remove_file(path);
}

/// A file that exists and is not empty, or a complaint that says which.
fn settled(path: &Path) -> Result<PathBuf> {
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if size == 0 {
        bail!("the camera produced an empty file; is something else using it?");
    }
    Ok(path.to_path_buf())
}

async fn run(args: &[&str]) -> Result<()> {
    let out = tokio::process::Command::new("ffmpeg")
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .context("running ffmpeg")?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr);
        let why = why.lines().last().unwrap_or("ffmpeg failed").trim();
        bail!("{why}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_machine_with_no_camera_says_so_rather_than_offering_a_dead_key() {
        let none = Camera::default();
        assert!(!none.ready());
        assert!(none.why_not().contains("/dev/video"));
        let half = Camera { device: Some("/dev/video0".into()), have_ffmpeg: false };
        assert!(!half.ready(), "a device with nothing to drive it is not a camera");
        let both = Camera { device: Some("/dev/video0".into()), have_ffmpeg: true };
        assert!(both.ready());
    }

    #[test]
    fn a_frame_becomes_two_pixel_rows_per_character() {
        // Four pixels: red, green on the top row; blue, white on the bottom.
        let rgb: Vec<u8> = vec![
            255, 0, 0, 0, 255, 0, // top
            0, 0, 255, 255, 255, 255, // bottom
        ];
        let rows = frame_rows(&rgb, 2, 2);
        assert_eq!(rows.len(), 1, "two pixel rows are one character row");
        let line = &rows[0];
        assert_eq!(line.matches('\u{2580}').count(), 2, "one block per column");
        assert!(line.contains("38;2;255;0;0"), "the top pixel is the foreground: {line}");
        assert!(line.contains("48;2;0;0;255"), "the bottom pixel is the background: {line}");
        assert!(line.ends_with("\x1b[0m"), "it must not leak colour into the next row");
    }

    #[test]
    fn an_odd_height_does_not_read_past_the_frame() {
        // Three rows of pixels is one and a half character rows; the half is
        // dropped rather than read out of bounds.
        let rgb = vec![7u8; 2 * 3 * 3];
        let rows = frame_rows(&rgb, 2, 3);
        assert_eq!(rows.len(), 1);
    }

    #[tokio::test]
    async fn asking_a_machine_with_no_camera_is_an_error_not_a_hang() {
        let dir = std::env::temp_dir();
        let none = Camera::default();
        assert!(photo(&none, &dir).await.is_err());
        assert!(clip(&none, &dir, 3).await.is_err());
    }
}

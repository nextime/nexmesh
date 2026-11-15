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
    let device = cam.device.clone().context("no camera on this machine")?;
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
    let device = cam.device.clone().context("no camera on this machine")?;
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

    #[tokio::test]
    async fn asking_a_machine_with_no_camera_is_an_error_not_a_hang() {
        let dir = std::env::temp_dir();
        let none = Camera::default();
        assert!(photo(&none, &dir).await.is_err());
        assert!(clip(&none, &dir, 3).await.is_err());
    }
}

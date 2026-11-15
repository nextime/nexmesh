// nexmesh -- multi-agent orchestration for agent CLIs across machines.
// Copyright (C) 2026 Stefy Lanza <stefy@nexlab.net>
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free Software
// Foundation, either version 3 of the License, or (at your option) any later
// version. This program is distributed WITHOUT ANY WARRANTY; see the LICENSE
// file, or <https://www.gnu.org/licenses/>, for the full terms.

//! The camera, inside an attached terminal.
//!
//! A still taken blind is not using a camera: you cannot tell whether the
//! thing you meant to photograph is in shot, or whether the lens is covered.
//! So this takes over the screen, shows what the camera sees, and lets you
//! choose a photo or a clip -- then puts it in the agent's workspace, on the
//! machine that agent is running on.
//!
//! The view is half-block characters in truecolour: each character cell is
//! two pixels, the upper one its foreground and the lower one its
//! background. It is a webcam in a terminal, so it looks like one -- but it
//! is enough to see whether the screen you are pointing at is readable,
//! which is what it is for.
//!
//! Getting out: the agent's screen is not ours to redraw, so the overlay
//! ends by nudging the remote size by one row and back, which is what makes
//! every full-screen program repaint itself.

use anyhow::Result;
use tokio::io::AsyncWriteExt;

/// What the overlay decided.
#[derive(Debug)]
pub enum Took {
    /// A file, ready to send.
    File(std::path::PathBuf, &'static str),
    /// Nothing; the operator left.
    Nothing,
    /// Something went wrong, said in one line.
    Failed(String),
}

/// Draw the camera over the screen until the operator picks something.
///
/// `keys` is the attach's own keyboard; it is borrowed for as long as the
/// overlay is up, because nothing typed here belongs to the agent.
pub async fn run(
    keys: &mut crate::Keyboard,
    cam: &crate::camera::Camera,
    dir: &std::path::Path,
    cols: u16,
    rows: u16,
) -> Result<Took> {
    let devices = crate::camera::devices();
    if devices.is_empty() || !cam.have_ffmpeg {
        return Ok(Took::Failed(cam.why_not().to_string()));
    }
    let mut device = 0usize;
    // Fill the window, keeping 16:9 so the picture is not a caricature of
    // what the camera sees. Two character rows per pixel row, and three
    // rows left over for the legend and a margin.
    let rows_for_picture = (rows as usize).saturating_sub(3).max(4);
    let h_from_rows = rows_for_picture * 2;
    let w_from_rows = h_from_rows * 16 / 9;
    let w = w_from_rows.min((cols as usize).saturating_sub(1)).max(24);
    let h = (w * 9 / 16).min(h_from_rows).max(8);
    let mut out = tokio::io::stdout();
    let _ = out.write_all(b"\x1b[2J").await;

    let mut recording: Option<(std::process::Child, std::path::PathBuf, std::time::Instant)> = None;
    loop {
        let mut view = crate::camera::preview(&devices[device], w, h)?;
        let mut stdout_pipe = view.child.stdout.take();
        let frame_len = view.w * view.h * 3;
        let mut frame = vec![0u8; frame_len];
        let mut buf = [0u8; 4096];
        let mut switch = false;

        loop {
            tokio::select! {
                read = async {
                    match stdout_pipe.as_mut() {
                        Some(p) => {
                            use tokio::io::AsyncReadExt;
                            p.read_exact(&mut frame).await.map(|_| true)
                        }
                        None => Ok(false),
                    }
                } => {
                    match read {
                        Ok(true) => {
                            let rows_text = crate::camera::frame_rows(&frame, view.w, view.h);
                            // Centred: a window wider than 16:9 leaves a
                            // margin, and a picture hugging the left edge
                            // looks like a mistake rather than a frame.
                            let pad = ((cols as usize).saturating_sub(view.w)) / 2;
                            let gap = " ".repeat(pad);
                            let top = ((rows as usize).saturating_sub(rows_text.len() + 2)) / 2;
                            let mut paint = String::from("\x1b[H\x1b[2J");
                            for (i, line) in rows_text.iter().enumerate() {
                                paint.push_str(&format!("\x1b[{};1H{gap}{line}", top + i + 1));
                            }
                            let legend = match &recording {
                                Some((_, _, since)) => format!(
                                    " \x1b[41m\x1b[97m RECORDING {:>3}s \x1b[0m  v stop and send \u{b7} Esc cancel",
                                    since.elapsed().as_secs()
                                ),
                                None => format!(
                                    " \x1b[45m\x1b[97m CAMERA \x1b[0m  p photo \u{b7} v video \u{b7} \
                                     c camera ({}) \u{b7} Esc cancel",
                                    devices[device]
                                ),
                            };
                            paint.push_str(&format!(
                                "\x1b[{};1H\x1b[2K{legend}",
                                (top + rows_text.len() + 2).min(rows as usize)
                            ));
                            let _ = out.write_all(paint.as_bytes()).await;
                            let _ = out.flush().await;
                        }
                        // The camera stopped: say so rather than freezing on
                        // the last frame.
                        _ => {
                            return Ok(Took::Failed(format!(
                                "{} stopped answering; is something else using it?",
                                devices[device]
                            )));
                        }
                    }
                }
                n = keys.read(&mut buf) => {
                    let n = match n { Ok(0) | Err(_) => return Ok(Took::Nothing), Ok(n) => n };
                    for &b in &buf[..n] {
                        match b {
                            b'p' | b'P' if recording.is_none() => {
                                drop(stdout_pipe.take());
                                let _ = view.child.kill().await;
                                let shot = crate::camera::photo_from(cam, &devices[device], dir).await;
                                return Ok(match shot {
                                    Ok(path) => Took::File(path, "photo"),
                                    Err(e) => Took::Failed(format!("{e:#}")),
                                });
                            }
                            b'v' | b'V' => match recording.take() {
                                // Stop: ffmpeg is asked to finish so the
                                // container gets its index written.
                                Some((child, path, _)) => {
                                    drop(stdout_pipe.take());
                                    let _ = view.child.kill().await;
                                    crate::camera::finish(child).await;
                                    return Ok(Took::File(path, "clip"));
                                }
                                None => {
                                    match crate::camera::record(cam, &devices[device], dir) {
                                        Ok((child, path)) => {
                                            recording = Some((child, path, std::time::Instant::now()));
                                        }
                                        Err(e) => return Ok(Took::Failed(format!("{e:#}"))),
                                    }
                                }
                            },
                            b'c' | b'C' if recording.is_none() && devices.len() > 1 => {
                                device = (device + 1) % devices.len();
                                switch = true;
                            }
                            0x1b | 0x03 | b'q' => {
                                if let Some((child, path, _)) = recording.take() {
                                    crate::camera::abandon(child, &path).await;
                                }
                                drop(stdout_pipe.take());
                                let _ = view.child.kill().await;
                                return Ok(Took::Nothing);
                            }
                            _ => {}
                        }
                    }
                    if switch {
                        break;
                    }
                }
            }
        }
        drop(stdout_pipe.take());
        let _ = view.child.kill().await;
    }
}

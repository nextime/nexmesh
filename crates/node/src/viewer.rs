//! Opening a terminal onto an agent that has no terminal of its own.
//!
//! An agent driven over HTTP runs headless: best message delivery, exact token
//! accounting, no pty to hold. That would normally mean you can only watch its
//! output, never drive it. But opencode ships a client -- `opencode attach
//! <url>` -- so when somebody attaches, the node starts that client under a
//! pty and bridges it.
//!
//! The result is the combination neither stdio-driven CLI can offer: a
//! headless agent that is nonetheless a real, take-over-able TUI the moment
//! you want one, and only for as long as you want it.

use crate::ctx::NodeCtx;
use nexmesh_proto as proto;
use std::sync::Arc;

pub struct Viewer {
    pub input: tokio::sync::mpsc::Sender<Vec<u8>>,
    master: std::sync::Mutex<Box<dyn portable_pty::MasterPty + Send>>,
    pid: Option<i32>,
}

impl Viewer {
    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.master.lock().unwrap().resize(portable_pty::PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    /// Stop the client. The agent itself is untouched -- it is a server, and
    /// this was only a window onto it.
    pub fn close(&self) {
        if let Some(pid) = self.pid {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
    }
}

/// Start a terminal client for `attach_id` and stream it to the hub.
pub async fn open(
    ctx: Arc<NodeCtx>,
    attach_id: String,
    agent_id: String,
    bin: String,
    args: Vec<String>,
    cwd: String,
    cols: u16,
    rows: u16,
) -> anyhow::Result<()> {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};

    let pair = native_pty_system()
        .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
        .map_err(|e| anyhow::anyhow!("opening a pty for the viewer: {e}"))?;

    let mut cmd = CommandBuilder::new(&bin);
    for a in &args {
        cmd.arg(a);
    }
    cmd.cwd(&cwd);
    if std::env::var("TERM").is_err() {
        cmd.env("TERM", "xterm-256color");
    }
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| anyhow::anyhow!("starting {bin}: {e}"))?;
    drop(pair.slave);

    let pid = child.process_id().map(|p| p as i32);
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| anyhow::anyhow!("cloning the viewer pty: {e}"))?;
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|e| anyhow::anyhow!("taking the viewer pty writer: {e}"))?;

    let (in_tx, mut in_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
    std::thread::spawn(move || {
        use std::io::Write;
        while let Some(b) = in_rx.blocking_recv() {
            if writer.write_all(&b).is_err() || writer.flush().is_err() {
                break;
            }
        }
    });

    ctx.add_viewer(
        &attach_id,
        Viewer { input: in_tx, master: std::sync::Mutex::new(pair.master), pid },
    );

    // pty output -> this attach session only.
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut reader = reader;
        let mut buf = vec![0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    {
        let ctx = ctx.clone();
        let attach_id = attach_id.clone();
        tokio::spawn(async move {
            while let Some(chunk) = out_rx.recv().await {
                let _ = ctx
                    .send_hub(&proto::NodeToHub::AttachData {
                        attach_id: attach_id.clone(),
                        data: chunk,
                    })
                    .await;
            }
        });
    }
    {
        let ctx = ctx.clone();
        let attach_id = attach_id.clone();
        let agent_id = agent_id.clone();
        tokio::spawn(async move {
            let _ = tokio::task::spawn_blocking(move || child.wait()).await;
            tracing::info!(%agent_id, %attach_id, "terminal client exited");
            ctx.remove_viewer(&attach_id);
            let _ = ctx
                .send_hub(&proto::NodeToHub::AttachClosed {
                    attach_id,
                    reason: "the terminal client exited".into(),
                })
                .await;
        });
    }
    tracing::info!(%agent_id, %attach_id, "opened a terminal onto the agent's server");
    Ok(())
}

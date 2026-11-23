use anyhow::{Context, Result, ensure};
use std::{env, path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    time::timeout,
};

pub const TEXT: &str = "kdeconnect.clipboard";
pub const LIMIT: usize = 8192;

#[derive(Clone, Copy)]
pub enum Clipboard {
    Wayland,
    X11,
}

fn installed(binary: &str) -> bool {
    env::var_os("PATH")
        .is_some_and(|path| env::split_paths(&path).any(|dir| dir.join(binary).is_file()))
}

impl Clipboard {
    pub fn detect() -> Option<Self> {
        if env::var_os("WAYLAND_DISPLAY").is_some() && installed("wl-copy") && installed("wl-paste")
        {
            Some(Self::Wayland)
        } else if env::var_os("DISPLAY").is_some() && installed("xclip") {
            Some(Self::X11)
        } else {
            None
        }
    }

    pub async fn read(self) -> Result<String> {
        let mut command = match self {
            Self::Wayland => {
                let mut cmd = Command::new("wl-paste");
                cmd.args(["--no-newline", "--type", "text"]);
                cmd
            }
            Self::X11 => {
                let mut cmd = Command::new("xclip");
                cmd.args(["-selection", "clipboard", "-o"]);
                cmd
            }
        };
        timeout(Duration::from_secs(4), async {
            let mut child = command
                .kill_on_drop(true)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()?;
            let mut buffer = Vec::new();
            child
                .stdout
                .take()
                .context("clipboard output unavailable")?
                .take((LIMIT + 1) as u64)
                .read_to_end(&mut buffer)
                .await?;
            ensure!(buffer.len() <= LIMIT, "clipboard text exceeds 8192 bytes");
            ensure!(child.wait().await?.success(), "could not read clipboard");
            String::from_utf8(buffer).context("clipboard text is not valid UTF-8")
        })
        .await
        .context("clipboard read timed out")?
    }

    pub async fn write(self, text: &str) -> Result<()> {
        ensure!(text.len() <= LIMIT, "clipboard text exceeds 8192 bytes");
        let mut command = match self {
            Self::Wayland => {
                let mut cmd = Command::new("wl-copy");
                cmd.args(["--type", "text/plain;charset=utf-8"]);
                cmd
            }
            Self::X11 => {
                let mut cmd = Command::new("xclip");
                cmd.args(["-selection", "clipboard", "-i"]);
                cmd
            }
        };
        timeout(Duration::from_secs(4), async {
            let mut child = command
                .kill_on_drop(true)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            let mut input = child.stdin.take().context("clipboard input unavailable")?;
            input.write_all(text.as_bytes()).await?;
            drop(input);
            ensure!(child.wait().await?.success(), "could not write clipboard");
            Ok(())
        })
        .await
        .context("clipboard write timed out")?
    }
}

pub fn open(path: &Path) -> Result<()> {
    let mut child = Command::new("xdg-open")
        .arg(path)
        .stdin(Stdio::null())
        .spawn()?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(())
}

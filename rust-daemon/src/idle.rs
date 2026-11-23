use crate::state::State;
use anyhow::{Context, Result, ensure};
use dbus::{arg::OwnedFd, blocking::Connection};
use std::{process::Stdio, time::Duration};
use tokio::{io::AsyncReadExt, process::Command, sync::watch, time::timeout};

pub const PLUGIN: &str = "screensaver_inhibit";
const SCREENSAVER: &str = "org.freedesktop.ScreenSaver";
const OBJECT: &str = "/org/freedesktop/ScreenSaver";
const APP: &str = "Caelestia Connect";
const REASON: &str = "Device connected";
const DEADLINE: Duration = Duration::from_secs(2);

struct DesktopInhibitor {
    session: Connection,
    cookie: u32,
}

impl DesktopInhibitor {
    fn acquire() -> Result<Self> {
        let session = Connection::new_session()?;
        let (cookie,) = session
            .with_proxy(SCREENSAVER, OBJECT, DEADLINE)
            .method_call(SCREENSAVER, "Inhibit", (APP, REASON))?;
        Ok(Self { session, cookie })
    }

    fn release(self) {
        let _: Result<(), _> = self
            .session
            .with_proxy(SCREENSAVER, OBJECT, DEADLINE)
            .method_call(SCREENSAVER, "UnInhibit", (self.cookie,));
    }
}

enum Inhibitor {
    Shell { changed: bool },
    Desktop(DesktopInhibitor),
    Logind(OwnedFd),
}

async fn shell(operation: &str) -> Result<Vec<u8>> {
    timeout(DEADLINE, async {
        let mut qs = Command::new("qs")
            .args(["-c", "caelestia", "ipc", "call", "idleInhibitor", operation])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let mut output = Vec::new();
        let stdout = qs.stdout.take().context("no shell output")?;
        stdout.take(257).read_to_end(&mut output).await?;
        ensure!(
            output.len() <= 256 && qs.wait().await?.success(),
            "shell idle inhibitor failed"
        );
        Ok(output)
    })
    .await?
}

impl Inhibitor {
    async fn acquire() -> Result<Self> {
        if let Ok(output) = shell("isEnabled").await {
            match std::str::from_utf8(&output).unwrap_or_default().trim() {
                "true" => return Ok(Self::Shell { changed: false }),
                "false" if shell("enable").await.is_ok() => {
                    return Ok(Self::Shell { changed: true });
                }
                _ => (),
            }
        }
        tokio::task::spawn_blocking(|| {
            if let Ok(inhibitor) = DesktopInhibitor::acquire() {
                return Ok(Self::Desktop(inhibitor));
            }
            let system = Connection::new_system()?;
            let service = "org.freedesktop.login1";
            let (fd,) = system
                .with_proxy(service, "/org/freedesktop/login1", DEADLINE)
                .method_call(
                    "org.freedesktop.login1.Manager",
                    "Inhibit",
                    ("idle", APP, REASON, "block"),
                )?;
            Ok(Self::Logind(fd))
        })
        .await?
    }

    async fn release(self) {
        match self {
            Self::Shell { changed: true } => {
                let _ = shell("disable").await;
            }
            Self::Shell { changed: false } => (),
            Self::Desktop(inhibitor) => {
                let _ = tokio::task::spawn_blocking(move || inhibitor.release()).await;
            }
            Self::Logind(fd) => drop(fd),
        }
    }
}

pub async fn run(state: &State, mut shutdown: watch::Receiver<()>) {
    let mut changed = state.subscribe();
    let mut inhibitor = None::<Inhibitor>;
    let mut requested = false;
    loop {
        let now = state.wants_idle_inhibition();
        if now && !requested {
            match Inhibitor::acquire().await {
                Ok(lock) => inhibitor = Some(lock),
                Err(error) => eprintln!("Could not inhibit desktop idle: {error:#}"),
            }
        } else if !now && let Some(lock) = inhibitor.take() {
            lock.release().await;
        }
        requested = now;
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = changed.changed() => (),
        }
    }
    if let Some(lock) = inhibitor {
        lock.release().await;
    }
}

use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{env, net::Ipv4Addr, process::Stdio, time::Duration};
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
    time::timeout,
};

const QUERY: Duration = Duration::from_secs(4);

pub fn installed(program: &str) -> bool {
    env::var_os("PATH")
        .is_some_and(|path| env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

pub async fn output(
    program: &str,
    args: &[&str],
    limit: u64,
    deadline: Duration,
) -> Result<String> {
    timeout(deadline, async {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let mut bytes = Vec::new();
        child
            .stdout
            .take()
            .context("missing command output")?
            .take(limit + 1)
            .read_to_end(&mut bytes)
            .await?;
        ensure!((bytes.len() as u64) <= limit, "command output too large");
        ensure!(child.wait().await?.success(), "{program} failed");
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    })
    .await
    .context("Android command timed out")?
}

async fn adb(args: &[&str]) -> Result<String> {
    output("adb", args, 2 * 1024 * 1024, QUERY).await
}

fn valid_serial(serial: &str) -> bool {
    (1..=255).contains(&serial.len())
        && !serial.starts_with('-')
        && serial
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._:-[]%".contains(&c))
}

#[derive(Default, Serialize)]
pub struct Scan {
    pub serial: String,
    pub devices: Vec<String>,
    pub unauthorized: bool,
}

pub async fn devices() -> Result<Scan> {
    let rows = adb(&["devices"]).await?;
    let mut scan = Scan::default();
    for line in rows.lines() {
        let Some((serial, status)) = line.split_once('\t') else {
            continue;
        };
        match status.trim() {
            "device" if valid_serial(serial) && scan.devices.len() < 16 => {
                scan.devices.push(serial.to_owned())
            }
            "unauthorized" => scan.unauthorized = true,
            _ => (),
        }
    }
    Ok(scan)
}

fn endpoint(value: &str) -> Option<(Ipv4Addr, u16)> {
    let (host, port) = value.rsplit_once(':')?;
    let port = port.parse::<u16>().ok().filter(|port| *port != 0)?;
    Some((host.parse().ok()?, port))
}

pub async fn find(name: &str, address: Option<Ipv4Addr>) -> Result<Scan> {
    let mut scan = devices().await?;
    if let Some(address) = address {
        if !scan
            .devices
            .iter()
            .any(|serial| endpoint(serial).is_some_and(|(host, _)| host == address))
            && let Ok(services) = adb(&["mdns", "services"]).await
            && let Some(remote) = services.lines().find_map(|line| {
                let mut fields = line.split_whitespace();
                let _ = fields.next()?;
                let kind = fields.next()?;
                let remote = fields.next()?;
                (kind.trim_end_matches('.') == "_adb-tls-connect._tcp"
                    && endpoint(remote).is_some_and(|(host, _)| host == address))
                .then_some(remote)
            })
        {
            let _ = adb(&["connect", remote]).await;
            scan = devices().await?;
        }
        if let Some(serial) = scan
            .devices
            .iter()
            .find(|serial| endpoint(serial).is_some_and(|(host, _)| host == address))
        {
            scan.serial.clone_from(serial);
            return Ok(scan);
        }
    }

    let name = name.trim().to_lowercase();
    let mut exact_address = Vec::new();
    let mut matching_names = Vec::new();
    for serial in &scan.devices {
        let ip_query = async {
            if address.is_some() {
                adb(&["-s", serial, "shell", "ip", "-f", "inet", "addr"])
                    .await
                    .ok()
            } else {
                None
            }
        };
        let (ips, model, alias) = tokio::join!(
            ip_query,
            async { adb(&["-s", serial, "shell", "getprop", "ro.product.model"]).await },
            async {
                adb(&[
                    "-s",
                    serial,
                    "shell",
                    "settings",
                    "get",
                    "global",
                    "device_name",
                ])
                .await
            }
        );
        if let (Some(address), Some(ips)) = (address, ips.as_deref())
            && ips.split_whitespace().any(|token| {
                token
                    .split('/')
                    .next()
                    .and_then(|part| part.parse::<Ipv4Addr>().ok())
                    == Some(address)
            })
        {
            exact_address.push(serial);
        }
        if !name.is_empty()
            && [model, alias]
                .into_iter()
                .filter_map(Result::ok)
                .any(|candidate| {
                    let candidate = candidate.trim().to_lowercase();
                    !candidate.is_empty()
                        && candidate != "null"
                        && (candidate.contains(&name) || name.contains(&candidate))
                })
        {
            matching_names.push(serial);
        }
    }
    if let [serial] = exact_address.as_slice() {
        scan.serial.clone_from(serial);
    } else if exact_address.is_empty()
        && let [serial] = matching_names.as_slice()
    {
        scan.serial.clone_from(serial);
    }
    Ok(scan)
}

pub async fn power(serial: &str) -> Option<bool> {
    let output = adb(&["-s", serial, "shell", "dumpsys", "display"])
        .await
        .ok()?;
    for line in output
        .lines()
        .filter_map(|line| line.split_once("mState=").map(|(_, state)| state))
    {
        match line
            .split(|ch: char| ch.is_whitespace() || ch == '}')
            .next()
        {
            Some("ON") => return Some(true),
            Some("OFF" | "DOZE" | "DOZE_SUSPEND") => return Some(false),
            _ => (),
        }
    }
    None
}

pub async fn set_power(serial: &str, on: bool) -> Result<()> {
    let cmd = if on { "power-reset" } else { "power-off" };
    let key = if on { "224" } else { "223" };
    if adb(&["-s", serial, "shell", "cmd", "display", cmd, "0"])
        .await
        .is_ok()
        && power(serial).await != Some(!on)
    {
        return Ok(());
    }
    adb(&["-s", serial, "shell", "input", "keyevent", key]).await?;
    for _ in 0..3 {
        if power(serial).await != Some(!on) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    anyhow::bail!("Android did not change the physical screen power")
}

pub async fn sensor(serial: &str, facing: &str) -> u16 {
    let default = if facing == "front" { 270 } else { 90 };
    let Ok(output) = adb(&["-s", serial, "shell", "dumpsys", "media.camera"]).await else {
        return default;
    };
    let value = |section: &str, tag: &str| -> Option<String> {
        Some(
            section
                .split_once(tag)?
                .1
                .lines()
                .nth(1)?
                .trim()
                .trim_matches(['[', ']'])
                .trim()
                .to_owned(),
        )
    };
    for section in output.split("== Camera HAL device ") {
        let Some(lens) = value(section, "android.lens.facing") else {
            continue;
        };
        if !matches!(lens.as_str(), "FRONT" | "BACK" | "0" | "1") {
            continue;
        }
        if matches!(lens.as_str(), "FRONT" | "0") == (facing == "front")
            && let Some(angle) = value(section, "android.sensor.orientation")
                .and_then(|angle| angle.parse::<u16>().ok())
            && matches!(angle, 0 | 90 | 180 | 270)
        {
            return angle;
        }
    }
    default
}

pub async fn rotation(serial: &str) -> Option<u16> {
    let output = adb(&["-s", serial, "shell", "dumpsys", "window", "displays"])
        .await
        .ok()?;
    for (field, scale) in [("mProposedRotation=ROTATION_", 1), ("mRotation=", 90)] {
        if let Some((_, tail)) = output.split_once(field)
            && let Some(value) = tail
                .split(|ch: char| !ch.is_ascii_digit())
                .next()
                .and_then(|s| s.parse::<u16>().ok())
            && let Some(angle) = value.checked_mul(scale)
            && matches!(angle, 0 | 90 | 180 | 270)
        {
            return Some(angle);
        }
    }
    None
}

pub fn tool(program: &str, args: &[String]) -> Result<Child> {
    let mut command = if installed("setpriv") {
        let mut prefix = Command::new("setpriv");
        prefix.args(["--pdeathsig", "TERM", "--", program]);
        prefix
    } else {
        Command::new(program)
    };
    Ok(command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?)
}

pub fn scrcpy(serial: &str, args: &[String]) -> Result<Child> {
    let mut all = vec!["--serial".to_owned(), serial.to_owned()];
    all.extend_from_slice(args);
    tool("scrcpy", &all)
}

pub async fn started(child: &mut Child) -> Result<()> {
    if let Ok(status) = timeout(Duration::from_millis(2500), child.wait()).await {
        anyhow::bail!("scrcpy closed during startup ({})", status?);
    }
    Ok(())
}

pub async fn stop(mut child: Child) {
    if let Some(pid) = child.id() {
        // Child owns this PID until it is reaped.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        if timeout(Duration::from_secs(2), child.wait()).await.is_err() {
            let _ = child.kill().await;
        }
    }
}

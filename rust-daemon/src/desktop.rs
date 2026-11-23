use crate::wire::Packet;
use dbus::{arg::PropMap, blocking::Connection};
use serde_json::{Value, json};
use std::{fs, path::Path, time::Duration};
use tokio::sync::Semaphore;

pub const BATTERY: &str = "kdeconnect.battery";
pub const BATTERY_REQUEST: &str = "kdeconnect.battery.request";
pub const BATTERY_PLUGIN: &str = "battery";
pub const CONNECTIVITY: &str = "kdeconnect.connectivity_report";
pub const RING: &str = "kdeconnect.findmyphone.request";

static NOTIFICATIONS: Semaphore = Semaphore::const_new(4);

pub fn notify(title: String, text: String, icon: &'static str) {
    let Ok(permit) = NOTIFICATIONS.try_acquire() else {
        return;
    };
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        if let Ok(bus) = Connection::new_session() {
            let proxy = bus.with_proxy(
                "org.freedesktop.Notifications",
                "/org/freedesktop/Notifications",
                Duration::from_secs(3),
            );
            let body = text
                .chars()
                .take(500)
                .collect::<String>()
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;");
            let _: Result<(u32,), _> = proxy.method_call(
                "org.freedesktop.Notifications",
                "Notify",
                (
                    "Caelestia Connect",
                    0_u32,
                    icon,
                    title,
                    body,
                    Vec::<String>::new(),
                    PropMap::new(),
                    -1_i32,
                ),
            );
        }
    });
}

pub fn reported_status(packet: &Packet) -> Option<(&'static str, Value)> {
    match packet.kind.as_str() {
        BATTERY => {
            let charge = &packet.body["currentCharge"];
            let charge = charge
                .as_f64()
                .or_else(|| charge.as_str()?.trim().parse().ok())?;
            if charge != -1.0 && !(0.0..=100.0).contains(&charge) {
                return None;
            }
            let charging = &packet.body["isCharging"];
            let charging = charging.as_bool().unwrap_or_else(|| {
                if let Some(text) = charging.as_str() {
                    let text = text.trim();
                    text == "1"
                        || text.eq_ignore_ascii_case("true")
                        || text.eq_ignore_ascii_case("yes")
                } else {
                    charging.as_i64() == Some(1)
                }
            });
            Some((
                BATTERY_PLUGIN,
                json!({"charge": charge as i64, "charging": charging}),
            ))
        }
        CONNECTIVITY => {
            let reports = packet.body.get("signalStrengths")?.as_object()?;
            let subscriptions: Vec<_> = reports
                .iter()
                .filter_map(|(id, report)| {
                    let kind = report
                        .get("networkType")?
                        .as_str()?
                        .chars()
                        .take(32)
                        .collect::<String>();
                    let strength = report
                        .get("signalStrength")
                        .and_then(Value::as_i64)
                        .unwrap_or(-1)
                        .clamp(-1, 5);
                    Some(json!({"id": id, "networkType": kind, "signalStrength": strength}))
                })
                .take(32)
                .collect();
            let first = subscriptions.first()?;
            Some((
                "connectivity_report",
                json!({
                    "networkType": first["networkType"], "strength": first["signalStrength"], "subscriptions": subscriptions,
                }),
            ))
        }
        _ => None,
    }
}

pub fn local_battery() -> Option<Value> {
    let supplies = fs::read_dir(Path::new("/sys/class/power_supply")).ok()?;
    for supply in supplies.filter_map(Result::ok) {
        let path = supply.path();
        let supply_type = fs::read_to_string(path.join("type")).unwrap_or_default();
        if supply_type.trim() != "Battery" {
            continue;
        }
        let scope = fs::read_to_string(path.join("scope")).unwrap_or_default();
        if scope.trim() == "Device" {
            continue;
        }
        let charge = fs::read_to_string(path.join("capacity"))
            .ok()?
            .trim()
            .parse::<u8>()
            .ok()?
            .min(100);
        let status = fs::read_to_string(path.join("status")).unwrap_or_default();
        return Some(json!({"currentCharge": charge, "batteryQuantity": 1,
            "isCharging": matches!(status.trim(), "Charging" | "Full"),
            "thresholdEvent": u8::from(charge <= 15 && status.trim() == "Discharging"),
        }));
    }
    None
}

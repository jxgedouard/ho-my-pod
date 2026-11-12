//! Finds AirPlay 2 receivers on the local network (`_airplay._tcp`).

use anyhow::Result;
use log::{debug, info};
use mdns_sd::{ServiceDaemon, ServiceEvent};
use serde::Serialize;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::thread;

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    /// Stable id: the receiver's deviceid (MAC-like)
    pub id: String,
    pub name: String,
    pub model: String,
    /// "homepod" | "homepod-mini" | "appletv" | "mac" | "speaker"
    pub kind: String,
    /// Human readable model name
    pub model_name: String,
    pub ip: IpAddr,
    pub port: u16,
}

const SERVICE: &str = "_airplay._tcp.local.";

fn parse_features(s: &str) -> u64 {
    let mut parts = s.split(',');
    let parse = |p: Option<&str>| {
        p.map(|x| x.trim().trim_start_matches("0x").trim_start_matches("0X"))
            .and_then(|x| u64::from_str_radix(x, 16).ok())
            .unwrap_or(0)
    };
    let lo = parse(parts.next());
    let hi = parse(parts.next());
    (hi << 32) | lo
}

fn classify(model: &str) -> (&'static str, &'static str) {
    match model {
        m if m.starts_with("AudioAccessory5") => ("homepod-mini", "HomePod mini"),
        m if m.starts_with("AudioAccessory6") => ("homepod", "HomePod (2e génération)"),
        m if m.starts_with("AudioAccessory1") => ("homepod", "HomePod"),
        m if m.starts_with("AudioAccessory") => ("homepod", "HomePod"),
        m if m.starts_with("AppleTV") => ("appletv", "Apple TV"),
        m if m.starts_with("Mac") || m.starts_with("iMac") => ("mac", "Mac"),
        _ => ("speaker", "Enceinte AirPlay"),
    }
}

/// Browses in the background and calls `on_change` with the full list of
/// usable receivers whenever it changes. Keep the daemon alive.
pub fn start(on_change: impl Fn(Vec<Device>) + Send + Sync + 'static) -> Result<ServiceDaemon> {
    let mdns = ServiceDaemon::new()?;
    let rx = mdns.browse(SERVICE)?;
    let on_change = Arc::new(on_change);
    thread::Builder::new()
        .name("mdns-browse".into())
        .spawn(move || {
            // fullname -> device
            let mut devices: HashMap<String, Device> = HashMap::new();
            let publish = |devices: &HashMap<String, Device>| {
                let mut list: Vec<Device> = devices.values().cloned().collect();
                list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                list.dedup_by(|a, b| a.id == b.id);
                on_change(list);
            };
            while let Ok(event) = rx.recv() {
                match event {
                    ServiceEvent::ServiceResolved(info) => {
                        let fullname = info.get_fullname().to_string();
                        let features = info
                            .get_property_val_str("features")
                            .map(parse_features)
                            .unwrap_or(0);
                        let audio = features & (1 << 9) != 0;
                        let pairing = features & (1 << 48) != 0;
                        let name = fullname
                            .strip_suffix(&format!(".{SERVICE}"))
                            .unwrap_or(&fullname)
                            .replace("\\032", " ")
                            .to_string();
                        if !audio || !pairing {
                            debug!("mdns: skipping '{name}' (audio={audio}, airplay2={pairing})");
                            continue;
                        }
                        let ip = info
                            .get_addresses_v4()
                            .into_iter()
                            .next()
                            .map(|ip| IpAddr::V4(*ip))
                            .or_else(|| info.get_addresses().iter().next().copied());
                        let Some(ip) = ip else { continue };
                        let model = info.get_property_val_str("model").unwrap_or("").to_string();
                        let (kind, model_name) = classify(&model);
                        let id = info
                            .get_property_val_str("deviceid")
                            .unwrap_or(&fullname)
                            .to_string();
                        let dev = Device {
                            id,
                            name,
                            model,
                            kind: kind.into(),
                            model_name: model_name.into(),
                            ip,
                            port: info.get_port(),
                        };
                        if devices.get(&fullname) != Some(&dev) {
                            info!("mdns: found '{}' ({}) at {}:{}", dev.name, dev.model, dev.ip, dev.port);
                            devices.insert(fullname, dev);
                            publish(&devices);
                        }
                    }
                    ServiceEvent::ServiceRemoved(_, fullname) => {
                        if devices.remove(&fullname).is_some() {
                            info!("mdns: lost '{fullname}'");
                            publish(&devices);
                        }
                    }
                    _ => {}
                }
            }
        })?;
    Ok(mdns)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn features_of_homepod_mini() {
        let f = parse_features("0x4A7FCA00,0x3C356BD0");
        assert!(f & (1 << 9) != 0, "audio");
        assert!(f & (1 << 48) != 0, "coreutils pairing");
        assert!(f & (1 << 41) != 0, "ptp");
    }
}

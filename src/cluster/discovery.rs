//! Finds the other orch nodes on the tailnet from `tailscale status --json`

use std::collections::HashMap;
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::NodeId;

#[derive(Deserialize)]
struct Status {
    #[serde(rename = "Self")]
    me: Peer,
    #[serde(rename = "Peer", default)]
    peers: HashMap<String, Peer>,
}

#[derive(Deserialize)]
struct Peer {
    #[serde(rename = "DNSName", default)]
    dns_name: String,
    #[serde(rename = "TailscaleIPs", default)]
    ips: Vec<String>,
    #[serde(rename = "Online", default)]
    online: bool,
}

/// A machine on the tailnet, by node name and Tailscale IPv4
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    pub name: String,
    pub ip: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tailnet {
    pub me: Host,
    /// Online peers only, sorted by name
    pub peers: Vec<Host>,
}

/// The first label of the MagicDNS name. `HostName` can hold spaces, like "m1 max"
fn node_name(dns_name: &str) -> String {
    dns_name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn host(peer: &Peer) -> Option<Host> {
    let name = node_name(&peer.dns_name);
    let ip = peer.ips.iter().find(|ip| ip.contains('.'))?.clone();
    (!name.is_empty()).then_some(Host { name, ip })
}

pub fn parse(json: &str) -> anyhow::Result<Tailnet> {
    let status: Status = serde_json::from_str(json)?;
    let me = host(&status.me)
        .ok_or_else(|| anyhow::anyhow!("tailscale reports no name or IPv4 for this machine"))?;
    let mut peers: Vec<Host> = status
        .peers
        .values()
        .filter(|p| p.online)
        .filter_map(host)
        .collect();
    peers.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Tailnet { me, peers })
}

pub async fn status() -> anyhow::Result<Tailnet> {
    let out = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new("tailscale")
            .args(["status", "--json"])
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    anyhow::ensure!(
        out.status.success(),
        "tailscale status failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    parse(&String::from_utf8_lossy(&out.stdout))
}

/// A stable id from the node name, so a reinstall of orch keeps its Raft id
pub fn node_id(name: &str) -> NodeId {
    let digest = Sha256::digest(name.as_bytes());
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = r#"{
        "Self": {"HostName": "m4", "DNSName": "m4.tail0.ts.net.", "TailscaleIPs": ["100.1.1.4", "fd7a::4"], "Online": true},
        "Peer": {
            "a": {"HostName": "m1 max", "DNSName": "m1-max.tail0.ts.net.", "TailscaleIPs": ["100.1.1.1"], "Online": true},
            "b": {"HostName": "legion", "DNSName": "Legion.tail0.ts.net.", "TailscaleIPs": ["fd7a::2", "100.1.1.2"], "Online": true},
            "c": {"HostName": "localhost", "DNSName": "phone.tail0.ts.net.", "TailscaleIPs": ["100.1.1.9"], "Online": false},
            "d": {"HostName": "odd", "DNSName": "", "TailscaleIPs": ["100.1.1.8"], "Online": true}
        }
    }"#;

    #[test]
    fn parse_names_hosts_by_dns_label_and_keeps_online_ipv4_peers() {
        let t = parse(STATUS).unwrap();
        assert_eq!(
            t.me,
            Host {
                name: "m4".into(),
                ip: "100.1.1.4".into()
            }
        );
        let names: Vec<(&str, &str)> = t
            .peers
            .iter()
            .map(|h| (h.name.as_str(), h.ip.as_str()))
            .collect();
        assert_eq!(names, [("legion", "100.1.1.2"), ("m1-max", "100.1.1.1")]);
    }

    #[test]
    fn node_id_is_stable_and_distinct() {
        assert_eq!(node_id("legion"), node_id("legion"));
        assert_ne!(node_id("legion"), node_id("m1-max"));
        // Pinned, so a change of hash would show here before it renumbers a live cluster
        assert_eq!(
            node_id("m4"),
            u64::from_be_bytes(Sha256::digest(b"m4")[..8].try_into().unwrap())
        );
    }

    #[test]
    fn parse_rejects_a_machine_without_ipv4() {
        let json = r#"{"Self": {"DNSName": "m4.t.ts.net.", "TailscaleIPs": ["fd7a::4"]}}"#;
        assert!(parse(json).is_err());
    }
}

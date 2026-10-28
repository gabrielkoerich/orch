use crate::cluster::{discovery, local};

/// The cluster's members as this machine's service sees them
pub async fn run() -> anyhow::Result<()> {
    if !crate::cluster::enabled() {
        println!("orch runs on this machine only: the config has no cluster section");
        return Ok(());
    }

    let Some(endpoint) = local::daemon_endpoint().await else {
        anyhow::bail!("the orch service is not running");
    };
    let members = local::fetch_nodes(&endpoint).await?;
    let tailnet = discovery::status().await.ok();
    let name_of = |addr: &str| {
        let ip = addr.split(':').next().unwrap_or_default();
        tailnet
            .as_ref()
            .and_then(|t| {
                std::iter::once(&t.me)
                    .chain(&t.peers)
                    .find(|h| h.ip == ip)
                    .map(|h| (h.name.clone(), h.online))
            })
            .unwrap_or_else(|| (ip.to_string(), true))
    };
    println!("{:<20} {:<22} {:<8} STATE", "NODE", "ADDRESS", "ROLE");
    for m in members {
        let (name, online) = name_of(&m.addr);
        let role = if m.voter { "voter" } else { "learner" };
        let mut state = Vec::new();
        if m.leader {
            state.push("leader");
        }
        if m.me {
            state.push("this machine");
        }
        if !online {
            state.push("offline");
        }
        println!("{name:<20} {:<22} {role:<8} {}", m.addr, state.join(", "));
    }

    Ok(())
}

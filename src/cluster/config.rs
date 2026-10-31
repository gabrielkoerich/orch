//! The `cluster:` section of the global config

use super::discovery::Tailnet;
use super::{NodeId, discovery};
use std::collections::BTreeMap;

pub const DEFAULT_PORT: u16 = 7420;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterConfig {
    pub secret: String,
    pub port: u16,
    /// Node names, as the first label of each machine's MagicDNS name
    pub voters: Vec<String>,
    /// Votes but never leads, runs no agents
    pub witness: bool,
}

impl ClusterConfig {
    /// None without a `cluster.secret`, which keeps orch on one machine
    pub fn load() -> anyhow::Result<Option<Self>> {
        let Ok(secret) = crate::config::get("cluster.secret") else {
            return Ok(None);
        };
        if secret.is_empty() {
            return Ok(None);
        }

        let port = match crate::config::get("cluster.port") {
            Ok(p) => p.parse()?,
            Err(_) => DEFAULT_PORT,
        };
        let voters = crate::config::get_list("cluster.voters")?
            .into_iter()
            .map(|v| v.to_ascii_lowercase())
            .collect();
        let witness = crate::config::get("cluster.witness").is_ok_and(|w| w == "true");
        Ok(Some(Self {
            secret,
            port,
            voters,
            witness,
        }))
    }
}

/// How this machine takes part, worked out from the config and the tailnet
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub name: String,
    pub id: NodeId,
    pub addr: String,
    /// The voters to initialize with. Empty for a witness or a learner
    pub voters: BTreeMap<NodeId, String>,
    pub elect: bool,
    pub learner: bool,
}

pub fn plan(cfg: &ClusterConfig, tailnet: &Tailnet) -> anyhow::Result<Plan> {
    let name = tailnet.me.name.clone();
    let mut voters = BTreeMap::new();
    let mut missing = Vec::new();
    for voter in &cfg.voters {
        let found = tailnet.all_named(voter);
        anyhow::ensure!(
            found.len() <= 1,
            "cluster.voters names {voter}, and {} machines on the tailnet have that name",
            found.len()
        );
        match found.first() {
            Some(host) => {
                voters.insert(
                    discovery::node_id(voter),
                    format!("{}:{}", host.ip, cfg.port),
                );
            }
            None => missing.push(voter.as_str()),
        }
    }

    anyhow::ensure!(
        missing.is_empty(),
        "cluster.voters names machines the tailnet does not know: {}",
        missing.join(", ")
    );
    let is_voter = cfg.voters.contains(&name);
    anyhow::ensure!(
        !cfg.witness || is_voter,
        "{name} has cluster.witness set but is not in cluster.voters"
    );
    let elect = is_voter && !cfg.witness;
    Ok(Plan {
        id: discovery::node_id(&name),
        addr: format!("{}:{}", tailnet.me.ip, cfg.port),
        voters: if elect { voters } else { BTreeMap::new() },
        elect,
        learner: !is_voter,
        name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::discovery::Host;

    fn host(name: &str, ip: &str, online: bool) -> Host {
        Host {
            name: name.into(),
            ip: ip.into(),
            online,
        }
    }

    fn tailnet(me: &str) -> Tailnet {
        let all = [
            host("m1-max", "100.1.1.1", true),
            host("legion", "100.1.1.2", true),
            host("vpsbr", "100.1.1.3", false),
            host("m4", "100.1.1.4", true),
        ];
        Tailnet {
            me: all.iter().find(|h| h.name == me).unwrap().clone(),
            peers: all.into_iter().filter(|h| h.name != me).collect(),
        }
    }

    fn cfg(witness: bool) -> ClusterConfig {
        ClusterConfig {
            secret: "s".into(),
            port: 7420,
            voters: vec!["m1-max".into(), "legion".into(), "vpsbr".into()],
            witness,
        }
    }

    #[test]
    fn a_voter_initializes_with_every_voter_even_one_that_is_offline() {
        let p = plan(&cfg(false), &tailnet("m1-max")).unwrap();
        assert!(p.elect && !p.learner);
        assert_eq!(p.addr, "100.1.1.1:7420");
        assert_eq!(p.voters.len(), 3);
        assert_eq!(p.voters[&discovery::node_id("vpsbr")], "100.1.1.3:7420");
    }

    #[test]
    fn a_witness_votes_without_leading_or_initializing() {
        let p = plan(&cfg(true), &tailnet("vpsbr")).unwrap();
        assert!(!p.elect && !p.learner);
        assert!(p.voters.is_empty());
    }

    #[test]
    fn a_machine_outside_the_voters_is_a_learner() {
        let p = plan(&cfg(false), &tailnet("m4")).unwrap();
        assert!(p.learner && !p.elect);
        assert!(p.voters.is_empty());
    }

    #[test]
    fn plan_rejects_an_unknown_voter_and_a_witness_outside_the_voters() {
        let mut c = cfg(false);
        c.voters.push("ghost".into());
        let err = plan(&c, &tailnet("m1-max")).unwrap_err().to_string();
        assert!(err.contains("ghost"), "{err}");
        assert!(plan(&cfg(true), &tailnet("m4")).is_err());
    }

    #[test]
    fn plan_rejects_a_voter_name_two_machines_share() {
        let mut t = tailnet("m1-max");
        t.peers.push(host("legion", "100.1.1.9", true));
        let err = plan(&cfg(false), &t).unwrap_err().to_string();
        assert!(err.contains("2 machines"), "{err}");
    }
}

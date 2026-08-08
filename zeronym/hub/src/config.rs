//! Command-line and environment configuration.
//!
//! Small on purpose: an address to accept submissions on, and the full nodes to
//! broadcast through. Everything the batching design would add (flush cadence,
//! expiry margins) belongs to a layer that does not exist yet.

use std::net::SocketAddr;

use clap::Parser;

use crate::chain::NodeEndpoint;

/// The hub's whole configuration surface (env prefix `ZIH_`).
#[derive(Parser, Debug, Clone)]
#[command(
    name = "zero-indexer-hub",
    version,
    about = "Receives diverted migration transactions and broadcasts them to the Zcash network"
)]
pub struct Config {
    /// Address to accept shim submissions on. Plaintext HTTP: on Caution the
    /// platform terminates wallet-facing TLS and forwards here, exactly as for
    /// the shim, so the shim-to-hub hop is protected by the transport around it,
    /// not by this socket.
    #[arg(long, env = "ZIH_LISTEN", default_value = "0.0.0.0:8090")]
    pub listen: SocketAddr,

    /// A Zcash full node's JSON-RPC address, `host:port`. Repeatable, and at
    /// least one is required. Every submission is broadcast to EVERY node: a
    /// migration that only ever entered one node's mempool is one outage away
    /// from never being mined.
    #[arg(long = "node", env = "ZIH_NODES", value_delimiter = ',', required = true)]
    pub nodes: Vec<String>,

    /// HTTP basic-auth user applied to every node. zebrad with
    /// `enable_cookie_auth = false` ignores it; zcashd and a cookie-auth zebrad
    /// do not.
    #[arg(long, env = "ZIH_NODE_USER")]
    pub node_user: Option<String>,

    /// HTTP basic-auth password applied to every node.
    #[arg(long, env = "ZIH_NODE_PASSWORD")]
    pub node_password: Option<String>,
}

impl Config {
    /// The configured nodes as [`NodeEndpoint`]s, with the shared credentials
    /// applied to each.
    pub fn node_endpoints(&self) -> Vec<NodeEndpoint> {
        self.nodes
            .iter()
            .map(|addr| NodeEndpoint {
                addr: addr.clone(),
                user: self.node_user.clone(),
                password: self.node_password.clone(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_credentials_apply_to_every_node() {
        let cfg = Config::parse_from([
            "zero-indexer-hub",
            "--node",
            "1.2.3.4:8232",
            "--node",
            "5.6.7.8:8232",
            "--node-user",
            "u",
            "--node-password",
            "p",
        ]);
        let eps = cfg.node_endpoints();
        assert_eq!(eps.len(), 2);
        assert_eq!(eps[0].addr, "1.2.3.4:8232");
        assert_eq!(eps[1].addr, "5.6.7.8:8232");
        assert_eq!(eps[0].user.as_deref(), Some("u"));
        assert_eq!(eps[1].password.as_deref(), Some("p"));
    }

    #[test]
    fn a_comma_separated_node_list_splits() {
        let cfg = Config::parse_from(["zero-indexer-hub", "--node", "1.2.3.4:8232,5.6.7.8:8232"]);
        assert_eq!(cfg.node_endpoints().len(), 2);
    }
}

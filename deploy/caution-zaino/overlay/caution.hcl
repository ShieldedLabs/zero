# Caution deployment for zainod (Zcash CompactTxStreamer indexer), v0 bring-up.
# Per-environment knobs:
#   1. unit.args          which baked profile under /etc/zaino/ runs
#   2. unit.env           validator address override (non-secret, env is legal)
#   3. network.egress     must match the validator address and port
# Caution's reserved ports 49500-49600 are avoided (we use 8137 + validator RPC).

enclave "zaino" {
  build {
    containerfile = "Containerfile"
  }

  resources {
    cpu       = 4
    memory_mb = 8192
  }

  network {
    # Variant A (bring-up): raw TCP ingress, plaintext gRPC on 8137.
    ingress {
      cidr_ipv4   = "0.0.0.0/0"
      port        = 8137
      ip_protocol = "tcp"
    }

    # Variant B (preferred once confirmed that the Caddy/STEVE path carries
    # gRPC h2 and TLS terminates inside the enclave): replace the raw ingress
    # above with
    # http {
    #   domain = "zaino-demo.example.com"
    #   port   = 8137
    # }

    # Egress to the zebra validator JSON-RPC (the Shielded Labs zebra in k8s,
    # zero-zebra v21). Two hard constraints, verified in the zaino source:
    #   1. zainod rejects validator addresses that resolve to public IPs at
    #      config load; the endpoint must present as a private IP or a
    #      cluster-internal hostname.
    #   2. the RPC hop is plaintext HTTP (scheme is hardcoded; no TLS path
    #      exists for this client), so it must NEVER transit the public
    #      internet: same-VPC, peering, or a host-level tunnel only.
    # Tighten cidr_ipv4 to the validator /32 once the private address is known.
    # Testnet RPC 18232, mainnet 8232.
    egress {
      cidr_ipv4   = "10.0.0.0/8"
      port        = 18232
      ip_protocol = "tcp"
    }
  }

  unit "default" {
    command = "/zainod"
    args    = ["start", "--config", "/etc/zaino/testnet-ephemeral.toml"]
    env = {
      # Non-secret, so env is allowed (zainod refuses only password/cookie/token
      # style keys via env). Must resolve to a private IP, or be a hostname.
      ZAINO_VALIDATOR_SETTINGS__VALIDATOR_JSONRPC_LISTEN_ADDRESS = "zebra.internal:18232"
    }
  }

  # Bring-up only: console and ssh access, disables attestation verification.
  # Remove this block for the attested demo build.
  debug {
    enabled  = true
    ssh_keys = [
      # "ssh-ed25519 AAAA... mark"
    ]
  }
}

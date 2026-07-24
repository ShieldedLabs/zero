# Co-located enclave: zebrad + zainod in one EIF, one attestation.
# Sizing is for a fully-synced RAM-only zebra: ~276 GB state on tmpfs + a few
# GB process + zaino ~77 MB. This is a TEMPORARY footprint until Caution ships
# disk support, after which the same design runs on a much smaller instance.

enclave "z3-node" {
  build {
    # The combined image (zebrad + zainod + run-both.sh) is assembled on the
    # Caution side from their zebra packaging plus our static-musl zainod.
    containerfile = "Containerfile"
  }

  resources {
    cpu       = 32
    memory_mb = 327680 # 320 GiB; tune against the r6i.12xlarge 384 GiB parent
  }

  network {
    # Wallet-facing gRPC.
    ingress {
      cidr_ipv4   = "0.0.0.0/0"
      port        = 8137
      ip_protocol = "tcp"
    }

    # zebra to the Zcash P2P network (stay at tip). Peers are many, so /0.
    egress {
      cidr_ipv4   = "0.0.0.0/0"
      port        = 8233
      ip_protocol = "tcp"
    }
    # DNS seeders for peer discovery.
    egress {
      cidr_ipv4   = "0.0.0.0/0"
      port        = 53
      ip_protocol = "udp"
    }

    # No zaino-to-zebra rule: that link is localhost inside the enclave.
    # http { domain = "..." port = 8137 } once gRPC-through-Caddy is confirmed.
  }

  unit "default" {
    command = "/run-both.sh"
  }

  debug {
    enabled  = true
    ssh_keys = [
      # "ssh-ed25519 AAAA... mark"
    ]
  }
}

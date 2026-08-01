# zero-indexer-shim as a standalone attested Nitro enclave on Caution.
#
# This is the small sibling of deploy/caution-zaino/combined/caution.hcl. That
# one ships zebrad + zainod and needs 64 GB because the whole chain state lives
# in enclave RAM. This one ships a single 4.4 MB static binary that holds no
# state at all, so it is cheap enough to run continuously.
#
# WHAT IT PROVES, which is the entire reason to deploy it. The Zeronym trust
# model asks an auditor to rebuild the shim from source, reach the published
# hash, and check that hash against the one bound into the enclave attestation.
# Reproducibility alone proves only that source and binary agree; attestation
# alone proves only that SOME binary runs in a genuine enclave. Together they
# say: the code you read is the code that is running, and the operator cannot
# see the traffic. Neither half is worth much without the other, and until this
# deploy exists the attestation half has never been demonstrated for the shim.
#
# The binary under audit is the one recorded in deploy/EXPECTED_SHA256, built
# from the same commit this deploy repo was assembled from.

enclave "zeronym-shim" {
  build {
    # Assembled by assemble-caution.sh, which copies it out of the
    # `git archive HEAD` context rather than the working tree, so the recipe is
    # pinned to the same commit as the sources it compiles. See the README.
    containerfile = "Containerfile"
  }

  resources {
    # The shim is stateless: it parses one protobuf field out of each
    # SendTransaction body and forwards bytes. There is no chain state, no
    # database, no cache. 2 GB is almost entirely EnclaveOS; the process itself
    # sits in single-digit MB.
    cpu       = 2
    memory_mb = 2048
  }

  network {
    # Wallet-facing gRPC. Deliberately the same port the combined zebra+zaino
    # enclave serves on, so any client or script already pointed at a Zero
    # enclave works unchanged when pointed here: that interchangeability IS the
    # transparency claim the shim makes.
    ingress {
      cidr_ipv4   = "0.0.0.0/0"
      port        = 8137
      ip_protocol = "tcp"
    }

    # Egress to exactly one host: the backing indexer, and nothing else.
    #
    # This narrowness is a security property, not tidiness. The shim sees every
    # wallet's queries in the clear, which is precisely the exposure Zeronym
    # exists to contain, so the enclave should be incapable of shipping that
    # anywhere except the one indexer it proxies for. A /32 and a single port
    # make exfiltration to a third party a network-level impossibility rather
    # than a promise about the code.
    #
    # Note what is absent: no port 53. The backend is configured as a literal
    # IP (ZIS_BACKEND parses as a SocketAddr, so a hostname would not even
    # parse), so the enclave never resolves DNS and cannot be redirected by a
    # poisoned answer. Update the CIDR here whenever the backend IP changes.
    egress {
      cidr_ipv4   = "15.152.68.236/32"
      port        = 8137
      ip_protocol = "tcp"
    }
  }

  unit "default" {
    # The runtime stage's ENTRYPOINT is this binary at the image root. Stated
    # explicitly because a previous enclave failed to boot on exactly this: the
    # unit command was /run-both.sh while the file was installed at
    # /usr/local/bin/run-both.sh, and the enclave paniced with nothing to say
    # why. Keep it agreeing with the Containerfile.
    command = "/zero-indexer-shim"

    env = {
      # Bind all interfaces: the default is 127.0.0.1:9068, which inside an
      # enclave would accept only connections that cannot exist.
      ZIS_LISTEN = "0.0.0.0:8137"

      # The backing indexer. Currently the attested testnet zebra+zaino
      # enclave, which means both ends of this hop are attested workloads.
      #
      # Unlike ZEBRA_* and ZAINO_*, this prefix is safe to set here: ZIS_ is
      # the shim's own clap env namespace and these two names are the whole of
      # it. The rule that burned us before was passing ZEBRA_CONF to zebrad,
      # which parsed it as an unknown config field `conf` and exited, panicking
      # the enclave into a reboot loop. The lesson was not "never use env
      # vars", it was "never hand a binary a variable in its own config
      # namespace that it does not define". These two it defines.
      ZIS_BACKEND = "15.152.68.236:8137"

      # Default is `info`, which deliberately omits the per-request zis::proxy
      # line naming the method each wallet called. That line is exactly the
      # metadata this component exists to deny an operator, so it stays off in
      # a deployed enclave. Turn it on only for a local demo, never here.
      # RUST_LOG = "zis::proxy=debug,info"
    }
  }

  debug {
    # FALSE is the point of the exercise: debug mode disables attestation, and
    # an unattested shim proves nothing that running it on a laptop would not.
    #
    # If the enclave boots but never serves, flip this to true and redeploy;
    # that opens port 22 on the parent so the console can be read at
    # /var/log/nitro_enclaves/enclave-console.log. Every previous "boots but
    # never serves" bug here was diagnosed that way and none was diagnosable
    # without it, because the Caution CLI has no logs command. The key is
    # already listed so the flip is one boolean.
    enabled  = false
    ssh_keys = [
      "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINcRkPvdbZJ4PJMTT6rjAsmeWO84rp8TAfSURX4Scjq4 shieldedmark@Mac",
    ]
  }
}

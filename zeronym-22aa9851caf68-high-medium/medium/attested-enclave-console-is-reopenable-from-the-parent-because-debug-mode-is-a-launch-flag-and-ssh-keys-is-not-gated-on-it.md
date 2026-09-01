# Attestation does not close the enclave console — a parent-side launch flag does, and `debug.ssh_keys` (ungated on `debug.enabled`) hands that flag to the operator, so an "attested" shim's per-migration INFO log can be turned back on at will

**Severity**: Medium
**Validation Status**: Confirmed
**Location**: `audit-target/zeronym/shim/deploy/caution/caution.hcl.tmpl:186-201` and `audit-target/zeronym/hub/deploy/caution/caution.hcl.tmpl:143-157` (the `debug` block and the claim "SSH is closed under attestation", shim `:198` / hub `:154`); `audit-target/zeronym/shim/deploy/caution/OPERATORS.md:354-357` ("Reading state from an attested enclave: **there is no SSH**"); `audit-target/zeronym/hub/deploy/caution/OPERATORS.md:138` ("an attested enclave has **no SSH**"); `audit-target/zeronym/deploy.sh:227-229` (the same coupling used as the design rationale for the unauthenticated `/nym-address` endpoint); the renderer that produces the combination — `audit-target/zeronym/shim/deploy/caution/assemble-caution.sh:120-123` (the NOTE) and `:444-462` (the `ssh_keys` block, branched on `$SSH_KEYS` and never on `$DEBUG`), identical in `audit-target/zeronym/hub/deploy/caution/assemble-caution.sh:141-144` and `:359-376`; the log stream this reopens is `audit-target/zeronym/shim/src/intercept.rs:577-637` (`log_verdict`, `Class::Migration` arm at `:582-601`) and `audit-target/zeronym/hub/src/server.rs:265-275` (the per-admission line at `:269`), both at INFO under `audit-target/zeronym/shim/src/main.rs:21-30` / `audit-target/zeronym/hub/src/main.rs:22-26`. Platform mechanism (outside `audit-target/`, re-read during validation from the public Caution clone `codeberg.org/caution/platform` @ `1f8d8cb`): `terraform/modules/aws/nitro-enclave/user-data.sh:15` (nitro-cli installed on every parent), `:17-27` (keys written to `ec2-user`, guarded by `length(ssh_keys) > 0` only), `:53` (`aws s3 cp … /opt/nitro/enclave.eif`), `:127-154` + `:174-190` (console-capture service, `debug_mode`-gated), `:157-173` (`nitro-enclave.service`, `--debug-mode` appended iff `debug_mode`, at `:166`), `:194-212` (the `socat` 443 vsock relay, `ExecStart` at `:210`), `src/api/src/main.rs:2420-2421` (`debug_enabled` and `ssh_keys` read independently), `src/api/src/deployment.rs:2158-2164` (`debug_mode` and `ssh_ingress` are independent template fields; `ssh_ingress` emits `ingress 22 … 0.0.0.0/0`), `:2018-2032` (that ingress lands on `aws_security_group.enclave`, attached to the instance at `:2075`, which carries a public EIP at `:2118-2126`), `:1871-1879` (AL2023 AMI), `src/enclave-builder/src/manifest.rs:14-45` (`EnclaveManifest` — no `debug` field, so the key list is in no attestation)
**Found by agent:** Global, focus area G18 (log and telemetry discipline as one policy) — re-deriving, as coordinator open item 6x requires, who can actually read these logs in each deployment shape
**In scope of audit?** Yes — priority area 6 ("Log and telemetry discipline… Every log line… is an egress channel to the adversary") and priority area 7 (the attestation chain); the two `caution.hcl.tmpl` files, both `OPERATORS.md` and `deploy.sh` are in scope as security claims

## Description

The audit's bounding of every log-discipline finding rests on one premise, settled
as coordinator open item 7: with `debug { enabled = false }` the parent host has
**no console channel**, so `log_verdict`'s per-migration value balances are "not
live in a correctly attested deployment". **That premise is true as stated, and
this issue does not contradict it.** What it corrects is the inference drawn from
it — that *attestation* is what closes the channel.

It is not. Attestation and the console are two independent switches on the
**parent host**, and only one of them is cryptographic:

1. `debug.enabled` decides whether `nitro-cli run-enclave` is invoked with
   `--debug-mode`. That is a **string in a systemd unit file on the parent's
   local disk**, rendered once at instance boot
   (`user-data.sh:166`, `%{if debug_mode == "true"}--debug-mode%{endif}`).
2. `debug.ssh_keys` decides whether port 22 is open on the parent and the
   operator's key is installed on `ec2-user`. It is read **independently**:
   `src/api/src/main.rs:2420-2421` reads `enabled` and `ssh_keys` as two separate
   fields, `deployment.rs:2158-2164` renders `debug_mode` and `ssh_ingress` as two
   separate fields of the same terraform template, and `user-data.sh:17-27` is
   guarded by `%{ if length(ssh_keys) > 0 ~}`, never by `debug_mode`.

So `debug { enabled = false; ssh_keys = [ "ssh-ed25519 …" ] }` is a **fully
attested** enclave — `caution verify` passes, PCRs reproduce — whose parent host
the operator can log into. Coordinator item 6x established that much and graded it
on packet capture, enclave restarts and vsock access; it also recorded, following
the filed `hub-manifest-debug-block-…` issue, that **"Note what is not at risk:
enclave console output."** That is the part that is wrong, and it is wrong in the
direction that matters: the console is not protected by attestation, it is
protected by a flag on a machine the operator is now logged into.

**The bound this issue must preserve, because several confirmed issues depend on
it.** `nitro-cli console` **refuses** on an enclave that was launched without
`--debug-mode` (AWS: *"can be used only on an enclave that was launched with the
`--debug-mode` option"*), and the console-capture service that would write
`/var/log/nitro_enclaves/enclave-console.log` is template-gated on `debug_mode`
and pinned by the platform's own unit test
`test_enclave_console_capture_is_debug_only` (`src/api/src/deployment.rs:554-571`).
So the *deployed* attested-with-`ssh_keys` state does **not** leak logs, and **no
log finding may be escalated by this route directly.** The escalation costs one
extra, deliberate act — terminate the enclave and relaunch the same signed EIF
with `--debug-mode` — during which the enclave's attestation document carries
zeroed PCRs.

Everything that act needs is already on the parent, installed unconditionally by
the platform's own bootstrap:

- `aws-nitro-enclaves-cli` is installed on every parent, debug or not
  (`user-data.sh:15`).
- The signed enclave image is at `/opt/nitro/enclave.eif` (`user-data.sh:53`).
- The launch command is a plain systemd unit at
  `/etc/systemd/system/nitro-enclave.service` (`user-data.sh:157-173`).
- The AMI is Amazon Linux 2023 (`deployment.rs:1871-1879`) and the keys land on
  `ec2-user`, that AMI's default administrative account with passwordless `sudo`.
  The 535-line `user-data.sh` contains no `sudo`, `sshd`, `usermod` or
  `PermitRootLogin` statement of any kind, so the AMI defaults stand unmodified.

**The binary and its environment are unchanged**, so what appears on the reopened
console is exactly the shipped INFO stream — on the shim `log_verdict`'s
`orchard_vb` / `ironwood_vb` / `sapling_vb` / `expiry` / `inputs` / `outputs` /
`tx_len`, one line per diverted migration (`intercept.rs:582-601`), and on the hub
the per-admission line (`server.rs:269`).

**Five zeronym texts assert the opposite**, and they are the texts an operator or
a reviewer would consult:

- `shim/deploy/caution/caution.hcl.tmpl:198` and the identical
  `hub/deploy/caution/caution.hcl.tmpl:154`: *"without `--debug` this list renders
  empty and is moot (**SSH is closed under attestation**)."*
- `shim/deploy/caution/OPERATORS.md:354`: *"Reading state from an attested
  enclave: **there is no SSH.**"*
- `hub/deploy/caution/OPERATORS.md:138`: *"That endpoint exists because an
  attested enclave has **no SSH**…"*
- `shim/deploy/caution/caution.hcl.tmpl:189-193`, which presents debug mode as the
  thing that "opens port 22 on the parent so the console can be read", conflating
  the two independent switches into one.
- `deploy.sh:227-229`, where the coupling is used as a **design rationale**: *"the
  enclave console is only open with --debug, and --debug disables attestation, so
  an address that could only be read from the console could never belong to a hub
  that had also been proven."*

## Attack Scenario and Steps

The actor is adversary #1, the indexer operator the shim fronts. They are not
required to breach anything; they configure their own deployment.

1. The operator assembles an **attested** shim, passing `--ssh-key` without
   `--debug`. `shim/deploy/caution/assemble-caution.sh:449-462` renders the key
   into the `debug` block next to `enabled = false` — the branch is on
   `$SSH_KEYS` and never on `$DEBUG` — and `:120-123` prints a note reassuring
   them that *"SSH is closed when attestation is on, so the key is recorded in the
   HCL but unused until a --debug build"*. Nothing in the tooling refuses this
   combination; the filed `hub-manifest-debug-block-…` issue owns this half.
2. They deploy. `caution verify` passes. `/attestation` serves a COSE-signed
   document with reproducing PCRs and a `.manifest.run_command` showing the
   expected environment. The `debug` block is **not** part of the served manifest
   (`src/enclave-builder/src/manifest.rs:14-45` has no such field), so no
   attestation consumer sees the key list at all. Wallets connect; real migrations
   flow.
3. At a moment of their choosing — for one batch, one hour, or one target's
   session — the operator SSHes to the parent's public address and runs:

   ```
   sudo nitro-cli describe-enclaves            # note EnclaveID, memory, cpus
   sudo nitro-cli terminate-enclave --all
   sudo nitro-cli run-enclave --eif-path /opt/nitro/enclave.eif \
        --memory <M> --cpu-count <C> --enclave-cid 16 --debug-mode
   sudo nitro-cli console --enclave-id <new id>
   ```

   (Editing `--debug-mode` into `/etc/systemd/system/nitro-enclave.service` and
   restarting the unit is equivalent.) The parent's `socat` relays
   (`user-data.sh:194-212`, `ExecStart=/usr/bin/socat TCP-LISTEN:$port,reuseaddr,fork VSOCK-CONNECT:16:$port`
   at `:210`) fork and re-dial CID 16 per connection, so the shim keeps serving on
   the same hostname. The in-enclave Caddy re-runs ACME and republishes its
   `certfp`.
4. Every subsequent `SendTransaction` prints one INFO line naming that
   transaction's Orchard, Ironwood and Sapling value balances, its expiry, its
   transparent input/output counts and its exact length — beside the TCP
   connection the operator's own parent is relaying. This is the confirmed High
   `log-verdict-logs-migration-value-balance-at-info.md`, delivered in a
   deployment whose manifest says `enabled = false`.
5. They terminate and relaunch without `--debug-mode` to restore reproducing
   PCRs.

**Attack Requirements and Assumptions:**

- **Access needed:** the operator must have put an SSH key in their own manifest.
  That is a supported, documented option of the project's own assembler, and the
  manifest comment tells them it is inert. `deploy.sh` does not produce the
  combination on its own (`deploy.sh:128-135` passes `--ssh-key` only together
  with `--debug`), so the reachable route is running `assemble-caution.sh`
  directly — which both `deploy/README.md` files document as the primary
  interface — or hand-editing the assembled `caution.hcl`, which nothing
  re-validates (`shim-assemble-never-verifies-the-manifest-it-rendered.md`).
- **Verified during validation, not assumed:** the security group rule the key
  list produces is `from_port = 22, to_port = 22, cidr_blocks = ["0.0.0.0/0"]`
  (`deployment.rs:2161-2164`), attached to the enclave instance's own security
  group (`:2018-2032`), and the instance carries an Elastic IP
  (`:2118-2126`). So the parent's sshd is exposed to the **whole internet**, not
  to the operator's address — the attacker set is "whoever holds that private
  key", which includes anyone who steals or compels it, and the exposure is a
  standing one for the life of the deployment.
- **Stated assumption:** that `ec2-user` on the AL2023 parent reaches root. This
  is the AWS default for that AMI and the platform's `user-data.sh` does not
  disable it (checked: the file contains no sudo/sshd hardening). The same
  assumption is already load-bearing for the other filed parent-host capabilities
  (tcpdump on the 443 relay, enclave restart).
- **What it costs the operator:** during the debug window `/attestation` returns
  zeroed PCRs, so anyone who fetches and checks it *in that window* sees the
  enclave is unattested; and each relaunch spends one of the domain's five weekly
  Let's Encrypt issuances and shows a new certificate in CT
  (`acme-nocache-issuance-budget-…`). Neither is observed by anything zeronym
  ships: the confirmed `attested-tls-binding-is-verified-once-by-hand-if-ever-…`
  establishes that no check is scheduled anywhere, and the runbook's own advice is
  to watch CT for *certificates you cannot account for* — an operator restarting
  their own enclave accounts for it.
- **Why this is not "requires the system to already be compromised":** nothing is
  compromised. The operator exercises a configuration option on infrastructure
  they are entitled to configure, and the security claim being broken is the one
  made to *users and auditors* — that an attested enclave's logs cannot reach the
  operator. In the deployment `deploy.sh` actually performs (fully managed, in
  **Caution's** AWS account — `shim/deploy/caution/OPERATORS.md:64`, coordinator
  item 6x) the operator does **not** hold the parent host by default, so
  `debug.ssh_keys` is how they *obtain* the position, not a restatement of one
  they already have.

## Impact on Users

- It removes the mitigation that bounds the audit's log-discipline findings. The
  correct statement is no longer "the amount leak is live only in the `DEBUG`
  deployment"; it is "the amount leak is live in the `DEBUG` deployment, and
  **re-armable on demand** in an attested deployment that carries an SSH key".
- For a user whose migration is logged during such a window, the harm is the one
  the confirmed High already describes: the operator holds their TCP source
  address at the parent's relay socket and, on the same host, a line giving the
  exact zatoshi value balances, expiry and length of the transaction that wallet
  just diverted — which `README.md:33` says the operator does not learn.
- The window is chosen by the adversary and is invisible to the user, because a
  wallet has no way to see PCRs and the shim's TLS identity is unchanged from its
  point of view.
- Second-order, and worth stating because it is a standing risk rather than an
  operator choice: the manifest option opens **22/tcp to `0.0.0.0/0`** on a
  machine in Caution's account for the lifetime of the deployment.

## Technical Details / Code Analysis

The two switches, in the platform's own template. From
`terraform/modules/aws/nitro-enclave/user-data.sh` (Caution SEZC, AGPL-3.0;
quoted for analysis):

```bash
# :15 — unconditional, on every parent
dnf install -y aws-nitro-enclaves-cli aws-nitro-enclaves-cli-devel docker socat dnsmasq iptables iproute

# :17-27 — guarded by ssh_keys ONLY
%{ if length(ssh_keys) > 0 ~}
mkdir -p /home/ec2-user/.ssh
%{ for key in ssh_keys ~}
echo "${key}" >> /home/ec2-user/.ssh/authorized_keys
%{ endfor ~}
%{ endif ~}

# :53 — unconditional
aws s3 cp "${eif_s3_path}" /opt/nitro/enclave.eif

# :166 — the entire console gate, in a unit file on the parent's disk
ExecStart=/bin/bash -c 'nitro-cli run-enclave --eif-path /opt/nitro/enclave.eif \
  --memory ${memory_mb} --cpu-count ${cpu_count} --enclave-cid 16 \
  %{if debug_mode == "true"}--debug-mode%{endif} && tail -f /dev/null'
```

The console-capture service that writes
`/var/log/nitro_enclaves/enclave-console.log` is `debug_mode`-gated (`:127-154`,
`:174-190`) and the platform even has a unit test pinning that —
`src/api/src/deployment.rs:554-571`, `test_enclave_console_capture_is_debug_only`,
which asserts every reference to `capture-enclave-console.sh`,
`nitro-enclave-console.service`, `/var/log/nitro_enclaves/enclave-console.log` and
`nitro-cli console` sits inside a `%{ if debug_mode == "true" ~}` block. **That
test is exactly the evidence for this finding**: the platform guarantees the
*file* is not written without debug mode; it guarantees nothing about a shell on
the parent invoking `nitro-cli console` itself, because the gate it enforces is a
template gate, not a privilege boundary.

The independence of the two fields, from `src/api/src/main.rs:2420-2421`:

```rust
    let debug_enabled = ec_debug.and_then(|d| d.enabled).unwrap_or(false);
    let ssh_keys = ec_debug.map(|d| d.ssh_keys.clone()).unwrap_or_default();
```

and from `src/api/src/deployment.rs:2158-2164`:

```rust
        debug_mode = if request.debug_mode { "true" } else { "false" },
        ssh_keys_json =
            serde_json::to_string(&request.ssh_keys).unwrap_or_else(|_| "[]".to_string()),
        ssh_ingress = if request.ssh_keys.is_empty() {
            "# SSH ingress disabled (no ssh_keys in Procfile)".to_string()
        } else {
            "…ingress {\n    from_port   = 22\n    to_port     = 22\n    protocol    = \"tcp\"\n    cidr_blocks = [\"0.0.0.0/0\"]…"
        },
```

Nothing anywhere in the platform validates the pair: a repository-wide search for
`ssh_keys` finds parsing (`src/caution-config/src/lib.rs:504-576`, where
`has_debug = debug.is_some() || !ssh_keys.is_empty()`), rendering, and tests —
and no rule relating it to `enabled`.

What the reopened console carries. `shim/src/main.rs:21-30` installs a
process-wide subscriber defaulting to `info`, with a comment that is precise about
the policy:

```rust
    // `info` deliberately does NOT include the per-request `zis::proxy` line:
    // that line names the method each wallet called, which is a metadata source
    // this component exists to deny the operator, and it would live in a log
    // file on the operator's box.
```

and `shim/src/intercept.rs:582-601` puts, at that same INFO level, the fields the
same reasoning would forbid:

```rust
            Class::Migration => tracing::info!(
                target: "zis::classify",
                version = %evidence.version,
                orchard_actions = evidence.orchard_actions,
                orchard_vb = %format!("{:+}", evidence.orchard_vb),
                ironwood_vb = %format!("{:+}", evidence.ironwood_vb),
                sapling_vb = %format!("{:+}", evidence.sapling_vb),
                expiry = ?evidence.expiry_height,
                inputs = evidence.inputs,
                outputs = evidence.outputs,
                tx_len = evidence.len,
                diverted_in_production,
                "MIGRATION detected: …"
            ),
```

**What attestation still buys, stated precisely.** `RUST_LOG` is not free for the
operator to raise at this point: it is an `export` line inside the measured
`run.sh` (`src/caution-config/src/lib.rs:253-276` emits `export KEY=value` for
every literal `env` entry; item 6q), so the reopened console shows the INFO stream
and not the `zis::proxy` debug stream. That constraint is real, but it is enforced
by **PCR0/PCR1 and by `.manifest.run_command`** — and `deploy.sh:220` instructs
verifiers to *expect PCR0/1 to fail and accept PCR2 alone* (confirmed
`deploy-script-tells-operators-to-expect-pcr01-failure-and-accept-pcr2-alone.md`),
while no zeronym document tells anyone to read `.manifest.run_command` at all
(confirmed `auditor-recipe-omits-…`). So the honest form of the slogan is:
**attestation constrains what is logged; it never constrains who reads it; and the
first half is enforced only by checks the project's own instructions skip.**

The zeronym texts that are falsified, verbatim
(`shim/deploy/caution/caution.hcl.tmpl:186-201`):

```hcl
  debug {
    # …
    # If the enclave boots but never serves, flip this to true and redeploy;
    # that opens port 22 on the parent so the console can be read at
    # /var/log/nitro_enclaves/enclave-console.log. …
    # With --debug the flip is one boolean and the key is already listed; without
    # --debug this list renders empty and is moot (SSH is closed under attestation).
    enabled  = false
    __DEBUG_SSH_KEYS__
  }
```

`shim/deploy/caution/OPERATORS.md:354-357`:

> **Reading state from an attested enclave**: there is no SSH. Use
> `https://<tls-domain>/attestation` and, on the hub, `/healthz` and
> `/nym-address`.

and `deploy.sh:227-229`, which uses the same belief as a design rationale:

> the enclave console is only open with --debug, and --debug disables
> attestation, so an address that could only be read from the console could never
> belong to a hub that had also been proven.

## Recommendations

1. **Refuse the combination in the assembler.** Both `assemble-caution.sh` scripts
   should `exit 2` on `--ssh-key` without `--debug`, exactly as they already do
   for other unsafe input combinations (shim `:185-192`, `:221-225`). This is a
   three-line change and it closes the whole issue for the deploy path.
2. **Correct the five texts.** Delete "SSH is closed under attestation" and "there
   is no SSH" from both `caution.hcl.tmpl`s, both `OPERATORS.md`s
   (shim `:354`, hub `:138`) and the rationale at `deploy.sh:227-229`, and replace
   them with the true statement: a non-empty `ssh_keys` list opens port 22 on the
   parent to `0.0.0.0/0` regardless of `debug.enabled`, and a shell on the parent
   can relaunch the enclave in debug mode and read the console.
3. **Give auditors a check that can see it.** The `debug` block is not in the
   attested manifest, so the only external evidence is the open port. Publish the
   deployment's public IP alongside the attestation URL and state that `22/tcp`
   open is a finding; the platform's own `dns_contains_deployment_ip` check on the
   raw-IP verify path already establishes the pattern.
4. **Reduce what the console is worth.** Remove `orchard_vb` / `ironwood_vb` /
   `sapling_vb` / `expiry` / `inputs` / `outputs` / `tx_len` from the
   `Class::Migration` and fail-safe arms of `log_verdict` (keeping the
   counts-and-verdict form the hub already uses), so that a reopened console
   yields a count rather than an amount. This is the recommendation of the
   confirmed `log-verdict-logs-migration-value-balance-at-info.md`; this issue is
   the reason it should not be deferred on the grounds that attestation contains
   it.
5. **Restrict the SSH ingress upstream.** Independently of zeronym, the platform's
   `0.0.0.0/0` SSH rule should be narrowed to an operator-supplied CIDR; worth
   raising with Caution and recording in `OPEN-QUESTIONS.md`.

## Validation Information

**Verdict: CONFIRMED at Medium** (severity as filed). Every mechanical claim was
re-derived during validation from the Caution platform clone
(`codeberg.org/caution/platform` @ `1f8d8cb`) and from the target, not inherited
from earlier passes.

**Re-verified from platform source:**

- `src/api/src/main.rs:2420-2421` — `debug_enabled` and `ssh_keys` are two
  independent reads of the same `debug` block; `debug_enabled` is not consulted
  when building `ssh_keys`.
- `src/api/src/deployment.rs:2158-2164` — `ssh_ingress` is predicated on
  `request.ssh_keys.is_empty()` alone and emits `from_port = 22 … cidr_blocks =
  ["0.0.0.0/0"]`; `debug_mode` is a separate template variable. This is inside
  `generate_nitro_deployment_main_tf`, i.e. the AWS Nitro path the fully-managed
  deploy uses (the on-prem generator at `:2458-2464` is identical).
- The ingress lands on `aws_security_group.enclave` (`:2018-2032`) which is
  attached to `aws_instance.enclave` (`:2075`), and the instance has an
  `aws_eip.enclave` (`:2118-2126`) — so the port is reachable from the internet.
- `user-data.sh:15` (nitro-cli unconditional), `:17-27` (keys under
  `length(ssh_keys) > 0` only), `:53` (EIF path), `:157-173` (the unit),
  `:166` (`--debug-mode` iff `debug_mode`), `:127-154` + `:174-190` (capture
  service, debug-gated), `:210` (the 443 `socat` relay, `fork`, per-connection).
  **Three citations in the filing were off and are corrected here:** the EIF copy
  is `:53` not `:52`, the unit is `:157-173` not `:155-172`, and the
  `assemble-caution.sh` line numbers in the original Location were the **hub's**
  copy while the text named the shim's — both files are now cited explicitly.
- `src/enclave-builder/src/manifest.rs:14-45` — `EnclaveManifest` has no `debug`
  field, so the key list appears in no attestation response. Confirms the
  "unmeasured" half of item 6x.
- No sshd/sudo/user hardening anywhere in the 535-line `user-data.sh`, so the
  AL2023 defaults (ec2-user, passwordless sudo, pubkey sshd) stand. This is the
  one assumption the finding rests on that could not be executed here; it is the
  documented AWS default for that AMI family and is already load-bearing for the
  audit's other parent-host findings.

**Re-verified in the target:** the five falsified texts at the exact lines quoted;
the assembler branch (`shim/deploy/caution/assemble-caution.sh:449-462`) tests
`$SSH_KEYS` and never `$DEBUG`, while `:120-123` prints the reassuring NOTE and
`:567-574` flips only `enabled` under `--debug`; `deploy.sh:52` (`DEBUG=${DEBUG:-1}`)
and `:128-135` (key only with `--debug`); `log_verdict` at `intercept.rs:577`
with the `Class::Migration` arm at `:582`; the hub's per-admission line at
`server.rs:269`; both subscribers defaulting to `info`.

**THE BOUND, which the report must carry verbatim and which no other finding may
quietly drop:** `debug.ssh_keys` on its own does **not** hand over the console.
The image was launched without `--debug-mode`, so `nitro-cli console` refuses and
the capture service was never installed. **No log finding may be escalated via
this route directly.** The confirmed
`hub-per-admission-info-log-is-a-real-time-per-entry-arrival-feed.md` (Low) and
the Case B bound of the confirmed
`log-verdict-logs-migration-value-balance-at-info.md` are correct as they stand and
must not be re-inflated on account of this issue. What this issue establishes is
the *conditional*: the closure is a parent-side launch flag, and an operator with
`ssh_keys` can flip it, at the price of zeroed PCRs for the duration.

**Anti-double-counting, checked against four neighbours:**

- `hub-manifest-debug-block-claims-ssh-keys-render-empty-and-ssh-is-closed-under-attestation.md`
  (plausible; item 6x recommends Low → High) owns **obtaining the parent-host
  shell** — the doc/renderer contradiction and the packet-capture, DNS-log,
  iptables and restart capabilities that follow. This issue owns **only the
  console leg**: that the shell also reaches the enclave's `tracing` stream, which
  that file explicitly and wrongly excludes. Its "Note what is not at risk:
  enclave console output" paragraph should be struck when it is graded; its
  severity argument should not be increased on account of this file, nor this
  file's on account of it.
- `core-linkage-survives-in-the-attested-deployment-…` (confirmed High) uses
  parent-host access as **step 1**. That step is the *shell*, owned by the sibling
  above — not the console. This issue adds nothing to the linkage chain and must
  not be counted into it.
- `deploy-script-defaults-to-debug-mode-which-turns-attestation-off.md` (confirmed)
  owns the shipped `DEBUG=1` default, which delivers the same console with no
  ssh key and no relaunch. This issue is the *attested* deployment's version of
  the same exposure and is strictly narrower.
- `log-verdict-logs-migration-value-balance-at-info.md` (confirmed High) owns the
  content of the leak. This issue owns only the reachability of the channel.

**One consequence explored during validation and deliberately NOT claimed here.**
The worst thing a root shell on the parent can do is not read the console: it is
replace `/opt/nitro/enclave.eif` and run a different image entirely. That
capability belongs to the parent-shell issue above, and its detectability is
already owned by two confirmed issues — `caution verify` would flag PCR0/PCR1, and
`deploy.sh:220` tells verifiers to expect exactly those two to fail and accept
PCR2 alone, which item 6q established is a universal constant. It is recorded here
so the report can state the residual once, in the right place, rather than three
times.

**Severity justification — Medium.**
*Why not Low:* the capability is the primary adversary position obtained inside a
deployment the product presents as operator-blind; it is invisible in the
attestation (no `debug` field in the manifest), invisible to wallets, and denied
five times in the project's own text, including once as a load-bearing design
rationale. The harm delivered — per-migration value balance beside the wallet's
TCP source address — is the exact property `README.md:33` promises.
*Why not High:* it needs a deliberate operator act that no shipped script
performs; the deployed attested state does not leak (the bound above); the
relaunch zeroes PCRs for its duration, so a verifier checking *at that moment*
sees it; and every downstream harm is separately graded, including two cheaper
routes to a worse outcome for the same adversary (the `DEBUG=1` default, and
`shim-submits-every-migration-to-every-configured-hub-…` at High).

**Nothing in the filing was found to be false.** The changes are: three corrected
platform citations, two additional falsified texts (`hub/deploy/caution/OPERATORS.md:138`
and `deploy.sh:227-229`), the verified `0.0.0.0/0` reach of the SSH rule and the
public EIP, the `EnclaveManifest` confirmation, the precise re-statement of what
attestation buys (PCR0/PCR1 and `.manifest.run_command`, both skipped by the
project's own recipe), and the explicit bound and anti-double-count map above.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).

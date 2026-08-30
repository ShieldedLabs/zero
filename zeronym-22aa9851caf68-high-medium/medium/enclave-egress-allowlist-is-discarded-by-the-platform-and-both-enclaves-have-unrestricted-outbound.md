# The Caution platform discards every `egress` rule in `caution.hcl` and grants the enclave unrestricted outbound internet, so the "exfiltration is a network-level impossibility" property both manifests state as a security property does not exist in any configuration

**Severity**: Medium
**Validation Status**: Confirmed
**Location**: `audit-target/zeronym/shim/deploy/caution/caution.hcl.tmpl:57-77` (the claim, and the `__HUB_EGRESS__` marker at `:78`); `audit-target/zeronym/hub/deploy/caution/caution.hcl.tmpl:57-73` (the same claim, and the `__EGRESS_BLOCKS__` marker at `:82`); `audit-target/zeronym/shim/deploy/caution/assemble-caution.sh:265-320` and `audit-target/zeronym/hub/deploy/caution/assemble-caution.sh:252-273` (the rule renderers); `audit-target/zeronym/deploy.env.example:63`; `audit-target/zeronym/shim/deploy/caution/OPERATORS.md:240-242` and `audit-target/zeronym/hub/deploy/caution/OPERATORS.md:77-78` ("the enclave's entire allowlist"); `audit-target/zeronym/hub/REVIEW.md` (the containment reasoning the allowlist supports). Platform side, read directly during this audit: `codeberg.org/caution/platform` `src/caution-config/src/lib.rs:322-327`, `src/api/src/main.rs:2451`, `src/api/src/deployment.rs:2055-2061`, `terraform/modules/aws/nitro-enclave/user-data.sh:69-95`.
**Found by agent:** Global (focus areas G10 / G12 / G13 — the attestation and reproducibility chain, end to end)
**In scope of audit?** Yes — `*/deploy/**` is explicitly in scope; `AUDIT-INSTRUCTIONS.md` priority area 7 asks in terms "whether the enclave's egress allowlists actually constrain exfiltration"; and manifest claims are in scope as security claims under ICTM.

## Description

Both enclave manifests state, explicitly as a *security property* rather than as
tidiness, that the enclave is structurally incapable of sending the plaintext it
holds anywhere except a named `/32`. The shim's, verbatim
(`shim/deploy/caution/caution.hcl.tmpl:57-72`):

```hcl
    # Egress to exactly one host and port: the backing indexer, nothing else.
    #
    # Deliberately narrower than the platform's example, which allows all
    # egress. This narrowness is a security property rather than tidiness. The
    # shim sees every wallet's queries in the clear, which is precisely the
    # exposure Zeronym exists to contain, so the enclave should be structurally
    # incapable of shipping that anywhere except the one indexer it fronts. A
    # /32 and a single port make exfiltration to a third party a network-level
    # impossibility instead of a promise about the code.
    #
    # Note what is absent: no port 53, even though the backend is authenticated
    # by NAME. That combination is the point. ZIS_BACKEND stays a literal
    # address, so the enclave dials an IP and never resolves DNS;
    # ZIS_BACKEND_TLS names what the certificate must say. A poisoned DNS answer
    # has nothing to poison, and a hijacked address cannot present a valid
    # certificate for the name. Update the CIDR whenever the backend IP moves.
```

The hub's manifest says the same thing about the enclave that holds every
migration in plaintext (`hub/deploy/caution/caution.hcl.tmpl:57-73`).

**The Caution platform never applies these rules.** The entire `egress { }` list
— every `cidr_ipv4`, every `port`, every `ip_protocol` — is parsed, validated
against the schema, and then reduced to a single boolean: *is the list empty or
not?* If it is non-empty the enclave is given a NATted TAP bridge to the parent
host with an unconditional `iptables ... -j ACCEPT`, an AWS security group whose
egress rule is `0.0.0.0/0` on all protocols and all ports, and a DHCP-supplied DNS
resolver on the parent.

So in the deployed configuration:

- Both enclaves can open a TCP or UDP connection to **any host on the internet, on
  any port**, not to a `/32` on one port.
- DNS is available and configured (`/etc/resolv.conf` → `nameserver 10.0.100.1`,
  the parent's `dnsmasq`), regardless of whether a port-53 rule was written. The
  "no port 53 … a poisoned DNS answer has nothing to poison" argument does not
  hold, and the `1.1.1.1/32:53:udp` rule the project does write buys nothing
  either.
- The parent host's `dnsmasq` runs with `--log-queries --log-dhcp`, so every
  hostname either enclave resolves is written to a log on the parent. **Who reads
  that log is deployment-model dependent** (open item 6x): `deploy.sh:156` runs
  `caution apps create`, which `shim/deploy/caution/OPERATORS.md:64` defines as
  "fully managed: in Caution's AWS account", so in the shipped shape the log is
  Caution's; on the BYOC model the same file documents at `:66-71`, it is the
  operator's — the threat model's adversary #1. Note this is a *convenience*, not
  a new channel: the parent is the enclave's NAT router, so it sees every
  destination IP either way, and a UDP/53 query to an allowlisted resolver would
  have crossed the same NAT in plaintext. The point is that the manifests reason
  as if neither were true.

This is not the same finding as
`hub-egress-allowlist-is-not-the-exfiltration-barrier-the-manifest-claims.md`,
which argues that the *shipped rule set* is too broad (`0.0.0.0/0:9000:tcp` plus
DNS). That issue's conclusion is right and its mechanism understates the problem:
the breadth of the rules is irrelevant, because **a perfectly narrow rule set
would be discarded in exactly the same way.** An operator who wrote a single
`/32:443:tcp` and nothing else would still get unrestricted outbound.

## Attack Scenario and Steps

The egress allowlist is positioned by both manifests as the control that holds
*"instead of a promise about the code"* — i.e. the one that survives the code not
being what the reader believes. The scenario is therefore the one it was written
for.

1. An attacker obtains code execution inside either enclave, or arranges for the
   attested build to contain code the reviewers did not see. Routes that require
   no platform break:
   - A memory-safety or logic defect on an untrusted-input path. Both enclaves
     parse attacker-supplied Sphinx packets and attacker-supplied transaction
     bytes from the whole internet, over `nym-sdk 1.21.5-rc.1` and its legacy
     transitive stack (`rustls 0.21.12`, `hyper 0.14.32`, `h2 0.3.27`,
     `sha2 0.9.9`).
   - A supply-chain compromise of any dependency. Attestation binds PCRs to a
     build; a malicious dependency reproduces and attests exactly as cleanly as a
     benign one.
   - `assemble-git-archive-honours-gitattributes-so-the-build-context-is-not-the-committed-tree.md`,
     which is the in-tree route to exactly this: source that reproduces and
     attests cleanly and is not what a reviewer read.
2. The code opens a TCP connection to any host the attacker chooses and writes out
   what the enclave holds: for the shim, every wallet's query stream and source
   address plus the plaintext of every diverted transaction; for the hub, the
   whole unpublished queue.
3. Nothing blocks it. There is no per-destination policy anywhere in the path —
   not in the security group, not in the parent's `iptables`, not in the enclave.

**Attack Requirements and Assumptions:**

- **Requires a second failure**, exactly as a defence-in-depth control implies:
  code execution, a subverted dependency, or a build the reviewers did not read.
  It is not directly exploitable by a network attacker on its own.
- **That is the whole point of the control.** Both manifests say the narrowness
  makes exfiltration "a network-level impossibility instead of a promise about the
  code". A defence-in-depth control that is entirely absent is a finding even
  though a second defect is needed to reach it.
- No AWS, Nitro, or Caution compromise is required, and no operator
  misconfiguration: this is how the platform behaves for a correctly written
  manifest.
- Under ICTM the documentation half stands on its own, with no second failure at
  all: a stated structural property that does not exist is itself the bug, and
  this one is stated in the artefact `caution verify` reproduces and an auditor is
  pointed at.

## Impact on Users

- The containment argument that both components' "we hold your plaintext but we
  cannot ship it anywhere" claim rests on does not exist. For the hub — the single
  point that sees every migration in the clear — the barrier is back to being "a
  promise about the code", which is precisely what the manifest says it is not.
- An auditor or operator who reads the deployed `caution.hcl` (the artefact
  published to `--app-source` and rebuilt by `caution verify`) is told the enclave
  can only reach one `/32` and cannot resolve DNS. Both statements are false.
  `shim/deploy/caution/OPERATORS.md:240-242` repeats it operationally: *"The
  `--nym-egress` rules are the enclave's **entire allowlist** for reaching the
  mixnet: the nym-api, a gateway, and a DNS resolver."* They are not an allowlist at all.
- Because a successful exfiltration from the hub is total and retrospective —
  every migration that ever passed through, joinable against the permanent public
  chain — the value of the missing control is high even though its exercise
  requires a second defect.
- Secondary, and live without any second defect: the enclave resolves DNS through
  the parent host's `dnsmasq --log-queries`, which the project believes it
  constrained by allowlisting exactly one resolver (`1.1.1.1/32:53:udp`). On BYOC
  that log is the operator's; on the shipped fully-managed path it is Caution's.
  This is a smaller point than it first appears — the parent is the enclave's NAT
  router and sees every destination IP either way — but it is a second place where
  the tree reasons from an allowlist that does not exist.

## Technical Details / Code Analysis

**1. Where the rules go to die.** `codeberg.org/caution/platform`,
`src/caution-config/src/lib.rs:322-327` — the *only* place `NetworkConfig::egress`
is consumed:

```rust
impl NetworkConfig {
    /// Outbound internet access is enabled iff at least one egress rule is declared.
    pub fn egress_enabled(&self) -> bool {
        !self.egress.is_empty()
    }
}
```

`src/api/src/main.rs:2451`:

```rust
let egress = ec_network.map(|n| n.egress_enabled()).unwrap_or(false);
```

That `bool` is the whole of the egress configuration from this point on. It is
threaded into the Terraform variables (`src/api/src/deployment.rs:2105`,
`:2170` — `egress = if request.egress { "true" } else { "false" }`) and into the
enclave's `run.sh` as a template-block toggle
(`src/enclave-builder/src/build.rs:411-413` — `if egress { enabled_blocks.push("EGRESS"); }`).
No CIDR, port or protocol is carried anywhere.

**2. What the security group actually says** (`src/api/src/deployment.rs:2055-2061`,
rendered into the enclave instance's Terraform):

```hcl
  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
    description = "Allow all outbound"
  }
```

**3. What the parent host does**
(`terraform/modules/aws/nitro-enclave/user-data.sh:69-95`, inside
`%{ if egress == "true" ~}`):

```sh
ip tuntap add mode tap name enclave0
ip addr add 10.0.100.1/24 dev enclave0
ip link set enclave0 up
socat TUN,tun-type=tap,iff-no-pi,tun-name=enclave0 VSOCK-LISTEN:3,fork,reuseaddr &
iptables -t nat -A POSTROUTING -s 10.0.100.0/24 -o "$DEFAULT_IFACE" -j MASQUERADE
iptables -A FORWARD -i enclave0 -o "$DEFAULT_IFACE" -j ACCEPT
iptables -A FORWARD -i "$DEFAULT_IFACE" -o enclave0 -m state --state RELATED,ESTABLISHED -j ACCEPT
dnsmasq \
  --interface=enclave0 --bind-interfaces \
  --dhcp-range=10.0.100.10,10.0.100.50,12h \
  --dhcp-option=3,10.0.100.1 --dhcp-option=6,10.0.100.1 \
  --no-daemon --log-queries --log-dhcp &
```

`-j ACCEPT` with no destination match is the entire forwarding policy. The last
five lines are the DNS resolver.

**4. What the enclave does with it** (`src/enclave-builder/templates/run.sh.template`,
`# {EGRESS` block):

```sh
/bin/socat TUN,tun-type=tap,iff-no-pi,iff-up,tun-name=eth0 VSOCK-CONNECT:3:3 &
...
/bin/busybox udhcpc -i eth0 -n -q -s /bin/udhcpc-script ...
echo "nameserver 10.0.100.1" > /etc/resolv.conf
```

So a resolver is configured unconditionally whenever egress is on, which makes
both manifests' "no port 53, and that is the point" paragraphs inoperative.

**5. What zeronym believes it is doing.** `hub/deploy/caution/assemble-caution.sh:270-272`
writes each rule into the manifest:

```sh
	printf '\n    # Nym mixnet egress (gateway / nym-api / DNS / Nyx), operator-allowlisted.\n' >> "$EGRESS"
	printf '    egress {\n      cidr_ipv4   = "%s"\n      port        = %s\n      ip_protocol = "%s"\n    }\n' \
		"$cidr" "$port" "$proto" >> "$EGRESS"
```

and `shim/deploy/caution/assemble-caution.sh` does the same. Both scripts spend
substantial effort on the rule set — the shim's warns when the nym-api or DNS rule
count is 0 or 1, and `deploy.env.example:56-59` refuses to allowlist the
Fastly-fronted nym-api on the explicit ground that shared anycast edges "would let
the enclave reach every origin fronted by that CDN". That refusal is reasoning
about a control that is not in force.

**6. The same fate for ingress CIDRs** (`src/api/src/main.rs:2452-2471` keeps only
port numbers; `src/api/src/deployment.rs:2044-2052` opens each to `0.0.0.0/0`
unconditionally). No security delta for zeronym, which declares `0.0.0.0/0`
anyway, but it means an operator who narrows an ingress CIDR gets no narrowing.

**Method note.** These are lines of the Caution platform's own public source
(`https://codeberg.org/caution/platform`, cloned during this audit), not vendor
prose. The vendor documentation is silent on the question, which is why
`audit-context/EXTERNAL-CONTEXT.md` §7 and `BRAINSTORM.md` §R2-O carried it as an
unanswerable premise. It was answerable.

## Recommendations

1. **Ask Caution whether per-destination egress filtering is planned, and until it
   exists, delete the claim.** The three paragraphs at
   `shim/deploy/caution/caution.hcl.tmpl:57-72` and
   `hub/deploy/caution/caution.hcl.tmpl:57-73` should say plainly that the
   `egress` block enables outbound access and does not restrict it, that the
   enclave can reach any host on the internet, and that a DNS resolver on the
   parent host is configured whenever egress is enabled. This is the cheapest fix
   and removes the ICTM overclaim immediately.
2. **Correct the two operator runbooks.** `shim/deploy/caution/OPERATORS.md:240-242`
   and `hub/deploy/caution/OPERATORS.md:77-78` both call `--nym-egress` "the
   enclave's entire allowlist"; it is not an allowlist.
3. **Remove the dependency on it from the threat model and from every document
   that leans on it**, including `hub/REVIEW.md`'s containment reasoning and
   `deploy.env.example:56-59`'s reasoning about which nym-api endpoints are safe
   to allowlist.
4. **If containment matters — and for the hub it does — implement it where it can
   be implemented.** Either (a) obtain per-destination filtering from Caution and
   make it part of what `caution verify` covers, or (b) accept that the only
   in-scope control is the code, and treat every "structurally impossible" claim
   in the tree as a claim about the code instead.
5. **Re-examine the DNS posture.** Both enclaves currently resolve through the
   parent host. The nym-sdk's `no_hostname` switch (already identified in
   `hub/deploy/caution/assemble-caution.sh:328-331` as "driver work") would remove
   zeronym's own reliance on it; nothing removes the resolver from the enclave.

## Validation Information

**Validated 2026-08-18. CONFIRMED at Medium.** The platform half of this issue is
an external, moving target, so it was re-verified from a **fresh clone** of
`https://codeberg.org/caution/platform` taken during validation (HEAD
`6051734af680bcb1a96e6034a0d6409af57891f1`, 2026-08-18) as well as against the
earlier clone the filing global audit used (`1f8d8cb39b29a09530b0c3087b5da9198eb3d295`,
2026-08-13). **The line numbers cited in the body are correct for the 2026-08-13
clone**; at 2026-08-18 HEAD the same code has moved slightly (the sole
`egress_enabled()` consumer is `src/api/src/main.rs:2471`, the security group
`egress {}` block is `src/api/src/deployment.rs:2077-2083` and `:2345-2351`).
The behaviour is identical in both.

### Every mechanism checked, in both clones

1. **The rules are reduced to a boolean.** `NetworkConfig::egress_enabled()` is
   literally `!self.egress.is_empty()` (`src/caution-config/src/lib.rs:322-327`).
   I enumerated **every** reader of `NetworkConfig::egress` across the whole
   platform tree: `src/api/src/main.rs` (the boolean), `src/cli/src/lib.rs:1157-1160`
   / `:6237` / `:6751` / `:6759` (the same boolean, `config_egress_enabled`), and
   `src/cli/src/apps/migrate_procfile.rs:217,248` (Procfile→HCL *conversion*, not
   enforcement). **No CIDR, port or protocol from an `egress` block is read
   anywhere else in the platform.**

2. **The security group allows everything outbound.**
   `egress { from_port = 0, to_port = 0, protocol = "-1", cidr_blocks = ["0.0.0.0/0"], description = "Allow all outbound" }`,
   rendered unconditionally into the enclave instance's Terraform in both
   deployment templates.

3. **The parent host forwards everything.**
   `terraform/modules/aws/nitro-enclave/user-data.sh:69-95`, inside
   `%{ if egress == "true" ~}`: a TAP device `enclave0` (10.0.100.1/24) bridged
   to the enclave over vsock by `socat`, then
   `iptables -t nat -A POSTROUTING -s 10.0.100.0/24 -o "$DEFAULT_IFACE" -j MASQUERADE`
   and `iptables -A FORWARD -i enclave0 -o "$DEFAULT_IFACE" -j ACCEPT` — **no
   destination match of any kind** — plus `dnsmasq … --log-queries --log-dhcp`.

4. **Nothing filters inside the enclave either.** `iptables`/`nft` appear nowhere
   in `src/enclave-builder/`; the only occurrences in the platform are the three
   parent-side rules above plus the `dnf install` line. The enclave's
   `run.sh.template:53-78` (`# {EGRESS`) brings up `eth0` over vsock, runs
   `udhcpc`, and writes `nameserver 10.0.100.1` — so a resolver is configured
   **unconditionally whenever egress is enabled**, which is what makes both
   manifests' "no port 53, and that is the point" paragraphs inoperative.

5. **The zeronym half is as quoted.** `shim/deploy/caution/caution.hcl.tmpl:57-72`
   and `hub/deploy/caution/caution.hcl.tmpl:57-73` both state the "network-level
   impossibility instead of a promise about the code" property verbatim;
   `shim/deploy/caution/OPERATORS.md:240-242` and
   `hub/deploy/caution/OPERATORS.md:77-78` both call the `--nym-egress` rules "the
   enclave's entire allowlist"; `deploy.env.example:56-63` refuses the
   Fastly-fronted nym-api on the explicit ground that shared anycast edges "would
   let the enclave reach every origin fronted by that CDN"; and
   `shim/deploy/caution/assemble-caution.sh:317-325` reasons at length that "DNS
   cannot be dropped from this deployment by editing egress; it needs driver
   work". **All four are engineering decisions taken about a control that is not
   in force.**

### Why this is a finding and not a defence-in-depth quibble

It is filed and graded as two things, and only the first needs a second failure:

- **Live, no second failure required (the ICTM half).** Both manifests state a
  *structural* security property, in the artefact that `caution verify` stages
  and rebuilds and that auditors are pointed at, and it does not exist in any
  configuration. Two runbooks restate it operationally. This is exactly the class
  of defect this engagement treats as first-class: a property a reader is told
  they have and does not have. `AUDIT-INSTRUCTIONS.md` priority area 7 asks the
  question directly ("whether the enclave's egress allowlists actually constrain
  exfiltration"); the answer is **no, for every rule set, on both components.**
- **Latent (the containment half).** The missing control is defence-in-depth by
  construction, so realising it needs code execution, a subverted dependency, or
  a build reviewers did not read. That is the scenario the control was written
  for, and its complete absence is a real reduction in the hub's containment: a
  hub with a working `/32` allowlist could only exfiltrate through the indexer
  hop, which is a materially harder covert channel than an outbound socket to
  any host on the internet.

### Why Medium, not High and not Low

- **Not High.** No attacker gains anything directly; there is no reachable attack
  path that this issue alone opens, and the *confidentiality* boundaries the
  system actually depends on (TLS with name pinning to the backend/indexer, the
  mixnet) are untouched by it.
- **Not Low.** It affects **both** enclaves, including the one that holds every
  migration in the world in plaintext; it is unfixable inside zeronym (the honest
  remedies are to delete the claim or to obtain the feature from the vendor); and
  it has already misdirected at least four documented design decisions in the
  tree. A stated structural guarantee that is absent in every configuration, in
  the enclave that holds the plaintext, is Medium.

### How double-counting was avoided

This issue is the **root cause and the owner** of "the enclave egress allowlist
provides no containment". Three neighbours touch it and none is counted here:

- `hub-egress-allowlist-is-not-the-exfiltration-barrier-the-manifest-claims.md`
  (still in `plausible/`, filed Medium) argues the narrower point that the
  *shipped hub rule set* is too broad (`0.0.0.0/0:9000:tcp` plus DNS). Its
  conclusion is correct but is strictly **subsumed** by this one, which shows a
  perfectly narrow rule set would be discarded identically, and which covers the
  shim as well. **Recommendation to whoever validates it: merge it into this
  issue, or confirm it at Info as the rule-breadth documentation half — do not
  confirm it at Medium, because that would count one harm twice.** I have not
  changed its status; it was not assigned to me.
- `assemble-git-archive-honours-gitattributes-so-the-build-context-is-not-the-committed-tree.md`
  already claims the *composition* ("injected code has unrestricted egress") as
  an argument for its **own** severity. This issue therefore lists the injection
  route as one attack path but does **not** take credit for the compound
  outcome; the compound harm belongs there.
- `shim-assemble-nym-egress-cidr-unvalidated-and-no-breadth-advisory.md` and
  `hub-assemble-indexers-validation-is-line-oriented-so-a-newline-injects-egress-blocks.md`
  both reason about the consequences of *wrong egress rules*. Whoever validates
  them should note that an injected or over-broad `egress { }` block changes
  nothing about the enclave's actual reachability — their surviving content is
  about manifest integrity and about the HCL injection primitive, not about
  network exposure.

### One claim in the body deliberately softened during validation

The original "the operator's own `dnsmasq` logs every hostname the enclave
resolves" was **qualified rather than kept as written**. Per open item 6x, the
shipped deploy path (`deploy.sh:156`, `caution apps create`) is fully managed —
`shim/deploy/caution/OPERATORS.md:64` puts that parent host in *Caution's* AWS
account, not the operator's — so the log lands with the operator only on BYOC. It
is also not a new channel: the parent is the NAT router and sees every
destination IP regardless, and a UDP/53 query to an allowlisted external resolver
would have crossed the same NAT in the clear. The corrected text is in the
Description and Impact sections.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).

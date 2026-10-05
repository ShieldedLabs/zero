## What to look for

Report a vulnerability only when you can show a real attacker exploiting it in
production against real users. Ordinary code quality is not the subject of this
review.

Every finding must name the adversary it assumes:

- **PEER** — a remote node sending arbitrary, malformed or adversarially ordered
  blocks, transactions and protocol messages. Assume unlimited peers.
- **CHAIN** — an attacker who can get a transaction or block mined, including
  one that is individually valid but adversarially shaped.
- **MINER** — an attacker with some hash power, able to reorder, withhold or
  reorg.
- **RPC** — a client reaching the node's RPC or wallet interface with whatever
  credentials that interface actually requires.
- **LOCAL** — a process on the same machine without the node's privileges.

Do not report an attack that requires the adversary to already hold the node's
keys, already have root, or already control the operator's machine. Do report
anything that escalates one of the adversaries above into that position.

## Attack the security invariant

- Name the property the changed code preserves, in words a user would
  recognize: a note cannot be spent twice; a transaction is invalid unless its
  signature commits to every input it spends; the wallet does not reveal which
  notes are mine.
- Then attack that property. A finding is a demonstrated violation of one.
- Look hardest where an invariant is made to depend on something outside its own
  enforcement: a caller's promise, a value from the wire, an ordering that is
  not actually guaranteed.
- Check boundaries between components, where one side assumes something the
  other does not guarantee. That includes first-party `zeronym/` code calling
  into vendored code.

## Where the bugs are

- **Validation.** Anything deciding whether a block or transaction is valid, and
  anything gating that decision on a version, branch ID or activation height.
- **Signatures and hashing.** A sighash that does not commit to everything it
  must, a domain separator reused across contexts, a digest computed over a
  different serialization than the one checked.
- **Deserialization.** Attacker-controlled length fields driving allocation,
  bounds derived from the input itself, two encodings that parse to one value or
  one value that re-encodes differently.
- **Keys and randomness.** Nonce reuse under a reused key, a secret from
  insufficient entropy, key material logged or left in a live buffer, a
  non-constant-time comparison on secret data reachable by a remote party.
- **Money arithmetic.** Overflow, truncation, sign confusion, or a rounding
  direction an attacker can drive.
- **Resource consumption.** Amplification only: a small message causing
  disproportionate memory, disk or CPU use. Unbounded queues and caches fed from
  the network.
- **Privacy.** Anything letting a remote party correlate notes, addresses or
  transactions with a wallet — timing, ordering, error distinguishability, or a
  request pattern that varies with private state.

## Do not report

These look like vulnerabilities and usually are not. If a finding matches one,
say why this instance is different or drop it.

- Assumptions an attacker cannot violate: fixed protocol sizes, consensus
  constants, invariants enforced where the attacker cannot reach.
- Exposure that is local only: an unencrypted connection to `127.0.0.1`, a
  loopback-bound interface, IPC inside one process.
- Disclosure of nothing sensitive: a stack trace with no data in it, a version
  string, timing that reveals only public state.
- Test and debug code: fixtures, mocks, credentials in tests, behaviour behind a
  debug build flag.
- Resource exhaustion needing the attacker to send enormous volume, or that OS
  and infrastructure limits cut off first.
- Intentional design: an interface meant to be public, a trust boundary
  inherent to the architecture.
- Insecure non-default configuration whose own name communicates the danger.
- Anything needing prior compromise: root, physical access, reading process
  memory, a known discrete log relation between group-hash outputs, or a
  256-bit hash collision.
- Flaws that, if real, would break the software visibly for everyone and would
  already have been caught.

## Severity

Rate on risk to real users — likelihood times impact — not on how interesting
the bug is.

- **High** — steals funds, breaks consensus, compromises users' systems, reads
  private data, deanonymizes a user, or violates an integrity guarantee. Say
  "critical" in the first sentence for the first three.
- **Medium** — denial of service taking meaningful effort, or a narrow
  integrity failure.
- **Low** — reconnaissance value only, or a real bug with no security
  consequence. Report these; do not dress them up as vulnerabilities.

## What each finding must say

- Which adversary it assumes.
- The defect, in the first sentence.
- Concrete attack steps: what the attacker sends, and what happens. Not "an
  attacker could somehow".
- What the attacker needs, and why that is realistic here.
- The impact on users.
- The smallest change that closes it.

If you are unsure the attack works, say so and mark the comment low confidence
rather than omitting it.

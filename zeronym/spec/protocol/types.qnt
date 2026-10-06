// -*- mode: Bluespec; -*-

/// The vocabulary of the zeronym protocol: every domain the components, the
/// wire and the properties talk about, and nothing that computes.
///
/// Domains are sum types. A value that the protocol cannot produce (a refusal
/// carrying a transaction, a lookup answer that is both found and not found)
/// has no representation here, so no definition downstream has to rule it out.
module types {
  import basicSpells.* from "./spells/basicSpells"

  type TxId = str
  type Height = int
  type Nonce = int
  type HubId = str

  // ------------------------------------------------------------------------
  // Transactions
  // ------------------------------------------------------------------------

  /// How the shim classifies a `SendTransaction` body. `Unparseable` is the
  /// shim's own parser giving up; it is treated as a migration, never forwarded.
  type Class = OrchardTouching | PassThrough | Unparseable

  /// A transaction's bytes, abstracted to what a party can compute from them.
  ///
  /// - `id` stands for the bytes themselves, and so for their SHA-256, the key
  ///   a hub's queue uses.
  /// - `txid` is what the hub's and the node's parser compute, absent when the
  ///   bytes do not deserialise. Two payloads with different `id` and the same
  ///   `txid` are twins: the txid does not commit to every byte (ZIP 244).
  /// - `created` is the chain height the wallet built the transaction at.
  /// - `expiry` is `nExpiryHeight`, absent when the transaction never expires
  ///   or does not parse.
  /// - `class` is the shim's classification. It is independent of `txid`: the
  ///   shim rejects trailing bytes that the hub's parser accepts.
  /// - `oversize`: the bytes do not fit the fixed hub frame.
  type Payload = {
    id: str,
    txid: Option[TxId],
    created: Height,
    expiry: Option[Height],
    class: Class,
    oversize: bool,
  }

  /// Whether `payload` was built by a wallet that honours the supported expiry
  /// floor: it never expires, or expires at least `minWalletExpiry` blocks
  /// after the height it was built at.
  pure def conforming(payload: Payload, minWalletExpiry: int): bool =
    match payload.expiry {
      | None => true
      | Some(expiry) => expiry >= payload.created + minWalletExpiry
    }

  /// Whether two payloads are different bytes with the same transaction id.
  pure def areTwins(left: Payload, right: Payload): bool =
    and {
      left.id != right.id,
      isSome(left.txid),
      left.txid == right.txid,
    }

  /// The transaction ids a set of payloads carries.
  pure def txidsOf(payloads: Set[Payload]): Set[TxId] =
    payloads.fold(Set(), (acc, payload) =>
      match payload.txid {
        | Some(txid) => acc.union(Set(txid))
        | None => acc
      })

  /// What the wallet hands the shim in a `SendTransaction`: a body the shim
  /// read in full, a body it could not read, or an empty one.
  type SendInput = Clean(Payload) | Unreadable | EmptyBody

  // ------------------------------------------------------------------------
  // Chain and indexer
  // ------------------------------------------------------------------------

  /// Where a transaction stands on the chain. It only ever moves forward.
  type Inclusion = Absent | InMempool | MinedAt(Height)

  /// The height a lookup answer carries for a transaction still in the mempool.
  pure val MEMPOOL_HEIGHT: Height = 0

  /// The height a lookup answer carries for `inclusion`.
  pure def answerHeight(inclusion: Inclusion): Height =
    match inclusion {
      | MinedAt(height) => height
      | InMempool => MEMPOOL_HEIGHT
      | Absent => MEMPOOL_HEIGHT
    }

  /// An indexer's answer to a transaction lookup. An honest indexer always
  /// returns a body with `IFound`; the type admits a missing one because the
  /// hub forwards whatever it is given.
  type IndexerAnswer =
    | IFound({ body: Option[Payload], height: Height })
    | INotFound
    | IUnavailable

  /// An indexer's verdict on one broadcast transaction. `Retryable` means
  /// nothing judged it: the indexer could not be reached or could not be read.
  type Verdict = Accepted | AlreadyKnown | Rejected | Retryable

  // ------------------------------------------------------------------------
  // Hub
  // ------------------------------------------------------------------------

  /// Why a hub refuses a submission, in the order admission checks them.
  type Refusal = TipStale | HubDraining | TooLarge | ExpiryTooTight | Full

  /// A hub's decision on a submission. `Duplicate` is a success: the bytes are
  /// already queued here.
  type AckKind = Admitted | Duplicate | Refused(Refusal)

  /// Whether a decision promises the hub holds the payload.
  pure def isAccepted(kind: AckKind): bool =
    match kind {
      | Admitted => true
      | Duplicate => true
      | Refused(_) => false
    }

  /// What a hub found for a lookup: its own queue, or its indexer's answer.
  type HubOutcome = QueueHit | FromIndexer(IndexerAnswer)

  // ------------------------------------------------------------------------
  // Participants
  // ------------------------------------------------------------------------

  /// A network address. The third party is any client of a hub's public,
  /// unauthenticated address other than the shim.
  type Addr = ShimAddr | HubAddr(HubId) | ThirdPartyAddr

  /// Whether a component follows the protocol. A Byzantine component is not
  /// marked in any message or state: it simply draws its transitions from a
  /// wider relation than the honest one.
  type Role = Honest | Byzantine

  /// The role of each component. Each hub has its own.
  type Roles = { shim: Role, hubs: HubId -> Role, indexer: Role }

  /// How a hub's view of the chain tip relates to the true height.
  ///
  /// - `TipTimely`: every running hub sees each block before the next one.
  /// - `TipMayRegress`: a report may be up to the reorg allowance behind.
  /// - `TipMayLag`: a hub may go without a report for a while, and is stale
  ///   once the silence reaches the staleness window.
  type TipModel = TipTimely | TipMayRegress | TipMayLag

  /// How a stale hub's free-running cadence clock relates to the true height.
  type FreeRun = NotSlower | MayBeSlower

  /// Who the wallet hears from on a diverted send. `DispatchOnly` is the mixnet
  /// transport: the shim answers once a frame is handed over and never waits
  /// for the ack. `AwaitVerdict` is the HTTP transport: the hub's decision is
  /// the answer.
  type SubmitMode = DispatchOnly | AwaitVerdict

  // ------------------------------------------------------------------------
  // What the wallet observes
  // ------------------------------------------------------------------------

  /// The answer to a `SendTransaction`.
  type SendObs =
    | SentOk            // diverted; error code 0
    | SentRejected      // the hub refused it
    | SentToOperator    // not a migration; the operator's indexer has it
    | SendUnavailable   // failed closed: unreadable body, or no hub reachable
    | SendInvalid       // empty body
    | SendTooLarge      // does not fit the hub frame

  /// The answer to a `GetTransaction`.
  type LookupObs =
    | Pending                                   // found, height 0, no body
    | Tx({ payload: Payload, height: Height })  // the transaction, as served
    | NotFound
    | Unavailable                               // failed closed

  /// One completed wallet request. For a lookup, `via` is the nonce of the hub
  /// request whose reply produced the answer, absent when no reply did.
  type WalletEvent =
    | Sent({ input: SendInput, obs: SendObs })
    | Got({ query: TxId, obs: LookupObs, via: Option[Nonce] })

  // ------------------------------------------------------------------------
  // Shapes shared by the components
  // ------------------------------------------------------------------------

  /// What a component's transition function returns: its next state and the
  /// one output the step produced.
  type Result[s, o] = { state: s, out: o }

  /// One configuration of the protocol: the transactions in play, the hub
  /// schedule, the bounds of the model, and the trust and timing assumptions.
  /// Field by field it is the list of constants in `protocol.qnt`.
  type Config = {
    payloads: Set[Payload],
    twins: Set[Payload],
    tpPayloads: Set[Payload],
    hubs: List[HubId],
    flushInterval: int,
    miningMargin: int,
    deliveryLag: int,
    reorgAllowance: int,
    staleWindow: int,
    freeRun: FreeRun,
    minWalletExpiry: int,
    maxAttempts: int,
    queueCap: int,
    maxHeight: Height,
    maxRequests: int,
    submitMode: SubmitMode,
    roles: Roles,
    tip: TipModel,
  }
}

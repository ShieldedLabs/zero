/**
 * [zero] @claude Invoice lifecycle: the policy a merchant has to decide, in one place.
 *
 * Detection tells you an amount arrived at an address. Turning that into "order 4837 is
 * paid" needs answers to questions Zcash does not have opinions about — how many
 * confirmations, what to do about underpayment, when an invoice stops being payable. They
 * are gathered here rather than scattered through the scanner so they can be argued about
 * and changed.
 */

/** Zatoshis in one ZEC. */
export const COIN = 100_000_000;

/**
 * How an invoice is judged. Every field is a policy decision, not a protocol rule.
 *
 * @typedef {object} Policy
 * @property {number} confirmations   Blocks before a payment is treated as final.
 * @property {number} expiryBlocks    Invoice lifetime, in blocks (~75s each).
 * @property {number} underpayZats    Shortfall tolerated before calling it underpaid.
 */

/** @type {Policy} */
export const DEFAULT_POLICY = {
  // Zcash reorgs are shallow in practice; ten blocks is roughly twelve minutes and is what
  // exchanges typically use. Lower it and you accept reorg risk, raise it and customers wait.
  confirmations: 10,
  // 40 blocks is about fifty minutes: long enough for a human to pay, short enough that a
  // stale price quote does not become a loss.
  expiryBlocks: 40,
  // Dust-level shortfalls are usually a wallet's fee rounding rather than an attempt to
  // underpay, and rejecting them creates support tickets.
  underpayZats: 1000,
};

/**
 * Applies payments and the clock to an invoice, returning its new state.
 *
 * Pure: it takes the invoice, what was seen, and the current height, and returns the
 * updated invoice. That makes the policy testable without a chain, and makes a rescan
 * idempotent — running it twice over the same payments cannot double-count, because totals
 * are recomputed from the transaction set rather than accumulated.
 *
 * @param {import('./store.js').Invoice} invoice
 * @param {{txid: string, zatoshis: number, height: number}[]} payments  All payments ever seen for this invoice.
 * @param {number} chainHeight
 * @param {Policy} policy
 * @returns {import('./store.js').Invoice}
 */
export function settle(invoice, payments, chainHeight, policy) {
  // Recomputed from scratch, not added to a running total: a rescan of a range already
  // scanned would otherwise count the same payment twice, and rescans are the normal way to
  // recover from a crash.
  const unique = new Map(payments.map((p) => [p.txid, p]));
  const confirmed = [...unique.values()].filter(
    (p) => chainHeight - p.height + 1 >= policy.confirmations,
  );
  const paidZats = confirmed.reduce((sum, p) => sum + p.zatoshis, 0);
  // Money that has arrived but is not yet final. Tracked separately rather than folded into
  // `paidZats`, because a customer who has paid and sees nothing for twelve minutes assumes
  // it failed and pays again — and the merchant must not treat it as settled either.
  const pending = [...unique.values()].filter(
    (p) => chainHeight - p.height + 1 < policy.confirmations,
  );
  const pendingZats = pending.reduce((sum, p) => sum + p.zatoshis, 0);
  const confirmationsSeen = pending.length
    ? Math.max(0, chainHeight - Math.max(...pending.map((p) => p.height)) + 1)
    : 0;
  const paidHeight = confirmed.length
    ? Math.min(...confirmed.map((p) => p.height))
    : null;

  let status = invoice.status;
  if (paidZats >= invoice.expectedZats - policy.underpayZats) {
    status = 'paid';
  } else if (paidZats > 0) {
    status = 'underpaid';
  } else if (chainHeight > invoice.expiresHeight) {
    status = 'expired';
  } else {
    status = 'pending';
  }

  return {
    ...invoice,
    status,
    paidZats,
    pendingZats,
    confirmationsSeen,
    confirmationsNeeded: policy.confirmations,
    paidHeight,
    txids: [...unique.keys()],
  };
}

/**
 * Builds the ZIP 321 payment URI a wallet can be pointed at.
 *
 * @param {string} address
 * @param {number} zatoshis
 * @param {string} [memo] Base64url, already encoded.
 */
export function paymentUri(address, zatoshis, memo) {
  const amount = (zatoshis / COIN).toFixed(8).replace(/0+$/, '').replace(/\.$/, '');
  let uri = `zcash:${address}?amount=${amount}`;
  if (memo) uri += `&memo=${memo}`;
  return uri;
}

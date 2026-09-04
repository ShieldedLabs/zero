/**
 * [zero] @claude Invoice storage.
 *
 * Deliberately an interface with a trivial in-memory default rather than a database. The
 * state a merchant integration keeps is small — an invoice row and one scan cursor — and it
 * belongs in whatever database the company already runs, not in a new one this library
 * imposes. The demo swaps in a localStorage version; a server would implement the same
 * three methods against Postgres.
 *
 * @typedef {object} Invoice
 * @property {string} id             Merchant's own order identifier.
 * @property {number} index          Diversifier index of the address, as minted.
 * @property {string} address        Encoded address shown to the customer.
 * @property {number} expectedZats   Amount expected, in zatoshis.
 * @property {number} createdHeight  Chain height when the invoice was created.
 * @property {number} expiresHeight  Height after which it should not be paid.
 * @property {'pending'|'paid'|'underpaid'|'expired'} status
 * @property {number} paidZats       Total received so far.
 * @property {string[]} txids        Transactions that paid it.
 * @property {number|null} paidHeight Height of the first payment.
 */

/** Invoice storage backed by a plain Map. Loses everything on restart. */
export class MemoryStore {
  constructor() {
    /** @type {Map<string, Invoice>} */
    this.invoices = new Map();
    /** Height below which every block has been scanned. */
    this.cursor = null;
  }

  /** @returns {Promise<Invoice[]>} */
  async allInvoices() {
    return [...this.invoices.values()];
  }

  /** @param {Invoice} invoice */
  async putInvoice(invoice) {
    this.invoices.set(invoice.id, invoice);
  }

  /** @returns {Promise<number|null>} */
  async getCursor() {
    return this.cursor;
  }

  /** @param {number} height */
  async setCursor(height) {
    this.cursor = height;
  }
}

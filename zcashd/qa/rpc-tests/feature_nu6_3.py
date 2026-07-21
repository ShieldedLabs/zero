#!/usr/bin/env python3
# Copyright (c) 2026 The Zcash developers
# Distributed under the MIT software license, see the accompanying
# file COPYING or https://www.opensource.org/licenses/mit-license.php .

# @claude: NU6.3 (Ironwood) end-to-end coverage, filling the ironwood_plan.md
# §8.3 gaps: activation boundary, v6 origination (wallet, miner, and a
# python-built tx exercising the §8.2 codec against a live node), the Ironwood
# RPC surface, a getblocktemplate proposal + submitblock round-trip, the §10.1
# -mineraddress gate, Sapling shielded coinbase post-activation, the S4
# z_shieldtoironwood flow (a non-empty Ironwood bundle: pool/tree/treestate
# accounting, P2P relay to a node without the origination flag, and the python
# codec's Ironwood action digests against a live transaction), and -reindex
# survival of the v6/Ironwood chain state.

from decimal import Decimal
from io import BytesIO
import time

from test_framework.test_framework import BitcoinTestFramework
from test_framework.authproxy import JSONRPCException
from test_framework.blocktools import solve_block_from_template, txs_from_template
from test_framework.mininode import (
    COIN,
    COutPoint,
    CTransaction,
    CTxIn,
    CTxOut,
    ZIP229_VERSION_GROUP_ID,
)
from test_framework.util import (
    OVERWINTER_BRANCH_ID,
    SAPLING_BRANCH_ID,
    BLOSSOM_BRANCH_ID,
    HEARTWOOD_BRANCH_ID,
    CANOPY_BRANCH_ID,
    NU5_BRANCH_ID,
    NU6_BRANCH_ID,
    NU6_1_BRANCH_ID,
    NU6_2_BRANCH_ID,
    NU6_3_BRANCH_ID,
    assert_equal,
    assert_start_raises_init_error,
    bitcoind_processes,
    connect_nodes_bi,
    nuparams,
    nustr,
    start_node,
    start_nodes,
    wait_and_assert_operationid_status,
)

NU6_3_ACTIVATION = 210

# The Ironwood tree reuses Orchard's MerkleCRH, so its empty root equals
# Orchard's (confirmed against ZIP 229/2005 and Zebra 6.0.0-rc.0).
IRONWOOD_TREE_EMPTY_ROOT = "ae2935f1dfd8a24aed7c70df7de3a668eb7a49b1319880dde2bbd9031ae5d82f"
NULL_FIELD = "0000000000000000000000000000000000000000000000000000000000000000"
V5_GROUP_ID = "26a7270a"
V6_GROUP_ID = "d884b698"


class Nu6_3Test(BitcoinTestFramework):

    def __init__(self):
        super().__init__()
        self.num_nodes = 2
        self.cache_behavior = 'clean'

    def base_args(self):
        return [
            nuparams(OVERWINTER_BRANCH_ID, 1),
            nuparams(SAPLING_BRANCH_ID, 1),
            nuparams(BLOSSOM_BRANCH_ID, 1),
            nuparams(HEARTWOOD_BRANCH_ID, 1),
            nuparams(CANOPY_BRANCH_ID, 1),
            nuparams(NU5_BRANCH_ID, 1),
            nuparams(NU6_BRANCH_ID, 1),
            nuparams(NU6_1_BRANCH_ID, 1),
            nuparams(NU6_2_BRANCH_ID, 1),
            nuparams(NU6_3_BRANCH_ID, NU6_3_ACTIVATION),
            '-nurejectoldversions=false',
            '-allowdeprecated=getnewaddress',
            '-allowdeprecated=z_getnewaddress',
            '-txindex',
            '-experimentalfeatures',
            '-lightwalletd',
        ]

    def setup_network(self, split=False):
        # Only node 0 opts into Ironwood origination: node 1 accepting its
        # transactions and blocks shows consensus does not depend on the flag.
        self.nodes = start_nodes(
            self.num_nodes, self.options.tmpdir,
            extra_args=[self.base_args() + ['-ironwoodorigination'],
                        self.base_args()])
        connect_nodes_bi(self.nodes, 0, 1)
        self.is_network_split = False
        self.sync_all()

    def build_v6_transparent_tx(self, node, expiry_height):
        # Build a v6 transparent-spend transaction with the python (§8.2)
        # codec: spend one mature coinbase output back to its own script.
        utxo = next(u for u in node.listunspent() if u['spendable'] and u['amount'] >= 1)
        tx = CTransaction()
        tx.nVersion = 6
        tx.nVersionGroupId = ZIP229_VERSION_GROUP_ID
        tx.nConsensusBranchId = NU6_3_BRANCH_ID
        tx.nExpiryHeight = expiry_height
        tx.vin = [CTxIn(COutPoint(int(utxo['txid'], 16), utxo['vout']), b"", 0xffffffff)]
        fee = Decimal('0.0001')
        tx.vout = [CTxOut(int((utxo['amount'] - fee) * COIN),
                          bytes.fromhex(utxo['scriptPubKey']))]
        return tx

    def run_test(self):
        node0, node1 = self.nodes

        print("Mining to just below the NU6.3 boundary")
        # Stop two short of activation first: mempool acceptance validates
        # against the *next* block height, so v6 rejection must be probed while
        # next-height (209) is still pre-NU6.3.
        node0.generate(NU6_3_ACTIVATION - 10)
        self.sync_all()

        # Pre-activation Orchard funding for the H-P2-1 change-pool phase
        # below: shielding coinbase before activation lands in Orchard — the
        # last legal inflow window for that pool. Done a few blocks below the
        # boundary: a transaction built closer to it has its expiry clamped to
        # activation-1 and is rejected as tx-expiring-soon. // @claude (review H-P2-1)
        self.shield_account = node0.z_getnewaccount()['account']
        self.shield_ua = node0.z_getaddressforaccount(self.shield_account)['address']
        opid = node0.z_shieldcoinbase('*', self.shield_ua)['opid']
        wait_and_assert_operationid_status(node0, opid)
        node0.generate(8)
        self.sync_all()

        # A v6 transaction must be rejected before activation
        # (bad-nu5-tx-version-group-id, §3.2).
        early_tx = self.build_v6_transparent_tx(node0, 0)
        try:
            node0.sendrawtransaction(early_tx.serialize().hex())
            raise AssertionError("v6 transaction accepted before NU6.3 activation")
        except JSONRPCException as e:
            assert 'version-group-id' in e.error['message'], e.error['message']

        node0.generate(1)
        self.sync_all()

        # --- Pre-activation state -------------------------------------------
        info = node0.getblockchaininfo()
        assert_equal(info['blocks'], NU6_3_ACTIVATION - 1)
        nu6_3 = info['upgrades'][nustr(NU6_3_BRANCH_ID)]
        assert_equal(nu6_3['status'], 'pending')
        assert_equal(nu6_3['activationheight'], NU6_3_ACTIVATION)

        blk = node0.getblock(str(NU6_3_ACTIVATION - 1))
        # Ironwood is appended last in valuePools, matching Zebra's ordering.
        assert_equal([p['id'] for p in blk['valuePools']],
                     ['transparent', 'sprout', 'sapling', 'orchard', 'lockbox', 'ironwood'])
        ironwood_pool = blk['valuePools'][-1]
        assert_equal(ironwood_pool['chainValueZat'], 0)
        assert_equal(ironwood_pool['valueDeltaZat'], 0)
        # No Ironwood tree exists yet.
        assert 'ironwood' not in blk['trees']

        # Pre-activation coinbase is v5.
        coinbase = node0.getrawtransaction(blk['tx'][0], 1)
        assert_equal(coinbase['version'], 5)
        assert_equal(coinbase['versiongroupid'], V5_GROUP_ID)
        assert 'ironwood' not in coinbase

        # z_gettreestate: the ironwood section exists (the activation height is
        # configured) but has no tree state yet.
        treestate = node0.z_gettreestate(str(NU6_3_ACTIVATION - 1))
        assert_equal(treestate['ironwood']['commitments']['finalRoot'], NULL_FIELD)
        assert 'finalState' not in treestate['ironwood']['commitments']

        # --- Activation ------------------------------------------------------
        print("Activating NU6.3")
        node0.generate(1)
        self.sync_all()
        for node in self.nodes:
            info = node.getblockchaininfo()
            assert_equal(info['blocks'], NU6_3_ACTIVATION)
            assert_equal(info['upgrades'][nustr(NU6_3_BRANCH_ID)]['status'], 'active')

        # The activation block's coinbase is v6 with empty shielded bundles.
        blk = node0.getblock(str(NU6_3_ACTIVATION))
        coinbase = node0.getrawtransaction(blk['tx'][0], 1)
        assert_equal(coinbase['version'], 6)
        assert_equal(coinbase['versiongroupid'], V6_GROUP_ID)
        assert_equal(coinbase['orchard']['actions'], [])
        assert_equal(coinbase['ironwood']['actions'], [])

        # The Ironwood tree state appears at activation, empty.
        assert_equal(blk['trees']['ironwood']['size'], 0)
        treestate = node0.z_gettreestate(str(NU6_3_ACTIVATION))
        assert_equal(treestate['ironwood']['commitments']['finalRoot'], IRONWOOD_TREE_EMPTY_ROOT)
        assert_equal(treestate['ironwood']['commitments']['finalState'], '000000')
        # Ironwood shares Orchard's MerkleCRH, so the empty roots coincide —
        # compare against the Orchard tree while it was still empty (the
        # H-P2-1 funding above put real commitments into Orchard by now).
        assert_equal(treestate['ironwood']['commitments']['finalRoot'],
                     node0.z_gettreestate('1')['orchard']['commitments']['finalRoot'])

        # Ironwood subtree index is served (empty; a real subtree needs 2^16 notes).
        subtrees = node0.z_getsubtreesbyindex('ironwood', 0)
        assert_equal(subtrees['pool'], 'ironwood')
        assert_equal(subtrees['start_index'], 0)
        assert_equal(len(subtrees['subtrees']), 0)

        # --- Wallet-built v6 transaction (§5.1a origination) -----------------
        print("Sending a wallet-built v6 transaction")
        taddr1 = node1.getnewaddress()
        txid = node0.sendtoaddress(taddr1, Decimal('1.23'))
        wallet_tx = node0.getrawtransaction(txid, 1)
        assert_equal(wallet_tx['version'], 6)
        assert_equal(wallet_tx['versiongroupid'], V6_GROUP_ID)
        self.sync_all()
        node0.generate(1)
        self.sync_all()
        assert_equal(node1.getreceivedbyaddress(taddr1), Decimal('1.23'))

        # --- H-P2-1: post-NU6.3 change-pool routing --------------------------
        print("Spending Orchard and Sapling balances with change (H-P2-1)")
        # Comfortable anchor depth for the pre-activation Orchard note.
        node0.generate(2)
        self.sync_all()

        # Class A: a partial spend of the pre-activation Orchard balance must
        # succeed, with change falling back to Sapling. Before the H-P2-1 fix,
        # change resolution routed to the closed Orchard pool and every
        # change-producing Orchard spend failed at build time with "Cannot
        # create an Orchard output". // @claude
        ua1_account = node1.z_getnewaccount()['account']
        ua1 = node1.z_getaddressforaccount(ua1_account)['address']
        balance0 = node0.z_getbalanceforaccount(self.shield_account)['pools']
        assert 'orchard' in balance0, balance0
        opid = node0.z_sendmany(
            self.shield_ua, [{'address': ua1, 'amount': 3}], 1, None,
            'AllowRevealedAmounts')
        txid_a = wait_and_assert_operationid_status(node0, opid)
        raw = node0.getrawtransaction(txid_a, 1)
        assert_equal(raw['version'], 6)
        # The spend nets value OUT of the closed Orchard pool...
        assert raw['orchard']['valueBalanceZat'] > 0, raw['orchard']
        # ...and both the payment and the change are Sapling outputs.
        assert_equal(len(raw['vShieldedOutput']), 2)
        self.sync_all()
        node0.generate(3)  # confirm + anchor depth for node1's fresh note
        self.sync_all()

        # Class B: a partial unshield from a Sapling-only account (the payment
        # above landed in ua1's Sapling receiver) must also succeed. Before
        # the fix, an AllowRevealedAmounts-or-weaker policy alone routed the
        # change into the closed Orchard pool even with no Orchard notes
        # anywhere in the account. // @claude
        balance1 = node1.z_getbalanceforaccount(ua1_account)['pools']
        assert_equal(sorted(balance1.keys()), ['sapling'])
        taddr_b = node0.getnewaddress()
        opid = node1.z_sendmany(
            ua1, [{'address': taddr_b, 'amount': 1}], 1, None,
            'AllowRevealedRecipients')
        txid_b = wait_and_assert_operationid_status(node1, opid)
        raw = node1.getrawtransaction(txid_b, 1)
        assert_equal(raw['version'], 6)
        assert_equal(raw['orchard']['actions'], [])
        assert_equal(len(raw['vout']), 1)
        assert len(raw['vShieldedSpend']) >= 1, raw
        self.sync_all()
        node0.generate(1)
        self.sync_all()

        # --- Python-built v6 transaction (§8.2 codec against a live node) ----
        print("Submitting a python-built v6 transaction")
        tip_height = node0.getblockcount()
        tx = self.build_v6_transparent_tx(node0, tip_height + 40)
        signed = node0.signrawtransaction(tx.serialize().hex())
        assert signed['complete']

        # Round-trip the node-signed bytes through the python codec and check
        # the python txid against the node's.
        parsed = CTransaction()
        parsed.deserialize(BytesIO(bytes.fromhex(signed['hex'])))
        parsed.calc_sha256()
        assert_equal(parsed.nVersion, 6)
        assert_equal(parsed.nVersionGroupId, ZIP229_VERSION_GROUP_ID)
        assert_equal(parsed.nConsensusBranchId, NU6_3_BRANCH_ID)
        assert_equal(parsed.serialize().hex(), signed['hex'])

        sent_txid = node0.sendrawtransaction(signed['hex'])
        assert_equal(sent_txid, parsed.hash)
        node0.generate(1)
        self.sync_all()
        confirmed = node0.getrawtransaction(sent_txid, 1)
        assert_equal(confirmed['version'], 6)
        assert confirmed['confirmations'] >= 1

        # --- S4: shield transparent funds into the Ironwood pool --------------
        print("Shielding transparent funds into the Ironwood pool")

        # The RPC is double-gated; node 1 runs without -ironwoodorigination.
        try:
            node1.z_shieldtoironwood('00' * 32, 0)
            raise AssertionError("z_shieldtoironwood succeeded without -ironwoodorigination")
        except JSONRPCException as e:
            assert 'disabled' in e.error['message'], e.error['message']

        utxo = next(u for u in node0.listunspent() if u['spendable'] and u['amount'] >= 1)
        conventional_fee = Decimal('0.00015')  # ZIP 317: 3 logical actions
        result = node0.z_shieldtoironwood(utxo['txid'], utxo['vout'])
        shield_txid = result['txid']
        assert_equal(result['fee'], conventional_fee)
        assert_equal(result['shielded'], utxo['amount'] - conventional_fee)
        # The recovery key material exists only in this response, by design.
        assert_equal(len(result['recoveryMnemonic'].split()), 24)
        shielded_zat = int((utxo['amount'] - conventional_fee) * COIN)

        # The transaction carries a real Ironwood bundle (one output, padded to
        # two actions) and an empty Orchard slot.
        raw = node0.getrawtransaction(shield_txid, 1)
        assert_equal(raw['version'], 6)
        assert_equal(raw['versiongroupid'], V6_GROUP_ID)
        assert_equal(raw['orchard']['actions'], [])
        ironwood = raw['ironwood']
        assert_equal(len(ironwood['actions']), 2)
        assert_equal(ironwood['valueBalanceZat'], -shielded_zat)
        assert_equal(ironwood['flags']['enableCrossAddress'], True)

        # First live verification of the python codec's non-empty Ironwood
        # action digests (§8.2 ported them before any Ironwood builder
        # existed): parse the node's bytes, re-serialize byte-identically, and
        # reproduce the node's txid through the v6 digest tree.
        parsed = CTransaction()
        parsed.deserialize(BytesIO(bytes.fromhex(raw['hex'])))
        assert_equal(parsed.serialize().hex(), raw['hex'])
        parsed.calc_sha256()
        assert_equal(parsed.hash, shield_txid)

        # The v6-with-Ironwood-bundle transaction relays over P2P: node 1
        # (no origination flag) accepts it into its mempool, then the block.
        self.sync_all()
        assert shield_txid in node1.getrawmempool()
        node0.generate(1)
        self.sync_all()

        # Both nodes agree on the pool value, tree size, and non-empty root.
        for node in self.nodes:
            blk = node.getblock(node.getbestblockhash())
            pool = blk['valuePools'][-1]
            assert_equal(pool['id'], 'ironwood')
            assert_equal(pool['valueDeltaZat'], shielded_zat)
            assert_equal(pool['chainValueZat'], shielded_zat)
            assert_equal(blk['trees']['ironwood']['size'], 2)
            ts = node.z_gettreestate(str(-1))
            root = ts['ironwood']['commitments']['finalRoot']
            assert root not in (IRONWOOD_TREE_EMPTY_ROOT, NULL_FIELD), root

        # --- getblocktemplate proposal + submitblock round-trip (§5.4) -------
        print("Round-tripping a getblocktemplate block through submitblock")
        tmpl = node0.getblocktemplate()
        coinbase_tx, other_txs = txs_from_template(tmpl)
        assert_equal(coinbase_tx.nVersion, 6)
        assert_equal(coinbase_tx.nVersionGroupId, ZIP229_VERSION_GROUP_ID)
        block = solve_block_from_template(tmpl, coinbase_tx, other_txs)
        proposal = node0.getblocktemplate({'mode': 'proposal', 'data': block.serialize().hex()})
        assert proposal is None, proposal
        result = node0.submitblock(block.serialize().hex())
        assert result is None, result
        self.sync_all()
        assert_equal(node0.getbestblockhash(), block.hash)
        assert_equal(node1.getbestblockhash(), block.hash)

        # --- §10.1 -mineraddress gate ----------------------------------------
        # An Orchard-preferring UA is refused at startup; a wallet-owned
        # Sapling address passes the gate and mines a v6 shielded coinbase.
        print("Checking the Orchard -mineraddress startup gate")
        account = node1.z_getnewaccount()['account']
        orchard_ua = node1.z_getaddressforaccount(account)['address']
        sapling_addr = node1.z_getnewaddress('sapling')
        node1.stop()
        bitcoind_processes[1].wait()
        assert_start_raises_init_error(
            1, self.options.tmpdir,
            self.base_args() + ['-mineraddress=' + orchard_ua],
            'shielded mining addresses have been disabled')

        print("Mining a v6 Sapling shielded coinbase")
        self.nodes[1] = node1 = start_node(
            1, self.options.tmpdir,
            self.base_args() + ['-mineraddress=' + sapling_addr])
        connect_nodes_bi(self.nodes, 0, 1)
        self.sync_all()
        node1.generate(1)
        self.sync_all()
        blk = node1.getblock(node1.getbestblockhash())
        coinbase = node1.getrawtransaction(blk['tx'][0], 1)
        assert_equal(coinbase['version'], 6)
        assert_equal(coinbase['versiongroupid'], V6_GROUP_ID)
        assert_equal(len(coinbase['vShieldedOutput']), 1)
        assert_equal(coinbase['orchard']['actions'], [])
        assert_equal(coinbase['ironwood']['actions'], [])

        # --- -reindex survival of the v6/Ironwood chain (§4.5/§8.3) ----------
        print("Reindexing across the NU6.3 boundary")
        best = node0.getbestblockhash()
        best_treestate = node0.z_gettreestate(str(-1))
        node0.stop()
        bitcoind_processes[0].wait()
        self.nodes[0] = node0 = start_node(
            0, self.options.tmpdir, self.base_args() + ['-reindex'])
        deadline = time.time() + 300
        while node0.getbestblockhash() != best:
            assert time.time() < deadline, "reindex did not reach the previous tip"
            time.sleep(1)
        # The replay reproduced the identical Ironwood tree state — including
        # the non-empty commitments from the z_shieldtoironwood transaction,
        # replayed without the origination flag (validation is unconditional).
        assert_equal(node0.z_gettreestate(str(-1)), best_treestate)
        connect_nodes_bi(self.nodes, 0, 1)
        self.sync_all()


if __name__ == '__main__':
    Nu6_3Test().main()

#include <gtest/gtest.h>
#include <gmock/gmock.h>

#include "main.h"
#include "primitives/transaction.h"
#include "consensus/merkle.h"
#include "consensus/validation.h"
#include "transaction_builder.h"
#include "util/test.h"
#include "gtest/utils.h"
#include "wallet/asyncrpcoperation_common.h"
#include "wallet/asyncrpcoperation_shieldcoinbase.h"
#include "wallet/asyncrpcoperation_sendmany.h"
#include "zcash/Address.hpp" // @claude for UnifiedAddress in the XR-4 test
#include "zcash/JoinSplit.hpp"
#include "zcash/address/mnemonic.h" // @claude for MnemonicSeed::Random in the XR-4 test
#include "zcash/memo.h"
#include "zip317.h"

#include <librustzcash.h>
#include <rust/bridge.h>
#include <rust/ed25519.h>

namespace
{
bool find_error(const UniValue& objError, const std::string& expected) {
    return find_value(objError, "message").get_str().find(expected) != string::npos;
}

CWalletTx FakeWalletTx() {
    CMutableTransaction mtx;
    mtx.vout.resize(1);
    mtx.vout[0].nValue = 1;
    return CWalletTx(nullptr, mtx);
}

/// Expects that the fee calculated during transaction construction matches the fee used by block
/// construction. It allows the fee included in the transaction to be `MARGINAL_FEE` higher than the
/// fee expected by block construction.
void ExpectConsistentFee(const TransactionStrategy& strategy, const TransactionEffects& effects)
{
    auto buildResult = effects.ApproveAndBuild(
            Params(),
            *pwalletMain,
            chainActive,
            strategy);
    auto tx = buildResult.GetTxOrThrow();

    auto expectedFee = tx.GetConventionalFee();
    // Allow one incremental fee tick of buffer above the conventional fee.
    EXPECT_TRUE(effects.GetFee() == expectedFee || effects.GetFee() == expectedFee + MARGINAL_FEE)
        << "effects.GetFee() = " << effects.GetFee() << std::endl
        << "tx.GetConventionalFee() = " << expectedFee;
}
}

TEST(WalletRPCTests, PrepareTransaction)
{
    LoadProofParameters();
    SelectParams(CBaseChainParams::TESTNET);

    LoadGlobalWallet();

    RegtestActivateSapling();
    {
        LOCK2(cs_main, pwalletMain->cs_wallet);

        if (!pwalletMain->HaveMnemonicSeed()) {
            pwalletMain->GenerateNewSeed();
        }

        KeyIO keyIO(Params());
        // add keys manually
        auto taddr = pwalletMain->GenerateNewKey(true).GetID();
        auto pa = pwalletMain->GenerateNewLegacySaplingZKey();

        const Consensus::Params& consensusParams = Params().GetConsensus();

        int nextBlockHeight = chainActive.Height() + 1;

        // Add a fake transaction to the wallet
        CMutableTransaction mtx = CreateNewContextualCMutableTransaction(consensusParams, nextBlockHeight, false);
        CScript scriptPubKey = CScript() << OP_DUP << OP_HASH160 << ToByteVector(taddr) << OP_EQUALVERIFY << OP_CHECKSIG;
        mtx.vout.push_back(CTxOut(5 * COIN, scriptPubKey));

        CWalletTx wtx(pwalletMain, mtx);
        pwalletMain->LoadWalletTx(wtx);

        // Fake-mine the transaction
        EXPECT_EQ(-1, chainActive.Height());
        CBlock block;
        block.vtx.push_back(wtx);
        block.hashMerkleRoot = BlockMerkleRoot(block);
        auto blockHash = block.GetHash();
        CBlockIndex fakeIndex {block};
        mapBlockIndex.insert(std::make_pair(blockHash, &fakeIndex));
        chainActive.SetTip(&fakeIndex);
        EXPECT_TRUE(chainActive.Contains(&fakeIndex));
        EXPECT_EQ(0, chainActive.Height());
        wtx.SetMerkleBranch(block);
        pwalletMain->LoadWalletTx(wtx);

        WalletTxBuilder builder(Params(), minRelayTxFee);

        auto selector = CWallet::LegacyTransparentZTXOSelector(
                true,
                TransparentCoinbasePolicy::Disallow);

        { // send from legacy account with change, but insufficient policy
            auto saplingKey = pwalletMain->GenerateNewLegacySaplingZKey();
            Payment saplingPayment(saplingKey, 4 * COIN, std::nullopt);
            std::vector<Payment> payments {saplingPayment};

            TransactionStrategy strategy(PrivacyPolicy::AllowRevealedSenders);

            SpendableInputs inputs;
            inputs.utxos.emplace_back(&wtx, 0, std::nullopt, 100, true);

            (void)builder.PrepareTransaction(
                    *pwalletMain,
                    selector,
                    inputs,
                    payments,
                    chainActive,
                    strategy,
                    std::nullopt,
                    1)
                .map_error([&](const auto& err) {
                    examine(err, match {
                        [](AddressResolutionError are) {
                            EXPECT_EQ(are, AddressResolutionError::TransparentChangeNotAllowed);
                        },
                        [&](const auto& e) {
                            try {
                                ThrowInputSelectionError(e, selector, strategy);
                            } catch (const UniValue& value) {
                                FAIL() << value.write();
                            }
                        },
                    });
                })
                .map([](const auto&) {
                    FAIL() << "Expected an error";
                });
        }

        // Tear down
        chainActive.SetTip(NULL);
        mapBlockIndex.erase(blockHash);

    }
    // Revert to default
    RegtestDeactivateSapling();
    UnloadGlobalWallet();
}

// review XR-4: from NU6.3 the Orchard pool no longer accepts new outputs, so
// payment resolution must stop preferring a UA's Orchard receiver — falling
// back to a usable Sapling receiver, and failing at *resolution* time with a
// migration-pointing error for Orchard-only recipients (instead of resolving
// to Orchard and failing late in the builder). // @claude
TEST(WalletRPCTests, OrchardResolutionAfterNU6_3)
{
    SelectParams(CBaseChainParams::REGTEST);
    RegtestActivateNU6point3();
    LoadGlobalWallet();
    {
        LOCK2(cs_main, pwalletMain->cs_wallet);

        if (!pwalletMain->HaveMnemonicSeed()) {
            pwalletMain->GenerateNewSeed();
        }

        // Fund the wallet with a fake-mined transparent UTXO (same scaffold as
        // the PrepareTransaction test above).
        auto taddr = pwalletMain->GenerateNewKey(true).GetID();
        int nextBlockHeight = chainActive.Height() + 1;
        CMutableTransaction mtx = CreateNewContextualCMutableTransaction(
                Params().GetConsensus(), nextBlockHeight, false);
        CScript scriptPubKey = CScript() << OP_DUP << OP_HASH160 << ToByteVector(taddr) << OP_EQUALVERIFY << OP_CHECKSIG;
        mtx.vout.push_back(CTxOut(5 * COIN, scriptPubKey));
        CWalletTx wtx(pwalletMain, mtx);
        pwalletMain->LoadWalletTx(wtx);

        CBlock block;
        block.vtx.push_back(wtx);
        block.hashMerkleRoot = BlockMerkleRoot(block);
        auto blockHash = block.GetHash();
        CBlockIndex fakeIndex {block};
        mapBlockIndex.insert(std::make_pair(blockHash, &fakeIndex));
        chainActive.SetTip(&fakeIndex);
        wtx.SetMerkleBranch(block);
        pwalletMain->LoadWalletTx(wtx);

        WalletTxBuilder builder(Params(), minRelayTxFee);
        auto selector = CWallet::LegacyTransparentZTXOSelector(
                true,
                TransparentCoinbasePolicy::Disallow);
        // Fully permissive so that neither the Sapling fallback nor transparent
        // change is blocked by policy — isolating the pool-closure behavior.
        TransactionStrategy strategy(PrivacyPolicy::AllowFullyTransparent);

        SpendableInputs inputs;
        inputs.utxos.emplace_back(&wtx, 0, std::nullopt, 100, true);

        auto coinType = Params().BIP44CoinType();
        auto orchardAddr =
            libzcash::OrchardSpendingKey::ForAccount(MnemonicSeed::Random(coinType), coinType, 0)
                .ToFullViewingKey().GetChangeAddress();
        auto saplingAddr = pwalletMain->GenerateNewLegacySaplingZKey();

        { // An Orchard-only UA must fail at resolution time with OrchardPoolClosed.
            auto ua = libzcash::UnifiedAddress::ForSingleReceiver(orchardAddr);
            std::vector<Payment> payments { Payment(ua, 1 * COIN, std::nullopt) };
            (void)builder.PrepareTransaction(
                    *pwalletMain, selector, inputs, payments, chainActive, strategy, std::nullopt, 1)
                .map_error([](const auto& err) {
                    EXPECT_TRUE(examine(err, match {
                        [](const AddressResolutionError& are) {
                            return are == AddressResolutionError::OrchardPoolClosed;
                        },
                        [](const auto&) { return false; },
                    })) << "expected AddressResolutionError::OrchardPoolClosed";
                })
                .map([](const auto&) { FAIL() << "expected resolution to fail"; });
        }

        { // A UA with both Orchard and Sapling receivers must resolve to Sapling.
            // EXPECT (not ASSERT) throughout: an early return here would skip the
            // tear-down below and poison chainActive for the following tests.
            libzcash::UnifiedAddress ua;
            ua.AddReceiver(orchardAddr);
            ua.AddReceiver(saplingAddr);
            std::vector<Payment> payments { Payment(ua, 1 * COIN, std::nullopt) };
            auto res = builder.PrepareTransaction(
                    *pwalletMain, selector, inputs, payments, chainActive, strategy, std::nullopt, 1);
            EXPECT_TRUE(res.has_value()) << "dual-receiver UA payment failed to resolve";
            if (res.has_value()) {
                // The resolved list also carries the (internal) change payment;
                // only the external payment's receiver choice is under test.
                size_t externalCount = 0;
                for (const auto& rp : res.value().GetPayments().GetResolvedPayments()) {
                    if (!rp.isInternal) {
                        externalCount++;
                        EXPECT_TRUE(std::holds_alternative<libzcash::SaplingPaymentAddress>(rp.address))
                            << "post-NU6.3 resolution must select the Sapling receiver, not Orchard";
                    }
                }
                EXPECT_EQ(externalCount, 1u);
            }
        }

        // Tear down
        chainActive.SetTip(NULL);
        mapBlockIndex.erase(blockHash);
    }
    RegtestDeactivateNU6point3();
    UnloadGlobalWallet();
}

// Shared scaffold for the H-P2-1 change-pool tests: fund the wallet with a
// fake-mined transparent UTXO, prepare a 1-ZEC Sapling-recipient payment from
// a unified-account selector under AllowFullyTransparent (a superset of the
// AllowRevealedAmounts trigger), and return the resolved *internal change*
// address. The caller asserts which pool it landed in; the network upgrade
// state is whatever the caller activated. Assumes the global wallet is
// loaded. // @claude (review H-P2-1)
static std::optional<libzcash::RecipientAddress> ResolveChangeForAccountSelector()
{
    std::optional<libzcash::RecipientAddress> changeAddr;
    LOCK2(cs_main, pwalletMain->cs_wallet);

    if (!pwalletMain->HaveMnemonicSeed()) {
        pwalletMain->GenerateNewSeed();
    }
    auto ufvkpair = pwalletMain->GenerateNewUnifiedSpendingKey();
    auto account = ufvkpair.second;

    // Fund the wallet with a fake-mined transparent UTXO (same scaffold as
    // the resolution tests above).
    auto taddr = pwalletMain->GenerateNewKey(true).GetID();
    int nextBlockHeight = chainActive.Height() + 1;
    CMutableTransaction mtx = CreateNewContextualCMutableTransaction(
            Params().GetConsensus(), nextBlockHeight, false);
    CScript scriptPubKey = CScript() << OP_DUP << OP_HASH160 << ToByteVector(taddr) << OP_EQUALVERIFY << OP_CHECKSIG;
    mtx.vout.push_back(CTxOut(5 * COIN, scriptPubKey));
    CWalletTx wtx(pwalletMain, mtx);
    pwalletMain->LoadWalletTx(wtx);

    CBlock block;
    block.vtx.push_back(wtx);
    block.hashMerkleRoot = BlockMerkleRoot(block);
    auto blockHash = block.GetHash();
    CBlockIndex fakeIndex {block};
    mapBlockIndex.insert(std::make_pair(blockHash, &fakeIndex));
    chainActive.SetTip(&fakeIndex);
    wtx.SetMerkleBranch(block);
    pwalletMain->LoadWalletTx(wtx);

    WalletTxBuilder builder(Params(), minRelayTxFee);
    auto selector = pwalletMain->ZTXOSelectorForAccount(
            account,
            true,
            TransparentCoinbasePolicy::Disallow,
            {libzcash::ReceiverType::P2PKH,
             libzcash::ReceiverType::Sapling,
             libzcash::ReceiverType::Orchard});
    EXPECT_TRUE(selector.has_value());
    TransactionStrategy strategy(PrivacyPolicy::AllowFullyTransparent);

    SpendableInputs inputs;
    inputs.utxos.emplace_back(&wtx, 0, std::nullopt, 100, true);

    auto saplingRecipient = pwalletMain->GenerateNewLegacySaplingZKey();
    std::vector<Payment> payments { Payment(saplingRecipient, 1 * COIN, std::nullopt) };

    if (selector.has_value()) {
        auto res = builder.PrepareTransaction(
                *pwalletMain, selector.value(), inputs, payments, chainActive, strategy,
                std::nullopt, 1);
        EXPECT_TRUE(res.has_value()) << "payment with change failed to resolve";
        if (res.has_value()) {
            for (const auto& rp : res.value().GetPayments().GetResolvedPayments()) {
                if (rp.isInternal) {
                    changeAddr = rp.address;
                }
            }
        }
    }

    // Tear down (EXPECTs above keep us reaching this on failure).
    chainActive.SetTip(NULL);
    mapBlockIndex.erase(blockHash);
    return changeAddr;
}

// review H-P2-1: from NU6.3 the Orchard pool no longer accepts new outputs —
// change included. Change resolution for an account whose receivers include
// Orchard must fall back to Sapling; before the fix the Orchard-first change
// preference resolved change into the closed pool (under AllowRevealedAmounts
// or with selected Orchard notes) and every change-producing spend then
// failed at build time. // @claude
TEST(WalletRPCTests, ChangePoolExcludesOrchardAfterNU6_3)
{
    SelectParams(CBaseChainParams::REGTEST);
    RegtestActivateNU6point3();
    LoadGlobalWallet();
    {
        auto changeAddr = ResolveChangeForAccountSelector();
        EXPECT_TRUE(changeAddr.has_value()) << "no internal change payment was resolved";
        if (changeAddr.has_value()) {
            EXPECT_TRUE(std::holds_alternative<libzcash::SaplingPaymentAddress>(changeAddr.value()))
                << "post-NU6.3 change must resolve to Sapling, not the closed Orchard pool";
        }
    }
    RegtestDeactivateNU6point3();
    UnloadGlobalWallet();
}

// Control for the gate above: before NU6.3 the Orchard-first change
// preference is intact — the same scaffold resolves change to Orchard. This
// pins that the H-P2-1 gate switches exactly at activation and does not
// regress pre-activation behavior. // @claude
TEST(WalletRPCTests, ChangePoolPrefersOrchardBeforeNU6_3)
{
    SelectParams(CBaseChainParams::REGTEST);
    RegtestActivateNU5();
    LoadGlobalWallet();
    {
        auto changeAddr = ResolveChangeForAccountSelector();
        EXPECT_TRUE(changeAddr.has_value()) << "no internal change payment was resolved";
        if (changeAddr.has_value()) {
            EXPECT_TRUE(std::holds_alternative<libzcash::OrchardRawAddress>(changeAddr.value()))
                << "pre-NU6.3 change must still prefer the Orchard pool";
        }
    }
    RegtestDeactivateNU5();
    UnloadGlobalWallet();
}

// TODO: test private methods
TEST(WalletRPCTests, RPCZMergeToAddressInternals)
{
    LoadProofParameters();

    SelectParams(CBaseChainParams::TESTNET);
    LoadGlobalWallet();

    const Consensus::Params& consensusParams = Params().GetConsensus();
    KeyIO keyIO(Params());
    {
    LOCK2(cs_main, pwalletMain->cs_wallet);

    EXPECT_EQ(-1, chainActive.Height());
    CBlock block;
    block.hashMerkleRoot = BlockMerkleRoot(block);
    auto blockHash = block.GetHash();
    CBlockIndex fakeIndex {block};
    mapBlockIndex.insert(std::make_pair(blockHash, &fakeIndex));
    chainActive.SetTip(&fakeIndex);
    EXPECT_TRUE(chainActive.Contains(&fakeIndex));
    EXPECT_EQ(0, chainActive.Height());

    // Mutable tx containing contextual information we need to build tx
    // We removed the ability to create pre-Sapling Sprout proofs, so we can
    // only create Sapling-onwards transactions.
    int nHeight = consensusParams.vUpgrades[Consensus::UPGRADE_SAPLING].nActivationHeight;
    CMutableTransaction mtx = CreateNewContextualCMutableTransaction(consensusParams, nHeight + 1, false);

    // Add keys manually
    auto taddr = pwalletMain->GenerateNewKey(true).GetID();
    std::string taddr_string = keyIO.EncodeDestination(taddr);

    NetAmountRecipient taddr1(keyIO.DecodePaymentAddress(taddr_string).value(), std::nullopt);
    auto sproutKey = pwalletMain->GenerateNewSproutZKey();
    NetAmountRecipient zaddr1(sproutKey, std::nullopt);

    auto saplingKey = pwalletMain->GenerateNewLegacySaplingZKey();
    NetAmountRecipient zaddr2(saplingKey, std::nullopt);

    WalletTxBuilder builder(Params(), minRelayTxFee);
    auto selector = CWallet::LegacyTransparentZTXOSelector(
            true,
            TransparentCoinbasePolicy::Disallow);
    TransactionStrategy strategy(PrivacyPolicy::AllowRevealedSenders);

    SpendableInputs inputs;
    auto wtx = FakeWalletTx();
    inputs.utxos.emplace_back(&wtx, 0, std::nullopt, 100, true);

    // Can’t send to Sprout
    (void)builder.PrepareTransaction(
            *pwalletMain,
            selector,
            inputs,
            zaddr1,
            chainActive,
            strategy,
            0,
            1)
        .map_error([](const auto& err) {
            EXPECT_TRUE(examine(err, match {
                [](const AddressResolutionError& are) {
                    return are == AddressResolutionError::SproutRecipientsNotSupported;
                },
                [](const auto&) { return false; },
            }));
        })
        .map([](const auto&) { EXPECT_TRUE(false); });

    // Insufficient funds
    (void)builder.PrepareTransaction(
            *pwalletMain,
            selector,
            inputs,
            zaddr2,
            chainActive,
            strategy,
            std::nullopt,
            1)
        .map_error([](const auto& err) {
            EXPECT_TRUE(examine(err, match {
                [](const InvalidFundsError& ife) {
                    return std::holds_alternative<InsufficientFundsError>(ife.reason);
                },
                [](const auto&) { return false; },
            }));
        })
        .map([](const auto&) { EXPECT_TRUE(false); });

    // Tear down
    chainActive.SetTip(NULL);
    mapBlockIndex.erase(blockHash);

    }
    UnloadGlobalWallet();
}

TEST(WalletRPCTests, RPCZsendmanyTaddrToSapling)
{
    LoadProofParameters();
    SelectParams(CBaseChainParams::TESTNET);

    LoadGlobalWallet();

    RegtestActivateSapling();
    {
    LOCK2(cs_main, pwalletMain->cs_wallet);

    if (!pwalletMain->HaveMnemonicSeed()) {
        pwalletMain->GenerateNewSeed();
    }

    KeyIO keyIO(Params());
    // add keys manually
    auto taddr = pwalletMain->GenerateNewKey(true).GetID();
    auto pa = pwalletMain->GenerateNewLegacySaplingZKey();

    const Consensus::Params& consensusParams = Params().GetConsensus();
    auto rustNetwork = Params().RustNetwork();

    int nextBlockHeight = chainActive.Height() + 1;

    // Add a fake transaction to the wallet
    CMutableTransaction mtx = CreateNewContextualCMutableTransaction(consensusParams, nextBlockHeight, false);
    CScript scriptPubKey = CScript() << OP_DUP << OP_HASH160 << ToByteVector(taddr) << OP_EQUALVERIFY << OP_CHECKSIG;
    mtx.vout.push_back(CTxOut(5 * COIN, scriptPubKey));
    CWalletTx wtx(pwalletMain, mtx);
    pwalletMain->LoadWalletTx(wtx);

    // Fake-mine the transaction
    EXPECT_EQ(-1, chainActive.Height());
    CBlock block;
    block.vtx.push_back(wtx);
    block.hashMerkleRoot = BlockMerkleRoot(block);
    auto blockHash = block.GetHash();
    CBlockIndex fakeIndex {block};
    mapBlockIndex.insert(std::make_pair(blockHash, &fakeIndex));
    chainActive.SetTip(&fakeIndex);
    EXPECT_TRUE(chainActive.Contains(&fakeIndex));
    EXPECT_EQ(0, chainActive.Height());
    wtx.SetMerkleBranch(block);
    pwalletMain->LoadWalletTx(wtx);

    // Context that z_sendmany requires
    auto builder = WalletTxBuilder(Params(), minRelayTxFee);
    mtx = CreateNewContextualCMutableTransaction(consensusParams, nextBlockHeight, false);

    // we need AllowFullyTransparent because the transaction will result
    // in transparent change as a consequence of sending from a legacy taddr
    TransactionStrategy strategy(PrivacyPolicy::AllowFullyTransparent);
    auto selector = pwalletMain->ZTXOSelectorForAddress(
            taddr,
            true,
            TransparentCoinbasePolicy::Disallow,
            strategy.PermittedAccountSpendingPolicy()).value();
    std::vector<Payment> recipients = { Payment(pa, 1*COIN, Memo::FromBytes({0xAB, 0xCD})) };
    std::shared_ptr<AsyncRPCOperation> operation(new AsyncRPCOperation_sendmany(std::move(builder), selector, recipients, 0, 0, strategy, std::nullopt));
    std::shared_ptr<AsyncRPCOperation_sendmany> ptr = std::dynamic_pointer_cast<AsyncRPCOperation_sendmany> (operation);

    // Enable test mode so tx is not sent
    static_cast<AsyncRPCOperation_sendmany *>(operation.get())->testmode = true;

    // Generate the Sapling shielding transaction
    operation->main();
    if (!operation->isSuccess()) {
        FAIL() << operation->getErrorMessage();
    }

    // Get the transaction
    auto result = operation->getResult();
    ASSERT_TRUE(result.isObject());
    auto hexTx = result["hex"].getValStr();
    CDataStream ss(ParseHex(hexTx), SER_NETWORK, PROTOCOL_VERSION);
    CTransaction tx;
    ss >> tx;
    ASSERT_NE(tx.GetSaplingOutputsCount(), 0);

    auto accountKey = pwalletMain->GetLegacyAccountKey().ToAccountPubKey();
    auto ovks = accountKey.GetOVKsForShielding();

    auto extDecryptSucceeded = 0;
    auto extDecryptFailed = 0;
    for (auto& output: tx.GetSaplingOutputs()) {
        auto enc_ciphertext = output.enc_ciphertext();
        auto out_ciphertext = output.out_ciphertext();
        auto cv = output.cv();
        auto cmu = output.cmu();
        auto ephemeral_key = output.ephemeral_key();

        // We shouldn't be able to decrypt with the empty ovk
        EXPECT_THROW(wallet::try_sapling_output_recovery(
            *rustNetwork,
            nextBlockHeight,
            uint256().GetRawBytes(),
            {
                cv,
                cmu,
                ephemeral_key,
                enc_ciphertext,
                out_ciphertext,
            }), rust::Error);

        // We shouldn't be able to decrypt with a random ovk
        EXPECT_THROW(wallet::try_sapling_output_recovery(
            *rustNetwork,
            nextBlockHeight,
            random_uint256().GetRawBytes(),
            {
                cv,
                cmu,
                ephemeral_key,
                enc_ciphertext,
                out_ciphertext,
            }), rust::Error);

        // We should not be able to decrypt with the internal change OVK for shielding
        EXPECT_THROW(wallet::try_sapling_output_recovery(
            *rustNetwork,
            nextBlockHeight,
            ovks.first.GetRawBytes(),
            {
                cv,
                cmu,
                ephemeral_key,
                enc_ciphertext,
                out_ciphertext,
            }), rust::Error);

        // We should be able to decrypt one of the outputs with the external OVK for shielding.
        try {
            wallet::try_sapling_output_recovery(
                *rustNetwork,
                nextBlockHeight,
                ovks.second.GetRawBytes(),
                {
                    cv,
                    cmu,
                    ephemeral_key,
                    enc_ciphertext,
                    out_ciphertext,
                });
            extDecryptSucceeded += 1;
        } catch (...) {
            extDecryptFailed += 1;
        }
    }
    EXPECT_EQ(extDecryptSucceeded, 1);
    EXPECT_EQ(extDecryptFailed, 1);

    // Tear down
    chainActive.SetTip(NULL);
    mapBlockIndex.erase(blockHash);

    }
    // Revert to default
    RegtestDeactivateSapling();
    UnloadGlobalWallet();
}

TEST(WalletRPCTests, ZIP317Fee)
{
    LoadProofParameters();
    SelectParams(CBaseChainParams::TESTNET);

    LoadGlobalWallet();

    RegtestActivateSapling();
    {
        LOCK2(cs_main, pwalletMain->cs_wallet);

        if (!pwalletMain->HaveMnemonicSeed()) {
            pwalletMain->GenerateNewSeed();
        }

        KeyIO keyIO(Params());
        // add keys manually
        auto taddr = pwalletMain->GenerateNewKey(true).GetID();
        auto pa = pwalletMain->GenerateNewLegacySaplingZKey();

        const Consensus::Params& consensusParams = Params().GetConsensus();

        int nextBlockHeight = chainActive.Height() + 1;

        // Add a fake transaction to the wallet
        CMutableTransaction mtx = CreateNewContextualCMutableTransaction(consensusParams, nextBlockHeight, false);
        CScript scriptPubKey = CScript() << OP_DUP << OP_HASH160 << ToByteVector(taddr) << OP_EQUALVERIFY << OP_CHECKSIG;
        size_t utxoCount = 100;
        for (size_t i = 0; i < utxoCount; i++) {
            mtx.vout.push_back(CTxOut(5 * COIN, scriptPubKey));
        }
        CWalletTx wtx(pwalletMain, mtx);
        pwalletMain->LoadWalletTx(wtx);

        // Fake-mine the transaction
        EXPECT_EQ(-1, chainActive.Height());
        CBlock block;
        block.vtx.push_back(wtx);
        block.hashMerkleRoot = BlockMerkleRoot(block);
        auto blockHash = block.GetHash();
        CBlockIndex fakeIndex {block};
        mapBlockIndex.insert(std::make_pair(blockHash, &fakeIndex));
        chainActive.SetTip(&fakeIndex);
        EXPECT_TRUE(chainActive.Contains(&fakeIndex));
        EXPECT_EQ(0, chainActive.Height());
        wtx.SetMerkleBranch(block);
        pwalletMain->LoadWalletTx(wtx);

        // Add keys manually
        std::string taddr_string = keyIO.EncodeDestination(taddr);

        WalletTxBuilder builder(Params(), minRelayTxFee);

        auto selector = CWallet::LegacyTransparentZTXOSelector(
                true,
                TransparentCoinbasePolicy::Disallow);

        { // test transparent inputs to NetAmountRecipient
            auto saplingKey = pwalletMain->GenerateNewLegacySaplingZKey();
            NetAmountRecipient zaddr(saplingKey, std::nullopt);

            TransactionStrategy strategy(PrivacyPolicy::AllowRevealedSenders);

            SpendableInputs inputs;
            for (size_t i = 0; i < utxoCount; i++) {
                CTxDestination address;
                ExtractDestination(scriptPubKey, address);
                inputs.utxos.emplace_back(&wtx, i, address, 100, true);
            }

            auto effects = builder.PrepareTransaction(
                    *pwalletMain,
                    selector,
                    inputs,
                    zaddr,
                    chainActive,
                    strategy,
                    std::nullopt,
                    1)
            .map_error([&](const auto& err) {
                try {
                    ThrowInputSelectionError(err, selector, strategy);
                } catch (const UniValue& value) {
                    FAIL() << value.write();
                }
            })
            .value();

            ExpectConsistentFee(strategy, effects);
        }

        { // test transparent inputs to Payment vector
            auto saplingKey = pwalletMain->GenerateNewLegacySaplingZKey();
            Payment saplingPayment(saplingKey, 200 * COIN, std::nullopt);
            auto saplingKey2 = pwalletMain->GenerateNewLegacySaplingZKey();
            Payment saplingPayment2(saplingKey2, 200 * COIN, std::nullopt);
            std::vector<Payment> payments {saplingPayment, saplingPayment2};

            TransactionStrategy strategy(PrivacyPolicy::AllowFullyTransparent);

            SpendableInputs inputs;
            for (size_t i = 0; i < utxoCount; i++) {
                CTxDestination address;
                ExtractDestination(scriptPubKey, address);
                inputs.utxos.emplace_back(&wtx, i, address, 100, true);
            }

            auto effects = builder.PrepareTransaction(
                    *pwalletMain,
                    selector,
                    inputs,
                    payments,
                    chainActive,
                    strategy,
                    std::nullopt,
                    1)
                .map_error([&](const auto& err) {
                    try {
                        ThrowInputSelectionError(err, selector, strategy);
                    } catch (const UniValue& value) {
                        FAIL() << value.write();
                    }
                })
                .value();

            ExpectConsistentFee(strategy, effects);
        }

        // Tear down
        chainActive.SetTip(NULL);
        mapBlockIndex.erase(blockHash);

    }
    // Revert to default
    RegtestDeactivateSapling();
    UnloadGlobalWallet();
}

#include <gtest/gtest.h>
#include <gmock/gmock.h>

#include "main.h"
#include "policy/policy.h"
#include "primitives/transaction.h"
#include "consensus/validation.h"
#include "consensus/upgrades.h"
#include "script/standard.h"
#include "transaction_builder.h"
#include "gtest/utils.h"
#include "test/test_util.h"
#include "util/test.h"
#include "zcash/JoinSplit.hpp"
#include "zip317.h"

#include <librustzcash.h>
#include <rust/bridge.h>
#include <rust/ed25519.h>
#include <rust/orchard.h>

// Subclass of CTransaction which doesn't call UpdateHash when constructing
// from a CMutableTransaction.  This enables us to create a CTransaction
// with bad values which normally trigger an exception during construction.
class UNSAFE_CTransaction : public CTransaction {
    public:
        UNSAFE_CTransaction(const CMutableTransaction &tx) : CTransaction(tx, true) {}
};

// The canonical Orchard account used across these Orchard/v6 fixtures: a seed of
// 32 zero bytes, account 133. Centralised so the key, its fvk, and its change
// address are derived one way. // @claude
static libzcash::OrchardSpendingKey TestOrchardSpendingKey() {
    RawHDSeed seed(32, 0);
    return libzcash::OrchardSpendingKey::ForAccount(seed, 133, 0);
}

// Stamps the v6 (ZIP 229) header fields onto `mtx` for the given consensus
// branch id. // @claude
static void SetV6TxHeader(CMutableTransaction& mtx, uint32_t consensusBranchId) {
    mtx.fOverwintered = true;
    mtx.nVersionGroupId = ZIP229_VERSION_GROUP_ID;
    mtx.nVersion = ZIP229_TX_VERSION;
    mtx.nConsensusBranchId = consensusBranchId;
}

TEST(ChecktransactionTests, CheckVpubNotBothNonzero) {
    CMutableTransaction tx;
    tx.nVersion = 2;

    {
        // Ensure that values within the joinsplit are well-formed.
        CMutableTransaction newTx(tx);
        CValidationState state;

        newTx.vJoinSplit.push_back(JSDescription());

        JSDescription *jsdesc = &newTx.vJoinSplit[0];
        jsdesc->vpub_old = 1;
        jsdesc->vpub_new = 1;

        EXPECT_FALSE(CheckTransactionWithoutProofVerification(newTx, state));
        EXPECT_EQ(state.GetRejectReason(), "bad-txns-vpubs-both-nonzero");
    }
}

class MockCValidationState : public CValidationState {
public:
    MOCK_METHOD6(DoS, bool(int level, bool ret,
             unsigned int chRejectCodeIn, const std::string &strRejectReasonIn,
             BodyCorruption bodyCorruption,
             const std::string &strDebugMessageIn));
    MOCK_METHOD4(Invalid, bool(bool ret,
                 unsigned int _chRejectCode, const std::string _strRejectReason,
                 const std::string &_strDebugMessage));
    MOCK_METHOD1(Error, bool(std::string strRejectReasonIn));
    MOCK_CONST_METHOD0(IsValid, bool());
    MOCK_CONST_METHOD0(IsInvalid, bool());
    MOCK_CONST_METHOD0(IsError, bool());
    MOCK_CONST_METHOD1(IsInvalid, bool(int &nDoSOut));
    MOCK_CONST_METHOD0(CorruptionPossible, bool());
    MOCK_CONST_METHOD0(GetRejectCode, unsigned int());
    MOCK_CONST_METHOD0(GetRejectReason, std::string());
    MOCK_CONST_METHOD0(GetDebugMessage, std::string());
};

void CreateJoinSplitSignature(CMutableTransaction& mtx, uint32_t consensusBranchId);

CMutableTransaction GetValidTransaction(uint32_t consensusBranchId=SPROUT_BRANCH_ID) {
    CMutableTransaction mtx;
    if (consensusBranchId == NetworkUpgradeInfo[Consensus::UPGRADE_OVERWINTER].nBranchId) {
        mtx.fOverwintered = true;
        mtx.nVersionGroupId = OVERWINTER_VERSION_GROUP_ID;
        mtx.nVersion = OVERWINTER_TX_VERSION;
    } else if (consensusBranchId == NetworkUpgradeInfo[Consensus::UPGRADE_SAPLING].nBranchId) {
        mtx.fOverwintered = true;
        mtx.nVersionGroupId = SAPLING_VERSION_GROUP_ID;
        mtx.nVersion = SAPLING_TX_VERSION;
    } else if (consensusBranchId != SPROUT_BRANCH_ID) {
        // Unsupported consensus branch ID
        assert(false);
    }

    mtx.vin.resize(2);
    mtx.vin[0].prevout.hash = uint256S("0000000000000000000000000000000000000000000000000000000000000001");
    mtx.vin[0].prevout.n = 0;
    mtx.vin[1].prevout.hash = uint256S("0000000000000000000000000000000000000000000000000000000000000002");
    mtx.vin[1].prevout.n = 0;
    mtx.vout.resize(2);
    // mtx.vout[0].scriptPubKey =
    mtx.vout[0].nValue = 0;
    mtx.vout[1].nValue = 0;
    mtx.vJoinSplit.resize(2);
    mtx.vJoinSplit[0].nullifiers.at(0) = uint256S("0000000000000000000000000000000000000000000000000000000000000000");
    mtx.vJoinSplit[0].nullifiers.at(1) = uint256S("0000000000000000000000000000000000000000000000000000000000000001");
    mtx.vJoinSplit[1].nullifiers.at(0) = uint256S("0000000000000000000000000000000000000000000000000000000000000002");
    mtx.vJoinSplit[1].nullifiers.at(1) = uint256S("0000000000000000000000000000000000000000000000000000000000000003");

    if (mtx.nVersion >= SAPLING_TX_VERSION) {
        libzcash::GrothProof emptyProof;
        mtx.vJoinSplit[0].proof = emptyProof;
        mtx.vJoinSplit[1].proof = emptyProof;
    }

    CreateJoinSplitSignature(mtx, consensusBranchId);
    return mtx;
}

void CreateJoinSplitSignature(CMutableTransaction& mtx, uint32_t consensusBranchId) {
    // Generate an ephemeral keypair.
    ed25519::SigningKey joinSplitPrivKey;
    ed25519::generate_keypair(joinSplitPrivKey, mtx.joinSplitPubKey);

    // Compute the correct hSig.
    // TODO: #966.
    static const uint256 one(uint256S("0000000000000000000000000000000000000000000000000000000000000001"));
    // Empty output script.
    CScript scriptCode;
    CTransaction signTx(mtx);
    // Fake coins being spent.
    std::vector<CTxOut> allPrevOutputs;
    allPrevOutputs.resize(signTx.vin.size());
    const PrecomputedTransactionData txdata(signTx, allPrevOutputs);
    uint256 dataToBeSigned = SignatureHash(scriptCode, signTx, NOT_AN_INPUT, SIGHASH_ALL, 0, consensusBranchId, txdata);
    if (dataToBeSigned == one) {
        throw std::runtime_error("SignatureHash failed");
    }

    // Add the signature
    ed25519::sign(
        joinSplitPrivKey,
        {dataToBeSigned.begin(), 32},
        mtx.joinSplitSig);
}

TEST(ChecktransactionTests, ValidTransaction) {
    CMutableTransaction mtx = GetValidTransaction();
    CTransaction tx(mtx);
    MockCValidationState state;
    EXPECT_TRUE(CheckTransactionWithoutProofVerification(tx, state));
}

TEST(ChecktransactionTests, BadVersionTooLow) {
    CMutableTransaction mtx = GetValidTransaction();
    mtx.nVersion = 0;

    EXPECT_THROW((CTransaction(mtx)), std::ios_base::failure);
    UNSAFE_CTransaction tx(mtx);
    MockCValidationState state;
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-txns-version-too-low", BodyCorruption::Default, "")).Times(1);
    CheckTransactionWithoutProofVerification(tx, state);
}

TEST(ChecktransactionTests, BadTxnsVinEmpty) {
    CMutableTransaction mtx = GetValidTransaction();
    mtx.vJoinSplit.resize(0);
    mtx.vin.resize(0);

    CTransaction tx(mtx);
    MockCValidationState state;
    EXPECT_CALL(state, DoS(10, false, REJECT_INVALID, "bad-txns-no-source-of-funds", BodyCorruption::Default, "")).Times(1);
    CheckTransactionWithoutProofVerification(tx, state);
}

TEST(ChecktransactionTests, BadTxnsVoutEmpty) {
    CMutableTransaction mtx = GetValidTransaction();
    mtx.vJoinSplit.resize(0);
    mtx.vout.resize(0);

    CTransaction tx(mtx);

    MockCValidationState state;
    EXPECT_CALL(state, DoS(10, false, REJECT_INVALID, "bad-txns-no-sink-of-funds", BodyCorruption::Default, "")).Times(1);
    CheckTransactionWithoutProofVerification(tx, state);
}

TEST(ChecktransactionTests, BadTxnsOversize) {
    SelectParams(CBaseChainParams::REGTEST);
    CMutableTransaction mtx = GetValidTransaction();

    mtx.vin[0].scriptSig = CScript();
    std::vector<unsigned char> vchData(520);
    for (unsigned int i = 0; i < 190; ++i)
        mtx.vin[0].scriptSig << vchData << OP_DROP;
    mtx.vin[0].scriptSig << OP_1;

    {
        // Transaction is just under the limit...
        CTransaction tx(mtx);
        CValidationState state;
        ASSERT_TRUE(CheckTransactionWithoutProofVerification(tx, state));
    }

    // Not anymore!
    mtx.vin[1].scriptSig << vchData << OP_DROP;
    mtx.vin[1].scriptSig << OP_1;

    {
        CTransaction tx(mtx);
        ASSERT_EQ(::GetSerializeSize(tx, SER_NETWORK, PROTOCOL_VERSION), 100202);

        // Passes non-contextual checks...
        MockCValidationState state;
        EXPECT_TRUE(CheckTransactionWithoutProofVerification(tx, state));

        // ... but fails contextual ones!
        EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-txns-oversize", BodyCorruption::Default, "")).Times(1);
        EXPECT_FALSE(ContextualCheckTransaction(tx, state, Params(), 1, true));
    }

    {
        // But should be fine again once Sapling activates!
        RegtestActivateSapling();

        mtx.fOverwintered = true;
        mtx.nVersionGroupId = SAPLING_VERSION_GROUP_ID;
        mtx.nVersion = SAPLING_TX_VERSION;

        // Change the proof types (which requires re-signing the JoinSplit data)
        mtx.vJoinSplit[0].proof = libzcash::GrothProof();
        mtx.vJoinSplit[1].proof = libzcash::GrothProof();
        CreateJoinSplitSignature(mtx, NetworkUpgradeInfo[Consensus::UPGRADE_SAPLING].nBranchId);

        CTransaction tx(mtx);
        EXPECT_EQ(::GetSerializeSize(tx, SER_NETWORK, PROTOCOL_VERSION), 103713);

        MockCValidationState state;
        EXPECT_TRUE(CheckTransactionWithoutProofVerification(tx, state));
        EXPECT_TRUE(ContextualCheckTransaction(tx, state, Params(), 1, true));

        // Revert to default
        RegtestDeactivateSapling();
    }
}

TEST(ChecktransactionTests, OversizeSaplingTxns) {
    RegtestActivateSapling();

    CMutableTransaction mtx = GetValidTransaction();
    mtx.fOverwintered = true;
    mtx.nVersionGroupId = SAPLING_VERSION_GROUP_ID;
    mtx.nVersion = SAPLING_TX_VERSION;

    // Change the proof types (which requires re-signing the JoinSplit data)
    mtx.vJoinSplit[0].proof = libzcash::GrothProof();
    mtx.vJoinSplit[1].proof = libzcash::GrothProof();
    CreateJoinSplitSignature(mtx, NetworkUpgradeInfo[Consensus::UPGRADE_SAPLING].nBranchId);

    // Transaction just under the limit
    mtx.vin[0].scriptSig = CScript();
    std::vector<unsigned char> vchData(520);
    for (unsigned int i = 0; i < 3809; ++i)
        mtx.vin[0].scriptSig << vchData << OP_DROP;
    std::vector<unsigned char> vchDataRemainder(453);
    mtx.vin[0].scriptSig << vchDataRemainder << OP_DROP;
    mtx.vin[0].scriptSig << OP_1;

    {
        CTransaction tx(mtx);
        EXPECT_EQ(::GetSerializeSize(tx, SER_NETWORK, PROTOCOL_VERSION), MAX_TX_SIZE_AFTER_SAPLING - 1);

        CValidationState state;
        EXPECT_TRUE(CheckTransactionWithoutProofVerification(tx, state));
    }

    // Transaction equal to the limit
    mtx.vin[1].scriptSig << OP_1;

    {
        CTransaction tx(mtx);
        EXPECT_EQ(::GetSerializeSize(tx, SER_NETWORK, PROTOCOL_VERSION), MAX_TX_SIZE_AFTER_SAPLING);

        CValidationState state;
        EXPECT_TRUE(CheckTransactionWithoutProofVerification(tx, state));
    }

    // Transaction just over the limit
    mtx.vin[1].scriptSig << OP_1;

    {
        CTransaction tx(mtx);
        EXPECT_EQ(::GetSerializeSize(tx, SER_NETWORK, PROTOCOL_VERSION), MAX_TX_SIZE_AFTER_SAPLING + 1);

        MockCValidationState state;
        EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-txns-oversize", BodyCorruption::Default, "")).Times(1);
        EXPECT_FALSE(CheckTransactionWithoutProofVerification(tx, state));
    }

    // Revert to default
    RegtestDeactivateSapling();
}

TEST(ChecktransactionTests, BadTxnsVoutNegative) {
    CMutableTransaction mtx = GetValidTransaction();
    mtx.vout[0].nValue = -1;

    EXPECT_THROW((CTransaction(mtx)), std::ios_base::failure);
    UNSAFE_CTransaction tx(mtx);

    MockCValidationState state;
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-txns-vout-negative", BodyCorruption::Default, "")).Times(1);
    CheckTransactionWithoutProofVerification(tx, state);
}

TEST(ChecktransactionTests, BadTxnsVoutToolarge) {
    CMutableTransaction mtx = GetValidTransaction();
    mtx.vout[0].nValue = MAX_MONEY + 1;

    EXPECT_THROW((CTransaction(mtx)), std::ios_base::failure);
    UNSAFE_CTransaction tx(mtx);

    MockCValidationState state;
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-txns-vout-toolarge", BodyCorruption::Default, "")).Times(1);
    CheckTransactionWithoutProofVerification(tx, state);
}

TEST(ChecktransactionTests, BadTxnsTxouttotalToolargeOutputs) {
    CMutableTransaction mtx = GetValidTransaction();
    mtx.vout[0].nValue = MAX_MONEY;
    mtx.vout[1].nValue = 1;

    CTransaction tx(mtx);

    MockCValidationState state;
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-txns-txouttotal-toolarge", BodyCorruption::Default, "")).Times(1);
    CheckTransactionWithoutProofVerification(tx, state);
}

// TODO: The new Sapling bundle API prevents us from constructing this case.
/*
TEST(ChecktransactionTests, ValueBalanceNonZero) {
    CMutableTransaction mtx = GetValidTransaction(NetworkUpgradeInfo[Consensus::UPGRADE_SAPLING].nBranchId);
    mtx.saplingBundle = sapling::test_only_invalid_bundle(0, 0, 10);

    CTransaction tx(mtx);

    MockCValidationState state;
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-txns-valuebalance-nonzero", BodyCorruption::Default, "")).Times(1);
    EXPECT_FALSE(CheckTransactionWithoutProofVerification(tx, state));
}
*/

TEST(ChecktransactionTests, ValueBalanceOverflowsTotal) {
    CMutableTransaction mtx = GetValidTransaction(NetworkUpgradeInfo[Consensus::UPGRADE_SAPLING].nBranchId);
    mtx.vout[0].nValue = 1;
    mtx.saplingBundle = sapling::test_only_invalid_bundle(1, 0, -MAX_MONEY);

    CTransaction tx(mtx);

    MockCValidationState state;
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-txns-txouttotal-toolarge", BodyCorruption::Default, "")).Times(1);
    CheckTransactionWithoutProofVerification(tx, state);
}

TEST(ChecktransactionTests, BadTxnsTxouttotalToolargeJoinsplit) {
    CMutableTransaction mtx = GetValidTransaction();
    mtx.vout[0].nValue = 1;
    mtx.vJoinSplit[0].vpub_old = MAX_MONEY;

    CTransaction tx(mtx);

    MockCValidationState state;
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-txns-txouttotal-toolarge", BodyCorruption::Default, "")).Times(1);
    CheckTransactionWithoutProofVerification(tx, state);
}

TEST(ChecktransactionTests, BadTxnsTxintotalToolargeJoinsplit) {
    CMutableTransaction mtx = GetValidTransaction();
    mtx.vJoinSplit[0].vpub_new = MAX_MONEY - 1;
    mtx.vJoinSplit[1].vpub_new = MAX_MONEY - 1;

    CTransaction tx(mtx);

    MockCValidationState state;
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-txns-txintotal-toolarge", BodyCorruption::Default, "")).Times(1);
    CheckTransactionWithoutProofVerification(tx, state);
}

TEST(ChecktransactionTests, BadTxnsVpubOldNegative) {
    CMutableTransaction mtx = GetValidTransaction();
    mtx.vJoinSplit[0].vpub_old = -1;

    CTransaction tx(mtx);

    MockCValidationState state;
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-txns-vpub_old-negative", BodyCorruption::Default, "")).Times(1);
    CheckTransactionWithoutProofVerification(tx, state);
}

TEST(ChecktransactionTests, BadTxnsVpubNewNegative) {
    CMutableTransaction mtx = GetValidTransaction();
    mtx.vJoinSplit[0].vpub_new = -1;

    CTransaction tx(mtx);

    MockCValidationState state;
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-txns-vpub_new-negative", BodyCorruption::Default, "")).Times(1);
    CheckTransactionWithoutProofVerification(tx, state);
}

TEST(ChecktransactionTests, BadTxnsVpubOldToolarge) {
    CMutableTransaction mtx = GetValidTransaction();
    mtx.vJoinSplit[0].vpub_old = MAX_MONEY + 1;

    CTransaction tx(mtx);

    MockCValidationState state;
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-txns-vpub_old-toolarge", BodyCorruption::Default, "")).Times(1);
    CheckTransactionWithoutProofVerification(tx, state);
}

TEST(ChecktransactionTests, BadTxnsVpubNewToolarge) {
    CMutableTransaction mtx = GetValidTransaction();
    mtx.vJoinSplit[0].vpub_new = MAX_MONEY + 1;

    CTransaction tx(mtx);

    MockCValidationState state;
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-txns-vpub_new-toolarge", BodyCorruption::Default, "")).Times(1);
    CheckTransactionWithoutProofVerification(tx, state);
}

TEST(ChecktransactionTests, BadTxnsVpubsBothNonzero) {
    CMutableTransaction mtx = GetValidTransaction();
    mtx.vJoinSplit[0].vpub_old = 1;
    mtx.vJoinSplit[0].vpub_new = 1;

    CTransaction tx(mtx);

    MockCValidationState state;
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-txns-vpubs-both-nonzero", BodyCorruption::Default, "")).Times(1);
    CheckTransactionWithoutProofVerification(tx, state);
}

TEST(ChecktransactionTests, BadTxnsInputsDuplicate) {
    CMutableTransaction mtx = GetValidTransaction();
    mtx.vin[1].prevout.hash = mtx.vin[0].prevout.hash;
    mtx.vin[1].prevout.n = mtx.vin[0].prevout.n;

    CTransaction tx(mtx);

    MockCValidationState state;
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-txns-inputs-duplicate", BodyCorruption::Default, "")).Times(1);
    CheckTransactionWithoutProofVerification(tx, state);
}

TEST(ChecktransactionTests, BadJoinsplitsNullifiersDuplicateSameJoinsplit) {
    CMutableTransaction mtx = GetValidTransaction();
    mtx.vJoinSplit[0].nullifiers.at(0) = uint256S("0000000000000000000000000000000000000000000000000000000000000000");
    mtx.vJoinSplit[0].nullifiers.at(1) = uint256S("0000000000000000000000000000000000000000000000000000000000000000");

    CTransaction tx(mtx);

    MockCValidationState state;
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-joinsplits-nullifiers-duplicate", BodyCorruption::Default, "")).Times(1);
    CheckTransactionWithoutProofVerification(tx, state);
}

TEST(ChecktransactionTests, BadJoinsplitsNullifiersDuplicateDifferentJoinsplit) {
    CMutableTransaction mtx = GetValidTransaction();
    mtx.vJoinSplit[0].nullifiers.at(0) = uint256S("0000000000000000000000000000000000000000000000000000000000000000");
    mtx.vJoinSplit[1].nullifiers.at(0) = uint256S("0000000000000000000000000000000000000000000000000000000000000000");

    CTransaction tx(mtx);

    MockCValidationState state;
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-joinsplits-nullifiers-duplicate", BodyCorruption::Default, "")).Times(1);
    CheckTransactionWithoutProofVerification(tx, state);
}

TEST(ChecktransactionTests, BadCbHasJoinsplits) {
    CMutableTransaction mtx = GetValidTransaction();
    // Make it a coinbase.
    mtx.vin.resize(1);
    mtx.vin[0].prevout.SetNull();

    mtx.vJoinSplit.resize(1);

    CTransaction tx(mtx);
    EXPECT_TRUE(tx.IsCoinBase());

    MockCValidationState state;
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-cb-has-joinsplits", BodyCorruption::Default, "")).Times(1);
    CheckTransactionWithoutProofVerification(tx, state);
}

TEST(ChecktransactionTests, BadCbEmptyScriptsig) {
    CMutableTransaction mtx = GetValidTransaction();
    // Make it a coinbase.
    mtx.vin.resize(1);
    mtx.vin[0].prevout.SetNull();

    mtx.vJoinSplit.resize(0);

    CTransaction tx(mtx);
    EXPECT_TRUE(tx.IsCoinBase());

    MockCValidationState state;
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-cb-length", BodyCorruption::Default, "")).Times(1);
    CheckTransactionWithoutProofVerification(tx, state);
}

TEST(ChecktransactionTests, BadTxnsPrevoutNull) {
    CMutableTransaction mtx = GetValidTransaction();
    mtx.vin[1].prevout.SetNull();

    CTransaction tx(mtx);
    EXPECT_FALSE(tx.IsCoinBase());

    MockCValidationState state;
    EXPECT_CALL(state, DoS(10, false, REJECT_INVALID, "bad-txns-prevout-null", BodyCorruption::Default, "")).Times(1);
    CheckTransactionWithoutProofVerification(tx, state);
}

TEST(ContextualCheckShieldedInputsTest, BadTxnsInvalidJoinsplitSignature) {
    SelectParams(CBaseChainParams::REGTEST);
    auto consensus = Params().GetConsensus();
    std::optional<rust::Box<sapling::BatchValidator>> saplingAuth = std::nullopt;
    std::optional<rust::Box<orchard::BatchValidator>> orchardAuth = std::nullopt;

    CMutableTransaction mtx = GetValidTransaction();
    mtx.joinSplitSig.bytes[0] += 1;
    CTransaction tx(mtx);

    // Recreate the fake coins being spent.
    std::vector<CTxOut> allPrevOutputs;
    allPrevOutputs.resize(tx.vin.size());
    const PrecomputedTransactionData txdata(tx, allPrevOutputs);

    MockCValidationState state;
    AssumeShieldedInputsExistAndAreSpendable baseView;
    CCoinsViewCache view(&baseView);
    // during initial block download, for transactions being accepted into the
    // mempool (and thus not mined), DoS ban score should be zero, else 10
    EXPECT_CALL(state, DoS(0, false, REJECT_INVALID, "bad-txns-invalid-joinsplit-signature", BodyCorruption::Default, "")).Times(1);
    ContextualCheckShieldedInputs(tx, txdata, state, view, saplingAuth, orchardAuth, consensus, 0, false, false, [](const Consensus::Params&) { return true; });
    EXPECT_CALL(state, DoS(10, false, REJECT_INVALID, "bad-txns-invalid-joinsplit-signature", BodyCorruption::Default, "")).Times(1);
    ContextualCheckShieldedInputs(tx, txdata, state, view, saplingAuth, orchardAuth, consensus, 0, false, false, [](const Consensus::Params&) { return false; });
    // for transactions that have been mined in a block, DoS ban score should
    // always be 100.
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-txns-invalid-joinsplit-signature", BodyCorruption::Default, "")).Times(1);
    ContextualCheckShieldedInputs(tx, txdata, state, view, saplingAuth, orchardAuth, consensus, 0, false, true, [](const Consensus::Params&) { return true; });
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-txns-invalid-joinsplit-signature", BodyCorruption::Default, "")).Times(1);
    ContextualCheckShieldedInputs(tx, txdata, state, view, saplingAuth, orchardAuth, consensus, 0, false, true, [](const Consensus::Params&) { return false; });
}

TEST(ContextualCheckShieldedInputsTest, JoinsplitSignatureDetectsOldBranchId) {
    SelectParams(CBaseChainParams::REGTEST);
    auto consensus = Params().GetConsensus();
    std::optional<rust::Box<sapling::BatchValidator>> saplingAuth = std::nullopt;
    std::optional<rust::Box<orchard::BatchValidator>> orchardAuth = std::nullopt;

    auto saplingBranchId = NetworkUpgradeInfo[Consensus::UPGRADE_SAPLING].nBranchId;
    auto blossomBranchId = NetworkUpgradeInfo[Consensus::UPGRADE_BLOSSOM].nBranchId;
    auto heartwoodBranchId = NetworkUpgradeInfo[Consensus::UPGRADE_HEARTWOOD].nBranchId;

    // Create a valid transaction for the Sapling epoch.
    CMutableTransaction mtx = GetValidTransaction(saplingBranchId);
    CTransaction tx(mtx);

    // Recreate the fake coins being spent.
    std::vector<CTxOut> allPrevOutputs;
    allPrevOutputs.resize(tx.vin.size());
    const PrecomputedTransactionData txdata(tx, allPrevOutputs);

    MockCValidationState state;
    AssumeShieldedInputsExistAndAreSpendable baseView;
    CCoinsViewCache view(&baseView);
    // Ensure that the transaction validates against Sapling.
    EXPECT_TRUE(ContextualCheckShieldedInputs(
        tx, txdata, state, view, saplingAuth, orchardAuth, consensus, saplingBranchId, false, false,
        [](const Consensus::Params&) { return false; }));

    // Attempt to validate the inputs against Blossom. We should be notified
    // that an old consensus branch ID was used for an input.
    EXPECT_CALL(state, DoS(
        10, false, REJECT_INVALID,
        strprintf("old-consensus-branch-id (Expected %s, found %s)",
            HexInt(blossomBranchId),
            HexInt(saplingBranchId)),
        BodyCorruption::Default, "")).Times(1);
    EXPECT_FALSE(ContextualCheckShieldedInputs(
        tx, txdata, state, view, saplingAuth, orchardAuth, consensus, blossomBranchId, false, false,
        [](const Consensus::Params&) { return false; }));

    // Attempt to validate the inputs against Heartwood. All we should learn is
    // that the signature is invalid, because we don't check more than one
    // network upgrade back.
    EXPECT_CALL(state, DoS(
        10, false, REJECT_INVALID,
        "bad-txns-invalid-joinsplit-signature", BodyCorruption::Default, "")).Times(1);
    EXPECT_FALSE(ContextualCheckShieldedInputs(
        tx, txdata, state, view, saplingAuth, orchardAuth, consensus, heartwoodBranchId, false, false,
        [](const Consensus::Params&) { return false; }));
}

TEST(ContextualCheckShieldedInputsTest, NonCanonicalEd25519Signature) {
    SelectParams(CBaseChainParams::REGTEST);
    auto consensus = Params().GetConsensus();
    std::optional<rust::Box<sapling::BatchValidator>> saplingAuth = std::nullopt;
    std::optional<rust::Box<orchard::BatchValidator>> orchardAuth = std::nullopt;

    AssumeShieldedInputsExistAndAreSpendable baseView;
    CCoinsViewCache view(&baseView);

    auto saplingBranchId = NetworkUpgradeInfo[Consensus::UPGRADE_SAPLING].nBranchId;
    CMutableTransaction mtx = GetValidTransaction(saplingBranchId);

    // Recreate the fake coins being spent.
    std::vector<CTxOut> allPrevOutputs;
    allPrevOutputs.resize(mtx.vin.size());

    // Check that the signature is valid before we add L
    {
        CTransaction tx(mtx);
        const PrecomputedTransactionData txdata(tx, allPrevOutputs);
        MockCValidationState state;
        EXPECT_TRUE(ContextualCheckShieldedInputs(tx, txdata, state, view, saplingAuth, orchardAuth, consensus, saplingBranchId, false, true));
    }

    // Copied from libsodium/crypto_sign/ed25519/ref10/open.c
    static const unsigned char L[32] =
      { 0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58,
        0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9, 0xde, 0x14,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10 };

    // Add L to S, which starts at mtx.joinSplitSig[32].
    unsigned int s = 0;
    for (size_t i = 0; i < 32; i++) {
        s = mtx.joinSplitSig.bytes[32 + i] + L[i] + (s >> 8);
        mtx.joinSplitSig.bytes[32 + i] = s & 0xff;
    }

    CTransaction tx(mtx);
    const PrecomputedTransactionData txdata(tx, allPrevOutputs);

    MockCValidationState state;
    // during initial block download, for transactions being accepted into the
    // mempool (and thus not mined), DoS ban score should be zero, else 10
    EXPECT_CALL(state, DoS(0, false, REJECT_INVALID, "bad-txns-invalid-joinsplit-signature", BodyCorruption::Default, "")).Times(1);
    ContextualCheckShieldedInputs(tx, txdata, state, view, saplingAuth, orchardAuth, consensus, saplingBranchId, false, false, [](const Consensus::Params&) { return true; });
    EXPECT_CALL(state, DoS(10, false, REJECT_INVALID, "bad-txns-invalid-joinsplit-signature", BodyCorruption::Default, "")).Times(1);
    ContextualCheckShieldedInputs(tx, txdata, state, view, saplingAuth, orchardAuth, consensus, saplingBranchId, false, false, [](const Consensus::Params&) { return false; });
    // for transactions that have been mined in a block, DoS ban score should
    // always be 100.
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-txns-invalid-joinsplit-signature", BodyCorruption::Default, "")).Times(1);
    ContextualCheckShieldedInputs(tx, txdata, state, view, saplingAuth, orchardAuth, consensus, saplingBranchId, false, true, [](const Consensus::Params&) { return true; });
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-txns-invalid-joinsplit-signature", BodyCorruption::Default, "")).Times(1);
    ContextualCheckShieldedInputs(tx, txdata, state, view, saplingAuth, orchardAuth, consensus, saplingBranchId, false, true, [](const Consensus::Params&) { return false; });
}

TEST(ChecktransactionTests, OverwinterConstructors) {
    CMutableTransaction mtx;
    mtx.fOverwintered = true;
    mtx.nVersion = OVERWINTER_TX_VERSION;
    mtx.nVersionGroupId = OVERWINTER_VERSION_GROUP_ID;
    mtx.nExpiryHeight = 20;

    // Check constructor with overwinter fields
    CTransaction tx(mtx);
    EXPECT_EQ(tx.nVersion, mtx.nVersion);
    EXPECT_EQ(tx.fOverwintered, mtx.fOverwintered);
    EXPECT_EQ(tx.nVersionGroupId, mtx.nVersionGroupId);
    EXPECT_EQ(tx.nExpiryHeight, mtx.nExpiryHeight);

    // Check constructor of mutable transaction struct
    CMutableTransaction mtx2(tx);
    EXPECT_EQ(mtx2.nVersion, mtx.nVersion);
    EXPECT_EQ(mtx2.fOverwintered, mtx.fOverwintered);
    EXPECT_EQ(mtx2.nVersionGroupId, mtx.nVersionGroupId);
    EXPECT_EQ(mtx2.nExpiryHeight, mtx.nExpiryHeight);
    EXPECT_TRUE(mtx2.GetHash() == mtx.GetHash());

    // Check assignment of overwinter fields
    CTransaction tx2 = tx;
    EXPECT_EQ(tx2.nVersion, mtx.nVersion);
    EXPECT_EQ(tx2.fOverwintered, mtx.fOverwintered);
    EXPECT_EQ(tx2.nVersionGroupId, mtx.nVersionGroupId);
    EXPECT_EQ(tx2.nExpiryHeight, mtx.nExpiryHeight);
    EXPECT_TRUE(tx2 == tx);
}

TEST(ChecktransactionTests, OverwinterSerialization) {
    CMutableTransaction mtx;
    mtx.fOverwintered = true;
    mtx.nVersion = OVERWINTER_TX_VERSION;
    mtx.nVersionGroupId = OVERWINTER_VERSION_GROUP_ID;
    mtx.nExpiryHeight = 99;

    // Check round-trip serialization and deserialization from mtx to tx.
    {
        CDataStream ss(SER_DISK, PROTOCOL_VERSION);
        ss << mtx;
        CTransaction tx;
        ss >> tx;
        EXPECT_EQ(mtx.nVersion, tx.nVersion);
        EXPECT_EQ(mtx.fOverwintered, tx.fOverwintered);
        EXPECT_EQ(mtx.nVersionGroupId, tx.nVersionGroupId);
        EXPECT_EQ(mtx.nExpiryHeight, tx.nExpiryHeight);

        EXPECT_EQ(mtx.GetHash(), CMutableTransaction(tx).GetHash());
        EXPECT_EQ(tx.GetHash(), CTransaction(mtx).GetHash());
    }

    // Also check mtx to mtx
    {
        CDataStream ss(SER_DISK, PROTOCOL_VERSION);
        ss << mtx;
        CMutableTransaction mtx2;
        ss >> mtx2;
        EXPECT_EQ(mtx.nVersion, mtx2.nVersion);
        EXPECT_EQ(mtx.fOverwintered, mtx2.fOverwintered);
        EXPECT_EQ(mtx.nVersionGroupId, mtx2.nVersionGroupId);
        EXPECT_EQ(mtx.nExpiryHeight, mtx2.nExpiryHeight);

        EXPECT_EQ(mtx.GetHash(), mtx2.GetHash());
    }

    // Also check tx to tx
    {
        CTransaction tx(mtx);
        CDataStream ss(SER_DISK, PROTOCOL_VERSION);
        ss << tx;
        CTransaction tx2;
        ss >> tx2;
        EXPECT_EQ(tx.nVersion, tx2.nVersion);
        EXPECT_EQ(tx.fOverwintered, tx2.fOverwintered);
        EXPECT_EQ(tx.nVersionGroupId, tx2.nVersionGroupId);
        EXPECT_EQ(tx.nExpiryHeight, tx2.nExpiryHeight);

        EXPECT_EQ(mtx.GetHash(), CMutableTransaction(tx).GetHash());
        EXPECT_EQ(tx.GetHash(), tx2.GetHash());
    }
}

TEST(ChecktransactionTests, OverwinterDefaultValues) {
    // Check default values (this will fail when defaults change; test should then be updated)
    CTransaction tx;
    EXPECT_EQ(tx.nVersion, 1);
    EXPECT_EQ(tx.fOverwintered, false);
    EXPECT_EQ(tx.nVersionGroupId, 0);
    EXPECT_EQ(tx.nExpiryHeight, 0);
}

// A valid v3 transaction with no joinsplits
TEST(ChecktransactionTests, OverwinterValidTx) {
    CMutableTransaction mtx = GetValidTransaction();
    mtx.vJoinSplit.resize(0);
    mtx.fOverwintered = true;
    mtx.nVersion = OVERWINTER_TX_VERSION;
    mtx.nVersionGroupId = OVERWINTER_VERSION_GROUP_ID;
    mtx.nExpiryHeight = 0;
    CTransaction tx(mtx);
    MockCValidationState state;
    EXPECT_TRUE(CheckTransactionWithoutProofVerification(tx, state));
}

TEST(ChecktransactionTests, OverwinterExpiryHeight) {
    const auto& params = RegtestActivateOverwinter();
    CMutableTransaction mtx = GetValidTransaction(0x5ba81b19);
    mtx.vJoinSplit.resize(0);
    mtx.nExpiryHeight = 0;

    {
        CTransaction tx(mtx);
        MockCValidationState state;
        EXPECT_TRUE(CheckTransactionWithoutProofVerification(tx, state));
        EXPECT_TRUE(ContextualCheckTransaction(tx, state, params, 1, true));
    }

    {
        mtx.nExpiryHeight = TX_EXPIRY_HEIGHT_THRESHOLD - 1;
        CTransaction tx(mtx);
        MockCValidationState state;
        EXPECT_TRUE(CheckTransactionWithoutProofVerification(tx, state));
        EXPECT_TRUE(ContextualCheckTransaction(tx, state, params, 1, true));
    }

    {
        mtx.nExpiryHeight = TX_EXPIRY_HEIGHT_THRESHOLD;
        CTransaction tx(mtx);
        MockCValidationState state;
        EXPECT_TRUE(CheckTransactionWithoutProofVerification(tx, state));
        EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-tx-expiry-height-too-high", BodyCorruption::Default, "")).Times(1);
        ContextualCheckTransaction(tx, state, params, 1, true);
    }

    {
        mtx.nExpiryHeight = std::numeric_limits<uint32_t>::max();
        CTransaction tx(mtx);
        MockCValidationState state;
        EXPECT_TRUE(CheckTransactionWithoutProofVerification(tx, state));
        EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-tx-expiry-height-too-high", BodyCorruption::Default, "")).Times(1);
        ContextualCheckTransaction(tx, state, params, 1, true);
    }

    RegtestDeactivateSapling();
}

TEST(checktransaction_tests, BlossomExpiryHeight) {
    const Consensus::Params& params = RegtestActivateBlossom(false, 100).GetConsensus();
    CMutableTransaction preBlossomMtx = CreateNewContextualCMutableTransaction(params, 99, false);
    EXPECT_EQ(preBlossomMtx.nExpiryHeight, 100 - 1);
    CMutableTransaction blossomMtx = CreateNewContextualCMutableTransaction(params, 100, false);
    EXPECT_EQ(blossomMtx.nExpiryHeight, 100 + 40);
    RegtestDeactivateBlossom();
}

// Test that a Sprout tx with a negative version number is detected
// given the new Overwinter logic
TEST(ChecktransactionTests, SproutTxVersionTooLow) {
    CMutableTransaction mtx = GetValidTransaction();
    mtx.vJoinSplit.resize(0);
    mtx.fOverwintered = false;
    mtx.nVersion = -1;

    EXPECT_THROW((CTransaction(mtx)), std::ios_base::failure);
    UNSAFE_CTransaction tx(mtx);
    MockCValidationState state;
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-txns-version-too-low", BodyCorruption::Default, "")).Times(1);
    CheckTransactionWithoutProofVerification(tx, state);
}



TEST(ChecktransactionTests, SaplingSproutInputSumsTooLarge) {
    CMutableTransaction mtx = GetValidTransaction();
    mtx.vJoinSplit.resize(0);
    mtx.fOverwintered = true;
    mtx.nVersion = SAPLING_TX_VERSION;
    mtx.nVersionGroupId = SAPLING_VERSION_GROUP_ID;
    mtx.nExpiryHeight = 0;

    {
        // create JSDescription
        uint256 rt;
        ed25519::VerificationKey joinSplitPubKey;
        std::array<libzcash::JSInput, ZC_NUM_JS_INPUTS> inputs = {
            libzcash::JSInput(),
            libzcash::JSInput()
        };
        std::array<libzcash::JSOutput, ZC_NUM_JS_OUTPUTS> outputs = {
            libzcash::JSOutput(),
            libzcash::JSOutput()
        };
        std::array<size_t, ZC_NUM_JS_INPUTS> inputMap;
        std::array<size_t, ZC_NUM_JS_OUTPUTS> outputMap;

        auto jsdesc = JSDescriptionInfo(
            joinSplitPubKey, rt,
            inputs, outputs,
            0, 0
        ).BuildRandomized(
            inputMap, outputMap,
            false);

        mtx.vJoinSplit.push_back(jsdesc);
    }

    mtx.saplingBundle = sapling::test_only_invalid_bundle(1, 0, 0);

    mtx.vJoinSplit[0].vpub_new = (MAX_MONEY / 2) + 10;

    {
        UNSAFE_CTransaction tx(mtx);
        CValidationState state;
        EXPECT_TRUE(CheckTransactionWithoutProofVerification(tx, state));
    }

    mtx.saplingBundle = sapling::test_only_invalid_bundle(1, 0, (MAX_MONEY / 2) + 10);

    {
        UNSAFE_CTransaction tx(mtx);
        MockCValidationState state;
        EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-txns-txintotal-toolarge", BodyCorruption::Default, "")).Times(1);
        CheckTransactionWithoutProofVerification(tx, state);
    }
}

// Test bad Overwinter version number in CheckTransactionWithoutProofVerification
TEST(ChecktransactionTests, OverwinterVersionNumberLow) {
    CMutableTransaction mtx = GetValidTransaction();
    mtx.vJoinSplit.resize(0);
    mtx.fOverwintered = true;
    mtx.nVersion = OVERWINTER_MIN_TX_VERSION - 1;
    mtx.nVersionGroupId = OVERWINTER_VERSION_GROUP_ID;
    mtx.nExpiryHeight = 0;

    EXPECT_THROW((CTransaction(mtx)), std::ios_base::failure);
    UNSAFE_CTransaction tx(mtx);
    MockCValidationState state;
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-tx-overwinter-version-too-low", BodyCorruption::Default, "")).Times(1);
    CheckTransactionWithoutProofVerification(tx, state);
}

// Test bad Overwinter version number in ContextualCheckTransaction
TEST(ChecktransactionTests, OverwinterVersionNumberHigh) {
    SelectParams(CBaseChainParams::REGTEST);
    UpdateNetworkUpgradeParameters(Consensus::UPGRADE_OVERWINTER, Consensus::NetworkUpgrade::ALWAYS_ACTIVE);

    CMutableTransaction mtx = GetValidTransaction();
    mtx.vJoinSplit.resize(0);
    mtx.fOverwintered = true;
    mtx.nVersion = OVERWINTER_MAX_TX_VERSION + 1;
    mtx.nVersionGroupId = OVERWINTER_VERSION_GROUP_ID;
    mtx.nExpiryHeight = 0;

    EXPECT_THROW((CTransaction(mtx)), std::ios_base::failure);
    UNSAFE_CTransaction tx(mtx);
    MockCValidationState state;
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-tx-overwinter-version-too-high", BodyCorruption::Default, "")).Times(1);
    ContextualCheckTransaction(tx, state, Params(), 1, true);

    // Revert to default
    UpdateNetworkUpgradeParameters(Consensus::UPGRADE_OVERWINTER, Consensus::NetworkUpgrade::NO_ACTIVATION_HEIGHT);
}


// Test bad Overwinter version group id
TEST(ChecktransactionTests, OverwinterBadVersionGroupId) {
    CMutableTransaction mtx = GetValidTransaction();
    mtx.vJoinSplit.resize(0);
    mtx.fOverwintered = true;
    mtx.nVersion = OVERWINTER_TX_VERSION;
    mtx.nExpiryHeight = 0;
    mtx.nVersionGroupId = 0x12345678;

    EXPECT_THROW((CTransaction(mtx)), std::ios_base::failure);
    UNSAFE_CTransaction tx(mtx);
    MockCValidationState state;
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-tx-version-group-id", BodyCorruption::Default, "")).Times(1);
    CheckTransactionWithoutProofVerification(tx, state);
}

// This tests an Overwinter transaction checked against Sprout
TEST(ChecktransactionTests, OverwinterNotActive) {
    SelectParams(CBaseChainParams::TESTNET);
    auto chainparams = Params();

    CMutableTransaction mtx = GetValidTransaction();
    mtx.fOverwintered = true;
    mtx.nVersion = OVERWINTER_TX_VERSION;
    mtx.nVersionGroupId = OVERWINTER_VERSION_GROUP_ID;
    mtx.nExpiryHeight = 0;

    CTransaction tx(mtx);
    MockCValidationState state;
    // during initial block download, for transactions being accepted into the
    // mempool (and thus not mined), DoS ban score should be zero, else 10
    EXPECT_CALL(state, DoS(0, false, REJECT_INVALID, "tx-overwinter-not-active", BodyCorruption::Default, "")).Times(1);
    ContextualCheckTransaction(tx, state, chainparams, 0, false, [](const Consensus::Params&) { return true; });
    EXPECT_CALL(state, DoS(10, false, REJECT_INVALID, "tx-overwinter-not-active", BodyCorruption::Default, "")).Times(1);
    ContextualCheckTransaction(tx, state, chainparams, 0, false, [](const Consensus::Params&) { return false; });
    // for transactions that have been mined in a block, DoS ban score should
    // always be 100.
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "tx-overwinter-not-active", BodyCorruption::Default, "")).Times(1);
    ContextualCheckTransaction(tx, state, chainparams, 0, true, [](const Consensus::Params&) { return true; });
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "tx-overwinter-not-active", BodyCorruption::Default, "")).Times(1);
    ContextualCheckTransaction(tx, state, chainparams, 0, true, [](const Consensus::Params&) { return false; });
}

// This tests a transaction without the fOverwintered flag set, against the Overwinter consensus rule set.
TEST(ChecktransactionTests, OverwinterFlagNotSet) {
    SelectParams(CBaseChainParams::REGTEST);
    UpdateNetworkUpgradeParameters(Consensus::UPGRADE_OVERWINTER, Consensus::NetworkUpgrade::ALWAYS_ACTIVE);

    CMutableTransaction mtx = GetValidTransaction();
    mtx.fOverwintered = false;
    mtx.nVersion = OVERWINTER_TX_VERSION;
    mtx.nVersionGroupId = OVERWINTER_VERSION_GROUP_ID;
    mtx.nExpiryHeight = 0;

    CTransaction tx(mtx);
    MockCValidationState state;
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "tx-overwintered-flag-not-set", BodyCorruption::Default, "")).Times(1);
    ContextualCheckTransaction(tx, state, Params(), 1, true);

    // Revert to default
    UpdateNetworkUpgradeParameters(Consensus::UPGRADE_OVERWINTER, Consensus::NetworkUpgrade::NO_ACTIVATION_HEIGHT);
}


// Overwinter (NU0) does not allow soft fork to version 4 Overwintered tx.
TEST(ChecktransactionTests, OverwinterInvalidSoftForkVersion) {
    CMutableTransaction mtx = GetValidTransaction();
    mtx.fOverwintered = true;
    mtx.nVersion = 4; // This is not allowed
    mtx.nVersionGroupId = OVERWINTER_VERSION_GROUP_ID;
    mtx.nExpiryHeight = 0;

    CDataStream ss(SER_DISK, PROTOCOL_VERSION);
    try {
        ss << mtx;
        FAIL() << "Expected std::ios_base::failure 'Unknown transaction format'";
    }
    catch(std::ios_base::failure & err) {
        EXPECT_THAT(err.what(), testing::HasSubstr(std::string("Unknown transaction format")));
    }
    catch(...) {
        FAIL() << "Expected std::ios_base::failure 'Unknown transaction format', got some other exception";
    }
}

static void ContextualCreateTxCheck(const Consensus::Params& params, int nHeight,
    int expectedVersion, bool expectedOverwintered, int expectedVersionGroupId, int expectedExpiryHeight)
{
    CMutableTransaction mtx = CreateNewContextualCMutableTransaction(params, nHeight, false);
    EXPECT_EQ(mtx.nVersion, expectedVersion);
    EXPECT_EQ(mtx.fOverwintered, expectedOverwintered);
    EXPECT_EQ(mtx.nVersionGroupId, expectedVersionGroupId);
    EXPECT_EQ(mtx.nExpiryHeight, expectedExpiryHeight);
}


// Test CreateNewContextualCMutableTransaction sets default values based on height
TEST(ChecktransactionTests, OverwinteredContextualCreateTx) {
    SelectParams(CBaseChainParams::REGTEST);
    const Consensus::Params& params = Params().GetConsensus();
    int overwinterActivationHeight = 5;
    int saplingActivationHeight = 30;
    UpdateNetworkUpgradeParameters(Consensus::UPGRADE_OVERWINTER, overwinterActivationHeight);
    UpdateNetworkUpgradeParameters(Consensus::UPGRADE_SAPLING, saplingActivationHeight);

    ContextualCreateTxCheck(params, overwinterActivationHeight - 1,
        1, false, 0, 0);
    // Overwinter activates
    ContextualCreateTxCheck(params, overwinterActivationHeight,
        OVERWINTER_TX_VERSION, true, OVERWINTER_VERSION_GROUP_ID, overwinterActivationHeight + DEFAULT_PRE_BLOSSOM_TX_EXPIRY_DELTA);
    // Close to Sapling activation
    ContextualCreateTxCheck(params, saplingActivationHeight - DEFAULT_PRE_BLOSSOM_TX_EXPIRY_DELTA - 2,
        OVERWINTER_TX_VERSION, true, OVERWINTER_VERSION_GROUP_ID, saplingActivationHeight - 2);
    ContextualCreateTxCheck(params, saplingActivationHeight - DEFAULT_PRE_BLOSSOM_TX_EXPIRY_DELTA - 1,
        OVERWINTER_TX_VERSION, true, OVERWINTER_VERSION_GROUP_ID, saplingActivationHeight - 1);
    ContextualCreateTxCheck(params, saplingActivationHeight - DEFAULT_PRE_BLOSSOM_TX_EXPIRY_DELTA,
        OVERWINTER_TX_VERSION, true, OVERWINTER_VERSION_GROUP_ID, saplingActivationHeight - 1);
    ContextualCreateTxCheck(params, saplingActivationHeight - DEFAULT_PRE_BLOSSOM_TX_EXPIRY_DELTA + 1,
        OVERWINTER_TX_VERSION, true, OVERWINTER_VERSION_GROUP_ID, saplingActivationHeight - 1);
    ContextualCreateTxCheck(params, saplingActivationHeight - DEFAULT_PRE_BLOSSOM_TX_EXPIRY_DELTA + 2,
        OVERWINTER_TX_VERSION, true, OVERWINTER_VERSION_GROUP_ID, saplingActivationHeight - 1);
    ContextualCreateTxCheck(params, saplingActivationHeight - DEFAULT_PRE_BLOSSOM_TX_EXPIRY_DELTA + 3,
        OVERWINTER_TX_VERSION, true, OVERWINTER_VERSION_GROUP_ID, saplingActivationHeight - 1);
    // Just before Sapling activation
    ContextualCreateTxCheck(params, saplingActivationHeight - 4,
        OVERWINTER_TX_VERSION, true, OVERWINTER_VERSION_GROUP_ID, saplingActivationHeight - 1);
    ContextualCreateTxCheck(params, saplingActivationHeight - 3,
        OVERWINTER_TX_VERSION, true, OVERWINTER_VERSION_GROUP_ID, saplingActivationHeight - 1);
    ContextualCreateTxCheck(params, saplingActivationHeight - 2,
        OVERWINTER_TX_VERSION, true, OVERWINTER_VERSION_GROUP_ID, saplingActivationHeight - 1);
    ContextualCreateTxCheck(params, saplingActivationHeight - 1,
        OVERWINTER_TX_VERSION, true, OVERWINTER_VERSION_GROUP_ID, saplingActivationHeight - 1);
    // Sapling activates
    ContextualCreateTxCheck(params, saplingActivationHeight,
        SAPLING_TX_VERSION, true, SAPLING_VERSION_GROUP_ID, saplingActivationHeight + DEFAULT_PRE_BLOSSOM_TX_EXPIRY_DELTA);

    // Revert to default
    RegtestDeactivateSapling();
}

// Test a v1 transaction which has a malformed header, perhaps modified in-flight
TEST(ChecktransactionTests, BadTxReceivedOverNetwork)
{
    // First four bytes <01 00 00 00> have been modified to be <FC FF FF FF> (-4 as an int32)
    std::string goodPrefix = "01000000";
    std::string badPrefix = "fcffffff";
    std::string hexTx = "0176c6541939b95f8d8b7779a77a0863b2a0267e281a050148326f0ea07c3608fb000000006a47304402207c68117a6263486281af0cc5d3bee6db565b6dce19ffacc4cb361906eece82f8022007f604382dee2c1fde41c4e6e7c1ae36cfa28b5b27350c4bfaa27f555529eace01210307ff9bef60f2ac4ceb1169a9f7d2c773d6c7f4ab6699e1e5ebc2e0c6d291c733feffffff02c0d45407000000001976a9145eaaf6718517ec8a291c6e64b16183292e7011f788ac5ef44534000000001976a91485e12fb9967c96759eae1c6b1e9c07ce977b638788acbe000000";

    // Good v1 tx
    {
        std::vector<unsigned char> txData(ParseHex(goodPrefix + hexTx ));
        CDataStream ssData(txData, SER_NETWORK, PROTOCOL_VERSION);
        CTransaction tx;
        ssData >> tx;
        EXPECT_EQ(tx.nVersion, 1);
        EXPECT_EQ(tx.fOverwintered, false);
    }

    // Good v1 mutable tx
    {
        std::vector<unsigned char> txData(ParseHex(goodPrefix + hexTx ));
        CDataStream ssData(txData, SER_NETWORK, PROTOCOL_VERSION);
        CMutableTransaction mtx;
        ssData >> mtx;
        EXPECT_EQ(mtx.nVersion, 1);
    }

    // Bad tx
    {
        std::vector<unsigned char> txData(ParseHex(badPrefix + hexTx ));
        CDataStream ssData(txData, SER_NETWORK, PROTOCOL_VERSION);
        try {
            CTransaction tx;
            ssData >> tx;
            FAIL() << "Expected std::ios_base::failure 'Unknown transaction format'";
        }
        catch(std::ios_base::failure & err) {
            EXPECT_THAT(err.what(), testing::HasSubstr(std::string("Unknown transaction format")));
        }
        catch(...) {
            FAIL() << "Expected std::ios_base::failure 'Unknown transaction format', got some other exception";
        }
    }

    // Bad mutable tx
    {
        std::vector<unsigned char> txData(ParseHex(badPrefix + hexTx ));
        CDataStream ssData(txData, SER_NETWORK, PROTOCOL_VERSION);
        try {
            CMutableTransaction mtx;
            ssData >> mtx;
            FAIL() << "Expected std::ios_base::failure 'Unknown transaction format'";
        }
        catch(std::ios_base::failure & err) {
            EXPECT_THAT(err.what(), testing::HasSubstr(std::string("Unknown transaction format")));
        }
        catch(...) {
            FAIL() << "Expected std::ios_base::failure 'Unknown transaction format', got some other exception";
        }
    }
}

TEST(ChecktransactionTests, InvalidSaplingShieldedCoinbase) {
    RegtestActivateSapling();

    CMutableTransaction mtx = GetValidTransaction();
    mtx.fOverwintered = true;
    mtx.nVersionGroupId = SAPLING_VERSION_GROUP_ID;
    mtx.nVersion = SAPLING_TX_VERSION;

    // Make it an invalid shielded coinbase (no ciphertexts or commitments).
    mtx.vin.resize(1);
    mtx.vin[0].prevout.SetNull();
    mtx.saplingBundle = sapling::test_only_invalid_bundle(0, 1, 0);
    mtx.vJoinSplit.resize(0);

    CTransaction tx(mtx);
    EXPECT_TRUE(tx.IsCoinBase());

    // Before Heartwood, output descriptions are rejected.
    MockCValidationState state;
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-cb-has-output-description", BodyCorruption::Default, "")).Times(1);
    ContextualCheckTransaction(tx, state, Params(), 10, 57);

    RegtestActivateHeartwood(false, Consensus::NetworkUpgrade::ALWAYS_ACTIVE);

    // From Heartwood, the output description is allowed but invalid (undecryptable).
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-cb-output-desc-invalid-ct", BodyCorruption::Default, "")).Times(1);
    ContextualCheckTransaction(tx, state, Params(), 10, 57);

    RegtestDeactivateHeartwood();
}

TEST(ChecktransactionTests, HeartwoodAcceptsSaplingShieldedCoinbase) {
    LoadProofParameters();

    RegtestActivateHeartwood(false, Consensus::NetworkUpgrade::ALWAYS_ACTIVE);
    auto chainparams = Params();

    auto saplingAnchor = SaplingMerkleTree::empty_root().GetRawBytes();
    auto builder = sapling::new_builder(*chainparams.RustNetwork(), 10, saplingAnchor, true);
    builder->add_recipient(
        uint256().GetRawBytes(),
        libzcash::SaplingSpendingKey::random().default_address().GetRawBytes(),
        123456,
        libzcash::Memo::ToBytes(std::nullopt));

    CMutableTransaction mtx = GetValidTransaction();
    mtx.fOverwintered = true;
    mtx.nVersionGroupId = SAPLING_VERSION_GROUP_ID;
    mtx.nVersion = SAPLING_TX_VERSION;

    mtx.vin.resize(1);
    mtx.vin[0].prevout.SetNull();
    mtx.vJoinSplit.resize(0);
    mtx.saplingBundle = sapling::apply_bundle_signatures(sapling::build_bundle(std::move(builder)), {});
    auto outputs = mtx.saplingBundle.GetDetails().outputs();
    auto& odesc = outputs[0];

    // Transaction should fail with a bad public cmu.
    {
        sapling::test_only_replace_output_parts(
            mtx.saplingBundle.GetDetailsMut(),
            0,
            uint256S("1234").GetRawBytes(),
            odesc.enc_ciphertext(),
            odesc.out_ciphertext());
        CTransaction tx(mtx);
        EXPECT_TRUE(tx.IsCoinBase());

        MockCValidationState state;
        EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-cb-output-desc-invalid-ct", BodyCorruption::Default, "")).Times(1);
        ContextualCheckTransaction(tx, state, chainparams, 10, 57);
    }

    // Transaction should fail with a bad outCiphertext.
    {
        sapling::test_only_replace_output_parts(
            mtx.saplingBundle.GetDetailsMut(),
            0,
            odesc.cmu(),
            odesc.enc_ciphertext(),
            {{}});
        CTransaction tx(mtx);
        EXPECT_TRUE(tx.IsCoinBase());

        MockCValidationState state;
        EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-cb-output-desc-invalid-ct", BodyCorruption::Default, "")).Times(1);
        ContextualCheckTransaction(tx, state, chainparams, 10, 57);
    }

    // Transaction should fail with a bad encCiphertext.
    // Error message is the same because the Rust decryptor doesn't say which failed.
    {
        sapling::test_only_replace_output_parts(
            mtx.saplingBundle.GetDetailsMut(),
            0,
            odesc.cmu(),
            {{}},
            odesc.out_ciphertext());
        CTransaction tx(mtx);
        EXPECT_TRUE(tx.IsCoinBase());

        MockCValidationState state;
        EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-cb-output-desc-invalid-ct", BodyCorruption::Default, "")).Times(1);
        ContextualCheckTransaction(tx, state, chainparams, 10, 57);
    }

    // Test the success case.
    {
        sapling::test_only_replace_output_parts(
            mtx.saplingBundle.GetDetailsMut(),
            0,
            odesc.cmu(),
            odesc.enc_ciphertext(),
            odesc.out_ciphertext());
        CTransaction tx(mtx);
        EXPECT_TRUE(tx.IsCoinBase());

        MockCValidationState state;
        EXPECT_TRUE(ContextualCheckTransaction(tx, state, chainparams, 10, 57));
    }

    RegtestDeactivateHeartwood();
}

// Check that the consensus rules relevant to valueBalanceSapling, vShieldedOutput, and
// bindingSig from https://zips.z.cash/protocol/protocol.pdf#txnencoding are
// applied to coinbase transactions.
TEST(ChecktransactionTests, HeartwoodEnforcesSaplingRulesOnShieldedCoinbase) {
    LoadProofParameters();

    RegtestActivateHeartwood(false, Consensus::NetworkUpgrade::ALWAYS_ACTIVE);
    auto chainparams = Params();

    CMutableTransaction mtx = GetValidTransaction();
    mtx.fOverwintered = true;
    mtx.nVersionGroupId = SAPLING_VERSION_GROUP_ID;
    mtx.nVersion = SAPLING_TX_VERSION;

    mtx.vin.resize(1);
    mtx.vin[0].prevout.SetNull();
    mtx.vin[0].scriptSig << 123;
    mtx.vJoinSplit.resize(0);
    mtx.saplingBundle = sapling::test_only_invalid_bundle(0, 1, -1000);

    // Coinbase transaction should fail non-contextual checks with no shielded
    // outputs and non-zero valueBalanceSapling.
    // TODO: The new Sapling bundle API prevents us from constructing this case.
    /*
    {
        CTransaction tx(mtx);
        EXPECT_TRUE(tx.IsCoinBase());

        MockCValidationState state;
        EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-txns-valuebalance-nonzero", BodyCorruption::Default, "")).Times(1);
        EXPECT_FALSE(CheckTransactionWithoutProofVerification(tx, state));
    }
    */

    // Add a Sapling output.
    auto saplingAnchor = SaplingMerkleTree::empty_root().GetRawBytes();
    auto builder = sapling::new_builder(*chainparams.RustNetwork(), 10, saplingAnchor, true);
    builder->add_recipient(
        uint256().GetRawBytes(),
        libzcash::SaplingSpendingKey::random().default_address().GetRawBytes(),
        1000,
        libzcash::Memo::ToBytes(std::nullopt));
    mtx.saplingBundle = sapling::apply_bundle_signatures(sapling::build_bundle(std::move(builder)), {});

    CTransaction tx(mtx);
    EXPECT_TRUE(tx.IsCoinBase());

    // Coinbase transaction should now pass non-contextual checks.
    MockCValidationState state;
    EXPECT_TRUE(CheckTransactionWithoutProofVerification(tx, state));

    // Coinbase transaction should pass contextual checks.
    EXPECT_TRUE(ContextualCheckTransaction(tx, state, chainparams, 10, 57));

    std::optional<rust::Box<sapling::BatchValidator>> saplingAuth = sapling::init_batch_validator(false);
    std::optional<rust::Box<orchard::BatchValidator>> orchardAuth = std::nullopt;
    auto heartwoodBranchId = NetworkUpgradeInfo[Consensus::UPGRADE_HEARTWOOD].nBranchId;

    // Coinbase transaction does not pass shielded input checks, as bindingSig
    // consensus rule is enforced. ContextualCheckShieldedInputs passes because
    // the rest of the input checks pass, but saplingAuth fails when it attempts
    // to validate the batch of signatures that includes bindingSig.
    // - Note that coinbase txs don't have a previous output corresponding to
    //   their transparent input; ZIP 244 handles this by making the coinbase
    //   sighash the txid.
    PrecomputedTransactionData txdata(tx, {});
    AssumeShieldedInputsExistAndAreSpendable baseView;
    CCoinsViewCache view(&baseView);
    EXPECT_TRUE(ContextualCheckShieldedInputs(
        tx, txdata, state, view, saplingAuth, orchardAuth, chainparams.GetConsensus(), heartwoodBranchId, false, true));
    EXPECT_FALSE(saplingAuth.value()->validate());

    RegtestDeactivateHeartwood();
}


TEST(ChecktransactionTests, CanopyRejectsNonzeroVPubOld) {
    RegtestActivateSapling();

    CMutableTransaction mtx = GetValidTransaction(NetworkUpgradeInfo[Consensus::UPGRADE_SAPLING].nBranchId);

    // Make a JoinSplit with nonzero vpub_old
    mtx.vJoinSplit.resize(1);
    mtx.vJoinSplit[0].vpub_old = 1;
    mtx.vJoinSplit[0].vpub_new = 0;
    mtx.vJoinSplit[0].proof = libzcash::GrothProof();
    CreateJoinSplitSignature(mtx, NetworkUpgradeInfo[Consensus::UPGRADE_SAPLING].nBranchId);

    CTransaction tx(mtx);

    // Before Canopy, nonzero vpub_old is accepted in both non-contextual and contextual checks
    MockCValidationState state;
    EXPECT_TRUE(CheckTransactionWithoutProofVerification(tx, state));
    EXPECT_TRUE(ContextualCheckTransaction(tx, state, Params(), 1, true));

    RegtestActivateCanopy(false, Consensus::NetworkUpgrade::ALWAYS_ACTIVE);

    // After Canopy, nonzero vpub_old is accepted in non-contextual checks but rejected in contextual checks
    EXPECT_TRUE(CheckTransactionWithoutProofVerification(tx, state));
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-txns-vpub_old-nonzero", BodyCorruption::Default, "")).Times(1);
    EXPECT_FALSE(ContextualCheckTransaction(tx, state, Params(), 10, true));

    RegtestDeactivateCanopy();

}

TEST(ChecktransactionTests, CanopyAcceptsZeroVPubOld) {

    CMutableTransaction mtx = GetValidTransaction(NetworkUpgradeInfo[Consensus::UPGRADE_SAPLING].nBranchId);

    // Make a JoinSplit with zero vpub_old
    mtx.vJoinSplit.resize(1);
    mtx.vJoinSplit[0].vpub_old = 0;
    mtx.vJoinSplit[0].vpub_new = 1;
    mtx.vJoinSplit[0].proof = libzcash::GrothProof();
    CreateJoinSplitSignature(mtx, NetworkUpgradeInfo[Consensus::UPGRADE_CANOPY].nBranchId);

    CTransaction tx(mtx);

    // After Canopy, zero value vpub_old (i.e. unshielding) is accepted in both non-contextual and contextual checks
    MockCValidationState state;

    RegtestActivateCanopy(false, Consensus::NetworkUpgrade::ALWAYS_ACTIVE);

    EXPECT_TRUE(CheckTransactionWithoutProofVerification(tx, state));
    EXPECT_TRUE(ContextualCheckTransaction(tx, state, Params(), 10, true));

    RegtestDeactivateCanopy();

}

TEST(ChecktransactionTests, InvalidOrchardShieldedCoinbase) {
    LoadProofParameters();
    RegtestActivateCanopy();

    CMutableTransaction mtx;
    mtx.fOverwintered = true;
    mtx.nVersionGroupId = ZIP225_VERSION_GROUP_ID;
    mtx.nVersion = ZIP225_TX_VERSION;
    mtx.nConsensusBranchId = NetworkUpgradeInfo[Consensus::UPGRADE_NU5].nBranchId;

    // Make it an invalid shielded coinbase, by creating an all-dummy Orchard bundle.
    auto to = TestOrchardSpendingKey()
        .ToFullViewingKey()
        .GetChangeAddress();
    mtx.vin.resize(1);
    mtx.vin[0].prevout.SetNull();
    // NU5-era test: the historical insecure revision is the one in force here.
    auto builder = orchard::Builder(
        true, {orchard::OrchardValuePool::Orchard, orchard::ProtocolVersion::InsecureV1}, uint256());
    EXPECT_TRUE(builder.AddOutput(std::nullopt, to, 0, std::nullopt));
    mtx.orchardBundle = builder
        .Build().value()
        .ProveAndSign({}, uint256()).value();

    CTransaction tx(mtx);
    EXPECT_TRUE(tx.IsCoinBase());

    // Before NU5, v5 transactions are rejected.
    MockCValidationState state;
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-sapling-tx-version-group-id", BodyCorruption::Default, "")).Times(1);
    ContextualCheckTransaction(tx, state, Params(), 10, 57);

    RegtestActivateNU5();

    // From NU5, the Orchard actions are allowed but invalid (undecryptable).
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-cb-action-invalid-ciphertext", BodyCorruption::Default, "")).Times(1);
    ContextualCheckTransaction(tx, state, Params(), 10, 57);

    RegtestDeactivateNU5();
}

TEST(ChecktransactionTests, NU5AcceptsOrchardShieldedCoinbase) {
    LoadProofParameters();
    RegtestActivateNU5();
    auto chainparams = Params();

    uint256 orchardAnchor;
    // NU5-era test: the historical insecure revision is the one in force here.
    auto builder = orchard::Builder(
        true, {orchard::OrchardValuePool::Orchard, orchard::ProtocolVersion::InsecureV1}, orchardAnchor);

    // Shielded coinbase outputs must be recoverable with an all-zeroes ovk.
    RawHDSeed rawSeed(32, 0);
    GetRandBytes(rawSeed.data(), 32);
    auto to = libzcash::OrchardSpendingKey::ForAccount(HDSeed(rawSeed), Params().BIP44CoinType(), 0)
        .ToFullViewingKey()
        .ToIncomingViewingKey()
        .Address(0);
    uint256 ovk;
    EXPECT_TRUE(builder.AddOutput(ovk, to, CAmount(123456), std::nullopt));

    // orchard::Builder pads to two Actions, but does so using a "no OVK" policy for
    // dummy outputs, which violates coinbase rules requiring all shielded outputs to
    // be recoverable. We manually add a dummy output to sidestep this issue.
    // TODO: If/when we have funding streams going to Orchard recipients, this dummy
    // output can be removed.
    EXPECT_TRUE(builder.AddOutput(ovk, to, 0, std::nullopt));

    auto bundle = builder
        .Build().value()
        .ProveAndSign({}, uint256()).value();

    CMutableTransaction mtx;
    mtx.fOverwintered = true;
    mtx.nVersionGroupId = ZIP225_VERSION_GROUP_ID;
    mtx.nVersion = ZIP225_TX_VERSION;
    mtx.nConsensusBranchId = NetworkUpgradeInfo[Consensus::UPGRADE_NU5].nBranchId;

    mtx.vin.resize(1);
    mtx.vin[0].prevout.SetNull();
    mtx.orchardBundle = bundle;

    CTransaction tx(mtx);
    EXPECT_TRUE(tx.IsCoinBase());

    // Write the transaction bytes out so we can modify them to test failure cases.
    CDataStream ss(SER_DISK, PROTOCOL_VERSION);
    ss << tx;

    // Define some constants to use when calculating offsets to modify below.
    const size_t HEADER_SIZE = 4 + 4 + 4 + 4 + 4;
    const size_t TRANSPARENT_BUNDLE_SIZE = 1 + 32 + 4 + 1 + 4 + 1;
    const size_t SAPLING_BUNDLE_SIZE = 1 + 1;
    const size_t ORCHARD_BUNDLE_START = (HEADER_SIZE + TRANSPARENT_BUNDLE_SIZE + SAPLING_BUNDLE_SIZE);
    const size_t ORCHARD_BUNDLE_CMX_OFFSET = (ORCHARD_BUNDLE_START + ZC_ZIP225_ORCHARD_NUM_ACTIONS_SIZE + 32 + 32 + 32);
    const size_t ORCHARD_CMX_SIZE = 32;
    const size_t ORCHARD_EPK_SIZE = 32;

    // Verify the transaction is the expected size.
    size_t txsize = ORCHARD_BUNDLE_START + ZC_ZIP225_ORCHARD_BASE_SIZE + ZC_ZIP225_ORCHARD_MARGINAL_SIZE * 2;
    EXPECT_EQ(ss.size(), txsize);

    // Transaction should fail with a bad public cmx.
    {
        auto cmxBad = uint256S("1234");
        std::vector<char> txBytes(ss.begin(), ss.end());
        std::copy(cmxBad.begin(), cmxBad.end(), txBytes.data() + ORCHARD_BUNDLE_CMX_OFFSET);

        CDataStream ssBad(txBytes, SER_DISK, PROTOCOL_VERSION);
        CTransaction tx;
        ssBad >> tx;
        EXPECT_TRUE(tx.IsCoinBase());

        MockCValidationState state;
        EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-cb-action-invalid-ciphertext", BodyCorruption::Default, "")).Times(1);
        ContextualCheckTransaction(tx, state, chainparams, 10, 57);
    }

    // Transaction should fail with the identity epk.
    {
        auto cmxBad = uint256S("0");
        std::vector<char> txBytes(ss.begin(), ss.end());
        std::copy(cmxBad.begin(), cmxBad.end(), txBytes.data() + ORCHARD_BUNDLE_CMX_OFFSET + ORCHARD_CMX_SIZE);

        CDataStream ssBad(txBytes, SER_DISK, PROTOCOL_VERSION);
        CTransaction tx;
        EXPECT_THROW((ssBad >> tx), std::ios_base::failure);
    }

    // Transaction should fail with an invalid epk.
    {
        auto cmxBad = uint256S("0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff");
        std::vector<char> txBytes(ss.begin(), ss.end());
        std::copy(cmxBad.begin(), cmxBad.end(), txBytes.data() + ORCHARD_BUNDLE_CMX_OFFSET + ORCHARD_CMX_SIZE);

        CDataStream ssBad(txBytes, SER_DISK, PROTOCOL_VERSION);
        CTransaction tx;
        EXPECT_THROW((ssBad >> tx), std::ios_base::failure);
    }

    // Transaction should fail with a bad encCiphertext.
    {
        std::vector<char> txBytes(ss.begin(), ss.end());
        for (int i = 0; i < libzcash::SAPLING_ENCCIPHERTEXT_SIZE; i++) {
            txBytes[ORCHARD_BUNDLE_CMX_OFFSET + ORCHARD_CMX_SIZE + ORCHARD_EPK_SIZE + i] = 0;
        }

        CDataStream ssBad(txBytes, SER_DISK, PROTOCOL_VERSION);
        CTransaction tx;
        ssBad >> tx;
        EXPECT_TRUE(tx.IsCoinBase());

        MockCValidationState state;
        EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-cb-action-invalid-ciphertext", BodyCorruption::Default, "")).Times(1);
        ContextualCheckTransaction(tx, state, chainparams, 10, 57);
    }

    // Transaction should fail with a bad outCiphertext.
    {
        std::vector<char> txBytes(ss.begin(), ss.end());
        auto byteOffset =
            ORCHARD_BUNDLE_CMX_OFFSET + ORCHARD_CMX_SIZE + ORCHARD_EPK_SIZE + libzcash::SAPLING_ENCCIPHERTEXT_SIZE;
        for (int i = 0; i < libzcash::SAPLING_OUTCIPHERTEXT_SIZE; i++) {
            txBytes[byteOffset + i] = 0;
        }

        CDataStream ssBad(txBytes, SER_DISK, PROTOCOL_VERSION);
        CTransaction tx;
        ssBad >> tx;
        EXPECT_TRUE(tx.IsCoinBase());

        MockCValidationState state;
        EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-cb-action-invalid-ciphertext", BodyCorruption::Default, "")).Times(1);
        ContextualCheckTransaction(tx, state, chainparams, 10, 57);
    }

    // Test the success case.
    {
        MockCValidationState state;
        EXPECT_TRUE(ContextualCheckTransaction(tx, state, chainparams, 10, 57));
    }

    RegtestDeactivateNU5();
}

// Check that the consensus rules relevant to valueBalanceOrchard, and
// vOrchardActions from https://zips.z.cash/protocol/protocol.pdf#txnencoding
// are applied to coinbase transactions.
TEST(ChecktransactionTests, NU5EnforcesOrchardRulesOnShieldedCoinbase) {
    LoadProofParameters();
    RegtestActivateNU5();
    auto chainparams = Params();

    uint256 orchardAnchor;
    // NU5-era test: the historical insecure revision is the one in force here.
    auto builder = orchard::Builder(
        true, {orchard::OrchardValuePool::Orchard, orchard::ProtocolVersion::InsecureV1}, orchardAnchor);

    // Shielded coinbase outputs must be recoverable with an all-zeroes ovk.
    RawHDSeed rawSeed(32, 0);
    GetRandBytes(rawSeed.data(), 32);
    auto to = libzcash::OrchardSpendingKey::ForAccount(HDSeed(rawSeed), Params().BIP44CoinType(), 0)
        .ToFullViewingKey()
        .ToIncomingViewingKey()
        .Address(0);
    uint256 ovk;
    EXPECT_TRUE(builder.AddOutput(ovk, to, CAmount(1000), std::nullopt));

    // orchard::Builder pads to two Actions, but does so using a "no OVK" policy for
    // dummy outputs, which violates coinbase rules requiring all shielded outputs to
    // be recoverable. We manually add a dummy output to sidestep this issue.
    // TODO: If/when we have funding streams going to Orchard recipients, this dummy
    // output can be removed.
    EXPECT_TRUE(builder.AddOutput(ovk, to, 0, std::nullopt));

    auto bundle = builder
        .Build().value()
        .ProveAndSign({}, uint256()).value();

    CMutableTransaction mtx;
    mtx.fOverwintered = true;
    mtx.nVersionGroupId = ZIP225_VERSION_GROUP_ID;
    mtx.nVersion = ZIP225_TX_VERSION;
    mtx.nConsensusBranchId = NetworkUpgradeInfo[Consensus::UPGRADE_NU5].nBranchId;

    mtx.vin.resize(1);
    mtx.vin[0].prevout.SetNull();
    mtx.vin[0].scriptSig << 123;
    mtx.orchardBundle = bundle;

    CTransaction tx(mtx);
    EXPECT_TRUE(tx.IsCoinBase());

    // Write the transaction bytes out so we can modify them to test failure cases.
    CDataStream ss(SER_DISK, PROTOCOL_VERSION);
    ss << tx;

    // Define some constants to use when calculating offsets to modify below.
    const size_t HEADER_SIZE = 4 + 4 + 4 + 4 + 4;
    const size_t TRANSPARENT_BUNDLE_SIZE = 1 + 32 + 4 + 1 + 2 + 4 + 1;
    const size_t SAPLING_BUNDLE_SIZE = 1 + 1;
    const size_t ORCHARD_BUNDLE_START = (HEADER_SIZE + TRANSPARENT_BUNDLE_SIZE + SAPLING_BUNDLE_SIZE);
    const size_t ORCHARD_BUNDLE_VALUEBALANCE_OFFSET = (
        ORCHARD_BUNDLE_START +
        ZC_ZIP225_ORCHARD_NUM_ACTIONS_SIZE +
        ZC_ZIP225_ORCHARD_ACTION_SIZE * 2 +
        ZC_ZIP225_ORCHARD_FLAGS_SIZE);

    // Verify the transaction is the expected size.
    size_t txsize = ORCHARD_BUNDLE_START + ZC_ZIP225_ORCHARD_BASE_SIZE + ZC_ZIP225_ORCHARD_MARGINAL_SIZE * 2;
    EXPECT_EQ(ss.size(), txsize);

    // Coinbase transaction should fail non-contextual checks with valueBalanceSapling
    // out of range.
    {
        std::vector<char> txBytes(ss.begin(), ss.end());
        uint64_t valueBalanceBad = htole64(MAX_MONEY + 1);
        std::copy((char*)&valueBalanceBad, (char*)&valueBalanceBad + 8, txBytes.data() + ORCHARD_BUNDLE_VALUEBALANCE_OFFSET);

        CDataStream ssBad(txBytes, SER_DISK, PROTOCOL_VERSION);
        CTransaction tx;
        EXPECT_THROW((ssBad >> tx), std::ios_base::failure);

        // We can't actually reach the CheckTransactionWithoutProofVerification
        // consensus rule, because Rust is doing this validation at parse time.
        // MockCValidationState state;
        // EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-txns-valuebalance-toolarge", BodyCorruption::Default, "")).Times(1);
        // EXPECT_FALSE(CheckTransactionWithoutProofVerification(tx, state));
    }
    {
        std::vector<char> txBytes(ss.begin(), ss.end());
        uint64_t valueBalanceBad = htole64(-MAX_MONEY - 1);
        std::copy((char*)&valueBalanceBad, (char*)&valueBalanceBad + 8, txBytes.data() + ORCHARD_BUNDLE_VALUEBALANCE_OFFSET);

        CDataStream ssBad(txBytes, SER_DISK, PROTOCOL_VERSION);
        CTransaction tx;
        EXPECT_THROW((ssBad >> tx), std::ios_base::failure);

        // We can't actually reach the CheckTransactionWithoutProofVerification
        // consensus rule, because Rust is doing this validation at parse time.
        // MockCValidationState state;
        // EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-txns-valuebalance-toolarge", BodyCorruption::Default, "")).Times(1);
        // EXPECT_FALSE(CheckTransactionWithoutProofVerification(tx, state));
    }

    // Test the success case.
    {
        // The unmodified coinbase transaction should pass non-contextual checks.
        MockCValidationState state;
        EXPECT_TRUE(CheckTransactionWithoutProofVerification(tx, state));

        // Coinbase transaction should pass contextual checks, as bindingSigOrchard
        // consensus rule is not enforced here.
        EXPECT_TRUE(ContextualCheckTransaction(tx, state, chainparams, 10, 57));
    }

    RegtestDeactivateNU5();
}

// A v6 (ZIP 229) transaction is contextually rejected while NU6.3 is not active,
// even when it carries NU6.3's consensus branch id.
TEST(ChecktransactionTests, V6TxRejectedBeforeNU6_3) {
    RegtestActivateNU6point2();

    CMutableTransaction mtx;
    SetV6TxHeader(mtx, NetworkUpgradeInfo[Consensus::UPGRADE_NU6_3].nBranchId);

    CTransaction tx(mtx);
    MockCValidationState state;
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-nu5-tx-version-group-id", BodyCorruption::Default, "")).Times(1);
    ContextualCheckTransaction(tx, state, Params(), 1, true);

    RegtestDeactivateNU6point2();
}

// An empty v6 (ZIP 229) transaction is contextually valid once NU6.3 is active.
TEST(ChecktransactionTests, V6TxAcceptedAtNU6_3) {
    RegtestActivateNU6point3();

    CMutableTransaction mtx;
    SetV6TxHeader(mtx, NetworkUpgradeInfo[Consensus::UPGRADE_NU6_3].nBranchId);

    CTransaction tx(mtx);
    MockCValidationState state;
    EXPECT_TRUE(ContextualCheckTransaction(tx, state, Params(), 1, true));

    RegtestDeactivateNU6point3();
}

// IsStandardTx admits the v6 (ZIP 229) format once NU6.3 is active. Without
// the NU6.3 branch in the policy ladder, every post-activation wallet-built
// transaction (v6 is the default from NU6.3, §5.1a) would be rejected from
// standardness-enforcing mempools as "nu5-version" — a full send/relay outage
// on testnet, invisible on regtest where fRequireStandard is false (preflight
// P1). v4/v5 stay standard; the pre-NU6.3 ladder is unchanged. // @claude
TEST(ChecktransactionTests, V6IsStandardAtNU6_3) {
    // Minimal standard transparent skeleton: push-only scriptSig, P2PKH
    // output above the dust threshold. // @claude
    CMutableTransaction skeleton;
    skeleton.vin.resize(1);
    skeleton.vin[0].scriptSig = CScript() << std::vector<unsigned char>(65, 0);
    skeleton.vout.resize(1);
    skeleton.vout[0].nValue = 90 * CENT;
    skeleton.vout[0].scriptPubKey = GetScriptForDestination(CKeyID());

    std::string reason;
    RegtestActivateNU6point3();
    {
        // v6 is standard from NU6.3.
        CMutableTransaction mtx(skeleton);
        SetV6TxHeader(mtx, NetworkUpgradeInfo[Consensus::UPGRADE_NU6_3].nBranchId);
        EXPECT_TRUE(IsStandardTx(CTransaction(mtx), reason, Params(), 1)) << reason;

        // v5 and v4 remain standard (v5 wind-down; Sprout stays on v4).
        CMutableTransaction mtxV5(skeleton);
        mtxV5.fOverwintered = true;
        mtxV5.nVersionGroupId = ZIP225_VERSION_GROUP_ID;
        mtxV5.nVersion = ZIP225_TX_VERSION;
        mtxV5.nConsensusBranchId = NetworkUpgradeInfo[Consensus::UPGRADE_NU6_3].nBranchId;
        EXPECT_TRUE(IsStandardTx(CTransaction(mtxV5), reason, Params(), 1)) << reason;

        CMutableTransaction mtxV4(skeleton);
        mtxV4.fOverwintered = true;
        mtxV4.nVersionGroupId = SAPLING_VERSION_GROUP_ID;
        mtxV4.nVersion = SAPLING_TX_VERSION;
        EXPECT_TRUE(IsStandardTx(CTransaction(mtxV4), reason, Params(), 1)) << reason;

        // v3 falls below the NU6.3 minimum.
        CMutableTransaction mtxV3(skeleton);
        mtxV3.fOverwintered = true;
        mtxV3.nVersionGroupId = OVERWINTER_VERSION_GROUP_ID;
        mtxV3.nVersion = OVERWINTER_TX_VERSION;
        EXPECT_FALSE(IsStandardTx(CTransaction(mtxV3), reason, Params(), 1));
        EXPECT_EQ(reason, "nu6.3-version");
    }
    RegtestDeactivateNU6point3();

    // Before NU6.3 the NU5 rules still reject v6.
    RegtestActivateNU6point2();
    {
        CMutableTransaction mtx(skeleton);
        SetV6TxHeader(mtx, NetworkUpgradeInfo[Consensus::UPGRADE_NU6_3].nBranchId);
        EXPECT_FALSE(IsStandardTx(CTransaction(mtx), reason, Params(), 1));
        EXPECT_EQ(reason, "nu5-version");
    }
    RegtestDeactivateNU6point2();
}

// From NU6.3, CreateNewContextualCMutableTransaction defaults to the v6 (ZIP 229)
// format so newly-built transactions carry the Ironwood bundle slot (§5.1a). A v6
// tx must also carry nConsensusBranchId (it is >= ZIP225) so that it can be signed.
TEST(ChecktransactionTests, ContextualCreateTxIsV6AtNU6_3) {
    const Consensus::Params& params = RegtestActivateNU6point3();

    CMutableTransaction mtx = CreateNewContextualCMutableTransaction(params, 1, false);
    EXPECT_TRUE(mtx.fOverwintered);
    EXPECT_EQ(mtx.nVersionGroupId, ZIP229_VERSION_GROUP_ID);
    EXPECT_EQ(mtx.nVersion, ZIP229_TX_VERSION);
    EXPECT_TRUE(CTransaction(mtx).IsZip229V6());
    ASSERT_TRUE(mtx.nConsensusBranchId.has_value());
    EXPECT_EQ(mtx.nConsensusBranchId.value(),
              (uint32_t)NetworkUpgradeInfo[Consensus::UPGRADE_NU6_3].nBranchId);

    // requireV4 still forces the Sapling (v4) format even when NU6.3 is active.
    CMutableTransaction mtxV4 = CreateNewContextualCMutableTransaction(params, 1, true);
    EXPECT_EQ(mtxV4.nVersion, SAPLING_TX_VERSION);
    EXPECT_EQ(mtxV4.nVersionGroupId, SAPLING_VERSION_GROUP_ID);

    RegtestDeactivateNU6point3();
}

// An Ironwood bundle's proof, spend-auth signatures, and binding signature validate
// as a batch under the PostNu6_3 verifying key; the same bundle queued under a
// different sighash must fail signature validation.
TEST(ChecktransactionTests, IronwoodBundleBatchValidates) {
    LoadProofParameters();
    RegtestActivateNU6point3();

    auto to = TestOrchardSpendingKey()
        .ToFullViewingKey()
        .GetChangeAddress();

    uint256 orchardAnchor;
    uint256 sighash = uint256S("aa");
    auto builder = orchard::Builder(
        false, {orchard::OrchardValuePool::Ironwood, orchard::ProtocolVersion::V3}, orchardAnchor);
    EXPECT_TRUE(builder.AddOutput(std::nullopt, to, 0, std::nullopt));
    auto bundle = builder.Build().value().ProveAndSign({}, sighash).value();

    {
        auto batch = orchard::init_batch_validator(false, orchard::OrchardCircuitVersion::PostNu6_3);
        EXPECT_TRUE(bundle.QueueAuthValidation(*batch, sighash, orchard::BundleFormat::V6Ironwood));
        EXPECT_TRUE(batch->validate());
    }

    {
        auto batch = orchard::init_batch_validator(false, orchard::OrchardCircuitVersion::PostNu6_3);
        EXPECT_TRUE(bundle.QueueAuthValidation(*batch, uint256S("bb"), orchard::BundleFormat::V6Ironwood));
        EXPECT_FALSE(batch->validate());
    }

    RegtestDeactivateNU6point3();
}

// Helper: build a valid Orchard-format bundle in the given pool whose net value
// balance is `-outputValue` (an output funded from the transparent pool, i.e.
// value moving INTO the shielded pool). Built with the historical insecure
// revision, which imposes no cross-address restriction, so an output to an
// arbitrary recipient is constructible regardless of pool.
static OrchardBundle BuildBundleWithOutput(orchard::OrchardValuePool pool, CAmount outputValue) {
    auto to = TestOrchardSpendingKey()
        .ToFullViewingKey()
        .GetChangeAddress();
    uint256 ovk;
    auto builder = orchard::Builder(
        true, {pool, orchard::ProtocolVersion::InsecureV1}, uint256());
    EXPECT_TRUE(builder.AddOutput(ovk, to, outputValue, std::nullopt));
    // orchard::Builder pads to two Actions; the second is a zero-value dummy.
    EXPECT_TRUE(builder.AddOutput(ovk, to, 0, std::nullopt));
    return builder.Build().value().ProveAndSign({}, uint256()).value();
}

// Helper: build a well-formed V6 Ironwood-format bundle. The v6 slots require the
// V3 protocol version; the InsecureV1 bundles from BuildBundleWithOutput are a
// V5-era format the v6 slots reject, and (Ironwood, InsecureV1) is not even a
// valid builder pairing. The Ironwood pool permits cross-address transfers, so an
// ordinary AddOutput suffices here.
static OrchardBundle BuildV6IronwoodBundle() {
    auto to = TestOrchardSpendingKey()
        .ToFullViewingKey()
        .GetChangeAddress();
    auto builder = orchard::Builder(
        false, {orchard::OrchardValuePool::Ironwood, orchard::ProtocolVersion::V3}, uint256());
    EXPECT_TRUE(builder.AddOutput(std::nullopt, to, 0, std::nullopt));
    return builder.Build().value().ProveAndSign({}, uint256S("aa")).value();
}

// Helper: build a well-formed V6 Orchard-format bundle. The Orchard pool under
// protocol V3 (the V6Orchard slot) mandates the cross-address restriction, so an
// ordinary AddOutput is rejected; the only non-empty content it can carry is a
// wallet-controlled change output (paired with a fabricated spend authorized by
// the spending key). This is the orchard-pool self-send shape.
static OrchardBundle BuildV6OrchardBundle() {
    auto sk = TestOrchardSpendingKey();
    auto fvk = sk.ToFullViewingKey();
    auto to = fvk.GetChangeAddress();
    auto builder = orchard::Builder(
        false, {orchard::OrchardValuePool::Orchard, orchard::ProtocolVersion::V3}, uint256());
    EXPECT_TRUE(builder.AddChangeOutput(fvk, std::nullopt, to, 0, std::nullopt));
    return builder.Build().value().ProveAndSign({sk}, uint256S("bb")).value();
}

// [NU6.3 onward] valueBalanceOrchard MUST be nonnegative (ZIP 258 / audit fix
// fcd92c1). A non-coinbase transaction whose Orchard bundle moves value into the
// pool (negative valueBalanceOrchard) is rejected from NU6.3; a zero balance is
// accepted. Applies to both v5 and v6; tested here on v5.
TEST(ChecktransactionTests, OrchardNegativeValueBalanceRejectedFromNU6_3) {
    LoadProofParameters();
    RegtestActivateNU6point3();

    // Reject case: negative valueBalanceOrchard.
    {
        auto bundle = BuildBundleWithOutput(orchard::OrchardValuePool::Orchard, CAmount(1000));
        ASSERT_LT(bundle.GetValueBalance(), 0);

        CMutableTransaction mtx;
        mtx.fOverwintered = true;
        mtx.nVersionGroupId = ZIP225_VERSION_GROUP_ID;
        mtx.nVersion = ZIP225_TX_VERSION;
        mtx.nConsensusBranchId = NetworkUpgradeInfo[Consensus::UPGRADE_NU6_3].nBranchId;
        mtx.vin.resize(1);
        mtx.vin[0].prevout = COutPoint(uint256S("1234"), 0); // non-null => not a coinbase
        mtx.orchardBundle = bundle;

        CTransaction tx(mtx);
        ASSERT_FALSE(tx.IsCoinBase());

        MockCValidationState state;
        EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-tx-orchard-negative-valuebalance", BodyCorruption::Default, "")).Times(1);
        ContextualCheckTransaction(tx, state, Params(), 10, true);
    }

    // Accept case: zero valueBalanceOrchard passes the nonnegativity rule.
    {
        auto bundle = BuildBundleWithOutput(orchard::OrchardValuePool::Orchard, CAmount(0));
        ASSERT_EQ(bundle.GetValueBalance(), 0);

        CMutableTransaction mtx;
        mtx.fOverwintered = true;
        mtx.nVersionGroupId = ZIP225_VERSION_GROUP_ID;
        mtx.nVersion = ZIP225_TX_VERSION;
        mtx.nConsensusBranchId = NetworkUpgradeInfo[Consensus::UPGRADE_NU6_3].nBranchId;
        mtx.vin.resize(1);
        mtx.vin[0].prevout = COutPoint(uint256S("1234"), 0);
        mtx.orchardBundle = bundle;

        CTransaction tx(mtx);
        MockCValidationState state;
        EXPECT_TRUE(ContextualCheckTransaction(tx, state, Params(), 10, true));
    }

    RegtestDeactivateNU6point3();
}

// A coinbase transaction must not have spend-enabled Ironwood actions
// (CheckTransactionWithoutProofVerification, non-contextual).
TEST(ChecktransactionTests, IronwoodCoinbaseRejectsSpends) {
    LoadProofParameters();
    RegtestActivateNU6point3();

    // A non-coinbase Ironwood builder produces a bundle with spends enabled.
    auto to = TestOrchardSpendingKey()
        .ToFullViewingKey()
        .GetChangeAddress();
    auto builder = orchard::Builder(
        false, {orchard::OrchardValuePool::Ironwood, orchard::ProtocolVersion::V3}, uint256());
    EXPECT_TRUE(builder.AddOutput(std::nullopt, to, 0, std::nullopt));
    auto bundle = builder.Build().value().ProveAndSign({}, uint256()).value();
    ASSERT_TRUE(bundle.SpendsEnabled());

    // Place it in an otherwise-valid coinbase transaction.
    CMutableTransaction mtx;
    SetV6TxHeader(mtx, NetworkUpgradeInfo[Consensus::UPGRADE_NU6_3].nBranchId);
    mtx.vin.resize(1);
    mtx.vin[0].prevout.SetNull();
    mtx.vin[0].scriptSig << 123; // valid coinbase scriptSig length
    mtx.ironwoodBundle = bundle;

    CTransaction tx(mtx);
    ASSERT_TRUE(tx.IsCoinBase());

    MockCValidationState state;
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-cb-has-ironwood-spend", BodyCorruption::Default, "")).Times(1);
    CheckTransactionWithoutProofVerification(tx, state);

    RegtestDeactivateNU6point3();
}

// A v6 transaction carrying a non-empty Ironwood bundle round-trips through
// serialization: constructing the CTransaction reparses the bytes via
// librustzcash (rejecting any non-canonical v6 encoding), and re-serialization
// is byte-identical. Complements V6EmptyBundlesRoundTrip.
TEST(ChecktransactionTests, V6NonEmptyIronwoodBundleRoundTrip) {
    LoadProofParameters();
    RegtestActivateNU6point3();

    auto to = TestOrchardSpendingKey()
        .ToFullViewingKey()
        .GetChangeAddress();
    auto builder = orchard::Builder(
        false, {orchard::OrchardValuePool::Ironwood, orchard::ProtocolVersion::V3}, uint256());
    EXPECT_TRUE(builder.AddOutput(std::nullopt, to, 0, std::nullopt));
    auto bundle = builder.Build().value().ProveAndSign({}, uint256S("aa")).value();

    CMutableTransaction mtx;
    SetV6TxHeader(mtx, NetworkUpgradeInfo[Consensus::UPGRADE_NU6_3].nBranchId);
    mtx.ironwoodBundle = bundle;

    CDataStream ss(SER_NETWORK, PROTOCOL_VERSION);
    ss << mtx;
    const std::vector<unsigned char> bytes(ss.begin(), ss.end());

    CTransaction tx(deserialize, ss);
    EXPECT_TRUE(tx.GetIronwoodBundle().IsPresent());
    EXPECT_FALSE(tx.GetOrchardBundle().IsPresent());
    EXPECT_EQ(tx.GetHash(), mtx.GetHash());
    EXPECT_EQ(tx.GetAuthDigest(), mtx.GetAuthDigest());

    CDataStream ss2(SER_NETWORK, PROTOCOL_VERSION);
    ss2 << tx;
    const std::vector<unsigned char> bytes2(ss2.begin(), ss2.end());
    EXPECT_EQ(bytes, bytes2);

    RegtestDeactivateNU6point3();
}

// Runs the full v6 serialization round-trip assertion battery for one content
// permutation: `orchard`/`ironwood` are the bundles to place in the respective
// v6 slots (std::nullopt means leave that slot empty). The central guarantee is
// cross-class byte equality — the CTransaction serializer and the
// CMutableTransaction serializer must emit identical bytes for the same content,
// since the two SerializationOp blocks are duplicated and could drift apart.
static void CheckV6BundlePermutation(
    std::optional<OrchardBundle> orchard,
    std::optional<OrchardBundle> ironwood) {
    CMutableTransaction mtx;
    SetV6TxHeader(mtx, NetworkUpgradeInfo[Consensus::UPGRADE_NU6_3].nBranchId);
    if (orchard.has_value()) {
        mtx.orchardBundle = orchard.value();
    }
    if (ironwood.has_value()) {
        mtx.ironwoodBundle = ironwood.value();
    }

    // (1) Serialize the mutable transaction to bytesM.
    CDataStream ss(SER_NETWORK, PROTOCOL_VERSION);
    ss << mtx;
    const std::vector<unsigned char> bytesM(ss.begin(), ss.end());

    // (2) Deserialize into a CTransaction. Its constructor runs UpdateHash, which
    // reparses the bytes via librustzcash and rejects any non-canonical v6
    // encoding — so a successful construction is itself a canonicality assertion.
    CDataStream ssT(bytesM, SER_NETWORK, PROTOCOL_VERSION);
    CTransaction tx(deserialize, ssT);
    EXPECT_EQ(tx.GetOrchardBundle().IsPresent(), orchard.has_value());
    EXPECT_EQ(tx.GetIronwoodBundle().IsPresent(), ironwood.has_value());
    EXPECT_EQ(tx.GetHash(), mtx.GetHash());
    EXPECT_EQ(tx.GetAuthDigest(), mtx.GetAuthDigest());
    EXPECT_EQ(tx.GetConsensusBranchId(), mtx.nConsensusBranchId);

    // (3) Cross-class byte equality: re-serializing through the CTransaction path
    // must reproduce the CMutableTransaction bytes exactly.
    CDataStream ss2(SER_NETWORK, PROTOCOL_VERSION);
    ss2 << tx;
    const std::vector<unsigned char> bytesT(ss2.begin(), ss2.end());
    EXPECT_EQ(bytesT, bytesM);

    // (4) tx-bytes -> fresh CMutableTransaction and back, closing the loop
    // mtx -> bytes -> tx -> bytes -> mtx.
    CDataStream ssM2(bytesM, SER_NETWORK, PROTOCOL_VERSION);
    CMutableTransaction mtx2;
    ssM2 >> mtx2;
    CDataStream ss3(SER_NETWORK, PROTOCOL_VERSION);
    ss3 << mtx2;
    const std::vector<unsigned char> bytes2M(ss3.begin(), ss3.end());
    EXPECT_EQ(bytes2M, bytesM);
    EXPECT_EQ(mtx2.GetHash(), mtx.GetHash());

    // (5) The converting constructor CMutableTransaction(const CTransaction&) must
    // copy every bundle slot (including ironwood); otherwise this re-serialization
    // would diverge from bytesM.
    CMutableTransaction mtx3(tx);
    CDataStream ss4(SER_NETWORK, PROTOCOL_VERSION);
    ss4 << mtx3;
    const std::vector<unsigned char> bytes3M(ss4.begin(), ss4.end());
    EXPECT_EQ(bytes3M, bytesM);
}

// Exercises every v6 bundle-content permutation (empty/empty, orchard-only,
// ironwood-only, both) through the full serialization round-trip battery,
// asserting cross-class byte equality in each. The bundles are proven once per
// pool and reused across permutations (proving is expensive).
TEST(ChecktransactionTests, V6BundlePermutationsSerializationRoundTrip) {
    LoadProofParameters();
    RegtestActivateNU6point3();

    const auto orchardBundle = BuildV6OrchardBundle();
    const auto ironwoodBundle = BuildV6IronwoodBundle();

    CheckV6BundlePermutation(std::nullopt, std::nullopt);      // A: empty / empty
    CheckV6BundlePermutation(orchardBundle, std::nullopt);     // B: orchard only
    CheckV6BundlePermutation(std::nullopt, ironwoodBundle);    // C: ironwood only
    CheckV6BundlePermutation(orchardBundle, ironwoodBundle);   // D: both present

    RegtestDeactivateNU6point3();
}

// v5 (ZIP 225) transactions have no ironwood slot on the wire. Setting
// ironwoodBundle on a v5 mtx is silently dropped by the serializer (isZip229V6 is
// false), not an error. This documents that "silently dropped" semantics: the
// bytes carry no ironwood section, the reconstructed tx has no ironwood bundle,
// and re-serialization is byte-identical.
TEST(ChecktransactionTests, V5IgnoresIronwoodBundleSlot) {
    LoadProofParameters();
    RegtestActivateNU6point3();

    const auto ironwoodBundle = BuildV6IronwoodBundle();

    CMutableTransaction mtx;
    mtx.fOverwintered = true;
    mtx.nVersionGroupId = ZIP225_VERSION_GROUP_ID;
    mtx.nVersion = ZIP225_TX_VERSION;
    mtx.nConsensusBranchId = NetworkUpgradeInfo[Consensus::UPGRADE_NU6_3].nBranchId;
    mtx.ironwoodBundle = ironwoodBundle;

    CDataStream ss(SER_NETWORK, PROTOCOL_VERSION);
    ss << mtx;
    const std::vector<unsigned char> bytesM(ss.begin(), ss.end());

    CDataStream ssT(bytesM, SER_NETWORK, PROTOCOL_VERSION);
    CTransaction tx(deserialize, ssT);
    EXPECT_FALSE(tx.GetIronwoodBundle().IsPresent());
    EXPECT_FALSE(tx.GetOrchardBundle().IsPresent());

    CDataStream ss2(SER_NETWORK, PROTOCOL_VERSION);
    ss2 << tx;
    const std::vector<unsigned char> bytesT(ss2.begin(), ss2.end());
    EXPECT_EQ(bytesT, bytesM);

    RegtestDeactivateNU6point3();
}

// CDiskBlockIndex serializes the NU6.3 Ironwood fields (hashFinalIronwoodRoot,
// nIronwoodValue) only when the write-time client version is >= NU6_3_DATA_VERSION.
// This checks both directions: the fields round-trip when the gate is open, and
// are correctly skipped (left at their null/zero defaults) when it is not — the
// per-version gating the whole *_DATA_VERSION mechanism relies on.
TEST(ChecktransactionTests, CDiskBlockIndexRoundTripsIronwoodFields) {
    const uint256 ironwoodRoot = uint256S("00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff");
    const CAmount ironwoodValue = 123456789;

    CDiskBlockIndex a;
    a.nStatus = 0; // no HAVE_DATA/UNDO/ACTIVATES_UPGRADE conditional fields
    a.nVersion = 4;
    a.hashFinalIronwoodRoot = ironwoodRoot;
    a.nIronwoodValue = ironwoodValue;

    // Gate open: written at NU6.3-aware version, the fields round-trip.
    {
        CDataStream ss(SER_DISK, NU6_3_DATA_VERSION);
        ss << a;
        CDiskBlockIndex b;
        ss >> b;
        EXPECT_EQ(b.hashFinalIronwoodRoot, ironwoodRoot);
        EXPECT_EQ(b.nIronwoodValue, ironwoodValue);
    }

    // Gate closed: written at an NU6-era version (before NU6.3), the Ironwood
    // fields are not serialized, so on read they remain at their defaults.
    {
        CDataStream ss(SER_DISK, NU6_DATA_VERSION);
        ss << a;
        CDiskBlockIndex b;
        ss >> b; // must not overrun / throw
        EXPECT_TRUE(b.hashFinalIronwoodRoot.IsNull());
        EXPECT_EQ(b.nIronwoodValue, 0);
    }
}

// Review C1: the signing-side and verifier-side sighashes must agree for a v6
// transaction carrying an Orchard bundle. Before the fix,
// shielded_signature_digest built its signing TransactionData via map_bundles,
// which applies the same Orchard closure to the Ironwood slot — the signer
// committed to a phantom clone of the Orchard bundle in the Ironwood slot
// while every verifier computed the digest over the transaction's real (empty)
// slot, invalidating all shielded signatures in any Orchard-carrying v6
// transaction. This drives the exact TransactionBuilder signing flow (digest
// over the partial tx + unauthorized bundle, then ProveAndSign with it) and
// checks it against the verifier's own digest. // @claude
TEST(ChecktransactionTests, V6OrchardSigningSighashMatchesVerifier) {
    LoadProofParameters();
    RegtestActivateNU6point3();
    {
        auto chainparams = Params();
        uint32_t consensusBranchId = NetworkUpgradeInfo[Consensus::UPGRADE_NU6_3].nBranchId;

        // A post-NU6.3 Orchard change-to-self bundle: the only constructible
        // non-empty content for the (Orchard, V3) cross-address-restricted slot.
        auto sk = TestOrchardSpendingKey();
        auto fvk = sk.ToFullViewingKey();
        auto to = fvk.GetChangeAddress();
        auto orchardBuilder = orchard::Builder(
            false, {orchard::OrchardValuePool::Orchard, orchard::ProtocolVersion::V3}, uint256());
        EXPECT_TRUE(orchardBuilder.AddChangeOutput(fvk, std::nullopt, to, 0, std::nullopt));
        auto unauthorized = orchardBuilder.Build();
        ASSERT_TRUE(unauthorized.has_value());

        // The partial v6 transaction: every shielded slot serializes empty.
        CMutableTransaction mtx;
        SetV6TxHeader(mtx, consensusBranchId);

        // An empty Sapling bundle, as TransactionBuilder always carries one.
        auto saplingAnchor = SaplingMerkleTree::empty_root().GetRawBytes();
        auto saplingBuilder = sapling::new_builder(*chainparams.RustNetwork(), 1, saplingAnchor, false);
        auto saplingBundle = sapling::build_bundle(std::move(saplingBuilder));

        // Builder-side digest, exactly as TransactionBuilder computes it.
        uint256 dataToBeSigned = ProduceShieldedSignatureHash(
            consensusBranchId, CTransaction(mtx), {}, *saplingBundle,
            unauthorized, std::nullopt);

        auto authorized = unauthorized.value().ProveAndSign({sk}, dataToBeSigned);
        ASSERT_TRUE(authorized.has_value());
        mtx.orchardBundle = authorized.value();
        CTransaction tx(mtx);

        // Verifier-side digest, exactly as ContextualCheckShieldedInputs computes it.
        const std::vector<CTxOut> noPrevOutputs;
        const PrecomputedTransactionData txdata(tx, noPrevOutputs);
        CScript scriptCode;
        uint256 verifierSighash = SignatureHash(
            scriptCode, tx, NOT_AN_INPUT, SIGHASH_ALL, 0, consensusBranchId, txdata);

        EXPECT_EQ(dataToBeSigned, verifierSighash);

        // The spend-auth and binding signatures made over the builder digest
        // must batch-validate under the verifier digest.
        auto batch = orchard::init_batch_validator(false, orchard::OrchardCircuitVersion::PostNu6_3);
        EXPECT_TRUE(tx.GetOrchardBundle().QueueAuthValidation(
            *batch, verifierSighash, orchard::BundleFormat::V6Orchard));
        EXPECT_TRUE(batch->validate());
    }
    RegtestDeactivateNU6point3();
}

// Review H1: the builder's output-rejection must be observable. From NU6.3 the
// (Orchard, V3) pool rejects every AddOutput recipient under the cross-address
// restriction — including the wallet's own address — while AddChangeOutput (the
// deliberate change-to-self exception) succeeds for the same address. Before
// the fix the C++ wrappers discarded the FFI result and recorded the action
// anyway, so an Orchard spend's dropped payment/change silently became miner
// fee. The Ironwood-V3 and Orchard-InsecureV1 rows pin the unrestricted
// behavior. // @claude
TEST(ChecktransactionTests, OrchardV3BuilderRejectsOutputsAndReportsIt) {
    auto sk = TestOrchardSpendingKey();
    auto fvk = sk.ToFullViewingKey();
    auto to = fvk.GetChangeAddress();

    // (Orchard, V3): every recipient rejected; change-to-self permitted.
    {
        auto builder = orchard::Builder(
            false, {orchard::OrchardValuePool::Orchard, orchard::ProtocolVersion::V3}, uint256());
        EXPECT_FALSE(builder.AddOutput(std::nullopt, to, 0, std::nullopt));
        EXPECT_TRUE(builder.AddChangeOutput(fvk, std::nullopt, to, 0, std::nullopt));
    }
    // (Ironwood, V3): cross-address transfers permitted.
    {
        auto builder = orchard::Builder(
            false, {orchard::OrchardValuePool::Ironwood, orchard::ProtocolVersion::V3}, uint256());
        EXPECT_TRUE(builder.AddOutput(std::nullopt, to, 0, std::nullopt));
    }
    // (Orchard, InsecureV1): pre-NU6.3 semantics, outputs accepted.
    {
        auto builder = orchard::Builder(
            false, {orchard::OrchardValuePool::Orchard, orchard::ProtocolVersion::InsecureV1}, uint256());
        EXPECT_TRUE(builder.AddOutput(std::nullopt, to, 0, std::nullopt));
    }
}

// ===== NU6.3 rule battery mirroring the Zebra v6.0.0 test suite =====
//
// Zebra fabricates invalid shielded data directly (zebra-consensus/src/
// transaction/tests.rs, zebra-chain/src/transaction/tests/vectors.rs); zcashd's
// bundles are opaque librustzcash objects, so the equivalent coverage is built
// by serializing a real builder-made transaction, patching specific bytes, and
// re-parsing. That also pins WHERE each rule is enforced in this stack: some
// violations are grammar errors the vendored crate rejects at parse
// (Flags::from_byte — reserved bits, Orchard-pool bit 2), others deserialize
// fine and must be caught by the C++ checks in
// CheckTransactionWithoutProofVerification. If a crate bump ever loosens the
// parse grammar, the EXPECT_THROW tests here fail — the signal to add the C++
// backstop Zebra carries. // @claude

// v5/v6 OrchardAction wire layout (the Ironwood component reuses it, ZIP 229):
// cv(32) nullifier(32) rk(32) cmx(32) ephemeralKey(32) encCiphertext(580)
// outCiphertext(80). A bundle section is: nActions(compactsize) actions
// flags(1) valueBalance(8) anchor(32) proof(compactsize+bytes) spendAuthSigs
// (64 per action) bindingSig(64).
constexpr size_t V6_ACTION_SIZE = 820;
constexpr size_t V6_ACTION_NULLIFIER_OFFSET = 32;
constexpr size_t V6_ACTION_EPK_OFFSET = 128;

static std::vector<unsigned char> SerializeTx(const CMutableTransaction& mtx) {
    CDataStream ss(SER_NETWORK, PROTOCOL_VERSION);
    ss << mtx;
    return std::vector<unsigned char>(ss.begin(), ss.end());
}

// Locates the byte offset where a trailing bundle section starts. `emptied` is
// the serialization of the same transaction with that bundle slot cleared: it
// must be a strict prefix of `full` ending in `emptySlotsAtTail` empty-slot
// 0x00 markers (nActions = 0), the first of which is where the section begins.
// Orchard and Ironwood are the final two v6 components (Ironwood last), so
// clearing the Ironwood slot leaves one trailing marker and clearing the
// Orchard slot leaves two; v5's Orchard section is last, leaving one.
//
// Returns nullopt (with a recorded test failure) if the prefix property does
// not hold, so a caller can bail before indexing with a bogus offset — the
// previous EXPECT_* guards were non-fatal and the helper's own std::equal
// could read out of bounds after one failed (review L-P2-3). Callers must
// ASSERT_TRUE(...has_value()). // @claude
static std::optional<size_t> LocateBundleSection(
    const std::vector<unsigned char>& full,
    const std::vector<unsigned char>& emptied,
    size_t emptySlotsAtTail)
{
    if (emptied.size() >= full.size()) {
        ADD_FAILURE() << "emptied serialization (" << emptied.size()
                      << " bytes) is not shorter than the full one (" << full.size() << ")";
        return std::nullopt;
    }
    if (emptied.size() < emptySlotsAtTail) {
        ADD_FAILURE() << "emptied serialization is shorter than the expected tail markers";
        return std::nullopt;
    }
    for (size_t i = 0; i < emptySlotsAtTail; i++) {
        if (emptied[emptied.size() - 1 - i] != 0x00) {
            ADD_FAILURE() << "expected empty-slot 0x00 marker at tail offset " << i;
            return std::nullopt;
        }
    }
    size_t start = emptied.size() - emptySlotsAtTail;
    if (!std::equal(emptied.begin(), emptied.begin() + start, full.begin())) {
        ADD_FAILURE() << "emptied serialization is not a prefix of the full one";
        return std::nullopt;
    }
    return start;
}

static CTransaction DeserializeTx(const std::vector<unsigned char>& bytes) {
    CDataStream ss(bytes, SER_NETWORK, PROTOCOL_VERSION);
    return CTransaction(deserialize, ss);
}

// [NU6.3] An Ironwood bundle with actions present must have flags permitting
// spends or outputs (Zebra: v6_transaction_with_ironwood_actions_must_have_flags
// / NotEnoughIronwoodFlags). Flags 0x00 is a valid byte for the parse grammar,
// so the transaction deserializes and the C++ check must catch it — this is a
// rule the crate does NOT enforce at parse. The transparent input/output keep
// the earlier source/sink-of-funds checks satisfied once the corrupted flags
// stop the Ironwood bundle counting as either. // @claude
TEST(ChecktransactionTests, IronwoodFlagsMustPermitActions) {
    LoadProofParameters();
    RegtestActivateNU6point3();

    CMutableTransaction mtx;
    SetV6TxHeader(mtx, NetworkUpgradeInfo[Consensus::UPGRADE_NU6_3].nBranchId);
    mtx.ironwoodBundle = BuildV6IronwoodBundle();
    mtx.vin.resize(1);
    mtx.vin[0].prevout = COutPoint(uint256S("1234"), 0);
    mtx.vout.resize(1);
    mtx.vout[0].nValue = 1000;
    mtx.vout[0].scriptPubKey = CScript() << OP_TRUE;

    auto bytes = SerializeTx(mtx);
    CMutableTransaction emptied(mtx);
    emptied.ironwoodBundle = OrchardBundle();
    auto startOpt = LocateBundleSection(bytes, SerializeTx(emptied), 1);
    ASSERT_TRUE(startOpt.has_value());
    size_t start = startOpt.value();

    ASSERT_EQ(bytes[start], 2);  // one real output + one dummy padding action
    size_t flagsOff = start + 1 + 2 * V6_ACTION_SIZE;
    // Non-coinbase Ironwood-V3 builder: spends | outputs | crossAddress.
    ASSERT_EQ(bytes[flagsOff], 0x07);

    bytes[flagsOff] = 0x00;
    CTransaction tx = DeserializeTx(bytes);

    MockCValidationState state;
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-tx-ironwood-flags-disable-actions", BodyCorruption::Default, "")).Times(1);
    CheckTransactionWithoutProofVerification(tx, state);

    RegtestDeactivateNU6point3();
}

// ZIP 229: flag bits 3..7 are reserved and MUST be 0. The vendored crate
// enforces this at parse (Flags::from_byte returns None), so the corrupted
// transaction must fail to deserialize — there is no C++ backstop. // @claude
TEST(ChecktransactionTests, IronwoodReservedFlagBitsRejectedAtParse) {
    LoadProofParameters();
    RegtestActivateNU6point3();

    CMutableTransaction mtx;
    SetV6TxHeader(mtx, NetworkUpgradeInfo[Consensus::UPGRADE_NU6_3].nBranchId);
    mtx.ironwoodBundle = BuildV6IronwoodBundle();

    auto bytes = SerializeTx(mtx);
    CMutableTransaction emptied(mtx);
    emptied.ironwoodBundle = OrchardBundle();
    auto startOpt = LocateBundleSection(bytes, SerializeTx(emptied), 1);
    ASSERT_TRUE(startOpt.has_value());
    size_t start = startOpt.value();
    size_t flagsOff = start + 1 + 2 * V6_ACTION_SIZE;
    ASSERT_EQ(bytes[flagsOff], 0x07);

    bytes[flagsOff] = 0x0F;  // set reserved bit 3
    EXPECT_THROW(DeserializeTx(bytes), std::exception);

    RegtestDeactivateNU6point3();
}

// [NU6.3] The enableCrossAddress flag (bit 2) MUST be 0 for the Orchard pool
// (Zebra: v6_orchard_bundle_must_not_enable_cross_address). Zebra enforces this
// as a consensus check; in zcashd the vendored crate rejects the bit at parse
// for any Orchard-pool bundle, so a v6 Orchard slot carrying it must fail to
// deserialize. // @claude
TEST(ChecktransactionTests, V6OrchardSlotRejectsCrossAddressFlagAtParse) {
    LoadProofParameters();
    RegtestActivateNU6point3();

    CMutableTransaction mtx;
    SetV6TxHeader(mtx, NetworkUpgradeInfo[Consensus::UPGRADE_NU6_3].nBranchId);
    mtx.orchardBundle = BuildV6OrchardBundle();

    auto bytes = SerializeTx(mtx);
    // The full tx still ends with the empty Ironwood slot's 0x00.
    ASSERT_EQ(bytes.back(), 0x00);
    CMutableTransaction emptied(mtx);
    emptied.orchardBundle = OrchardBundle();
    auto startOpt = LocateBundleSection(bytes, SerializeTx(emptied), 2);
    ASSERT_TRUE(startOpt.has_value());
    size_t start = startOpt.value();

    ASSERT_EQ(bytes[start], 2);  // change output + dummy padding action
    size_t flagsOff = start + 1 + 2 * V6_ACTION_SIZE;
    // Non-coinbase Orchard-V3 builder: spends | outputs, no cross-address.
    ASSERT_EQ(bytes[flagsOff], 0x03);

    bytes[flagsOff] = 0x07;  // set enableCrossAddress on the Orchard slot
    EXPECT_THROW(DeserializeTx(bytes), std::exception);

    RegtestDeactivateNU6point3();
}

// v5 wind-down, parse half (§10.2): flag bit 2 remains reserved-zero for the
// v5 Orchard format after NU6.3 — Zebra documents that "a v5 Orchard bundle
// rejects the flag bit at deserialization", and our parser must agree. A
// regression here (e.g. a crate bump reading v5 flags with v6 semantics) would
// be a consensus split. // @claude
TEST(ChecktransactionTests, V5OrchardRejectsCrossAddressFlagAtParse) {
    LoadProofParameters();
    RegtestActivateNU6point3();

    CMutableTransaction mtx;
    mtx.fOverwintered = true;
    mtx.nVersionGroupId = ZIP225_VERSION_GROUP_ID;
    mtx.nVersion = ZIP225_TX_VERSION;
    mtx.nConsensusBranchId = NetworkUpgradeInfo[Consensus::UPGRADE_NU6_3].nBranchId;
    mtx.orchardBundle = BuildBundleWithOutput(orchard::OrchardValuePool::Orchard, 1000);

    auto bytes = SerializeTx(mtx);
    CMutableTransaction emptied(mtx);
    emptied.orchardBundle = OrchardBundle();
    auto startOpt = LocateBundleSection(bytes, SerializeTx(emptied), 1);
    ASSERT_TRUE(startOpt.has_value());
    size_t start = startOpt.value();

    ASSERT_EQ(bytes[start], 2);
    size_t flagsOff = start + 1 + 2 * V6_ACTION_SIZE;
    // Coinbase-style builder: outputs enabled, spends disabled.
    ASSERT_EQ(bytes[flagsOff], 0x02);

    bytes[flagsOff] = 0x06;  // set the (v5-reserved) cross-address bit
    EXPECT_THROW(DeserializeTx(bytes), std::exception);

    RegtestDeactivateNU6point3();
}

// [NU6.3] A transaction revealing the same Ironwood nullifier twice is
// rejected (Zebra: v6_transaction_with_duplicate_ironwood_nullifier_is_rejected).
// Nullifiers are plain field elements at parse, so the duplicate deserializes
// and the C++ in-transaction dedup must catch it. This is the in-tx half; the
// cross-transaction half is covered by IronwoodShieldedRequirements. // @claude
TEST(ChecktransactionTests, IronwoodDuplicateNullifierInTxRejected) {
    LoadProofParameters();
    RegtestActivateNU6point3();

    CMutableTransaction mtx;
    SetV6TxHeader(mtx, NetworkUpgradeInfo[Consensus::UPGRADE_NU6_3].nBranchId);
    mtx.ironwoodBundle = BuildV6IronwoodBundle();

    auto bytes = SerializeTx(mtx);
    CMutableTransaction emptied(mtx);
    emptied.ironwoodBundle = OrchardBundle();
    auto startOpt = LocateBundleSection(bytes, SerializeTx(emptied), 1);
    ASSERT_TRUE(startOpt.has_value());
    size_t start = startOpt.value();
    ASSERT_EQ(bytes[start], 2);

    size_t nf0 = start + 1 + V6_ACTION_NULLIFIER_OFFSET;
    size_t nf1 = start + 1 + V6_ACTION_SIZE + V6_ACTION_NULLIFIER_OFFSET;
    // The two (dummy-spend) nullifiers are random and distinct before the copy.
    ASSERT_FALSE(std::equal(bytes.begin() + nf0, bytes.begin() + nf0 + 32, bytes.begin() + nf1));
    std::copy(bytes.begin() + nf0, bytes.begin() + nf0 + 32, bytes.begin() + nf1);

    CTransaction tx = DeserializeTx(bytes);
    MockCValidationState state;
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-ironwood-nullifiers-duplicate", BodyCorruption::Default, "")).Times(1);
    CheckTransactionWithoutProofVerification(tx, state);

    RegtestDeactivateNU6point3();
}

// Ironwood action-field encoding: ephemeralKey must be a valid, non-identity
// Pallas point (§5.4.9.4). In this stack the rejection happens at parse —
// zcashd's parse_v6 validates epk while building the bundle ("not a valid
// non-identity Pallas point"), so the all-zeroes identity encoding must fail
// to deserialize. The C++ bad-ironwood-action-identity-point check
// (ValidateWithoutProofVerification) is the backstop should a crate bump move
// that validation; if this EXPECT_THROW starts failing, the backstop is what
// must fire instead. // @claude
TEST(ChecktransactionTests, IronwoodActionIdentityPointRejected) {
    LoadProofParameters();
    RegtestActivateNU6point3();

    CMutableTransaction mtx;
    SetV6TxHeader(mtx, NetworkUpgradeInfo[Consensus::UPGRADE_NU6_3].nBranchId);
    mtx.ironwoodBundle = BuildV6IronwoodBundle();

    auto bytes = SerializeTx(mtx);
    CMutableTransaction emptied(mtx);
    emptied.ironwoodBundle = OrchardBundle();
    auto startOpt = LocateBundleSection(bytes, SerializeTx(emptied), 1);
    ASSERT_TRUE(startOpt.has_value());
    size_t start = startOpt.value();
    ASSERT_EQ(bytes[start], 2);

    size_t epkOff = start + 1 + V6_ACTION_EPK_OFFSET;
    std::vector<unsigned char> zeros(32, 0x00);
    ASSERT_FALSE(std::equal(zeros.begin(), zeros.end(), bytes.begin() + epkOff));
    std::copy(zeros.begin(), zeros.end(), bytes.begin() + epkOff);

    EXPECT_THROW(DeserializeTx(bytes), std::exception);

    RegtestDeactivateNU6point3();
}

// A coinbase transaction cannot have a positive Ironwood value balance: with
// spends disabled it would be unsatisfiable by the binding signature, and
// rejecting it early keeps a malformed coinbase away from the chain-supply
// accounting (GHSA-g4x5-crjh-29ff; our rule is deliberately stricter than
// Zebra's — confirmed inert in the H4 diff). Unreachable by any valid build,
// so constructed here by patching the sign of a real bundle's valueBalance
// (and disabling its spend flag so the earlier bad-cb-has-ironwood-spend check
// passes). // @claude
TEST(ChecktransactionTests, IronwoodCoinbasePositiveValueBalanceRejected) {
    LoadProofParameters();
    RegtestActivateNU6point3();

    auto to = TestOrchardSpendingKey()
        .ToFullViewingKey()
        .GetChangeAddress();
    auto builder = orchard::Builder(
        false, {orchard::OrchardValuePool::Ironwood, orchard::ProtocolVersion::V3}, uint256());
    EXPECT_TRUE(builder.AddOutput(std::nullopt, to, 4000, std::nullopt));
    auto bundle = builder.Build().value().ProveAndSign({}, uint256S("cc")).value();

    CMutableTransaction mtx;
    SetV6TxHeader(mtx, NetworkUpgradeInfo[Consensus::UPGRADE_NU6_3].nBranchId);
    mtx.ironwoodBundle = bundle;
    // Coinbase shape: single null-prevout input with a 2..100 byte scriptSig.
    mtx.vin.resize(1);
    mtx.vin[0].prevout.SetNull();
    mtx.vin[0].scriptSig = CScript() << 1 << OP_0;

    auto bytes = SerializeTx(mtx);
    CMutableTransaction emptied(mtx);
    emptied.ironwoodBundle = OrchardBundle();
    auto startOpt = LocateBundleSection(bytes, SerializeTx(emptied), 1);
    ASSERT_TRUE(startOpt.has_value());
    size_t start = startOpt.value();
    ASSERT_EQ(bytes[start], 2);

    size_t flagsOff = start + 1 + 2 * V6_ACTION_SIZE;
    size_t vbOff = flagsOff + 1;
    ASSERT_EQ(bytes[flagsOff], 0x07);
    // valueBalance is -4000 (little-endian two's complement int64).
    const std::vector<unsigned char> vbNeg = {0x60, 0xf0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff};
    ASSERT_TRUE(std::equal(vbNeg.begin(), vbNeg.end(), bytes.begin() + vbOff));

    // Disable the spend flag (outputs + crossAddress remain — a valid Ironwood
    // flag byte) so the coinbase checks reach the value-balance rule.
    bytes[flagsOff] = 0x06;

    // Control: coinbase-shaped, spends disabled, negative balance — passes the
    // non-contextual checks (the shielding direction is legal here).
    {
        CTransaction tx = DeserializeTx(bytes);
        EXPECT_TRUE(tx.IsCoinBase());
        CValidationState state;
        EXPECT_TRUE(CheckTransactionWithoutProofVerification(tx, state));
    }

    // Flip the sign: +4000.
    const std::vector<unsigned char> vbPos = {0xa0, 0x0f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00};
    std::copy(vbPos.begin(), vbPos.end(), bytes.begin() + vbOff);

    CTransaction tx = DeserializeTx(bytes);
    EXPECT_TRUE(tx.IsCoinBase());
    EXPECT_EQ(tx.GetIronwoodBundle().GetValueBalance(), 4000);
    MockCValidationState state;
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-cb-positive-ironwood-valuebalance", BodyCorruption::Default, "")).Times(1);
    CheckTransactionWithoutProofVerification(tx, state);

    RegtestDeactivateNU6point3();
}

// [NU6.3 onward] coinbase transactions MUST have an empty Orchard component
// (ZIP 229; Zebra: coinbase_orchard_component_empty_at_nu6_3). The rule applies
// to every transaction version, so a v5 coinbase carrying Orchard actions is
// rejected from NU6.3 even though the v5 format itself is unchanged. The
// pre-NU6.3 acceptance of the same shape is pinned by
// NU5AcceptsOrchardShieldedCoinbase. // @claude
TEST(ChecktransactionTests, CoinbaseOrchardComponentEmptyAtNU6_3) {
    LoadProofParameters();
    RegtestActivateNU6point3();

    // Zero-ovk output, so the ZIP 213 recoverable-ciphertext check (which runs
    // before the NU6.3 block) passes and the NU6.3 rule itself is what fires.
    auto bundle = BuildBundleWithOutput(orchard::OrchardValuePool::Orchard, 0);

    CMutableTransaction mtx;
    mtx.fOverwintered = true;
    mtx.nVersionGroupId = ZIP225_VERSION_GROUP_ID;
    mtx.nVersion = ZIP225_TX_VERSION;
    mtx.nConsensusBranchId = NetworkUpgradeInfo[Consensus::UPGRADE_NU6_3].nBranchId;
    mtx.orchardBundle = bundle;
    mtx.vin.resize(1);
    mtx.vin[0].prevout.SetNull();

    CTransaction tx(mtx);
    EXPECT_TRUE(tx.IsCoinBase());

    MockCValidationState state;
    EXPECT_CALL(state, DoS(100, false, REJECT_INVALID, "bad-cb-has-orchard-actions", BodyCorruption::Default, "")).Times(1);
    ContextualCheckTransaction(tx, state, Params(), 10, true);

    // The rule only constrains coinbase transactions: the same bundle on a
    // non-coinbase v5 transaction passes the contextual checks at NU6.3
    // (its value balance is zero, so the ZIP 2006 inflow freeze is satisfied).
    CMutableTransaction mtxNonCb(mtx);
    mtxNonCb.vin.clear();
    CTransaction txNonCb(mtxNonCb);
    EXPECT_FALSE(txNonCb.IsCoinBase());
    CValidationState plainState;
    EXPECT_TRUE(ContextualCheckTransaction(txNonCb, plainState, Params(), 10, true));

    RegtestDeactivateNU6point3();
}

// v5 wind-down, verification half (§10.2): from NU6.3 the batch validator for a
// block height uses that height's circuit era for EVERY queued bundle,
// including v5 Orchard bundles — so a proof created against the old circuit
// fails under the NU6.3 key (Zebra: halo2::tests pins the same routing,
// "regardless of transaction version"). A regression that routed v5 bundles by
// version instead of era would accept old-circuit proofs post-activation on
// one side only — a chain split (review M5's drift scenario). // @claude
TEST(ChecktransactionTests, V5OrchardBundleVerifiesUnderEraKeyNotVersionKey) {
    LoadProofParameters();
    RegtestActivateNU6point3();

    // InsecureV1-proven v5 Orchard bundle, signed over a zero sighash.
    auto bundle = BuildBundleWithOutput(orchard::OrchardValuePool::Orchard, 1000);

    CMutableTransaction mtx;
    mtx.fOverwintered = true;
    mtx.nVersionGroupId = ZIP225_VERSION_GROUP_ID;
    mtx.nVersion = ZIP225_TX_VERSION;
    mtx.nConsensusBranchId = NetworkUpgradeInfo[Consensus::UPGRADE_NU5].nBranchId;
    mtx.orchardBundle = bundle;
    CTransaction tx(mtx);

    // Under the era key it was proven against, the bundle validates.
    {
        auto batch = orchard::init_batch_validator(
            false, orchard::OrchardCircuitVersion::InsecurePreNu6_2);
        EXPECT_TRUE(tx.GetOrchardBundle().QueueAuthValidation(
            *batch, uint256(), orchard::BundleFormat::V5));
        EXPECT_TRUE(batch->validate());
    }
    // Under the NU6.3 era key — what every post-activation block/mempool batch
    // uses — the same v5 bundle is rejected. The rejection may surface at
    // queueing or at validation; either way it must not pass.
    {
        auto batch = orchard::init_batch_validator(
            false, orchard::OrchardCircuitVersion::PostNu6_3);
        bool queued = tx.GetOrchardBundle().QueueAuthValidation(
            *batch, uint256(), orchard::BundleFormat::V5);
        EXPECT_FALSE(queued && batch->validate());
    }

    RegtestDeactivateNU6point3();
}

// Validation-side circuit ladder, defined in main.cpp and not exported in a
// header; declared here so the drift test below can compare it against the
// building-side ladder. // @claude
::orchard::OrchardCircuitVersion OrchardCircuitVersionFromHeight(
    const Consensus::Params& consensusParams, int height);

// The height→circuit ladder exists twice: OrchardCircuitVersionFromHeight
// (main.cpp, selects the verifying key) and orchard::ProtocolVersionForHeight
// (transaction_builder.cpp, selects the proving circuit). If they ever
// disagree, the node proves transactions its own validator rejects (review
// M5). This pins their era agreement at every boundary from NU6.2 onward —
// every height where new proofs can still be created. (Mainnet heights below
// NU6.2 are deliberately not compared: there the builder intentionally
// returns V2 for all pre-NU6.3 heights while historical validation differs;
// no new proving happens at those heights.) Zebra's halo2 routing test pins
// the same property for its verifier. // @claude
TEST(ChecktransactionTests, OrchardCircuitLaddersAgree) {
    // Regtest with staged boundaries: NU6.2 at height 5, NU6.3 at height 10.
    RegtestActivateNU6point3(false, 10);
    UpdateNetworkUpgradeParameters(Consensus::UPGRADE_NU6_2, 5);
    {
        const CChainParams& params = Params();
        const Consensus::Params& consensus = params.GetConsensus();
        struct Row {
            int height;
            orchard::ProtocolVersion build;
            orchard::OrchardCircuitVersion validate;
        };
        const Row rows[] = {
            {1,    orchard::ProtocolVersion::InsecureV1, orchard::OrchardCircuitVersion::InsecurePreNu6_2},
            {4,    orchard::ProtocolVersion::InsecureV1, orchard::OrchardCircuitVersion::InsecurePreNu6_2},
            {5,    orchard::ProtocolVersion::V2,         orchard::OrchardCircuitVersion::FixedPostNu6_2},
            {9,    orchard::ProtocolVersion::V2,         orchard::OrchardCircuitVersion::FixedPostNu6_2},
            {10,   orchard::ProtocolVersion::V3,         orchard::OrchardCircuitVersion::PostNu6_3},
            {1000, orchard::ProtocolVersion::V3,         orchard::OrchardCircuitVersion::PostNu6_3},
        };
        for (const Row& row : rows) {
            EXPECT_EQ(orchard::ProtocolVersionForHeight(params, row.height), row.build)
                << "builder ladder at height " << row.height;
            EXPECT_EQ(OrchardCircuitVersionFromHeight(consensus, row.height), row.validate)
                << "validator ladder at height " << row.height;
        }
    }
    RegtestDeactivateNU6point3();  // restores mainnet params

    // Mainnet NU6.3 boundary (activation 3,428,143): both ladders switch on
    // the same block.
    {
        const CChainParams& params = Params();
        const Consensus::Params& consensus = params.GetConsensus();
        EXPECT_EQ(orchard::ProtocolVersionForHeight(params, 3428142),
                  orchard::ProtocolVersion::V2);
        EXPECT_EQ(OrchardCircuitVersionFromHeight(consensus, 3428142),
                  orchard::OrchardCircuitVersion::FixedPostNu6_2);
        EXPECT_EQ(orchard::ProtocolVersionForHeight(params, 3428143),
                  orchard::ProtocolVersion::V3);
        EXPECT_EQ(OrchardCircuitVersionFromHeight(consensus, 3428143),
                  orchard::OrchardCircuitVersion::PostNu6_3);
    }
}

// ZIP 317 r1: Ironwood actions contribute to the logical action count as their
// own additive term — arithmetically identical to zcashd's combined
// orchard+ironwood pass-through (verified against Zebra v6.0.0's
// conventional_actions(); resolves the transaction.cpp marker's question at
// test time). A 2-action Ironwood tx pays the 2-action grace fee; adding one
// transparent input makes 3 logical actions = 15000 zats — the fee the
// z_shieldtoironwood default must cover. // @claude
TEST(ChecktransactionTests, IronwoodActionsCountTowardZip317Fee) {
    LoadProofParameters();
    RegtestActivateNU6point3();

    CMutableTransaction mtx;
    SetV6TxHeader(mtx, NetworkUpgradeInfo[Consensus::UPGRADE_NU6_3].nBranchId);
    mtx.ironwoodBundle = BuildV6IronwoodBundle();

    CTransaction tx(mtx);
    EXPECT_EQ(tx.GetIronwoodBundle().GetNumActions(), 2);
    EXPECT_EQ(tx.GetLogicalActionCount(), 2);
    EXPECT_EQ(tx.GetConventionalFee(), CalculateConventionalFee(2));
    EXPECT_EQ(tx.GetConventionalFee(), 10000);

    mtx.vin.resize(1);
    mtx.vin[0].prevout = COutPoint(uint256S("1234"), 0);
    CTransaction tx2(mtx);
    EXPECT_EQ(tx2.GetLogicalActionCount(), 3);
    EXPECT_EQ(tx2.GetConventionalFee(), CalculateConventionalFee(3));
    EXPECT_EQ(tx2.GetConventionalFee(), 15000);

    RegtestDeactivateNU6point3();
}

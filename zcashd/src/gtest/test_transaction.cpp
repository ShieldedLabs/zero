#include <gtest/gtest.h>

#include "gtest/utils.h"
#include "consensus/upgrades.h" // @claude for NetworkUpgradeInfo[UPGRADE_NU6_3].nBranchId
#include "primitives/transaction.h"
#include "streams.h" // @claude for CDataStream in the v6 round-trip test
#include "transaction_builder.h"
#include "version.h" // @claude for PROTOCOL_VERSION in the v6 round-trip test
#include "zcash/Note.hpp"
#include "zcash/Address.hpp"

#include <array>

#include <rust/ed25519.h>

// Round-trips an empty v6 (ZIP 229) transaction through serialization. Constructing // @claude
// the CTransaction exercises UpdateHash, whose librustzcash reparse rejects any // @claude
// non-canonical v6 encoding — including a missing or malformed Ironwood slot — so // @claude
// this doubles as a check that the C++ serializer emits the canonical v6 format. // @claude
TEST(Transaction, V6EmptyBundlesRoundTrip) { // @claude
    CMutableTransaction mtx; // @claude
    mtx.fOverwintered = true; // @claude
    mtx.nVersionGroupId = ZIP229_VERSION_GROUP_ID; // @claude
    mtx.nVersion = ZIP229_TX_VERSION; // @claude
    mtx.nConsensusBranchId = NetworkUpgradeInfo[Consensus::UPGRADE_NU6_3].nBranchId; // @claude

    CDataStream ss(SER_NETWORK, PROTOCOL_VERSION); // @claude
    ss << mtx; // @claude
    const std::vector<unsigned char> bytes(ss.begin(), ss.end()); // @claude

    CTransaction tx(deserialize, ss); // @claude
    EXPECT_EQ(tx.GetHash(), mtx.GetHash()); // @claude
    EXPECT_EQ(tx.GetAuthDigest(), mtx.GetAuthDigest()); // @claude
    EXPECT_FALSE(tx.GetOrchardBundle().IsPresent()); // @claude
    EXPECT_FALSE(tx.GetIronwoodBundle().IsPresent()); // @claude
    EXPECT_EQ(tx.GetConsensusBranchId(), mtx.nConsensusBranchId); // @claude

    CDataStream ss2(SER_NETWORK, PROTOCOL_VERSION); // @claude
    ss2 << tx; // @claude
    const std::vector<unsigned char> bytes2(ss2.begin(), ss2.end()); // @claude
    EXPECT_EQ(bytes, bytes2); // @claude
} // @claude

TEST(Transaction, JSDescriptionRandomized) {
    // construct a merkle tree
    SproutMerkleTree merkleTree;

    libzcash::SproutSpendingKey k = libzcash::SproutSpendingKey::random();
    libzcash::SproutPaymentAddress addr = k.address();

    libzcash::SproutNote note(addr.a_pk, 100, uint256(), uint256());

    // commitment from coin
    uint256 commitment = note.cm();

    // insert commitment into the merkle tree
    merkleTree.append(commitment);

    // compute the merkle root we will be working with
    uint256 rt = merkleTree.root();

    auto witness = merkleTree.witness();

    // create JSDescription
    ed25519::VerificationKey joinSplitPubKey;
    std::array<libzcash::JSInput, ZC_NUM_JS_INPUTS> inputs = {
        libzcash::JSInput(witness, note, k),
        libzcash::JSInput() // dummy input of zero value
    };
    std::array<libzcash::JSOutput, ZC_NUM_JS_OUTPUTS> outputs = {
        libzcash::JSOutput(addr, 50),
        libzcash::JSOutput(addr, 50)
    };
    std::array<size_t, ZC_NUM_JS_INPUTS> inputMap;
    std::array<size_t, ZC_NUM_JS_OUTPUTS> outputMap;

    {
        auto jsdesc = JSDescriptionInfo(
            joinSplitPubKey, rt,
            inputs, outputs,
            0, 0
        ).BuildRandomized(
            inputMap, outputMap,
            false);

        std::set<size_t> inputSet(inputMap.begin(), inputMap.end());
        std::set<size_t> expectedInputSet {0, 1};
        EXPECT_EQ(expectedInputSet, inputSet);

        std::set<size_t> outputSet(outputMap.begin(), outputMap.end());
        std::set<size_t> expectedOutputSet {0, 1};
        EXPECT_EQ(expectedOutputSet, outputSet);
    }

    {
        auto jsdesc = JSDescriptionInfo(
            joinSplitPubKey, rt,
            inputs, outputs,
            0, 0
        ).BuildRandomized(
            inputMap, outputMap,
            false, nullptr, GenZero);

        std::array<size_t, ZC_NUM_JS_INPUTS> expectedInputMap {1, 0};
        std::array<size_t, ZC_NUM_JS_OUTPUTS> expectedOutputMap {1, 0};
        EXPECT_EQ(expectedInputMap, inputMap);
        EXPECT_EQ(expectedOutputMap, outputMap);
    }

    {
        auto jsdesc = JSDescriptionInfo(
            joinSplitPubKey, rt,
            inputs, outputs,
            0, 0
        ).BuildRandomized(
            inputMap, outputMap,
            false, nullptr, GenMax);

        std::array<size_t, ZC_NUM_JS_INPUTS> expectedInputMap {0, 1};
        std::array<size_t, ZC_NUM_JS_OUTPUTS> expectedOutputMap {0, 1};
        EXPECT_EQ(expectedInputMap, inputMap);
        EXPECT_EQ(expectedOutputMap, outputMap);
    }
}

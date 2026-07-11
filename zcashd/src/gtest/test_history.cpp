#include <gtest/gtest.h>

#include "main.h"
#include "util/test.h"
#include "dbwrapper.h" // @claude (width-tolerant read test)
#include "zcash/History.hpp"

HistoryNode getLeafN(uint64_t block_num) {
    HistoryNode node = libzcash::NewV1Leaf(
        uint256(),
        block_num*10,
        block_num*13,
        uint256(),
        uint256(),
        block_num,
        3
    );
    return node;
}

TEST(History, Smoky) {
    // Fake an empty view
    CCoinsViewDummy fakeDB;
    CCoinsViewCache view(&fakeDB);

    uint32_t epochId = 0;

    // Test initial value
    EXPECT_EQ(view.GetHistoryLength(epochId), 0);

    view.PushHistoryNode(epochId, getLeafN(1));

    EXPECT_EQ(view.GetHistoryLength(epochId), 1);

    view.PushHistoryNode(epochId, getLeafN(2));

    EXPECT_EQ(view.GetHistoryLength(epochId), 3);

    view.PushHistoryNode(epochId, getLeafN(3));

    EXPECT_EQ(view.GetHistoryLength(epochId), 4);

    view.PushHistoryNode(epochId, getLeafN(4));

    uint256 h4Root = view.GetHistoryRoot(epochId);

    EXPECT_EQ(view.GetHistoryLength(epochId), 7);

    view.PushHistoryNode(epochId, getLeafN(5));
    EXPECT_EQ(view.GetHistoryLength(epochId), 8);

    view.PopHistoryNode(epochId);

    EXPECT_EQ(view.GetHistoryLength(epochId), 7);
    EXPECT_EQ(h4Root, view.GetHistoryRoot(epochId));
}


TEST(History, EpochBoundaries) {
    // Fake an empty view
    CCoinsViewDummy fakeDB;
    CCoinsViewCache view(&fakeDB);

    // Test with the Heartwood and Canopy epochs
    uint32_t epoch1 = 0xf5b9230b;
    uint32_t epoch2 = 0xe9ff75a6;

    view.PushHistoryNode(epoch1, getLeafN(1));

    EXPECT_EQ(view.GetHistoryLength(epoch1), 1);

    view.PushHistoryNode(epoch1, getLeafN(2));

    EXPECT_EQ(view.GetHistoryLength(epoch1), 3);

    view.PushHistoryNode(epoch1, getLeafN(3));

    EXPECT_EQ(view.GetHistoryLength(epoch1), 4);

    view.PushHistoryNode(epoch1, getLeafN(4));

    uint256 h4Root = view.GetHistoryRoot(epoch1);

    EXPECT_EQ(view.GetHistoryLength(epoch1), 7);

    view.PushHistoryNode(epoch1, getLeafN(5));
    EXPECT_EQ(view.GetHistoryLength(epoch1), 8);


    // Move to Canopy epoch
    view.PushHistoryNode(epoch2, getLeafN(6));
    EXPECT_EQ(view.GetHistoryLength(epoch1), 8);
    EXPECT_EQ(view.GetHistoryLength(epoch2), 1);

    view.PushHistoryNode(epoch2, getLeafN(7));
    EXPECT_EQ(view.GetHistoryLength(epoch1), 8);
    EXPECT_EQ(view.GetHistoryLength(epoch2), 3);

    view.PushHistoryNode(epoch2, getLeafN(8));
    EXPECT_EQ(view.GetHistoryLength(epoch1), 8);
    EXPECT_EQ(view.GetHistoryLength(epoch2), 4);

    // Rolling epoch back to 1
    view.PopHistoryNode(epoch2);
    EXPECT_EQ(view.GetHistoryLength(epoch2), 3);

    view.PopHistoryNode(epoch2);
    EXPECT_EQ(view.GetHistoryLength(epoch2), 1);
    EXPECT_EQ(view.GetHistoryLength(epoch1), 8);

    // And even rolling epoch 1 back a bit
    view.PopHistoryNode(epoch1);
    EXPECT_EQ(view.GetHistoryLength(epoch1), 7);

    // And also rolling epoch 2 back to 0
    view.PopHistoryNode(epoch2);
    EXPECT_EQ(view.GetHistoryLength(epoch2), 0);

    // Trying to truncate an empty tree is a no-op
    view.PopHistoryNode(epoch2);
    EXPECT_EQ(view.GetHistoryLength(epoch2), 0);

}

TEST(History, GarbageMemoryHash) {
    const auto consensusBranchId = NetworkUpgradeInfo[Consensus::UPGRADE_HEARTWOOD].nBranchId;

    CCoinsViewDummy fakeDB;
    CCoinsViewCache view(&fakeDB);

    // Hash two history nodes
    HistoryNode node0 = getLeafN(1);
    HistoryNode node1 = getLeafN(2);

    view.PushHistoryNode(consensusBranchId, node0);
    view.PushHistoryNode(consensusBranchId, node1);

    uint256 historyRoot = view.GetHistoryRoot(consensusBranchId);

    // Change garbage memory and re-hash nodes
    CCoinsViewDummy fakeDBGarbage;
    CCoinsViewCache viewGarbage(&fakeDBGarbage);

    HistoryNode node0Garbage = getLeafN(1);
    HistoryNode node1Garbage = getLeafN(2);

    node0Garbage[NODE_SERIALIZED_LENGTH - 1] = node0[NODE_SERIALIZED_LENGTH - 1] ^ 1;
    node1Garbage[NODE_SERIALIZED_LENGTH - 1] = node1[NODE_SERIALIZED_LENGTH - 1] ^ 1;

    viewGarbage.PushHistoryNode(consensusBranchId, node0Garbage);
    viewGarbage.PushHistoryNode(consensusBranchId, node1Garbage);

    uint256 historyRootGarbage = viewGarbage.GetHistoryRoot(consensusBranchId);

    // Check history root and garbage history root are equal
    EXPECT_EQ(historyRoot, historyRootGarbage);
}

// GetHistoryAt (txdb.cpp) reads history-node records via a raw, width-tolerant
// read: records written at any historical width (V1 171 bytes, V2 244, V3 317)
// are accepted and zero-padded into the full-width node, so an in-place datadir
// upgrade across a node-format widening cannot abort (review C3). This test
// exercises the mechanism at the CDBWrapper level with the exact key shape and
// record widths GetHistoryAt uses: fixed-width array records of every era plus
// the exact-read failure that motivated the raw read. // @claude
TEST(History, WidthTolerantHistoryNodeRead) {
    static_assert(NODE_V1_SERIALIZED_LENGTH == 171, "V1 history node width");
    static_assert(NODE_V2_SERIALIZED_LENGTH == 244, "V2 history node width");
    static_assert(NODE_SERIALIZED_LENGTH == 317, "V3 history node width");

    CDBWrapper db(fs::temp_directory_path() / fs::unique_path(), 1 << 20, true /* fMemory */, true /* fWipe */);
    static const char DB_MMR_NODE = 'm';
    const uint32_t epochId = 0xc2d6d0b4; // NU5, a V2-era epoch
    auto key = [&](uint64_t index) {
        return std::make_pair(DB_MMR_NODE, std::make_pair(epochId, HistoryIndex(index)));
    };

    // Simulate records written by clients of each era.
    std::array<unsigned char, NODE_V1_SERIALIZED_LENGTH> v1Rec;
    v1Rec.fill(0x11);
    std::array<unsigned char, NODE_V2_SERIALIZED_LENGTH> v2Rec;
    v2Rec.fill(0x22);
    HistoryNode v3Rec = {};
    v3Rec.fill(0x33);
    ASSERT_TRUE(db.Write(key(0), v1Rec));
    ASSERT_TRUE(db.Write(key(1), v2Rec));
    ASSERT_TRUE(db.Write(key(2), v3Rec));

    // The failure the raw read exists to avoid: an exact full-width array read
    // of a shorter-era record fails.
    HistoryNode fullWidth;
    EXPECT_FALSE(db.Read(key(1), fullWidth));

    // The width-tolerant read accepts every era's width; shorter records
    // zero-pad, matching what a current client writes for those nodes.
    const std::tuple<uint64_t, size_t, unsigned char> cases[] = {
        {0, NODE_V1_SERIALIZED_LENGTH, 0x11},
        {1, NODE_V2_SERIALIZED_LENGTH, 0x22},
        {2, NODE_SERIALIZED_LENGTH, 0x33},
    };
    for (const auto& [index, expectedSize, fillByte] : cases) {
        std::string raw;
        ASSERT_TRUE(db.ReadRaw(key(index), raw)) << "index " << index;
        EXPECT_EQ(raw.size(), expectedSize) << "index " << index;

        HistoryNode node = {};
        ASSERT_LE(raw.size(), node.size());
        std::copy(raw.begin(), raw.end(), node.begin());
        EXPECT_EQ(node[0], fillByte) << "index " << index;
        EXPECT_EQ(node[expectedSize - 1], fillByte) << "index " << index;
        if (expectedSize < NODE_SERIALIZED_LENGTH) {
            EXPECT_EQ(node[expectedSize], 0) << "index " << index; // zero-padded tail
        }
    }

    // A missing record reads as not-found, not as an empty node.
    std::string raw;
    EXPECT_FALSE(db.ReadRaw(key(3), raw));
}

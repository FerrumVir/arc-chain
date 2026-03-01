// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import "@openzeppelin/contracts/access/Ownable.sol";

/// @title ArcStateRoot
/// @notice On-chain oracle that stores ARC Chain state roots and transaction roots,
///         committed by an authorized relayer. Provides Merkle proof verification
///         against any stored root.
/// @dev Designed to be consumed by bridges, light clients, and other contracts that
///      need to verify ARC Chain state on Ethereum.
contract ArcStateRoot is Ownable {

    // -------------------------------------------------------------------------
    // Types
    // -------------------------------------------------------------------------

    /// @notice Committed root data for a single ARC Chain block.
    struct RootData {
        bytes32 stateRoot;
        bytes32 txRoot;
        uint256 committedAt;
    }

    // -------------------------------------------------------------------------
    // State
    // -------------------------------------------------------------------------

    /// @notice The authorized relayer address that may commit new roots.
    address public relayer;

    /// @notice The latest ARC Chain block height for which roots have been committed.
    uint256 public latestBlockHeight;

    /// @notice Mapping of ARC Chain block height to committed root data.
    mapping(uint256 => RootData) public roots;

    // -------------------------------------------------------------------------
    // Events
    // -------------------------------------------------------------------------

    event StateRootCommitted(
        uint256 indexed blockHeight,
        bytes32 stateRoot,
        bytes32 txRoot
    );

    event RelayerUpdated(address indexed oldRelayer, address indexed newRelayer);

    // -------------------------------------------------------------------------
    // Modifiers
    // -------------------------------------------------------------------------

    modifier onlyRelayer() {
        require(msg.sender == relayer, "ArcStateRoot: caller is not the relayer");
        _;
    }

    // -------------------------------------------------------------------------
    // Constructor
    // -------------------------------------------------------------------------

    /// @param initialOwner The admin address (can update the relayer).
    /// @param _relayer     The initial authorized relayer.
    constructor(
        address initialOwner,
        address _relayer
    ) Ownable(initialOwner) {
        require(_relayer != address(0), "ArcStateRoot: zero relayer");
        relayer = _relayer;
    }

    // -------------------------------------------------------------------------
    // External — Relayer
    // -------------------------------------------------------------------------

    /// @notice Commit a state root and transaction root for a given ARC Chain block height.
    /// @dev Heights must be strictly increasing to prevent history rewriting.
    /// @param blockHeight The ARC Chain block height.
    /// @param stateRoot   The state root hash at that height.
    /// @param txRoot      The transaction root hash at that height.
    function commitStateRoot(
        uint256 blockHeight,
        bytes32 stateRoot,
        bytes32 txRoot
    ) external onlyRelayer {
        require(stateRoot != bytes32(0), "ArcStateRoot: zero state root");
        require(
            blockHeight > latestBlockHeight,
            "ArcStateRoot: height must be strictly increasing"
        );

        roots[blockHeight] = RootData({
            stateRoot: stateRoot,
            txRoot: txRoot,
            committedAt: block.timestamp
        });

        latestBlockHeight = blockHeight;

        emit StateRootCommitted(blockHeight, stateRoot, txRoot);
    }

    // -------------------------------------------------------------------------
    // External — Views
    // -------------------------------------------------------------------------

    /// @notice Verify a Merkle proof against the state root stored at `blockHeight`.
    /// @param blockHeight The ARC Chain block height whose state root to verify against.
    /// @param leaf        The leaf hash to prove inclusion of.
    /// @param proof       The Merkle proof (array of sibling hashes from leaf to root).
    /// @param index       The leaf index in the tree (determines left/right ordering at each level).
    /// @return valid True if the proof reconstructs to the stored state root.
    function verifyProof(
        uint256 blockHeight,
        bytes32 leaf,
        bytes32[] calldata proof,
        uint256 index
    ) external view returns (bool valid) {
        bytes32 storedRoot = roots[blockHeight].stateRoot;
        require(storedRoot != bytes32(0), "ArcStateRoot: no root at height");

        bytes32 computedHash = leaf;

        for (uint256 i = 0; i < proof.length; i++) {
            bytes32 sibling = proof[i];

            if ((index >> i) & 1 == 0) {
                // Current node is on the left.
                computedHash = keccak256(abi.encodePacked(computedHash, sibling));
            } else {
                // Current node is on the right.
                computedHash = keccak256(abi.encodePacked(sibling, computedHash));
            }
        }

        valid = (computedHash == storedRoot);
    }

    /// @notice Get the state root committed at a specific ARC Chain block height.
    /// @param height The block height to query.
    /// @return The state root hash, or bytes32(0) if none committed.
    function stateRootAt(uint256 height) external view returns (bytes32) {
        return roots[height].stateRoot;
    }

    /// @notice Get the transaction root committed at a specific ARC Chain block height.
    /// @param height The block height to query.
    /// @return The transaction root hash, or bytes32(0) if none committed.
    function txRootAt(uint256 height) external view returns (bytes32) {
        return roots[height].txRoot;
    }

    /// @notice Get the full root data at a specific block height.
    /// @param height The block height to query.
    /// @return stateRoot The state root hash.
    /// @return txRoot    The transaction root hash.
    /// @return committedAt The timestamp when the root was committed.
    function rootDataAt(uint256 height)
        external
        view
        returns (bytes32 stateRoot, bytes32 txRoot, uint256 committedAt)
    {
        RootData storage data = roots[height];
        return (data.stateRoot, data.txRoot, data.committedAt);
    }

    // -------------------------------------------------------------------------
    // External — Admin
    // -------------------------------------------------------------------------

    /// @notice Update the authorized relayer address.
    /// @param _relayer The new relayer address.
    function setRelayer(address _relayer) external onlyOwner {
        require(_relayer != address(0), "ArcStateRoot: zero relayer");
        address old = relayer;
        relayer = _relayer;
        emit RelayerUpdated(old, _relayer);
    }
}

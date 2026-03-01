// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import "@openzeppelin/contracts/access/Ownable.sol";
import "@openzeppelin/contracts/utils/Pausable.sol";
import "@openzeppelin/contracts/utils/ReentrancyGuard.sol";

/// @title ArcBridge
/// @notice Cross-chain bridge for ARC tokens between Ethereum and ARC Chain.
///         Users lock tokens on one side; a relayer commits ARC Chain state roots
///         and users prove inclusion to unlock tokens on the other side.
/// @dev Uses Merkle proof verification against relayer-committed state roots.
///      Pausable by the owner for emergency stops. ReentrancyGuard on value-moving functions.
contract ArcBridge is Ownable, Pausable, ReentrancyGuard {
    using SafeERC20 for IERC20;

    // -------------------------------------------------------------------------
    // Constants
    // -------------------------------------------------------------------------

    /// @notice The ARC ERC-20 token contract.
    IERC20 public constant ARC_TOKEN =
        IERC20(0x672fdBA7055bddFa8fD6bD45B1455cE5eB97f499);

    // -------------------------------------------------------------------------
    // State
    // -------------------------------------------------------------------------

    /// @notice Authorized relayer that commits state roots from ARC Chain.
    address public relayer;

    /// @notice Monotonically increasing nonce for lock operations (replay prevention).
    uint256 public lockNonce;

    /// @notice Mapping of block height to the committed ARC Chain state root.
    mapping(uint256 => bytes32) public stateRoots;

    /// @notice Set of nonces that have already been used for unlock (replay prevention).
    mapping(uint256 => bool) public usedNonces;

    /// @notice The latest block height for which a state root has been committed.
    uint256 public latestCommittedHeight;

    // -------------------------------------------------------------------------
    // Events
    // -------------------------------------------------------------------------

    event Locked(
        address indexed sender,
        uint256 amount,
        uint256 indexed nonce
    );

    event Unlocked(
        address indexed to,
        uint256 amount,
        uint256 indexed nonce
    );

    event StateRootCommitted(
        uint256 indexed blockHeight,
        bytes32 stateRoot
    );

    event RelayerUpdated(address indexed oldRelayer, address indexed newRelayer);

    // -------------------------------------------------------------------------
    // Modifiers
    // -------------------------------------------------------------------------

    modifier onlyRelayer() {
        require(msg.sender == relayer, "ArcBridge: caller is not the relayer");
        _;
    }

    // -------------------------------------------------------------------------
    // Constructor
    // -------------------------------------------------------------------------

    /// @param initialOwner The admin address (can pause, set relayer).
    /// @param _relayer     The initial authorized relayer address.
    constructor(
        address initialOwner,
        address _relayer
    ) Ownable(initialOwner) {
        require(_relayer != address(0), "ArcBridge: zero relayer");
        relayer = _relayer;
    }

    // -------------------------------------------------------------------------
    // External — User operations
    // -------------------------------------------------------------------------

    /// @notice Lock ARC tokens on this chain to be bridged to ARC Chain.
    ///         Emits a `Locked` event with a unique nonce that the relayer monitors.
    /// @param amount The number of ARC tokens (18 decimals) to lock.
    function lock(uint256 amount) external nonReentrant whenNotPaused {
        require(amount > 0, "ArcBridge: zero amount");

        ARC_TOKEN.safeTransferFrom(msg.sender, address(this), amount);

        uint256 nonce = lockNonce;
        lockNonce = nonce + 1;

        emit Locked(msg.sender, amount, nonce);
    }

    /// @notice Unlock ARC tokens after proving inclusion in a committed ARC Chain state root.
    /// @param to        The recipient address.
    /// @param amount    The number of ARC tokens to unlock.
    /// @param nonce     The bridge nonce (must not have been used before).
    /// @param stateRoot The state root to verify against (must match a committed root).
    /// @param proof     The Merkle proof (array of sibling hashes).
    function unlock(
        address to,
        uint256 amount,
        uint256 nonce,
        bytes32 stateRoot,
        bytes32[] calldata proof
    ) external nonReentrant whenNotPaused {
        require(to != address(0), "ArcBridge: zero recipient");
        require(amount > 0, "ArcBridge: zero amount");
        require(!usedNonces[nonce], "ArcBridge: nonce already used");

        // Verify the state root has been committed by the relayer.
        // We search for a matching committed root. The caller provides the root they
        // built their proof against; we check it matches the on-chain record.
        require(
            _isCommittedStateRoot(stateRoot),
            "ArcBridge: unknown state root"
        );

        // Reconstruct the leaf from the unlock parameters.
        bytes32 leaf = keccak256(abi.encodePacked(to, amount, nonce));

        // Verify the Merkle proof.
        require(
            _verifyProof(proof, stateRoot, leaf),
            "ArcBridge: invalid proof"
        );

        usedNonces[nonce] = true;

        ARC_TOKEN.safeTransfer(to, amount);

        emit Unlocked(to, amount, nonce);
    }

    // -------------------------------------------------------------------------
    // External — Relayer operations
    // -------------------------------------------------------------------------

    /// @notice Commit a state root from ARC Chain at a given block height.
    /// @param blockHeight The ARC Chain block height.
    /// @param stateRoot   The state root hash at that height.
    function commitStateRoot(
        uint256 blockHeight,
        bytes32 stateRoot
    ) external onlyRelayer whenNotPaused {
        require(stateRoot != bytes32(0), "ArcBridge: zero state root");
        require(
            blockHeight > latestCommittedHeight,
            "ArcBridge: height not increasing"
        );

        stateRoots[blockHeight] = stateRoot;
        latestCommittedHeight = blockHeight;

        emit StateRootCommitted(blockHeight, stateRoot);
    }

    // -------------------------------------------------------------------------
    // External — Admin
    // -------------------------------------------------------------------------

    /// @notice Set a new authorized relayer address.
    /// @param _relayer The new relayer address.
    function setRelayer(address _relayer) external onlyOwner {
        require(_relayer != address(0), "ArcBridge: zero relayer");
        address old = relayer;
        relayer = _relayer;
        emit RelayerUpdated(old, _relayer);
    }

    /// @notice Pause all lock, unlock, and commit operations.
    function pause() external onlyOwner {
        _pause();
    }

    /// @notice Unpause the bridge.
    function unpause() external onlyOwner {
        _unpause();
    }

    // -------------------------------------------------------------------------
    // Internal helpers
    // -------------------------------------------------------------------------

    /// @dev Check whether `root` matches any committed state root. We verify against
    ///      the latest committed height for gas efficiency; callers should always use
    ///      the latest root.
    function _isCommittedStateRoot(bytes32 root) internal view returns (bool) {
        // Walk backwards from the latest height to find a match.
        // In practice the caller should provide the block height, but for a simpler
        // interface we do a bounded reverse scan (up to 256 blocks).
        uint256 height = latestCommittedHeight;
        for (uint256 i = 0; i < 256 && height > 0; i++) {
            if (stateRoots[height] == root) return true;
            height--;
        }
        // Also check height 0.
        if (stateRoots[0] == root) return true;
        return false;
    }

    /// @dev Verify a Merkle proof. Iterates through the proof array, hashing the
    ///      current computed hash with each sibling. The ordering is determined by
    ///      successive bits of a running index derived from the leaf position.
    /// @param proof Array of sibling hashes.
    /// @param root  The expected Merkle root.
    /// @param leaf  The leaf hash to prove inclusion of.
    /// @return True if the proof is valid.
    function _verifyProof(
        bytes32[] calldata proof,
        bytes32 root,
        bytes32 leaf
    ) internal pure returns (bool) {
        bytes32 computedHash = leaf;

        for (uint256 i = 0; i < proof.length; i++) {
            bytes32 sibling = proof[i];

            if (computedHash <= sibling) {
                computedHash = keccak256(abi.encodePacked(computedHash, sibling));
            } else {
                computedHash = keccak256(abi.encodePacked(sibling, computedHash));
            }
        }

        return computedHash == root;
    }
}

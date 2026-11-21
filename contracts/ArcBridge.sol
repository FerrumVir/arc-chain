// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import "@openzeppelin/contracts/access/Ownable.sol";
import "@openzeppelin/contracts/utils/Pausable.sol";
import "@openzeppelin/contracts/utils/ReentrancyGuard.sol";

contract ArcBridge is Ownable, Pausable, ReentrancyGuard {
    using SafeERC20 for IERC20;

    IERC20 public constant ARC_TOKEN = IERC20(0x672fdBA7055bddFa8fD6bD45B1455cE5eB97f499);

    address public relayer;

    uint256 public lockNonce;

    mapping(uint256 => bytes32) public stateRoots;

    mapping(uint256 => bool) public usedNonces;

    uint256 public latestCommittedHeight;

    event Locked(address indexed sender, uint256 amount, uint256 indexed nonce);

    event Unlocked(address indexed to, uint256 amount, uint256 indexed nonce);

    event StateRootCommitted(uint256 indexed blockHeight, bytes32 stateRoot);

    event RelayerUpdated(address indexed oldRelayer, address indexed newRelayer);

    modifier onlyRelayer() {
        require(msg.sender == relayer, "ArcBridge: caller is not the relayer");
        _;
    }

    constructor(address initialOwner, address _relayer) Ownable(initialOwner) {
        require(_relayer != address(0), "ArcBridge: zero relayer");
        relayer = _relayer;
    }

    function lock(uint256 amount) external nonReentrant whenNotPaused {
        require(amount > 0, "ArcBridge: zero amount");

        ARC_TOKEN.safeTransferFrom(msg.sender, address(this), amount);

        uint256 nonce = lockNonce;
        lockNonce = nonce + 1;

        emit Locked(msg.sender, amount, nonce);
    }

    function unlock(address to, uint256 amount, uint256 nonce, bytes32 stateRoot, bytes32[] calldata proof)
        external
        nonReentrant
        whenNotPaused
    {
        require(to != address(0), "ArcBridge: zero recipient");
        require(amount > 0, "ArcBridge: zero amount");
        require(!usedNonces[nonce], "ArcBridge: nonce already used");

        require(_isCommittedStateRoot(stateRoot), "ArcBridge: unknown state root");

        bytes32 leaf = keccak256(abi.encodePacked(to, amount, nonce));

        require(_verifyProof(proof, stateRoot, leaf), "ArcBridge: invalid proof");

        usedNonces[nonce] = true;

        ARC_TOKEN.safeTransfer(to, amount);

        emit Unlocked(to, amount, nonce);
    }

    function commitStateRoot(uint256 blockHeight, bytes32 stateRoot) external onlyRelayer whenNotPaused {
        require(stateRoot != bytes32(0), "ArcBridge: zero state root");
        require(blockHeight > latestCommittedHeight, "ArcBridge: height not increasing");

        stateRoots[blockHeight] = stateRoot;
        latestCommittedHeight = blockHeight;

        emit StateRootCommitted(blockHeight, stateRoot);
    }

    function setRelayer(address _relayer) external onlyOwner {
        require(_relayer != address(0), "ArcBridge: zero relayer");
        address old = relayer;
        relayer = _relayer;
        emit RelayerUpdated(old, _relayer);
    }

    function pause() external onlyOwner {
        _pause();
    }

    function unpause() external onlyOwner {
        _unpause();
    }

    function _isCommittedStateRoot(bytes32 root) internal view returns (bool) {
        uint256 height = latestCommittedHeight;
        for (uint256 i = 0; i < 256 && height > 0; i++) {
            if (stateRoots[height] == root) return true;
            height--;
        }
        if (stateRoots[0] == root) return true;
        return false;
    }

    function _verifyProof(bytes32[] calldata proof, bytes32 root, bytes32 leaf) internal pure returns (bool) {
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

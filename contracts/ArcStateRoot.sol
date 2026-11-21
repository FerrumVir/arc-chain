// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import "@openzeppelin/contracts/access/Ownable.sol";

contract ArcStateRoot is Ownable {
    struct RootData {
        bytes32 stateRoot;
        bytes32 txRoot;
        uint256 committedAt;
    }

    address public relayer;

    uint256 public latestBlockHeight;

    mapping(uint256 => RootData) public roots;

    event StateRootCommitted(uint256 indexed blockHeight, bytes32 stateRoot, bytes32 txRoot);

    event RelayerUpdated(address indexed oldRelayer, address indexed newRelayer);

    modifier onlyRelayer() {
        require(msg.sender == relayer, "ArcStateRoot: caller is not the relayer");
        _;
    }

    constructor(address initialOwner, address _relayer) Ownable(initialOwner) {
        require(_relayer != address(0), "ArcStateRoot: zero relayer");
        relayer = _relayer;
    }

    function commitStateRoot(uint256 blockHeight, bytes32 stateRoot, bytes32 txRoot) external onlyRelayer {
        require(stateRoot != bytes32(0), "ArcStateRoot: zero state root");
        require(blockHeight > latestBlockHeight, "ArcStateRoot: height must be strictly increasing");

        roots[blockHeight] = RootData({stateRoot: stateRoot, txRoot: txRoot, committedAt: block.timestamp});

        latestBlockHeight = blockHeight;

        emit StateRootCommitted(blockHeight, stateRoot, txRoot);
    }

    function verifyProof(uint256 blockHeight, bytes32 leaf, bytes32[] calldata proof, uint256 index)
        external
        view
        returns (bool valid)
    {
        bytes32 storedRoot = roots[blockHeight].stateRoot;
        require(storedRoot != bytes32(0), "ArcStateRoot: no root at height");

        bytes32 computedHash = leaf;

        for (uint256 i = 0; i < proof.length; i++) {
            bytes32 sibling = proof[i];

            if ((index >> i) & 1 == 0) {
                computedHash = keccak256(abi.encodePacked(computedHash, sibling));
            } else {
                computedHash = keccak256(abi.encodePacked(sibling, computedHash));
            }
        }

        valid = (computedHash == storedRoot);
    }

    function stateRootAt(uint256 height) external view returns (bytes32) {
        return roots[height].stateRoot;
    }

    function txRootAt(uint256 height) external view returns (bytes32) {
        return roots[height].txRoot;
    }

    function rootDataAt(uint256 height) external view returns (bytes32 stateRoot, bytes32 txRoot, uint256 committedAt) {
        RootData storage data = roots[height];
        return (data.stateRoot, data.txRoot, data.committedAt);
    }

    function setRelayer(address _relayer) external onlyOwner {
        require(_relayer != address(0), "ArcStateRoot: zero relayer");
        address old = relayer;
        relayer = _relayer;
        emit RelayerUpdated(old, _relayer);
    }
}

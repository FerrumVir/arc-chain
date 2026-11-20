// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import "@openzeppelin/contracts/access/Ownable.sol";
import "@openzeppelin/contracts/utils/ReentrancyGuard.sol";

contract ARCStaking is Ownable, ReentrancyGuard {
    using SafeERC20 for IERC20;

    IERC20 public constant ARC_TOKEN = IERC20(0x672fdBA7055bddFa8fD6bD45B1455cE5eB97f499);

    uint256 public constant YEAR = 365.25 days;

    uint256 public constant COOLDOWN = 7 days;

    uint256 public constant BPS = 10_000;

    uint256 public constant SPARK_MIN = 500_000e18;
    uint256 public constant ARC_MIN = 5_000_000e18;
    uint256 public constant CORE_MIN = 50_000_000e18;

    uint256 public constant SPARK_APY = 800;
    uint256 public constant ARC_APY = 1500;
    uint256 public constant CORE_APY = 2500;

    enum Tier {
        None,
        Spark,
        Arc,
        Core
    }

    struct StakeInfo {
        uint256 amount;
        uint256 stakedAt;
        uint256 lastClaimed;
        Tier tier;
        bool pendingUnstake;
        uint256 unstakeRequestedAt;
    }

    mapping(address => StakeInfo) private _stakes;

    uint256 public rewardsPool;

    event Staked(address indexed staker, uint256 amount, Tier tier);
    event UnstakeRequested(address indexed staker, uint256 amount);
    event Withdrawn(address indexed staker, uint256 amount);
    event RewardsClaimed(address indexed staker, uint256 reward);
    event RewardsPoolFunded(address indexed funder, uint256 amount);
    event TPSReported(address indexed node, uint256 tps, uint256 bonus);

    constructor(address initialOwner) Ownable(initialOwner) {}

    function stake(uint256 amount) external nonReentrant {
        require(amount > 0, "ARCStaking: zero amount");

        StakeInfo storage info = _stakes[msg.sender];
        require(!info.pendingUnstake, "ARCStaking: unstake pending");

        if (info.amount > 0) {
            _claimRewards(msg.sender);
        }

        ARC_TOKEN.safeTransferFrom(msg.sender, address(this), amount);

        info.amount += amount;
        info.stakedAt = block.timestamp;
        info.lastClaimed = block.timestamp;

        Tier newTier = _tierFor(info.amount);
        require(newTier != Tier.None, "ARCStaking: below minimum tier");
        info.tier = newTier;

        emit Staked(msg.sender, amount, newTier);
    }

    function requestUnstake() external nonReentrant {
        StakeInfo storage info = _stakes[msg.sender];
        require(info.amount > 0, "ARCStaking: nothing staked");
        require(!info.pendingUnstake, "ARCStaking: already requested");

        _claimRewards(msg.sender);

        info.pendingUnstake = true;
        info.unstakeRequestedAt = block.timestamp;

        emit UnstakeRequested(msg.sender, info.amount);
    }

    function withdraw() external nonReentrant {
        StakeInfo storage info = _stakes[msg.sender];
        require(info.pendingUnstake, "ARCStaking: no pending unstake");
        require(block.timestamp >= info.unstakeRequestedAt + COOLDOWN, "ARCStaking: cooldown not elapsed");

        uint256 amount = info.amount;

        delete _stakes[msg.sender];

        ARC_TOKEN.safeTransfer(msg.sender, amount);

        emit Withdrawn(msg.sender, amount);
    }

    function claimRewards() external nonReentrant {
        _claimRewards(msg.sender);
    }

    function fundRewards(uint256 amount) external {
        require(amount > 0, "ARCStaking: zero amount");
        ARC_TOKEN.safeTransferFrom(msg.sender, address(this), amount);
        rewardsPool += amount;
        emit RewardsPoolFunded(msg.sender, amount);
    }

    function reportTPS(address[] calldata nodes, uint256[] calldata tps) external onlyOwner {
        require(nodes.length == tps.length, "ARCStaking: length mismatch");

        for (uint256 i = 0; i < nodes.length; i++) {
            uint256 bonus = tps[i] * 1e18;
            if (bonus == 0) continue;
            require(rewardsPool >= bonus, "ARCStaking: rewards pool depleted");

            rewardsPool -= bonus;
            ARC_TOKEN.safeTransfer(nodes[i], bonus);

            emit TPSReported(nodes[i], tps[i], bonus);
        }
    }

    function pendingRewards(address staker) external view returns (uint256 reward) {
        reward = _pendingRewards(staker);
    }

    function getStakeInfo(address staker) external view returns (StakeInfo memory info) {
        info = _stakes[staker];
    }

    function _claimRewards(address staker) internal {
        StakeInfo storage info = _stakes[staker];
        require(info.amount > 0, "ARCStaking: nothing staked");

        uint256 reward = _pendingRewards(staker);
        if (reward == 0) return;

        require(rewardsPool >= reward, "ARCStaking: rewards pool insufficient");

        rewardsPool -= reward;
        info.lastClaimed = block.timestamp;

        ARC_TOKEN.safeTransfer(staker, reward);

        emit RewardsClaimed(staker, reward);
    }

    function _pendingRewards(address staker) internal view returns (uint256) {
        StakeInfo storage info = _stakes[staker];
        if (info.amount == 0) return 0;

        uint256 apyBps = _apyForTier(info.tier);
        uint256 elapsed = block.timestamp - info.lastClaimed;

        return (info.amount * apyBps * elapsed) / YEAR / BPS;
    }

    function _tierFor(uint256 amount) internal pure returns (Tier) {
        if (amount >= CORE_MIN) return Tier.Core;
        if (amount >= ARC_MIN) return Tier.Arc;
        if (amount >= SPARK_MIN) return Tier.Spark;
        return Tier.None;
    }

    function _apyForTier(Tier tier) internal pure returns (uint256) {
        if (tier == Tier.Core) return CORE_APY;
        if (tier == Tier.Arc) return ARC_APY;
        if (tier == Tier.Spark) return SPARK_APY;
        return 0;
    }
}

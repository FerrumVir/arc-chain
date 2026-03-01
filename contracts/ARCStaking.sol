// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import "@openzeppelin/contracts/access/Ownable.sol";
import "@openzeppelin/contracts/utils/ReentrancyGuard.sol";

/// @title ARCStaking
/// @notice Tiered staking contract for ARC tokens with APY rewards, cooldown-based unstaking,
///         and owner-reported TPS performance rewards.
/// @dev Uses SafeERC20 for all token interactions. APY is calculated per-second.
contract ARCStaking is Ownable, ReentrancyGuard {
    using SafeERC20 for IERC20;

    // -------------------------------------------------------------------------
    // Constants
    // -------------------------------------------------------------------------

    /// @notice The ARC ERC-20 token contract.
    IERC20 public constant ARC_TOKEN =
        IERC20(0x672fdBA7055bddFa8fD6bD45B1455cE5eB97f499);

    /// @notice Seconds in one year (365.25 days).
    uint256 public constant YEAR = 365.25 days;

    /// @notice Cooldown period before a withdrawal is allowed after requesting unstake.
    uint256 public constant COOLDOWN = 7 days;

    /// @notice Basis-point denominator used for APY math (10000 = 100%).
    uint256 public constant BPS = 10_000;

    // Tier minimum stake thresholds (18 decimals)
    uint256 public constant SPARK_MIN = 500_000e18;
    uint256 public constant ARC_MIN   = 5_000_000e18;
    uint256 public constant CORE_MIN  = 50_000_000e18;

    // APY in basis points
    uint256 public constant SPARK_APY = 800;   // 8%
    uint256 public constant ARC_APY   = 1_500; // 15%
    uint256 public constant CORE_APY  = 2_500; // 25%

    // -------------------------------------------------------------------------
    // Types
    // -------------------------------------------------------------------------

    /// @notice Staking tiers ordered by minimum threshold.
    enum Tier {
        None,
        Spark,
        Arc,
        Core
    }

    /// @notice Full position data for a single staker.
    struct StakeInfo {
        uint256 amount;
        uint256 stakedAt;
        uint256 lastClaimed;
        Tier    tier;
        bool    pendingUnstake;
        uint256 unstakeRequestedAt;
    }

    // -------------------------------------------------------------------------
    // State
    // -------------------------------------------------------------------------

    /// @dev Staker address => StakeInfo.
    mapping(address => StakeInfo) private _stakes;

    /// @notice Total ARC tokens held in the dedicated rewards pool.
    uint256 public rewardsPool;

    // -------------------------------------------------------------------------
    // Events
    // -------------------------------------------------------------------------

    event Staked(address indexed staker, uint256 amount, Tier tier);
    event UnstakeRequested(address indexed staker, uint256 amount);
    event Withdrawn(address indexed staker, uint256 amount);
    event RewardsClaimed(address indexed staker, uint256 reward);
    event RewardsPoolFunded(address indexed funder, uint256 amount);
    event TPSReported(address indexed node, uint256 tps, uint256 bonus);

    // -------------------------------------------------------------------------
    // Constructor
    // -------------------------------------------------------------------------

    /// @param initialOwner The address that will own the contract (admin).
    constructor(address initialOwner) Ownable(initialOwner) {}

    // -------------------------------------------------------------------------
    // External — Staking lifecycle
    // -------------------------------------------------------------------------

    /// @notice Stake ARC tokens. The tier is determined automatically from the total
    ///         staked amount. Caller must have approved this contract for at least `amount`.
    /// @param amount The number of ARC tokens (18 decimals) to stake.
    function stake(uint256 amount) external nonReentrant {
        require(amount > 0, "ARCStaking: zero amount");

        StakeInfo storage info = _stakes[msg.sender];
        require(!info.pendingUnstake, "ARCStaking: unstake pending");

        // If this is a top-up, auto-claim accrued rewards first.
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

    /// @notice Request to unstake. Starts a 7-day cooldown before `withdraw()` is available.
    function requestUnstake() external nonReentrant {
        StakeInfo storage info = _stakes[msg.sender];
        require(info.amount > 0, "ARCStaking: nothing staked");
        require(!info.pendingUnstake, "ARCStaking: already requested");

        // Auto-claim any accrued rewards before locking the position.
        _claimRewards(msg.sender);

        info.pendingUnstake = true;
        info.unstakeRequestedAt = block.timestamp;

        emit UnstakeRequested(msg.sender, info.amount);
    }

    /// @notice Withdraw staked tokens after the cooldown period has elapsed.
    function withdraw() external nonReentrant {
        StakeInfo storage info = _stakes[msg.sender];
        require(info.pendingUnstake, "ARCStaking: no pending unstake");
        require(
            block.timestamp >= info.unstakeRequestedAt + COOLDOWN,
            "ARCStaking: cooldown not elapsed"
        );

        uint256 amount = info.amount;

        // Reset position entirely.
        delete _stakes[msg.sender];

        ARC_TOKEN.safeTransfer(msg.sender, amount);

        emit Withdrawn(msg.sender, amount);
    }

    /// @notice Claim pending APY rewards without unstaking.
    function claimRewards() external nonReentrant {
        _claimRewards(msg.sender);
    }

    // -------------------------------------------------------------------------
    // External — Admin
    // -------------------------------------------------------------------------

    /// @notice Fund the rewards pool. Caller must have approved this contract.
    /// @param amount ARC tokens to add to the rewards pool.
    function fundRewards(uint256 amount) external {
        require(amount > 0, "ARCStaking: zero amount");
        ARC_TOKEN.safeTransferFrom(msg.sender, address(this), amount);
        rewardsPool += amount;
        emit RewardsPoolFunded(msg.sender, amount);
    }

    /// @notice Report TPS performance for a set of nodes and distribute bonus rewards.
    /// @dev Called by the owner (off-chain oracle / admin). Bonus = tps * 1e18 per node.
    /// @param nodes Array of node operator addresses.
    /// @param tps   Array of TPS values corresponding to each node.
    function reportTPS(
        address[] calldata nodes,
        uint256[] calldata tps
    ) external onlyOwner {
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

    // -------------------------------------------------------------------------
    // External — Views
    // -------------------------------------------------------------------------

    /// @notice Calculate the pending (unclaimed) APY reward for `staker`.
    /// @param staker The address to query.
    /// @return reward The pending reward amount in ARC tokens (18 decimals).
    function pendingRewards(address staker) external view returns (uint256 reward) {
        reward = _pendingRewards(staker);
    }

    /// @notice Return the full StakeInfo for `staker`.
    /// @param staker The address to query.
    /// @return info The StakeInfo struct.
    function getStakeInfo(address staker) external view returns (StakeInfo memory info) {
        info = _stakes[staker];
    }

    // -------------------------------------------------------------------------
    // Internal helpers
    // -------------------------------------------------------------------------

    /// @dev Claim accrued APY rewards for `staker` and transfer from the rewards pool.
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

    /// @dev Pure reward calculation: amount * apyBps * elapsed / YEAR / BPS.
    function _pendingRewards(address staker) internal view returns (uint256) {
        StakeInfo storage info = _stakes[staker];
        if (info.amount == 0) return 0;

        uint256 apyBps = _apyForTier(info.tier);
        uint256 elapsed = block.timestamp - info.lastClaimed;

        return (info.amount * apyBps * elapsed) / YEAR / BPS;
    }

    /// @dev Determine the tier for a given stake `amount`.
    function _tierFor(uint256 amount) internal pure returns (Tier) {
        if (amount >= CORE_MIN) return Tier.Core;
        if (amount >= ARC_MIN)  return Tier.Arc;
        if (amount >= SPARK_MIN) return Tier.Spark;
        return Tier.None;
    }

    /// @dev Return APY in basis points for a given `tier`.
    function _apyForTier(Tier tier) internal pure returns (uint256) {
        if (tier == Tier.Core)  return CORE_APY;
        if (tier == Tier.Arc)   return ARC_APY;
        if (tier == Tier.Spark) return SPARK_APY;
        return 0;
    }
}

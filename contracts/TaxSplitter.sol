// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import "@openzeppelin/contracts/access/Ownable.sol";

/// @title TaxSplitter
/// @notice Distributes ARC token tax revenue to staking rewards, treasury, and liquidity
///         according to a fixed 50 / 30 / 20 split.
/// @dev Receives ARC tokens from the on-chain tax module and holds them until `distribute()`
///      is called. Uses SafeERC20 for all token interactions.
contract TaxSplitter is Ownable {
    using SafeERC20 for IERC20;

    // -------------------------------------------------------------------------
    // Constants
    // -------------------------------------------------------------------------

    /// @notice The ARC ERC-20 token contract.
    IERC20 public constant ARC_TOKEN =
        IERC20(0x672fdBA7055bddFa8fD6bD45B1455cE5eB97f499);

    /// @notice Percentage denominator (100%).
    uint256 public constant PERCENT_BASE = 100;

    /// @notice Percentage of tax revenue allocated to staking rewards.
    uint256 public constant STAKING_SHARE = 50;

    /// @notice Percentage of tax revenue allocated to the treasury.
    uint256 public constant TREASURY_SHARE = 30;

    /// @notice Percentage of tax revenue allocated to the liquidity pool.
    uint256 public constant LIQUIDITY_SHARE = 20;

    // -------------------------------------------------------------------------
    // State
    // -------------------------------------------------------------------------

    /// @notice Address that receives the staking rewards share.
    address public stakingAddress;

    /// @notice Address that receives the treasury share.
    address public treasuryAddress;

    /// @notice Address that receives the liquidity share.
    address public liquidityAddress;

    // -------------------------------------------------------------------------
    // Events
    // -------------------------------------------------------------------------

    event Distributed(
        uint256 stakingAmount,
        uint256 treasuryAmount,
        uint256 liquidityAmount
    );

    event AddressesUpdated(
        address staking,
        address treasury,
        address liquidity
    );

    // -------------------------------------------------------------------------
    // Constructor
    // -------------------------------------------------------------------------

    /// @param initialOwner The admin address that can update recipient addresses.
    /// @param _staking     Initial staking rewards recipient.
    /// @param _treasury    Initial treasury recipient.
    /// @param _liquidity   Initial liquidity recipient.
    constructor(
        address initialOwner,
        address _staking,
        address _treasury,
        address _liquidity
    ) Ownable(initialOwner) {
        require(_staking   != address(0), "TaxSplitter: zero staking address");
        require(_treasury  != address(0), "TaxSplitter: zero treasury address");
        require(_liquidity != address(0), "TaxSplitter: zero liquidity address");

        stakingAddress   = _staking;
        treasuryAddress  = _treasury;
        liquidityAddress = _liquidity;
    }

    // -------------------------------------------------------------------------
    // External
    // -------------------------------------------------------------------------

    /// @notice Distribute the entire ARC balance held by this contract according to the
    ///         fixed split: 50% staking, 30% treasury, 20% liquidity.
    /// @dev Anyone may call this function. The split is performed on the full balance at
    ///      the time of the call.
    function distribute() external {
        uint256 balance = ARC_TOKEN.balanceOf(address(this));
        require(balance > 0, "TaxSplitter: nothing to distribute");

        uint256 stakingAmount   = (balance * STAKING_SHARE)   / PERCENT_BASE;
        uint256 treasuryAmount  = (balance * TREASURY_SHARE)  / PERCENT_BASE;
        // Liquidity gets the remainder to avoid rounding dust.
        uint256 liquidityAmount = balance - stakingAmount - treasuryAmount;

        ARC_TOKEN.safeTransfer(stakingAddress,   stakingAmount);
        ARC_TOKEN.safeTransfer(treasuryAddress,  treasuryAmount);
        ARC_TOKEN.safeTransfer(liquidityAddress, liquidityAmount);

        emit Distributed(stakingAmount, treasuryAmount, liquidityAmount);
    }

    /// @notice Update the recipient addresses for the three revenue streams.
    /// @param _staking   New staking rewards recipient.
    /// @param _treasury  New treasury recipient.
    /// @param _liquidity New liquidity recipient.
    function setAddresses(
        address _staking,
        address _treasury,
        address _liquidity
    ) external onlyOwner {
        require(_staking   != address(0), "TaxSplitter: zero staking address");
        require(_treasury  != address(0), "TaxSplitter: zero treasury address");
        require(_liquidity != address(0), "TaxSplitter: zero liquidity address");

        stakingAddress   = _staking;
        treasuryAddress  = _treasury;
        liquidityAddress = _liquidity;

        emit AddressesUpdated(_staking, _treasury, _liquidity);
    }
}

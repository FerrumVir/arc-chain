// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import "@openzeppelin/contracts/access/Ownable.sol";

contract TaxSplitter is Ownable {
    using SafeERC20 for IERC20;

    IERC20 public constant ARC_TOKEN = IERC20(0x672fdBA7055bddFa8fD6bD45B1455cE5eB97f499);

    uint256 public constant PERCENT_BASE = 100;

    uint256 public constant STAKING_SHARE = 50;

    uint256 public constant TREASURY_SHARE = 30;

    uint256 public constant LIQUIDITY_SHARE = 20;

    address public stakingAddress;

    address public treasuryAddress;

    address public liquidityAddress;

    event Distributed(uint256 stakingAmount, uint256 treasuryAmount, uint256 liquidityAmount);

    event AddressesUpdated(address staking, address treasury, address liquidity);

    constructor(address initialOwner, address _staking, address _treasury, address _liquidity) Ownable(initialOwner) {
        require(_staking != address(0), "TaxSplitter: zero staking address");
        require(_treasury != address(0), "TaxSplitter: zero treasury address");
        require(_liquidity != address(0), "TaxSplitter: zero liquidity address");

        stakingAddress = _staking;
        treasuryAddress = _treasury;
        liquidityAddress = _liquidity;
    }

    function distribute() external {
        uint256 balance = ARC_TOKEN.balanceOf(address(this));
        require(balance > 0, "TaxSplitter: nothing to distribute");

        uint256 stakingAmount = (balance * STAKING_SHARE) / PERCENT_BASE;
        uint256 treasuryAmount = (balance * TREASURY_SHARE) / PERCENT_BASE;
        uint256 liquidityAmount = balance - stakingAmount - treasuryAmount;

        ARC_TOKEN.safeTransfer(stakingAddress, stakingAmount);
        ARC_TOKEN.safeTransfer(treasuryAddress, treasuryAmount);
        ARC_TOKEN.safeTransfer(liquidityAddress, liquidityAmount);

        emit Distributed(stakingAmount, treasuryAmount, liquidityAmount);
    }

    function setAddresses(address _staking, address _treasury, address _liquidity) external onlyOwner {
        require(_staking != address(0), "TaxSplitter: zero staking address");
        require(_treasury != address(0), "TaxSplitter: zero treasury address");
        require(_liquidity != address(0), "TaxSplitter: zero liquidity address");

        stakingAddress = _staking;
        treasuryAddress = _treasury;
        liquidityAddress = _liquidity;

        emit AddressesUpdated(_staking, _treasury, _liquidity);
    }
}

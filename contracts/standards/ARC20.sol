// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/**
 * @title ARC20
 * @notice Fungible token standard for ARC Chain (ERC-20 compatible)
 * @dev Implements the ERC-20 interface with mint/burn controlled by the contract owner.
 */
contract ARC20 {
    // ── State ──────────────────────────────────────────────────────────────

    string public name;
    string public symbol;
    uint8 public constant decimals = 18;

    uint256 public totalSupply;
    address public owner;

    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;

    // ── Events ─────────────────────────────────────────────────────────────

    event Transfer(address indexed from, address indexed to, uint256 value);
    event Approval(address indexed owner, address indexed spender, uint256 value);
    event OwnershipTransferred(address indexed previousOwner, address indexed newOwner);

    // ── Errors ─────────────────────────────────────────────────────────────

    error NotOwner();
    error ZeroAddress();
    error InsufficientBalance(uint256 available, uint256 required);
    error InsufficientAllowance(uint256 available, uint256 required);

    // ── Modifiers ──────────────────────────────────────────────────────────

    modifier onlyOwner() {
        if (msg.sender != owner) revert NotOwner();
        _;
    }

    // ── Constructor ────────────────────────────────────────────────────────

    /**
     * @param _name    Token name (e.g. "ARC Token")
     * @param _symbol  Token symbol (e.g. "ARC")
     * @param _initialSupply  Initial supply in whole tokens (decimals applied automatically)
     */
    constructor(string memory _name, string memory _symbol, uint256 _initialSupply) {
        name = _name;
        symbol = _symbol;
        owner = msg.sender;

        if (_initialSupply > 0) {
            _mint(msg.sender, _initialSupply * 10 ** decimals);
        }
    }

    // ── ERC-20 Interface ───────────────────────────────────────────────────

    /**
     * @notice Transfer tokens to a recipient.
     * @param to     Recipient address
     * @param value  Amount of tokens to transfer
     * @return       True on success
     */
    function transfer(address to, uint256 value) external returns (bool) {
        _transfer(msg.sender, to, value);
        return true;
    }

    /**
     * @notice Approve a spender to transfer tokens on your behalf.
     * @param spender  Address authorized to spend
     * @param value    Maximum amount the spender can transfer
     * @return         True on success
     */
    function approve(address spender, uint256 value) external returns (bool) {
        _approve(msg.sender, spender, value);
        return true;
    }

    /**
     * @notice Transfer tokens from one address to another using an allowance.
     * @param from   Source address
     * @param to     Destination address
     * @param value  Amount to transfer
     * @return       True on success
     */
    function transferFrom(address from, address to, uint256 value) external returns (bool) {
        uint256 currentAllowance = allowance[from][msg.sender];
        if (currentAllowance != type(uint256).max) {
            if (currentAllowance < value) {
                revert InsufficientAllowance(currentAllowance, value);
            }
            unchecked {
                allowance[from][msg.sender] = currentAllowance - value;
            }
        }
        _transfer(from, to, value);
        return true;
    }

    // ── Owner Functions ────────────────────────────────────────────────────

    /**
     * @notice Mint new tokens. Owner only.
     * @param to      Recipient of the minted tokens
     * @param amount  Number of tokens to mint (with decimals)
     */
    function mint(address to, uint256 amount) external onlyOwner {
        _mint(to, amount);
    }

    /**
     * @notice Burn tokens from the caller's balance.
     * @param amount  Number of tokens to burn (with decimals)
     */
    function burn(uint256 amount) external {
        _burn(msg.sender, amount);
    }

    /**
     * @notice Burn tokens from an account using an allowance.
     * @param from    Account to burn from
     * @param amount  Number of tokens to burn (with decimals)
     */
    function burnFrom(address from, uint256 amount) external {
        uint256 currentAllowance = allowance[from][msg.sender];
        if (currentAllowance != type(uint256).max) {
            if (currentAllowance < amount) {
                revert InsufficientAllowance(currentAllowance, amount);
            }
            unchecked {
                allowance[from][msg.sender] = currentAllowance - amount;
            }
        }
        _burn(from, amount);
    }

    /**
     * @notice Transfer ownership of the contract.
     * @param newOwner  New owner address
     */
    function transferOwnership(address newOwner) external onlyOwner {
        if (newOwner == address(0)) revert ZeroAddress();
        emit OwnershipTransferred(owner, newOwner);
        owner = newOwner;
    }

    // ── Internal ───────────────────────────────────────────────────────────

    function _transfer(address from, address to, uint256 value) internal {
        if (from == address(0)) revert ZeroAddress();
        if (to == address(0)) revert ZeroAddress();

        uint256 fromBalance = balanceOf[from];
        if (fromBalance < value) {
            revert InsufficientBalance(fromBalance, value);
        }
        unchecked {
            balanceOf[from] = fromBalance - value;
            balanceOf[to] += value;
        }
        emit Transfer(from, to, value);
    }

    function _mint(address to, uint256 amount) internal {
        if (to == address(0)) revert ZeroAddress();

        totalSupply += amount;
        unchecked {
            balanceOf[to] += amount;
        }
        emit Transfer(address(0), to, amount);
    }

    function _burn(address from, uint256 amount) internal {
        if (from == address(0)) revert ZeroAddress();

        uint256 fromBalance = balanceOf[from];
        if (fromBalance < amount) {
            revert InsufficientBalance(fromBalance, amount);
        }
        unchecked {
            balanceOf[from] = fromBalance - amount;
            totalSupply -= amount;
        }
        emit Transfer(from, address(0), amount);
    }

    function _approve(address _owner, address spender, uint256 value) internal {
        if (_owner == address(0)) revert ZeroAddress();
        if (spender == address(0)) revert ZeroAddress();

        allowance[_owner][spender] = value;
        emit Approval(_owner, spender, value);
    }
}

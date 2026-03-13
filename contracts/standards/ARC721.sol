// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/**
 * @title ARC721
 * @notice Non-fungible token standard for ARC Chain (ERC-721 compatible)
 * @dev Implements the ERC-721 interface with mint/burn controlled by the contract owner.
 *      Includes ERC-165 introspection and safe transfer checks.
 */
contract ARC721 {
    // ── State ──────────────────────────────────────────────────────────────

    string public name;
    string public symbol;
    address public owner;

    mapping(uint256 => address) private _owners;
    mapping(address => uint256) private _balances;
    mapping(uint256 => address) private _tokenApprovals;
    mapping(address => mapping(address => bool)) private _operatorApprovals;
    mapping(uint256 => string) private _tokenURIs;

    string private _baseURI;

    // ── Events ─────────────────────────────────────────────────────────────

    event Transfer(address indexed from, address indexed to, uint256 indexed tokenId);
    event Approval(address indexed owner, address indexed approved, uint256 indexed tokenId);
    event ApprovalForAll(address indexed owner, address indexed operator, bool approved);
    event OwnershipTransferred(address indexed previousOwner, address indexed newOwner);

    // ── Errors ─────────────────────────────────────────────────────────────

    error NotOwner();
    error ZeroAddress();
    error TokenNotFound(uint256 tokenId);
    error TokenAlreadyExists(uint256 tokenId);
    error NotAuthorized(address caller);
    error UnsafeRecipient(address to);

    // ── Modifiers ──────────────────────────────────────────────────────────

    modifier onlyOwner() {
        if (msg.sender != owner) revert NotOwner();
        _;
    }

    // ── Constructor ────────────────────────────────────────────────────────

    /**
     * @param _name    Collection name
     * @param _symbol  Collection symbol
     */
    constructor(string memory _name, string memory _symbol) {
        name = _name;
        symbol = _symbol;
        owner = msg.sender;
    }

    // ── ERC-165 ────────────────────────────────────────────────────────────

    /**
     * @notice Query interface support (ERC-165).
     * @param interfaceId  4-byte interface identifier
     * @return             True if the interface is supported
     */
    function supportsInterface(bytes4 interfaceId) external pure returns (bool) {
        return
            interfaceId == 0x80ac58cd || // ERC-721
            interfaceId == 0x5b5e139f || // ERC-721 Metadata
            interfaceId == 0x01ffc9a7;   // ERC-165
    }

    // ── ERC-721 Interface ──────────────────────────────────────────────────

    /**
     * @notice Get the number of tokens owned by an address.
     */
    function balanceOf(address _holder) external view returns (uint256) {
        if (_holder == address(0)) revert ZeroAddress();
        return _balances[_holder];
    }

    /**
     * @notice Get the owner of a specific token.
     */
    function ownerOf(uint256 tokenId) public view returns (address) {
        address tokenOwner = _owners[tokenId];
        if (tokenOwner == address(0)) revert TokenNotFound(tokenId);
        return tokenOwner;
    }

    /**
     * @notice Get the approved address for a specific token.
     */
    function getApproved(uint256 tokenId) public view returns (address) {
        if (_owners[tokenId] == address(0)) revert TokenNotFound(tokenId);
        return _tokenApprovals[tokenId];
    }

    /**
     * @notice Check if an operator is approved for all tokens of an owner.
     */
    function isApprovedForAll(address _holder, address operator) public view returns (bool) {
        return _operatorApprovals[_holder][operator];
    }

    /**
     * @notice Approve an address to transfer a specific token.
     */
    function approve(address to, uint256 tokenId) external {
        address tokenOwner = ownerOf(tokenId);
        if (msg.sender != tokenOwner && !isApprovedForAll(tokenOwner, msg.sender)) {
            revert NotAuthorized(msg.sender);
        }
        _tokenApprovals[tokenId] = to;
        emit Approval(tokenOwner, to, tokenId);
    }

    /**
     * @notice Set or revoke approval for an operator to manage all your tokens.
     */
    function setApprovalForAll(address operator, bool approved) external {
        if (operator == address(0)) revert ZeroAddress();
        _operatorApprovals[msg.sender][operator] = approved;
        emit ApprovalForAll(msg.sender, operator, approved);
    }

    /**
     * @notice Transfer a token (caller must be owner, approved, or operator).
     */
    function transferFrom(address from, address to, uint256 tokenId) public {
        if (!_isApprovedOrOwner(msg.sender, tokenId)) {
            revert NotAuthorized(msg.sender);
        }
        _transfer(from, to, tokenId);
    }

    /**
     * @notice Safe transfer — reverts if recipient is a contract that doesn't implement onERC721Received.
     */
    function safeTransferFrom(address from, address to, uint256 tokenId) external {
        safeTransferFrom(from, to, tokenId, "");
    }

    /**
     * @notice Safe transfer with data payload.
     */
    function safeTransferFrom(address from, address to, uint256 tokenId, bytes memory data) public {
        transferFrom(from, to, tokenId);
        if (to.code.length > 0) {
            try IERC721Receiver(to).onERC721Received(msg.sender, from, tokenId, data) returns (bytes4 retval) {
                if (retval != IERC721Receiver.onERC721Received.selector) {
                    revert UnsafeRecipient(to);
                }
            } catch {
                revert UnsafeRecipient(to);
            }
        }
    }

    // ── Metadata ───────────────────────────────────────────────────────────

    /**
     * @notice Get the token URI for a specific token.
     */
    function tokenURI(uint256 tokenId) external view returns (string memory) {
        if (_owners[tokenId] == address(0)) revert TokenNotFound(tokenId);

        string memory _tokenURI = _tokenURIs[tokenId];
        if (bytes(_tokenURI).length > 0) {
            return _tokenURI;
        }

        if (bytes(_baseURI).length > 0) {
            return string.concat(_baseURI, _toString(tokenId));
        }

        return "";
    }

    /**
     * @notice Set the base URI for all tokens. Owner only.
     */
    function setBaseURI(string memory baseURI_) external onlyOwner {
        _baseURI = baseURI_;
    }

    /**
     * @notice Set the URI for a specific token. Owner only.
     */
    function setTokenURI(uint256 tokenId, string memory _tokenURI) external onlyOwner {
        if (_owners[tokenId] == address(0)) revert TokenNotFound(tokenId);
        _tokenURIs[tokenId] = _tokenURI;
    }

    // ── Owner Functions ────────────────────────────────────────────────────

    /**
     * @notice Mint a new token. Owner only.
     * @param to       Recipient address
     * @param tokenId  Token ID to mint
     */
    function mint(address to, uint256 tokenId) external onlyOwner {
        _mint(to, tokenId);
    }

    /**
     * @notice Mint with a token URI. Owner only.
     */
    function mintWithURI(address to, uint256 tokenId, string memory _tokenURI) external onlyOwner {
        _mint(to, tokenId);
        _tokenURIs[tokenId] = _tokenURI;
    }

    /**
     * @notice Burn a token. Caller must be owner or approved.
     */
    function burn(uint256 tokenId) external {
        if (!_isApprovedOrOwner(msg.sender, tokenId)) {
            revert NotAuthorized(msg.sender);
        }
        _burn(tokenId);
    }

    /**
     * @notice Transfer contract ownership.
     */
    function transferOwnership(address newOwner) external onlyOwner {
        if (newOwner == address(0)) revert ZeroAddress();
        emit OwnershipTransferred(owner, newOwner);
        owner = newOwner;
    }

    // ── Internal ───────────────────────────────────────────────────────────

    function _transfer(address from, address to, uint256 tokenId) internal {
        if (ownerOf(tokenId) != from) revert NotAuthorized(from);
        if (to == address(0)) revert ZeroAddress();

        // Clear approvals
        delete _tokenApprovals[tokenId];

        unchecked {
            _balances[from] -= 1;
            _balances[to] += 1;
        }
        _owners[tokenId] = to;

        emit Transfer(from, to, tokenId);
    }

    function _mint(address to, uint256 tokenId) internal {
        if (to == address(0)) revert ZeroAddress();
        if (_owners[tokenId] != address(0)) revert TokenAlreadyExists(tokenId);

        unchecked {
            _balances[to] += 1;
        }
        _owners[tokenId] = to;

        emit Transfer(address(0), to, tokenId);
    }

    function _burn(uint256 tokenId) internal {
        address tokenOwner = ownerOf(tokenId);

        // Clear approvals and URI
        delete _tokenApprovals[tokenId];
        delete _tokenURIs[tokenId];

        unchecked {
            _balances[tokenOwner] -= 1;
        }
        delete _owners[tokenId];

        emit Transfer(tokenOwner, address(0), tokenId);
    }

    function _isApprovedOrOwner(address spender, uint256 tokenId) internal view returns (bool) {
        address tokenOwner = ownerOf(tokenId);
        return (spender == tokenOwner ||
                getApproved(tokenId) == spender ||
                isApprovedForAll(tokenOwner, spender));
    }

    function _toString(uint256 value) internal pure returns (string memory) {
        if (value == 0) return "0";
        uint256 temp = value;
        uint256 digits;
        while (temp != 0) {
            digits++;
            temp /= 10;
        }
        bytes memory buffer = new bytes(digits);
        while (value != 0) {
            digits -= 1;
            buffer[digits] = bytes1(uint8(48 + uint256(value % 10)));
            value /= 10;
        }
        return string(buffer);
    }
}

/**
 * @dev Interface for contracts that want to support safeTransfers from ERC-721 contracts.
 */
interface IERC721Receiver {
    function onERC721Received(
        address operator,
        address from,
        uint256 tokenId,
        bytes calldata data
    ) external returns (bytes4);
}

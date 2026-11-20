// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

contract ARC1155 {
    address public owner;
    string private _uri;

    mapping(uint256 => mapping(address => uint256)) private _balances;
    mapping(address => mapping(address => bool)) private _operatorApprovals;

    event TransferSingle(address indexed operator, address indexed from, address indexed to, uint256 id, uint256 value);

    event TransferBatch(
        address indexed operator, address indexed from, address indexed to, uint256[] ids, uint256[] values
    );

    event ApprovalForAll(address indexed account, address indexed operator, bool approved);

    event URI(string value, uint256 indexed id);

    event OwnershipTransferred(address indexed previousOwner, address indexed newOwner);

    error NotOwner();
    error ZeroAddress();
    error InsufficientBalance(uint256 id, uint256 available, uint256 required);
    error ArrayLengthMismatch();
    error NotAuthorized(address caller);
    error UnsafeRecipient(address to);
    error SettingApprovalForSelf();

    modifier onlyOwner() {
        if (msg.sender != owner) revert NotOwner();
        _;
    }

    constructor(string memory uri_) {
        owner = msg.sender;
        _uri = uri_;
    }

    function supportsInterface(bytes4 interfaceId) external pure returns (bool) {
        return interfaceId == 0xd9b67a26 || interfaceId == 0x0e89341c || interfaceId == 0x01ffc9a7;
    }

    function balanceOf(address account, uint256 id) public view returns (uint256) {
        if (account == address(0)) revert ZeroAddress();
        return _balances[id][account];
    }

    function balanceOfBatch(address[] calldata accounts, uint256[] calldata ids)
        external
        view
        returns (uint256[] memory)
    {
        if (accounts.length != ids.length) revert ArrayLengthMismatch();

        uint256[] memory batchBalances = new uint256[](accounts.length);
        for (uint256 i = 0; i < accounts.length; i++) {
            batchBalances[i] = balanceOf(accounts[i], ids[i]);
        }
        return batchBalances;
    }

    function setApprovalForAll(address operator, bool approved) external {
        if (operator == msg.sender) revert SettingApprovalForSelf();
        _operatorApprovals[msg.sender][operator] = approved;
        emit ApprovalForAll(msg.sender, operator, approved);
    }

    function isApprovedForAll(address account, address operator) public view returns (bool) {
        return _operatorApprovals[account][operator];
    }

    function safeTransferFrom(address from, address to, uint256 id, uint256 amount, bytes calldata data) external {
        if (from != msg.sender && !isApprovedForAll(from, msg.sender)) {
            revert NotAuthorized(msg.sender);
        }
        _safeTransferFrom(from, to, id, amount, data);
    }

    function safeBatchTransferFrom(
        address from,
        address to,
        uint256[] calldata ids,
        uint256[] calldata amounts,
        bytes calldata data
    ) external {
        if (from != msg.sender && !isApprovedForAll(from, msg.sender)) {
            revert NotAuthorized(msg.sender);
        }
        _safeBatchTransferFrom(from, to, ids, amounts, data);
    }

    function uri(uint256) external view returns (string memory) {
        return _uri;
    }

    function setURI(string memory newuri) external onlyOwner {
        _uri = newuri;
    }

    function emitURI(string memory value, uint256 id) external onlyOwner {
        emit URI(value, id);
    }

    function mint(address to, uint256 id, uint256 amount, bytes calldata data) external onlyOwner {
        _mint(to, id, amount, data);
    }

    function mintBatch(address to, uint256[] calldata ids, uint256[] calldata amounts, bytes calldata data)
        external
        onlyOwner
    {
        _mintBatch(to, ids, amounts, data);
    }

    function burn(address from, uint256 id, uint256 amount) external {
        if (from != msg.sender && !isApprovedForAll(from, msg.sender)) {
            revert NotAuthorized(msg.sender);
        }
        _burn(from, id, amount);
    }

    function burnBatch(address from, uint256[] calldata ids, uint256[] calldata amounts) external {
        if (from != msg.sender && !isApprovedForAll(from, msg.sender)) {
            revert NotAuthorized(msg.sender);
        }
        _burnBatch(from, ids, amounts);
    }

    function transferOwnership(address newOwner) external onlyOwner {
        if (newOwner == address(0)) revert ZeroAddress();
        emit OwnershipTransferred(owner, newOwner);
        owner = newOwner;
    }

    function _safeTransferFrom(address from, address to, uint256 id, uint256 amount, bytes calldata data) internal {
        if (to == address(0)) revert ZeroAddress();

        uint256 fromBalance = _balances[id][from];
        if (fromBalance < amount) {
            revert InsufficientBalance(id, fromBalance, amount);
        }
        unchecked {
            _balances[id][from] = fromBalance - amount;
            _balances[id][to] += amount;
        }

        emit TransferSingle(msg.sender, from, to, id, amount);

        _checkOnERC1155Received(msg.sender, from, to, id, amount, data);
    }

    function _safeBatchTransferFrom(
        address from,
        address to,
        uint256[] calldata ids,
        uint256[] calldata amounts,
        bytes calldata data
    ) internal {
        if (ids.length != amounts.length) revert ArrayLengthMismatch();
        if (to == address(0)) revert ZeroAddress();

        for (uint256 i = 0; i < ids.length; i++) {
            uint256 id = ids[i];
            uint256 amount = amounts[i];

            uint256 fromBalance = _balances[id][from];
            if (fromBalance < amount) {
                revert InsufficientBalance(id, fromBalance, amount);
            }
            unchecked {
                _balances[id][from] = fromBalance - amount;
                _balances[id][to] += amount;
            }
        }

        emit TransferBatch(msg.sender, from, to, ids, amounts);

        _checkOnERC1155BatchReceived(msg.sender, from, to, ids, amounts, data);
    }

    function _mint(address to, uint256 id, uint256 amount, bytes calldata data) internal {
        if (to == address(0)) revert ZeroAddress();

        unchecked {
            _balances[id][to] += amount;
        }

        emit TransferSingle(msg.sender, address(0), to, id, amount);

        _checkOnERC1155Received(msg.sender, address(0), to, id, amount, data);
    }

    function _mintBatch(address to, uint256[] calldata ids, uint256[] calldata amounts, bytes calldata data) internal {
        if (to == address(0)) revert ZeroAddress();
        if (ids.length != amounts.length) revert ArrayLengthMismatch();

        for (uint256 i = 0; i < ids.length; i++) {
            unchecked {
                _balances[ids[i]][to] += amounts[i];
            }
        }

        emit TransferBatch(msg.sender, address(0), to, ids, amounts);

        _checkOnERC1155BatchReceived(msg.sender, address(0), to, ids, amounts, data);
    }

    function _burn(address from, uint256 id, uint256 amount) internal {
        uint256 fromBalance = _balances[id][from];
        if (fromBalance < amount) {
            revert InsufficientBalance(id, fromBalance, amount);
        }
        unchecked {
            _balances[id][from] = fromBalance - amount;
        }

        emit TransferSingle(msg.sender, from, address(0), id, amount);
    }

    function _burnBatch(address from, uint256[] calldata ids, uint256[] calldata amounts) internal {
        if (ids.length != amounts.length) revert ArrayLengthMismatch();

        for (uint256 i = 0; i < ids.length; i++) {
            uint256 id = ids[i];
            uint256 amount = amounts[i];

            uint256 fromBalance = _balances[id][from];
            if (fromBalance < amount) {
                revert InsufficientBalance(id, fromBalance, amount);
            }
            unchecked {
                _balances[id][from] = fromBalance - amount;
            }
        }

        emit TransferBatch(msg.sender, from, address(0), ids, amounts);
    }

    function _checkOnERC1155Received(
        address operator,
        address from,
        address to,
        uint256 id,
        uint256 amount,
        bytes calldata data
    ) private {
        if (to.code.length > 0) {
            try IERC1155Receiver(to).onERC1155Received(operator, from, id, amount, data) returns (bytes4 retval) {
                if (retval != IERC1155Receiver.onERC1155Received.selector) {
                    revert UnsafeRecipient(to);
                }
            } catch {
                revert UnsafeRecipient(to);
            }
        }
    }

    function _checkOnERC1155BatchReceived(
        address operator,
        address from,
        address to,
        uint256[] calldata ids,
        uint256[] calldata amounts,
        bytes calldata data
    ) private {
        if (to.code.length > 0) {
            try IERC1155Receiver(to).onERC1155BatchReceived(operator, from, ids, amounts, data) returns (
                bytes4 retval
            ) {
                if (retval != IERC1155Receiver.onERC1155BatchReceived.selector) {
                    revert UnsafeRecipient(to);
                }
            } catch {
                revert UnsafeRecipient(to);
            }
        }
    }
}

interface IERC1155Receiver {
    function onERC1155Received(address operator, address from, uint256 id, uint256 value, bytes calldata data)
        external
        returns (bytes4);

    function onERC1155BatchReceived(
        address operator,
        address from,
        uint256[] calldata ids,
        uint256[] calldata values,
        bytes calldata data
    ) external returns (bytes4);
}

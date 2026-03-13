// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/**
 * @title UUPSProxy
 * @notice Minimal UUPS (Universal Upgradeable Proxy Standard) proxy for ARC Chain
 * @dev Uses EIP-1967 storage slots for implementation and admin addresses.
 *      All calls are delegated to the implementation contract via fallback.
 *      Upgrades are performed by calling `upgradeTo()` on the proxy itself.
 *
 *      Storage slots (EIP-1967):
 *        implementation: bytes32(uint256(keccak256("eip1967.proxy.implementation")) - 1)
 *        admin:          bytes32(uint256(keccak256("eip1967.proxy.admin")) - 1)
 */
contract UUPSProxy {
    // ── EIP-1967 Storage Slots ─────────────────────────────────────────────

    /// @dev keccak256("eip1967.proxy.implementation") - 1
    bytes32 internal constant _IMPLEMENTATION_SLOT =
        0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc;

    /// @dev keccak256("eip1967.proxy.admin") - 1
    bytes32 internal constant _ADMIN_SLOT =
        0xb53127684a568b3173ae13b9f8a6016e243e63b6e8ee1178d6a717850b5d6103;

    // ── Events ─────────────────────────────────────────────────────────────

    event Upgraded(address indexed implementation);
    event AdminChanged(address indexed previousAdmin, address indexed newAdmin);

    // ── Errors ─────────────────────────────────────────────────────────────

    error NotAdmin();
    error ZeroAddress();
    error InvalidImplementation(address impl);

    // ── Constructor ────────────────────────────────────────────────────────

    /**
     * @param initialImplementation  Address of the initial logic contract
     * @dev Sets the caller as the proxy admin and the initial implementation.
     *      If the implementation has an `initialize()` function, call it
     *      separately after deployment.
     */
    constructor(address initialImplementation) {
        if (initialImplementation == address(0)) revert ZeroAddress();
        if (initialImplementation.code.length == 0) revert InvalidImplementation(initialImplementation);

        _setAdmin(msg.sender);
        _setImplementation(initialImplementation);
        emit Upgraded(initialImplementation);
    }

    // ── Admin Functions ────────────────────────────────────────────────────

    /**
     * @notice Upgrade the implementation contract. Admin only.
     * @param newImplementation  Address of the new logic contract
     */
    function upgradeTo(address newImplementation) external {
        if (msg.sender != _getAdmin()) revert NotAdmin();
        if (newImplementation == address(0)) revert ZeroAddress();
        if (newImplementation.code.length == 0) revert InvalidImplementation(newImplementation);

        _setImplementation(newImplementation);
        emit Upgraded(newImplementation);
    }

    /**
     * @notice Upgrade and call an initialization function on the new implementation. Admin only.
     * @param newImplementation  Address of the new logic contract
     * @param data               Calldata for the initialization function
     */
    function upgradeToAndCall(address newImplementation, bytes calldata data) external payable {
        if (msg.sender != _getAdmin()) revert NotAdmin();
        if (newImplementation == address(0)) revert ZeroAddress();
        if (newImplementation.code.length == 0) revert InvalidImplementation(newImplementation);

        _setImplementation(newImplementation);
        emit Upgraded(newImplementation);

        if (data.length > 0) {
            (bool success, bytes memory returndata) = newImplementation.delegatecall(data);
            if (!success) {
                // Bubble up revert reason
                if (returndata.length > 0) {
                    assembly {
                        revert(add(returndata, 32), mload(returndata))
                    }
                } else {
                    revert InvalidImplementation(newImplementation);
                }
            }
        }
    }

    /**
     * @notice Transfer admin rights. Admin only.
     * @param newAdmin  New admin address
     */
    function changeAdmin(address newAdmin) external {
        if (msg.sender != _getAdmin()) revert NotAdmin();
        if (newAdmin == address(0)) revert ZeroAddress();

        emit AdminChanged(_getAdmin(), newAdmin);
        _setAdmin(newAdmin);
    }

    /**
     * @notice Get the current implementation address.
     */
    function implementation() external view returns (address) {
        return _getImplementation();
    }

    /**
     * @notice Get the current admin address.
     */
    function admin() external view returns (address) {
        return _getAdmin();
    }

    // ── Fallback ───────────────────────────────────────────────────────────

    /**
     * @dev Delegates all calls to the implementation contract.
     *      Uses EIP-1967 implementation slot for the target address.
     */
    fallback() external payable {
        address impl = _getImplementation();
        assembly {
            // Copy calldata to memory
            calldatacopy(0, 0, calldatasize())

            // Delegatecall to implementation
            let result := delegatecall(gas(), impl, 0, calldatasize(), 0, 0)

            // Copy returndata to memory
            returndatacopy(0, 0, returndatasize())

            switch result
            case 0 {
                // Revert with returndata
                revert(0, returndatasize())
            }
            default {
                // Return with returndata
                return(0, returndatasize())
            }
        }
    }

    /**
     * @dev Accept ETH transfers.
     */
    receive() external payable {}

    // ── Internal Storage Helpers ───────────────────────────────────────────

    function _getImplementation() internal view returns (address impl) {
        bytes32 slot = _IMPLEMENTATION_SLOT;
        assembly {
            impl := sload(slot)
        }
    }

    function _setImplementation(address newImplementation) internal {
        bytes32 slot = _IMPLEMENTATION_SLOT;
        assembly {
            sstore(slot, newImplementation)
        }
    }

    function _getAdmin() internal view returns (address adm) {
        bytes32 slot = _ADMIN_SLOT;
        assembly {
            adm := sload(slot)
        }
    }

    function _setAdmin(address newAdmin) internal {
        bytes32 slot = _ADMIN_SLOT;
        assembly {
            sstore(slot, newAdmin)
        }
    }
}

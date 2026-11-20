// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

contract UUPSProxy {
    bytes32 internal constant _IMPLEMENTATION_SLOT = 0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc;

    bytes32 internal constant _ADMIN_SLOT = 0xb53127684a568b3173ae13b9f8a6016e243e63b6e8ee1178d6a717850b5d6103;

    event Upgraded(address indexed implementation);
    event AdminChanged(address indexed previousAdmin, address indexed newAdmin);

    error NotAdmin();
    error ZeroAddress();
    error InvalidImplementation(address impl);

    constructor(address initialImplementation) {
        if (initialImplementation == address(0)) revert ZeroAddress();
        if (initialImplementation.code.length == 0) revert InvalidImplementation(initialImplementation);

        _setAdmin(msg.sender);
        _setImplementation(initialImplementation);
        emit Upgraded(initialImplementation);
    }

    function upgradeTo(address newImplementation) external {
        if (msg.sender != _getAdmin()) revert NotAdmin();
        if (newImplementation == address(0)) revert ZeroAddress();
        if (newImplementation.code.length == 0) revert InvalidImplementation(newImplementation);

        _setImplementation(newImplementation);
        emit Upgraded(newImplementation);
    }

    function upgradeToAndCall(address newImplementation, bytes calldata data) external payable {
        if (msg.sender != _getAdmin()) revert NotAdmin();
        if (newImplementation == address(0)) revert ZeroAddress();
        if (newImplementation.code.length == 0) revert InvalidImplementation(newImplementation);

        _setImplementation(newImplementation);
        emit Upgraded(newImplementation);

        if (data.length > 0) {
            (bool success, bytes memory returndata) = newImplementation.delegatecall(data);
            if (!success) {
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

    function changeAdmin(address newAdmin) external {
        if (msg.sender != _getAdmin()) revert NotAdmin();
        if (newAdmin == address(0)) revert ZeroAddress();

        emit AdminChanged(_getAdmin(), newAdmin);
        _setAdmin(newAdmin);
    }

    function implementation() external view returns (address) {
        return _getImplementation();
    }

    function admin() external view returns (address) {
        return _getAdmin();
    }

    fallback() external payable {
        address impl = _getImplementation();
        assembly {
            calldatacopy(0, 0, calldatasize())

            let result := delegatecall(gas(), impl, 0, calldatasize(), 0, 0)

            returndatacopy(0, 0, returndatasize())

            switch result
            case 0 {
                revert(0, returndatasize())
            }
            default {
                return(0, returndatasize())
            }
        }
    }

    receive() external payable {}

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

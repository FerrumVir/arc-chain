// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {ARC1155} from "../contracts/standards/ARC1155.sol";
import {ARC20} from "../contracts/standards/ARC20.sol";
import {ARC721} from "../contracts/standards/ARC721.sol";
import {UUPSProxy} from "../contracts/standards/UUPSProxy.sol";

interface ICounter {
    function setValue(uint256 newValue) external;
    function value() external view returns (uint256);
}

interface ICounterV2 is ICounter {
    function increment() external;
}

contract CounterV1 {
    uint256 public value;

    function setValue(uint256 newValue) external {
        value = newValue;
    }
}

contract CounterV2 {
    uint256 public value;

    function setValue(uint256 newValue) external {
        value = newValue;
    }

    function increment() external {
        value += 1;
    }
}

contract TokenReceiver {
    function approve20(ARC20 token, address spender, uint256 amount) external {
        token.approve(spender, amount);
    }

    function mint20(ARC20 token, address to, uint256 amount) external {
        token.mint(to, amount);
    }

    function transfer721(ARC721 token, address to, uint256 tokenId) external {
        token.transferFrom(address(this), to, tokenId);
    }

    function transfer1155Batch(ARC1155 token, address to, uint256[] calldata ids, uint256[] calldata amounts) external {
        token.safeBatchTransferFrom(address(this), to, ids, amounts, "");
    }

    function onERC721Received(address, address, uint256, bytes calldata) external pure returns (bytes4) {
        return this.onERC721Received.selector;
    }

    function onERC1155Received(address, address, uint256, uint256, bytes calldata) external pure returns (bytes4) {
        return this.onERC1155Received.selector;
    }

    function onERC1155BatchReceived(address, address, uint256[] calldata, uint256[] calldata, bytes calldata)
        external
        pure
        returns (bytes4)
    {
        return this.onERC1155BatchReceived.selector;
    }
}

contract NonReceiver {}

contract ARCStandardsTest {
    function testARC20SupplyAllowanceAndOwnerBoundary() public {
        ARC20 token = new ARC20("ARC Token", "ARC", 1);
        TokenReceiver actor = new TokenReceiver();

        require(token.totalSupply() == 1 ether, "unexpected initial supply");
        require(token.transfer(address(actor), 100), "transfer failed");
        actor.approve20(token, address(this), 60);
        require(token.transferFrom(address(actor), address(this), 40), "transferFrom failed");

        require(token.balanceOf(address(actor)) == 60, "actor balance mismatch");
        require(token.balanceOf(address(this)) == 1 ether - 60, "owner balance mismatch");
        require(token.allowance(address(actor), address(this)) == 20, "allowance mismatch");

        (bool unauthorizedMint,) = address(actor).call(abi.encodeCall(TokenReceiver.mint20, (token, address(actor), 1)));
        require(!unauthorizedMint, "non-owner mint unexpectedly succeeded");
        require(token.totalSupply() == 1 ether, "failed mint changed supply");
    }

    function testARC721SafeTransferAndFailureAtomicity() public {
        ARC721 token = new ARC721("ARC NFT", "ANFT");
        TokenReceiver receiver = new TokenReceiver();
        NonReceiver unsafeReceiver = new NonReceiver();

        token.mintWithURI(address(this), 7, "ipfs://arc/7");
        token.safeTransferFrom(address(this), address(receiver), 7);
        require(token.ownerOf(7) == address(receiver), "safe transfer owner mismatch");
        require(token.balanceOf(address(receiver)) == 1, "receiver balance mismatch");

        receiver.transfer721(token, address(this), 7);
        (bool unsafeTransfer,) = address(token)
            .call(
                abi.encodeWithSignature(
                    "safeTransferFrom(address,address,uint256)", address(this), address(unsafeReceiver), 7
                )
            );
        require(!unsafeTransfer, "unsafe contract accepted NFT");
        require(token.ownerOf(7) == address(this), "failed transfer changed owner");

        (bool duplicateMint,) = address(token).call(abi.encodeCall(ARC721.mint, (address(this), 7)));
        require(!duplicateMint, "duplicate token ID minted");
        require(keccak256(bytes(token.tokenURI(7))) == keccak256(bytes("ipfs://arc/7")), "token URI changed");
    }

    function testARC1155BatchConservationAndLengthBoundary() public {
        ARC1155 token = new ARC1155("ipfs://arc/{id}");
        TokenReceiver first = new TokenReceiver();
        TokenReceiver second = new TokenReceiver();
        uint256[] memory ids = new uint256[](2);
        uint256[] memory minted = new uint256[](2);
        uint256[] memory moved = new uint256[](2);
        ids[0] = 1;
        ids[1] = 2;
        minted[0] = 5;
        minted[1] = 9;
        moved[0] = 2;
        moved[1] = 4;

        token.mintBatch(address(first), ids, minted, "");
        first.transfer1155Batch(token, address(second), ids, moved);

        require(token.balanceOf(address(first), 1) == 3, "first id-1 balance mismatch");
        require(token.balanceOf(address(first), 2) == 5, "first id-2 balance mismatch");
        require(token.balanceOf(address(second), 1) == 2, "second id-1 balance mismatch");
        require(token.balanceOf(address(second), 2) == 4, "second id-2 balance mismatch");

        uint256[] memory invalidAmounts = new uint256[](1);
        invalidAmounts[0] = 10;
        (bool mismatchedMint,) =
            address(token).call(abi.encodeCall(ARC1155.mintBatch, (address(first), ids, invalidAmounts, "")));
        require(!mismatchedMint, "mismatched batch minted");
        require(token.balanceOf(address(first), 1) == 3, "failed batch changed balances");
    }

    function testUUPSUpgradePreservesStateAndRejectsCodeLessTarget() public {
        CounterV1 firstImplementation = new CounterV1();
        UUPSProxy proxy = new UUPSProxy(address(firstImplementation));
        ICounter(address(proxy)).setValue(41);

        address originalImplementation = proxy.implementation();
        (bool invalidUpgrade,) = address(proxy).call(abi.encodeCall(UUPSProxy.upgradeTo, (address(0xBEEF))));
        require(!invalidUpgrade, "code-less implementation accepted");
        require(proxy.implementation() == originalImplementation, "failed upgrade changed implementation");
        require(ICounter(address(proxy)).value() == 41, "failed upgrade changed state");

        CounterV2 secondImplementation = new CounterV2();
        proxy.upgradeTo(address(secondImplementation));
        require(ICounter(address(proxy)).value() == 41, "upgrade lost proxy state");
        ICounterV2(address(proxy)).increment();
        require(ICounter(address(proxy)).value() == 42, "new implementation unavailable");
    }
}

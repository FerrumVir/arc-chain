"""
Unit tests for the ARC Chain Python SDK.

Uses unittest.mock to mock httpx responses so no running node is needed.
"""

import unittest
from unittest.mock import MagicMock, patch, PropertyMock

from arc_sdk import ArcClient, KeyPair, TransactionBuilder
from arc_sdk.errors import (
    ArcConnectionError,
    ArcError,
    ArcTransactionError,
    ArcValidationError,
    ArcCryptoError,
)
from arc_sdk.types import Account, Block, ChainInfo, ChainStats, Receipt


class MockResponse:
    """Minimal mock for httpx.Response."""

    def __init__(self, json_data: dict, status_code: int = 200):
        self._json_data = json_data
        self.status_code = status_code
        self.text = str(json_data)

    def json(self):
        return self._json_data


# ---------------------------------------------------------------------------
# Client tests
# ---------------------------------------------------------------------------


class TestGetBlock(unittest.TestCase):
    """Tests for ArcClient.get_block()."""

    @patch("arc_sdk.client.httpx.Client")
    def test_get_block_success(self, MockHttpClient):
        mock_instance = MockHttpClient.return_value
        block_data = {
            "hash": "ab" * 32,
            "header": {
                "height": 42,
                "parent_hash": "cd" * 32,
                "tx_root": "ef" * 32,
                "state_root": "01" * 32,
                "tx_count": 5,
                "timestamp": 1700000000,
                "producer": "aa" * 32,
            },
            "tx_hashes": ["ff" * 32],
        }
        mock_instance.get.return_value = MockResponse(block_data)

        client = ArcClient("http://localhost:9000")
        result = client.get_block(42)

        self.assertEqual(result["header"]["height"], 42)
        self.assertEqual(result["header"]["tx_count"], 5)
        mock_instance.get.assert_called_once()

    @patch("arc_sdk.client.httpx.Client")
    def test_get_block_not_found(self, MockHttpClient):
        mock_instance = MockHttpClient.return_value
        mock_instance.get.return_value = MockResponse({}, status_code=404)

        client = ArcClient("http://localhost:9000")
        with self.assertRaises(ArcError) as ctx:
            client.get_block(999999)

        self.assertEqual(ctx.exception.status_code, 404)


class TestGetAccount(unittest.TestCase):
    """Tests for ArcClient.get_account()."""

    @patch("arc_sdk.client.httpx.Client")
    def test_get_account_success(self, MockHttpClient):
        mock_instance = MockHttpClient.return_value
        addr = "ab" * 32
        account_data = {
            "address": addr,
            "balance": 1000000,
            "nonce": 5,
        }
        mock_instance.get.return_value = MockResponse(account_data)

        client = ArcClient("http://localhost:9000")
        result = client.get_account(addr)

        self.assertEqual(result["balance"], 1000000)
        self.assertEqual(result["nonce"], 5)

    @patch("arc_sdk.client.httpx.Client")
    def test_get_account_typed(self, MockHttpClient):
        mock_instance = MockHttpClient.return_value
        addr = "ab" * 32
        account_data = {
            "address": addr,
            "balance": 500,
            "nonce": 3,
        }
        mock_instance.get.return_value = MockResponse(account_data)

        client = ArcClient("http://localhost:9000")
        acct = client.get_account_typed(addr)

        self.assertIsInstance(acct, Account)
        self.assertEqual(acct.balance, 500)
        self.assertEqual(acct.nonce, 3)


class TestSubmitTransaction(unittest.TestCase):
    """Tests for ArcClient.submit_transaction()."""

    @patch("arc_sdk.client.httpx.Client")
    def test_submit_raw_tx(self, MockHttpClient):
        mock_instance = MockHttpClient.return_value
        expected_hash = "de" * 32
        mock_instance.post.return_value = MockResponse({
            "tx_hash": expected_hash,
            "status": "pending",
        })

        client = ArcClient("http://localhost:9000")
        tx_hash = client.submit_transaction({
            "from": "aa" * 32,
            "to": "bb" * 32,
            "amount": 100,
            "nonce": 0,
        })

        self.assertEqual(tx_hash, expected_hash)

    @patch("arc_sdk.client.httpx.Client")
    def test_submit_builder_tx(self, MockHttpClient):
        mock_instance = MockHttpClient.return_value
        expected_hash = "de" * 32
        mock_instance.post.return_value = MockResponse({
            "tx_hash": expected_hash,
            "status": "pending",
        })

        kp = KeyPair.generate()
        tx = TransactionBuilder.transfer(
            from_addr=kp.address(),
            to_addr="bb" * 32,
            amount=100,
        )
        signed = TransactionBuilder.sign(tx, kp)

        client = ArcClient("http://localhost:9000")
        tx_hash = client.submit_transaction(signed)

        self.assertEqual(tx_hash, expected_hash)

    @patch("arc_sdk.client.httpx.Client")
    def test_submit_conflict(self, MockHttpClient):
        mock_instance = MockHttpClient.return_value
        mock_instance.post.return_value = MockResponse({}, status_code=409)

        client = ArcClient("http://localhost:9000")
        with self.assertRaises(ArcTransactionError):
            client.submit_transaction({
                "from": "aa" * 32,
                "to": "bb" * 32,
                "amount": 100,
                "nonce": 0,
            })


class TestSubmitBatch(unittest.TestCase):
    """Tests for ArcClient.submit_batch()."""

    @patch("arc_sdk.client.httpx.Client")
    def test_submit_batch_success(self, MockHttpClient):
        mock_instance = MockHttpClient.return_value
        mock_instance.post.return_value = MockResponse({
            "accepted": 2,
            "rejected": 0,
            "tx_hashes": ["aa" * 32, "bb" * 32],
        })

        client = ArcClient("http://localhost:9000")
        result = client.submit_batch([
            {"from": "11" * 32, "to": "22" * 32, "amount": 10, "nonce": 0},
            {"from": "33" * 32, "to": "44" * 32, "amount": 20, "nonce": 0},
        ])

        self.assertEqual(result["accepted"], 2)
        self.assertEqual(len(result["tx_hashes"]), 2)


class TestChainInfo(unittest.TestCase):
    """Tests for ArcClient.get_chain_info() and get_stats()."""

    @patch("arc_sdk.client.httpx.Client")
    def test_get_chain_info(self, MockHttpClient):
        mock_instance = MockHttpClient.return_value
        mock_instance.get.return_value = MockResponse({
            "chain": "ARC Chain",
            "version": "0.1.0",
            "block_height": 100,
            "account_count": 50,
            "mempool_size": 3,
            "gpu": {"name": "Apple M2"},
        })

        client = ArcClient("http://localhost:9000")
        info = client.get_chain_info()

        self.assertEqual(info["chain"], "ARC Chain")
        self.assertEqual(info["block_height"], 100)

    @patch("arc_sdk.client.httpx.Client")
    def test_get_stats(self, MockHttpClient):
        mock_instance = MockHttpClient.return_value
        mock_instance.get.return_value = MockResponse({
            "chain": "ARC Chain",
            "version": "0.1.0",
            "block_height": 100,
            "total_accounts": 50,
            "mempool_size": 3,
            "total_transactions": 1000,
            "indexed_hashes": 500,
            "indexed_receipts": 500,
        })

        client = ArcClient("http://localhost:9000")
        stats = client.get_stats()

        self.assertEqual(stats["total_transactions"], 1000)

    @patch("arc_sdk.client.httpx.Client")
    def test_get_stats_typed(self, MockHttpClient):
        mock_instance = MockHttpClient.return_value
        mock_instance.get.return_value = MockResponse({
            "chain": "ARC Chain",
            "version": "0.1.0",
            "block_height": 100,
            "total_accounts": 50,
            "mempool_size": 3,
            "total_transactions": 1000,
            "indexed_hashes": 500,
            "indexed_receipts": 500,
        })

        client = ArcClient("http://localhost:9000")
        stats = client.get_stats_typed()

        self.assertIsInstance(stats, ChainStats)
        self.assertEqual(stats.total_transactions, 1000)
        self.assertEqual(stats.block_height, 100)


class TestEthCall(unittest.TestCase):
    """Tests for ArcClient.eth_call()."""

    @patch("arc_sdk.client.httpx.Client")
    def test_eth_chain_id(self, MockHttpClient):
        mock_instance = MockHttpClient.return_value
        mock_instance.post.return_value = MockResponse({
            "jsonrpc": "2.0",
            "id": 1,
            "result": "0x415243",
        })

        client = ArcClient("http://localhost:9000")
        result = client.eth_call("eth_chainId")

        self.assertEqual(result["result"], "0x415243")

    @patch("arc_sdk.client.httpx.Client")
    def test_eth_block_number(self, MockHttpClient):
        mock_instance = MockHttpClient.return_value
        mock_instance.post.return_value = MockResponse({
            "jsonrpc": "2.0",
            "id": 1,
            "result": "0x64",
        })

        client = ArcClient("http://localhost:9000")
        result = client.eth_call("eth_blockNumber")

        self.assertEqual(result["result"], "0x64")


# ---------------------------------------------------------------------------
# Crypto tests
# ---------------------------------------------------------------------------


class TestKeyPairGeneration(unittest.TestCase):
    """Tests for KeyPair generation and address derivation."""

    def test_generate_random(self):
        kp = KeyPair.generate()
        self.assertEqual(len(kp.address()), 64)
        self.assertEqual(len(kp.public_key_hex()), 64)
        self.assertEqual(len(kp.public_key_bytes()), 32)

    def test_two_random_keys_differ(self):
        kp1 = KeyPair.generate()
        kp2 = KeyPair.generate()
        self.assertNotEqual(kp1.address(), kp2.address())
        self.assertNotEqual(kp1.public_key_hex(), kp2.public_key_hex())

    def test_from_seed_deterministic(self):
        seed = bytes(range(32))
        kp1 = KeyPair.from_seed(seed)
        kp2 = KeyPair.from_seed(seed)
        self.assertEqual(kp1.address(), kp2.address())
        self.assertEqual(kp1.public_key_hex(), kp2.public_key_hex())

    def test_from_seed_wrong_length(self):
        with self.assertRaises(ArcCryptoError):
            KeyPair.from_seed(b"too short")

    def test_from_private_key_hex(self):
        seed = bytes(range(32))
        kp1 = KeyPair.from_seed(seed)
        kp2 = KeyPair.from_private_key(seed.hex())
        self.assertEqual(kp1.address(), kp2.address())

    def test_address_is_blake3_of_pubkey(self):
        import blake3
        kp = KeyPair.generate()
        expected = blake3.blake3(kp.public_key_bytes()).hexdigest()
        self.assertEqual(kp.address(), expected)


class TestKeyPairSigning(unittest.TestCase):
    """Tests for Ed25519 signing and verification."""

    def test_sign_and_verify(self):
        kp = KeyPair.generate()
        msg = b"hello ARC chain"
        sig = kp.sign(msg)
        self.assertEqual(len(sig), 64)
        self.assertTrue(kp.verify(msg, sig))

    def test_wrong_message_fails(self):
        kp = KeyPair.generate()
        sig = kp.sign(b"message A")
        self.assertFalse(kp.verify(b"message B", sig))

    def test_wrong_key_fails(self):
        kp1 = KeyPair.generate()
        kp2 = KeyPair.generate()
        sig = kp1.sign(b"test")
        self.assertFalse(kp2.verify(b"test", sig))

    def test_verify_with_public_key(self):
        kp = KeyPair.generate()
        msg = b"verify me"
        sig = kp.sign(msg)
        self.assertTrue(
            KeyPair.verify_with_public_key(kp.public_key_bytes(), msg, sig)
        )


# ---------------------------------------------------------------------------
# Transaction builder tests
# ---------------------------------------------------------------------------


class TestTransactionBuilder(unittest.TestCase):
    """Tests for TransactionBuilder."""

    def test_transfer_builds_valid_tx(self):
        addr_from = "aa" * 32
        addr_to = "bb" * 32
        tx = TransactionBuilder.transfer(addr_from, addr_to, 1000, fee=5, nonce=1)

        self.assertEqual(tx["tx_type"], "Transfer")
        self.assertEqual(tx["from"], addr_from)
        self.assertEqual(tx["to"], addr_to)
        self.assertEqual(tx["amount"], 1000)
        self.assertEqual(tx["fee"], 5)
        self.assertEqual(tx["nonce"], 1)
        self.assertIsNotNone(tx["hash"])
        self.assertEqual(len(tx["hash"]), 64)
        self.assertIsNone(tx["signature"])

    def test_transfer_hash_deterministic(self):
        addr_from = "aa" * 32
        addr_to = "bb" * 32
        tx1 = TransactionBuilder.transfer(addr_from, addr_to, 1000)
        tx2 = TransactionBuilder.transfer(addr_from, addr_to, 1000)
        self.assertEqual(tx1["hash"], tx2["hash"])

    def test_transfer_hash_changes_with_nonce(self):
        addr_from = "aa" * 32
        addr_to = "bb" * 32
        tx1 = TransactionBuilder.transfer(addr_from, addr_to, 1000, nonce=0)
        tx2 = TransactionBuilder.transfer(addr_from, addr_to, 1000, nonce=1)
        self.assertNotEqual(tx1["hash"], tx2["hash"])

    def test_transfer_invalid_address(self):
        with self.assertRaises(ArcValidationError):
            TransactionBuilder.transfer("short", "bb" * 32, 1000)

    def test_transfer_zero_amount(self):
        with self.assertRaises(ArcValidationError):
            TransactionBuilder.transfer("aa" * 32, "bb" * 32, 0)

    def test_deploy_contract(self):
        addr = "aa" * 32
        code = b"\x00asm\x01\x00\x00\x00"
        tx = TransactionBuilder.deploy_contract(addr, code, gas_limit=100000)

        self.assertEqual(tx["tx_type"], "DeployContract")
        self.assertEqual(tx["gas_limit"], 100000)
        self.assertIsNotNone(tx["hash"])

    def test_call_contract(self):
        addr = "aa" * 32
        contract = "cc" * 32
        tx = TransactionBuilder.call_contract(
            addr, contract, b"\x01\x02", value=50, function="transfer",
        )

        self.assertEqual(tx["tx_type"], "WasmCall")
        self.assertEqual(tx["body"]["contract"], contract)
        self.assertEqual(tx["body"]["function"], "transfer")

    def test_stake(self):
        addr = "aa" * 32
        tx = TransactionBuilder.stake(addr, 10000)

        self.assertEqual(tx["tx_type"], "Stake")
        self.assertTrue(tx["body"]["is_stake"])
        self.assertEqual(tx["body"]["amount"], 10000)

    def test_unstake(self):
        addr = "aa" * 32
        tx = TransactionBuilder.stake(addr, 5000, is_stake=False)

        self.assertFalse(tx["body"]["is_stake"])

    def test_settle(self):
        addr = "aa" * 32
        agent = "bb" * 32
        service = "cc" * 32
        tx = TransactionBuilder.settle(addr, agent, service, 500, 100)

        self.assertEqual(tx["tx_type"], "Settle")
        self.assertEqual(tx["fee"], 0)


class TestTransactionSigning(unittest.TestCase):
    """Tests for transaction signing."""

    def test_sign_transfer(self):
        kp = KeyPair.generate()
        tx = TransactionBuilder.transfer(
            from_addr=kp.address(),
            to_addr="bb" * 32,
            amount=1000,
        )
        signed = TransactionBuilder.sign(tx, kp)

        self.assertIsNotNone(signed["signature"])
        self.assertIn("Ed25519", signed["signature"])
        self.assertEqual(
            signed["signature"]["Ed25519"]["public_key"],
            kp.public_key_hex(),
        )
        # Verify the signature
        sig_bytes = bytes.fromhex(signed["signature"]["Ed25519"]["signature"])
        hash_bytes = bytes.fromhex(signed["hash"])
        self.assertTrue(kp.verify(hash_bytes, sig_bytes))

    def test_sign_wrong_key_raises(self):
        kp1 = KeyPair.generate()
        kp2 = KeyPair.generate()
        tx = TransactionBuilder.transfer(
            from_addr=kp1.address(),
            to_addr="bb" * 32,
            amount=1000,
        )
        with self.assertRaises(ArcValidationError):
            TransactionBuilder.sign(tx, kp2)

    def test_sign_does_not_mutate_original(self):
        kp = KeyPair.generate()
        tx = TransactionBuilder.transfer(
            from_addr=kp.address(),
            to_addr="bb" * 32,
            amount=1000,
        )
        original_sig = tx["signature"]
        _ = TransactionBuilder.sign(tx, kp)
        self.assertIsNone(tx["signature"])
        self.assertEqual(tx["signature"], original_sig)


# ---------------------------------------------------------------------------
# Type deserialization tests
# ---------------------------------------------------------------------------


class TestTypes(unittest.TestCase):
    """Tests for typed dataclass deserialization."""

    def test_account_from_dict(self):
        acct = Account.from_dict({
            "address": "aa" * 32,
            "balance": 999,
            "nonce": 7,
        })
        self.assertEqual(acct.balance, 999)
        self.assertEqual(acct.nonce, 7)

    def test_receipt_from_dict(self):
        receipt = Receipt.from_dict({
            "tx_hash": "ff" * 32,
            "block_height": 10,
            "block_hash": "ee" * 32,
            "index": 0,
            "success": True,
            "gas_used": 21000,
            "logs": [],
        })
        self.assertTrue(receipt.success)
        self.assertEqual(receipt.gas_used, 21000)

    def test_chain_info_from_dict(self):
        info = ChainInfo.from_dict({
            "chain": "ARC Chain",
            "version": "0.1.0",
            "block_height": 42,
            "account_count": 10,
            "mempool_size": 2,
        })
        self.assertEqual(info.chain, "ARC Chain")
        self.assertEqual(info.block_height, 42)

    def test_block_from_dict(self):
        block = Block.from_dict({
            "hash": "ab" * 32,
            "header": {
                "height": 5,
                "parent_hash": "cd" * 32,
                "tx_root": "ef" * 32,
                "state_root": "01" * 32,
                "tx_count": 2,
                "timestamp": 1700000000,
                "producer": "aa" * 32,
            },
            "tx_hashes": ["ff" * 32, "ee" * 32],
        })
        self.assertEqual(block.header.height, 5)
        self.assertEqual(len(block.tx_hashes), 2)


if __name__ == "__main__":
    unittest.main()

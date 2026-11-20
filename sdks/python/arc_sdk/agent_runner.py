from __future__ import annotations

import asyncio
import logging
import time
from dataclasses import dataclass, field
from typing import Any, Awaitable, Callable, Dict, List, Optional

import blake3

from .client import ArcClient
from .crypto import KeyPair
from .errors import ArcError

logger = logging.getLogger(__name__)

InferenceFn = Callable[[str, str], Awaitable[str]]


def _blake3_hex(data: bytes) -> str:
    return blake3.blake3(data).hexdigest()


def _model_id(name: str) -> str:
    return _blake3_hex(f"arc-model-{name}".encode())


@dataclass
class AgentStats:
    requests_processed: int = 0
    requests_failed: int = 0
    attestations_submitted: int = 0
    settlements_submitted: int = 0
    total_inference_ms: float = 0.0
    total_earned: int = 0
    started_at: float = field(default_factory=time.time)

    @property
    def uptime_seconds(self) -> float:
        return time.time() - self.started_at

    @property
    def avg_inference_ms(self) -> float:
        if self.requests_processed == 0:
            return 0.0
        return self.total_inference_ms / self.requests_processed


@dataclass
class InferenceRequest:
    request_id: str
    sender: str
    input_text: str
    model_id: str
    fee: int
    block_height: int


@dataclass
class InferenceResult:
    request_id: str
    output_text: str
    input_hash: str
    output_hash: str
    inference_ms: float
    attestation_tx: Optional[str] = None
    settlement_tx: Optional[str] = None


class AgentRunner:
    def __init__(
        self,
        client: ArcClient,
        keypair: KeyPair,
        name: str,
        inference_fn: InferenceFn,
        *,
        model_name: str = "",
        capabilities: str = "inference",
        endpoint: str = "",
        poll_interval: float = 1.0,
        challenge_period: int = 100,
        bond_amount: int = 1000,
        fee_per_request: int = 100,
    ):
        self.client = client
        self.keypair = keypair
        self.name = name
        self.inference_fn = inference_fn
        self.model_name = model_name or name
        self.model_id = _model_id(self.model_name)
        self.capabilities = capabilities
        self.endpoint = endpoint
        self.poll_interval = poll_interval
        self.challenge_period = challenge_period
        self.bond_amount = bond_amount
        self.fee_per_request = fee_per_request
        self.address = keypair.address()
        self.stats = AgentStats()
        self._running = False
        self._nonce = 0

    async def register(self) -> str:
        tx = {
            "from": self.address,
            "nonce": self._next_nonce(),
            "tx_type": "RegisterAgent",
            "body": {
                "agent_name": self.name,
                "capabilities": self.capabilities.encode().hex(),
                "endpoint": self.endpoint,
                "protocol": self.model_id,
                "metadata": b"{}".hex(),
            },
            "fee": 0,
            "gas_limit": 50_000,
        }
        tx_hash = self.client.submit_transaction(tx)
        logger.info(
            f"Agent registered: {self.name} addr={self.address[:16]}... tx={tx_hash[:16]}..."
        )
        return tx_hash

    async def start(self, max_iterations: Optional[int] = None):
        logger.info(f"Starting AgentRunner: {self.name}")
        logger.info(f"  Address:    {self.address[:16]}...")
        logger.info(f"  Model:      {self.model_name} ({self.model_id[:16]}...)")
        logger.info(f"  Poll:       {self.poll_interval}s")
        logger.info(f"  Bond:       {self.bond_amount} ARC")
        logger.info(f"  Fee:        {self.fee_per_request} ARC/request")

        try:
            await self.register()
        except Exception as e:
            logger.warning(f"Registration failed (may already be registered): {e}")

        self._running = True
        iterations = 0

        while self._running:
            try:
                requests = await self._poll_requests()
                for req in requests:
                    await self._process_request(req)

                iterations += 1
                if max_iterations and iterations >= max_iterations:
                    break

                await asyncio.sleep(self.poll_interval)

            except KeyboardInterrupt:
                logger.info("Agent stopped by user")
                break
            except Exception as e:
                logger.error(f"Agent loop error: {e}")
                await asyncio.sleep(self.poll_interval * 2)

        self._running = False
        logger.info(f"Agent stopped. Stats: {self.stats}")

    def stop(self):
        self._running = False

    async def _process_request(self, req: InferenceRequest) -> InferenceResult:
        start = time.time()

        try:
            output_text = await self.inference_fn(req.input_text, req.model_id)
            inference_ms = (time.time() - start) * 1000

            input_hash = _blake3_hex(req.input_text.encode("utf-8"))
            output_hash = _blake3_hex(output_text.encode("utf-8"))

            attestation_tx = await self._submit_attestation(input_hash, output_hash)

            settlement_tx = await self._submit_settlement(req.sender, req.fee)

            self.stats.requests_processed += 1
            self.stats.attestations_submitted += 1
            self.stats.settlements_submitted += 1
            self.stats.total_inference_ms += inference_ms
            self.stats.total_earned += req.fee

            result = InferenceResult(
                request_id=req.request_id,
                output_text=output_text,
                input_hash=input_hash,
                output_hash=output_hash,
                inference_ms=inference_ms,
                attestation_tx=attestation_tx,
                settlement_tx=settlement_tx,
            )

            logger.info(
                f"Processed: {req.request_id[:12]}... "
                f"in {inference_ms:.0f}ms "
                f"attest={attestation_tx[:12] if attestation_tx else 'none'}... "
                f"settle={settlement_tx[:12] if settlement_tx else 'none'}..."
            )
            return result

        except Exception as e:
            self.stats.requests_failed += 1
            logger.error(f"Request {req.request_id[:12]}... failed: {e}")
            raise

    async def _submit_attestation(self, input_hash: str, output_hash: str) -> Optional[str]:
        tx = {
            "from": self.address,
            "nonce": self._next_nonce(),
            "tx_type": "InferenceAttestation",
            "body": {
                "model_id": self.model_id,
                "input_hash": input_hash,
                "output_hash": output_hash,
                "challenge_period": self.challenge_period,
                "bond": self.bond_amount,
            },
            "fee": 0,
            "gas_limit": 50_000,
        }
        try:
            return self.client.submit_transaction(tx)
        except ArcError as e:
            logger.warning(f"Attestation submission failed: {e}")
            return None

    async def _submit_settlement(self, recipient: str, amount: int) -> Optional[str]:
        tx = {
            "from": self.address,
            "nonce": self._next_nonce(),
            "tx_type": "Settle",
            "body": {
                "agent_id": recipient,
                "service_hash": _blake3_hex(b"inference"),
                "amount": amount,
            },
            "fee": 0,
            "gas_limit": 25_000,
        }
        try:
            return self.client.submit_transaction(tx)
        except ArcError as e:
            logger.warning(f"Settlement submission failed: {e}")
            return None

    async def _poll_requests(self) -> List[InferenceRequest]:
        return []

    async def infer(self, input_text: str, sender: str = "", fee: int = 0) -> InferenceResult:
        req = InferenceRequest(
            request_id=_blake3_hex(f"{input_text}{time.time()}".encode())[:16],
            sender=sender or self.address,
            input_text=input_text,
            model_id=self.model_id,
            fee=fee,
            block_height=0,
        )
        return await self._process_request(req)

    def _next_nonce(self) -> int:
        n = self._nonce
        self._nonce += 1
        return n

    def status(self) -> Dict[str, Any]:
        return {
            "name": self.name,
            "address": self.address,
            "model": self.model_name,
            "model_id": self.model_id,
            "running": self._running,
            "stats": {
                "requests_processed": self.stats.requests_processed,
                "requests_failed": self.stats.requests_failed,
                "attestations_submitted": self.stats.attestations_submitted,
                "avg_inference_ms": round(self.stats.avg_inference_ms, 1),
                "total_earned": self.stats.total_earned,
                "uptime_seconds": round(self.stats.uptime_seconds, 1),
            },
        }


def openai_runner(
    client: ArcClient,
    keypair: KeyPair,
    *,
    model: str = "gpt-4o",
    api_key: Optional[str] = None,
    name: str = "openai-agent",
    **kwargs,
) -> AgentRunner:
    import os

    _api_key = api_key or os.getenv("OPENAI_API_KEY", "")

    async def _infer(input_text: str, model_id: str) -> str:
        import httpx

        async with httpx.AsyncClient() as http:
            resp = await http.post(
                "https://api.openai.com/v1/chat/completions",
                headers={"Authorization": f"Bearer {_api_key}"},
                json={"model": model, "messages": [{"role": "user", "content": input_text}]},
                timeout=60.0,
            )
            resp.raise_for_status()
            return resp.json()["choices"][0]["message"]["content"]

    return AgentRunner(
        client=client, keypair=keypair, name=name, inference_fn=_infer, model_name=model, **kwargs
    )


def ollama_runner(
    client: ArcClient,
    keypair: KeyPair,
    *,
    model: str = "llama3",
    ollama_url: str = "http://localhost:11434",
    name: str = "ollama-agent",
    **kwargs,
) -> AgentRunner:
    async def _infer(input_text: str, model_id: str) -> str:
        import httpx

        async with httpx.AsyncClient() as http:
            resp = await http.post(
                f"{ollama_url}/api/generate",
                json={"model": model, "prompt": input_text, "stream": False},
                timeout=120.0,
            )
            resp.raise_for_status()
            return resp.json()["response"]

    return AgentRunner(
        client=client, keypair=keypair, name=name, inference_fn=_infer, model_name=model, **kwargs
    )


def openclaw_runner(
    client: ArcClient,
    keypair: KeyPair,
    *,
    gateway_url: str = "http://localhost:8000",
    agent_id: str = "default",
    token: str = "",
    name: str = "openclaw-agent",
    **kwargs,
) -> AgentRunner:
    async def _infer(input_text: str, model_id: str) -> str:
        import httpx

        headers = {}
        if token:
            headers["Authorization"] = f"Bearer {token}"
        async with httpx.AsyncClient() as http:
            resp = await http.post(
                f"{gateway_url}/v1/run",
                json={"agent_id": agent_id, "input": input_text},
                headers=headers,
                timeout=120.0,
            )
            resp.raise_for_status()
            data = resp.json()
            return data.get("output", data.get("result", str(data)))

    return AgentRunner(
        client=client,
        keypair=keypair,
        name=name,
        inference_fn=_infer,
        model_name=f"openclaw-{agent_id}",
        **kwargs,
    )

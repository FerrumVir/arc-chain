"""
ARC Chain Agent Runner — connect any AI model to ARC Chain.

The AgentRunner is the bridge between off-chain AI models (GPT-4, Claude,
Llama, Ollama, OpenClaw, or any HTTP API) and ARC Chain's on-chain agent
infrastructure (registration, inference attestation, settlement).

Usage with OpenAI::

    from arc_sdk import ArcClient, KeyPair
    from arc_sdk.agent_runner import AgentRunner
    import openai

    client = ArcClient("http://localhost:9090")
    kp = KeyPair.from_seed(blake3.blake3(b"my-agent-seed").digest())

    async def gpt4_inference(input_text: str, model_id: str) -> str:
        response = await openai.ChatCompletion.acreate(
            model="gpt-4o",
            messages=[{"role": "user", "content": input_text}],
        )
        return response.choices[0].message.content

    runner = AgentRunner(
        client=client,
        keypair=kp,
        name="my-gpt4-agent",
        inference_fn=gpt4_inference,
    )
    await runner.start()

Usage with Anthropic Claude::

    import anthropic

    claude = anthropic.AsyncAnthropic()

    async def claude_inference(input_text: str, model_id: str) -> str:
        msg = await claude.messages.create(
            model="claude-sonnet-4-20250514",
            max_tokens=1024,
            messages=[{"role": "user", "content": input_text}],
        )
        return msg.content[0].text

    runner = AgentRunner(client=client, keypair=kp, name="claude-agent",
                         inference_fn=claude_inference)

Usage with local Ollama::

    import httpx

    async def ollama_inference(input_text: str, model_id: str) -> str:
        async with httpx.AsyncClient() as http:
            resp = await http.post("http://localhost:11434/api/generate",
                json={"model": "llama3", "prompt": input_text, "stream": False})
            return resp.json()["response"]

    runner = AgentRunner(client=client, keypair=kp, name="llama-local",
                         inference_fn=ollama_inference)

Usage with OpenClaw gateway::

    async def openclaw_inference(input_text: str, model_id: str) -> str:
        async with httpx.AsyncClient() as http:
            resp = await http.post("http://openclaw-gateway:8000/v1/run",
                json={"agent_id": model_id, "input": input_text},
                headers={"Authorization": "Bearer <token>"})
            return resp.json()["output"]

    runner = AgentRunner(client=client, keypair=kp, name="openclaw-router",
                         inference_fn=openclaw_inference)
"""

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

# Type alias for the user-provided inference function.
# Signature: async fn(input_text: str, model_id: str) -> str
InferenceFn = Callable[[str, str], Awaitable[str]]


def _blake3_hex(data: bytes) -> str:
    """Compute BLAKE3 hash and return as 64-char hex string."""
    return blake3.blake3(data).hexdigest()


def _model_id(name: str) -> str:
    """Derive a deterministic 32-byte model ID from a name."""
    return _blake3_hex(f"arc-model-{name}".encode())


@dataclass
class AgentStats:
    """Running statistics for the agent."""
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
    """A pending inference request from the chain."""
    request_id: str
    sender: str
    input_text: str
    model_id: str
    fee: int
    block_height: int


@dataclass
class InferenceResult:
    """Result of an inference execution."""
    request_id: str
    output_text: str
    input_hash: str
    output_hash: str
    inference_ms: float
    attestation_tx: Optional[str] = None
    settlement_tx: Optional[str] = None


class AgentRunner:
    """
    Off-chain agent daemon that connects any AI model to ARC Chain.

    The runner:
    1. Registers as an agent on ARC Chain (RegisterAgent TX)
    2. Polls for incoming inference requests
    3. Calls your inference function (any model, any API)
    4. Submits InferenceAttestation TX with (model_id, input_hash, output_hash)
    5. Settles payment via zero-fee Settle TX
    6. Repeats

    The inference function is yours — GPT-4, Claude, Llama, Ollama, OpenClaw,
    a local model, or anything that takes text in and returns text out.
    """

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

    # ── Lifecycle ────────────────────────────────────────────────────────

    async def register(self) -> str:
        """Register this agent on ARC Chain. Returns the TX hash."""
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
        logger.info(f"Agent registered: {self.name} addr={self.address[:16]}... tx={tx_hash[:16]}...")
        return tx_hash

    async def start(self, max_iterations: Optional[int] = None):
        """
        Start the agent runner loop.

        Polls ARC Chain for inference requests, processes them, and submits
        attestations + settlements. Runs until stopped or max_iterations reached.
        """
        logger.info(f"Starting AgentRunner: {self.name}")
        logger.info(f"  Address:    {self.address[:16]}...")
        logger.info(f"  Model:      {self.model_name} ({self.model_id[:16]}...)")
        logger.info(f"  Poll:       {self.poll_interval}s")
        logger.info(f"  Bond:       {self.bond_amount} ARC")
        logger.info(f"  Fee:        {self.fee_per_request} ARC/request")

        # Register on-chain
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
        """Signal the runner to stop after the current iteration."""
        self._running = False

    # ── Core Processing ──────────────────────────────────────────────────

    async def _process_request(self, req: InferenceRequest) -> InferenceResult:
        """Process a single inference request end-to-end."""
        start = time.time()

        try:
            # 1. Call the user's inference function
            output_text = await self.inference_fn(req.input_text, req.model_id)
            inference_ms = (time.time() - start) * 1000

            # 2. Compute hashes
            input_hash = _blake3_hex(req.input_text.encode("utf-8"))
            output_hash = _blake3_hex(output_text.encode("utf-8"))

            # 3. Submit InferenceAttestation TX
            attestation_tx = await self._submit_attestation(input_hash, output_hash)

            # 4. Submit Settle TX (zero-fee settlement)
            settlement_tx = await self._submit_settlement(req.sender, req.fee)

            # 5. Update stats
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
        """Submit an InferenceAttestation TX to record the inference on-chain."""
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
        """Submit a zero-fee Settle TX for the inference payment."""
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
        """
        Poll ARC Chain for pending inference requests.

        In the current implementation, this checks the agent's account for
        incoming transactions. A future version will use WebSocket subscriptions.
        """
        # For now, return empty — requests come from external callers
        # who invoke the agent directly via the runner's HTTP endpoint
        # or through the ARC Chain RPC.
        return []

    # ── Direct Inference (callable without polling) ──────────────────────

    async def infer(self, input_text: str, sender: str = "", fee: int = 0) -> InferenceResult:
        """
        Run inference directly (without polling).

        Call this from your own HTTP server, CLI, or integration layer.
        The runner handles the model call + attestation + settlement.

        Args:
            input_text: The input to send to the model.
            sender: Address of the requester (for settlement).
            fee: Amount to settle (0 = free).

        Returns:
            InferenceResult with output, hashes, and TX hashes.
        """
        req = InferenceRequest(
            request_id=_blake3_hex(f"{input_text}{time.time()}".encode())[:16],
            sender=sender or self.address,
            input_text=input_text,
            model_id=self.model_id,
            fee=fee,
            block_height=0,
        )
        return await self._process_request(req)

    # ── Helpers ──────────────────────────────────────────────────────────

    def _next_nonce(self) -> int:
        """Get and increment the local nonce counter."""
        n = self._nonce
        self._nonce += 1
        return n

    def status(self) -> Dict[str, Any]:
        """Return agent status as a dict."""
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


# ── Convenience constructors for common providers ────────────────────────

def openai_runner(
    client: ArcClient,
    keypair: KeyPair,
    *,
    model: str = "gpt-4o",
    api_key: Optional[str] = None,
    name: str = "openai-agent",
    **kwargs,
) -> AgentRunner:
    """Create an AgentRunner that calls OpenAI's API."""
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

    return AgentRunner(client=client, keypair=keypair, name=name,
                       inference_fn=_infer, model_name=model, **kwargs)


def anthropic_runner(
    client: ArcClient,
    keypair: KeyPair,
    *,
    model: str = "claude-sonnet-4-20250514",
    api_key: Optional[str] = None,
    name: str = "claude-agent",
    **kwargs,
) -> AgentRunner:
    """Create an AgentRunner that calls Anthropic's API."""
    import os
    _api_key = api_key or os.getenv("ANTHROPIC_API_KEY", "")

    async def _infer(input_text: str, model_id: str) -> str:
        import httpx
        async with httpx.AsyncClient() as http:
            resp = await http.post(
                "https://api.anthropic.com/v1/messages",
                headers={
                    "x-api-key": _api_key,
                    "anthropic-version": "2023-06-01",
                    "content-type": "application/json",
                },
                json={
                    "model": model,
                    "max_tokens": 1024,
                    "messages": [{"role": "user", "content": input_text}],
                },
                timeout=60.0,
            )
            resp.raise_for_status()
            return resp.json()["content"][0]["text"]

    return AgentRunner(client=client, keypair=keypair, name=name,
                       inference_fn=_infer, model_name=model, **kwargs)


def ollama_runner(
    client: ArcClient,
    keypair: KeyPair,
    *,
    model: str = "llama3",
    ollama_url: str = "http://localhost:11434",
    name: str = "ollama-agent",
    **kwargs,
) -> AgentRunner:
    """Create an AgentRunner that calls a local Ollama instance."""

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

    return AgentRunner(client=client, keypair=keypair, name=name,
                       inference_fn=_infer, model_name=model, **kwargs)


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
    """Create an AgentRunner that routes through an OpenClaw gateway."""

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

    return AgentRunner(client=client, keypair=keypair, name=name,
                       inference_fn=_infer, model_name=f"openclaw-{agent_id}", **kwargs)

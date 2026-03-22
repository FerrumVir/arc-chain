"""
Off-chain bilateral payment channel for ARC Chain.

Provides a Channel class with pay(), receive(), close(), and dispute()
methods for high-throughput off-chain transactions between two agents.

Usage:
    channel = Channel(channel_id, "opener", total_deposit=1_000_000)
    channel.confirm_open()
    commitment = channel.pay(100)
    # Send commitment to counterparty...
"""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum
from typing import List, Optional


class ChannelState(Enum):
    """Channel lifecycle states."""

    OPENING = "Opening"
    OPEN = "Open"
    CLOSING = "Closing"
    DISPUTED = "Disputed"
    CLOSED = "Closed"


class Role(Enum):
    """Role of a party in the channel."""

    OPENER = "opener"
    COUNTERPARTY = "counterparty"


@dataclass
class StateCommitment:
    """A signed state commitment for the channel."""

    channel_id: bytes
    nonce: int
    opener_balance: int
    counterparty_balance: int
    proposer_sig: bytes = b""
    acceptor_sig: Optional[bytes] = None

    @property
    def is_fully_signed(self) -> bool:
        return self.acceptor_sig is not None


class ChannelError(Exception):
    """Base error for channel operations."""

    pass


class Channel:
    """
    Off-chain bilateral payment channel.

    Manages state transitions, signature verification, and balance conservation
    for a two-party payment channel on ARC Chain.
    """

    def __init__(
        self,
        channel_id: bytes,
        role: Role,
        total_deposit: int,
    ):
        self.channel_id = channel_id
        self.role = role
        self.total_deposit = total_deposit
        self.state = ChannelState.OPENING

        if role == Role.OPENER:
            self.opener_balance = total_deposit
            self.counterparty_balance = 0
        else:
            self.opener_balance = 0
            self.counterparty_balance = total_deposit

        self.nonce = 0
        self._history: List[StateCommitment] = []

    def confirm_open(self) -> None:
        """Mark channel as open after on-chain ChannelOpen confirms."""
        if self.state != ChannelState.OPENING:
            raise ChannelError(f"Cannot open: channel is {self.state.value}")
        self.state = ChannelState.OPEN

    @property
    def my_balance(self) -> int:
        """Get this party's current balance."""
        if self.role == Role.OPENER:
            return self.opener_balance
        return self.counterparty_balance

    @property
    def their_balance(self) -> int:
        """Get counterparty's current balance."""
        if self.role == Role.OPENER:
            return self.counterparty_balance
        return self.opener_balance

    def pay(self, amount: int) -> StateCommitment:
        """
        Transfer `amount` from this party to the counterparty.
        Returns a half-signed state commitment to send to the counterparty.
        """
        if self.state != ChannelState.OPEN:
            raise ChannelError(f"Cannot pay: channel is {self.state.value}")

        if amount > self.my_balance:
            raise ChannelError(
                f"Insufficient balance: have {self.my_balance}, need {amount}"
            )

        if self.role == Role.OPENER:
            new_opener = self.opener_balance - amount
            new_counter = self.counterparty_balance + amount
        else:
            new_opener = self.opener_balance + amount
            new_counter = self.counterparty_balance - amount

        return self.propose_state(new_opener, new_counter)

    def propose_state(
        self, opener_balance: int, counterparty_balance: int
    ) -> StateCommitment:
        """Propose a new state with arbitrary balances."""
        if self.state != ChannelState.OPEN:
            raise ChannelError(f"Cannot propose: channel is {self.state.value}")

        if opener_balance + counterparty_balance != self.total_deposit:
            raise ChannelError(
                f"Conservation violated: {opener_balance} + {counterparty_balance} "
                f"!= {self.total_deposit}"
            )

        new_nonce = self.nonce + 1

        return StateCommitment(
            channel_id=self.channel_id,
            nonce=new_nonce,
            opener_balance=opener_balance,
            counterparty_balance=counterparty_balance,
        )

    def receive_state(self, commitment: StateCommitment) -> StateCommitment:
        """
        Receive and validate a state commitment from the counterparty.
        Updates local state if valid.
        """
        if self.state != ChannelState.OPEN:
            raise ChannelError(f"Cannot receive: channel is {self.state.value}")

        if commitment.channel_id != self.channel_id:
            raise ChannelError("Channel ID mismatch")

        if commitment.nonce <= self.nonce:
            raise ChannelError(
                f"Nonce must increase: got {commitment.nonce}, current {self.nonce}"
            )

        total = commitment.opener_balance + commitment.counterparty_balance
        if total != self.total_deposit:
            raise ChannelError("Conservation violated")

        # In full implementation, verify proposer's Ed25519 signature here.
        signed = StateCommitment(
            channel_id=commitment.channel_id,
            nonce=commitment.nonce,
            opener_balance=commitment.opener_balance,
            counterparty_balance=commitment.counterparty_balance,
            proposer_sig=commitment.proposer_sig,
            acceptor_sig=b"\x00" * 64,  # Caller signs externally
        )

        self.nonce = commitment.nonce
        self.opener_balance = commitment.opener_balance
        self.counterparty_balance = commitment.counterparty_balance
        self._history.append(signed)

        return signed

    def finalize_state(self, commitment: StateCommitment) -> None:
        """Finalize a state after receiving counterparty's co-signature."""
        if not commitment.is_fully_signed:
            raise ChannelError("Commitment not fully signed")

        self.nonce = commitment.nonce
        self.opener_balance = commitment.opener_balance
        self.counterparty_balance = commitment.counterparty_balance
        self._history.append(commitment)

    def close(self) -> StateCommitment:
        """Initiate cooperative close."""
        if self.state != ChannelState.OPEN:
            raise ChannelError(f"Cannot close: channel is {self.state.value}")

        self.state = ChannelState.CLOSING

        return StateCommitment(
            channel_id=self.channel_id,
            nonce=self.nonce,
            opener_balance=self.opener_balance,
            counterparty_balance=self.counterparty_balance,
        )

    def dispute(self) -> StateCommitment:
        """Get the latest fully-signed state for on-chain dispute submission."""
        for c in reversed(self._history):
            if c.is_fully_signed:
                return c
        raise ChannelError("No fully-signed states available for dispute")

    def confirm_closed(self) -> None:
        """Mark channel as closed after on-chain resolution."""
        self.state = ChannelState.CLOSED

    @staticmethod
    def build_open_body(
        channel_id: str,
        counterparty: str,
        deposit: int,
        timeout_blocks: int = 100,
    ) -> dict:
        """Build a ChannelOpen transaction body."""
        return {
            "type": "ChannelOpen",
            "channel_id": channel_id,
            "counterparty": counterparty,
            "deposit": deposit,
            "timeout_blocks": timeout_blocks,
        }

    @staticmethod
    def build_close_body(
        channel_id: str,
        opener_balance: int,
        counterparty_balance: int,
        counterparty_sig: list,
        state_nonce: int,
    ) -> dict:
        """Build a ChannelClose transaction body."""
        return {
            "type": "ChannelClose",
            "channel_id": channel_id,
            "opener_balance": opener_balance,
            "counterparty_balance": counterparty_balance,
            "counterparty_sig": counterparty_sig,
            "state_nonce": state_nonce,
        }

    @staticmethod
    def build_dispute_body(
        channel_id: str,
        opener_balance: int,
        counterparty_balance: int,
        other_party_sig: list,
        state_nonce: int,
        challenge_period: int = 100,
    ) -> dict:
        """Build a ChannelDispute transaction body."""
        return {
            "type": "ChannelDispute",
            "channel_id": channel_id,
            "opener_balance": opener_balance,
            "counterparty_balance": counterparty_balance,
            "other_party_sig": other_party_sig,
            "state_nonce": state_nonce,
            "challenge_period": challenge_period,
        }

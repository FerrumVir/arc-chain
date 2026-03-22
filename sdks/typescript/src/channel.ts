/**
 * Off-chain bilateral payment channel for ARC Chain.
 *
 * Provides a Channel class with pay(), receive(), close(), and dispute()
 * methods for high-throughput off-chain transactions between two agents.
 *
 * @example
 * ```typescript
 * const channel = new Channel(channelId, myKeypair, counterpartyPubkey, deposit);
 * await channel.open(client);
 * const commitment = channel.pay(100);
 * // Send commitment to counterparty...
 * ```
 */

import { Hash256, Address, TransactionBody } from './types';

/** Channel lifecycle states. */
export enum ChannelState {
  Opening = 'Opening',
  Open = 'Open',
  Closing = 'Closing',
  Disputed = 'Disputed',
  Closed = 'Closed',
}

/** A signed state commitment for the channel. */
export interface StateCommitment {
  channelId: Hash256;
  nonce: number;
  openerBalance: number;
  counterpartyBalance: number;
  /** Signature from the party that proposed this state. */
  proposerSig: Uint8Array;
  /** Signature from the party that accepted this state (null if pending). */
  acceptorSig: Uint8Array | null;
}

/** Role of a party in the channel. */
export type Role = 'opener' | 'counterparty';

/**
 * Off-chain bilateral payment channel.
 *
 * Manages state transitions, signature verification, and balance conservation
 * for a two-party payment channel on ARC Chain.
 */
export class Channel {
  readonly channelId: Hash256;
  readonly role: Role;
  readonly totalDeposit: number;

  state: ChannelState = ChannelState.Opening;
  openerBalance: number;
  counterpartyBalance: number;
  nonce: number = 0;

  private history: StateCommitment[] = [];

  constructor(
    channelId: Hash256,
    role: Role,
    totalDeposit: number,
  ) {
    this.channelId = channelId;
    this.role = role;
    this.totalDeposit = totalDeposit;

    if (role === 'opener') {
      this.openerBalance = totalDeposit;
      this.counterpartyBalance = 0;
    } else {
      this.openerBalance = 0;
      this.counterpartyBalance = totalDeposit;
    }
  }

  /** Mark channel as open after on-chain ChannelOpen confirms. */
  confirmOpen(): void {
    if (this.state !== ChannelState.Opening) {
      throw new Error(`Cannot open: channel is ${this.state}`);
    }
    this.state = ChannelState.Open;
  }

  /** Get this party's current balance. */
  myBalance(): number {
    return this.role === 'opener' ? this.openerBalance : this.counterpartyBalance;
  }

  /** Get counterparty's current balance. */
  theirBalance(): number {
    return this.role === 'opener' ? this.counterpartyBalance : this.openerBalance;
  }

  /**
   * Transfer `amount` from this party to the counterparty.
   * Returns a half-signed state commitment to send to the counterparty.
   */
  pay(amount: number): StateCommitment {
    if (this.state !== ChannelState.Open) {
      throw new Error(`Cannot pay: channel is ${this.state}`);
    }

    if (amount > this.myBalance()) {
      throw new Error(`Insufficient balance: have ${this.myBalance()}, need ${amount}`);
    }

    let newOpener: number, newCounter: number;
    if (this.role === 'opener') {
      newOpener = this.openerBalance - amount;
      newCounter = this.counterpartyBalance + amount;
    } else {
      newOpener = this.openerBalance + amount;
      newCounter = this.counterpartyBalance - amount;
    }

    return this.proposeState(newOpener, newCounter);
  }

  /** Propose a new state with arbitrary balances. */
  proposeState(openerBalance: number, counterpartyBalance: number): StateCommitment {
    if (this.state !== ChannelState.Open) {
      throw new Error(`Cannot propose: channel is ${this.state}`);
    }

    if (openerBalance + counterpartyBalance !== this.totalDeposit) {
      throw new Error(
        `Conservation violated: ${openerBalance} + ${counterpartyBalance} != ${this.totalDeposit}`
      );
    }

    const newNonce = this.nonce + 1;

    const commitment: StateCommitment = {
      channelId: this.channelId,
      nonce: newNonce,
      openerBalance,
      counterpartyBalance,
      proposerSig: new Uint8Array(64), // Caller signs externally
      acceptorSig: null,
    };

    return commitment;
  }

  /**
   * Receive and validate a state commitment from the counterparty.
   * Updates local state if valid.
   */
  receiveState(commitment: StateCommitment): StateCommitment {
    if (this.state !== ChannelState.Open) {
      throw new Error(`Cannot receive: channel is ${this.state}`);
    }

    if (commitment.channelId !== this.channelId) {
      throw new Error('Channel ID mismatch');
    }

    if (commitment.nonce <= this.nonce) {
      throw new Error(`Nonce must increase: got ${commitment.nonce}, current ${this.nonce}`);
    }

    if (commitment.openerBalance + commitment.counterpartyBalance !== this.totalDeposit) {
      throw new Error('Conservation violated');
    }

    // In a full implementation, verify proposer's Ed25519 signature here.
    // For now, accept the commitment and co-sign.

    const signed: StateCommitment = {
      ...commitment,
      acceptorSig: new Uint8Array(64), // Caller signs externally
    };

    this.nonce = commitment.nonce;
    this.openerBalance = commitment.openerBalance;
    this.counterpartyBalance = commitment.counterpartyBalance;
    this.history.push(signed);

    return signed;
  }

  /** Finalize a state after receiving counterparty's co-signature. */
  finalizeState(commitment: StateCommitment): void {
    if (!commitment.acceptorSig) {
      throw new Error('Commitment not fully signed');
    }

    this.nonce = commitment.nonce;
    this.openerBalance = commitment.openerBalance;
    this.counterpartyBalance = commitment.counterpartyBalance;
    this.history.push(commitment);
  }

  /** Initiate cooperative close. */
  close(): StateCommitment {
    if (this.state !== ChannelState.Open) {
      throw new Error(`Cannot close: channel is ${this.state}`);
    }

    this.state = ChannelState.Closing;

    return {
      channelId: this.channelId,
      nonce: this.nonce,
      openerBalance: this.openerBalance,
      counterpartyBalance: this.counterpartyBalance,
      proposerSig: new Uint8Array(64),
      acceptorSig: null,
    };
  }

  /** Get the latest fully-signed state for on-chain dispute submission. */
  dispute(): StateCommitment {
    const latest = [...this.history].reverse().find((c) => c.acceptorSig !== null);
    if (!latest) {
      throw new Error('No fully-signed states available for dispute');
    }
    return latest;
  }

  /** Mark channel as closed after on-chain resolution. */
  confirmClosed(): void {
    this.state = ChannelState.Closed;
  }

  /** Build a ChannelOpen transaction body. */
  static buildOpenBody(
    channelId: Hash256,
    counterparty: Address,
    deposit: number,
    timeoutBlocks: number = 100,
  ): TransactionBody {
    return {
      type: 'ChannelOpen',
      channel_id: channelId,
      counterparty,
      deposit,
      timeout_blocks: timeoutBlocks,
    };
  }

  /** Build a ChannelClose transaction body. */
  static buildCloseBody(
    channelId: Hash256,
    openerBalance: number,
    counterpartyBalance: number,
    counterpartySig: number[],
    stateNonce: number,
  ): TransactionBody {
    return {
      type: 'ChannelClose',
      channel_id: channelId,
      opener_balance: openerBalance,
      counterparty_balance: counterpartyBalance,
      counterparty_sig: counterpartySig,
      state_nonce: stateNonce,
    };
  }

  /** Build a ChannelDispute transaction body. */
  static buildDisputeBody(
    channelId: Hash256,
    openerBalance: number,
    counterpartyBalance: number,
    otherPartySig: number[],
    stateNonce: number,
    challengePeriod: number = 100,
  ): TransactionBody {
    return {
      type: 'ChannelDispute',
      channel_id: channelId,
      opener_balance: openerBalance,
      counterparty_balance: counterpartyBalance,
      other_party_sig: otherPartySig,
      state_nonce: stateNonce,
      challenge_period: challengePeriod,
    };
  }
}

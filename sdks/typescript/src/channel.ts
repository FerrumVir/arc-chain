import { Hash256, Address, TransactionBody } from './types';

function assertSafeU64Number(value: number, fieldName: string): void {
  if (!Number.isSafeInteger(value) || value < 0) {
    throw new RangeError(`${fieldName} must be a non-negative safe integer`);
  }
}

export enum ChannelState {
  Opening = 'Opening',
  Open = 'Open',
  Closing = 'Closing',
  Disputed = 'Disputed',
  Closed = 'Closed',
}

export interface StateCommitment {
  channelId: Hash256;
  nonce: number;
  openerBalance: number;
  counterpartyBalance: number;
  proposerSig: Uint8Array;
  acceptorSig: Uint8Array | null;
}

export type Role = 'opener' | 'counterparty';

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
    assertSafeU64Number(totalDeposit, 'totalDeposit');
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

  confirmOpen(): void {
    if (this.state !== ChannelState.Opening) {
      throw new Error(`Cannot open: channel is ${this.state}`);
    }
    this.state = ChannelState.Open;
  }

  myBalance(): number {
    return this.role === 'opener' ? this.openerBalance : this.counterpartyBalance;
  }

  theirBalance(): number {
    return this.role === 'opener' ? this.counterpartyBalance : this.openerBalance;
  }

  pay(amount: number): StateCommitment {
    if (this.state !== ChannelState.Open) {
      throw new Error(`Cannot pay: channel is ${this.state}`);
    }

    assertSafeU64Number(amount, 'amount');
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

    assertSafeU64Number(newOpener, 'openerBalance');
    assertSafeU64Number(newCounter, 'counterpartyBalance');

    return this.proposeState(newOpener, newCounter);
  }

  proposeState(openerBalance: number, counterpartyBalance: number): StateCommitment {
    if (this.state !== ChannelState.Open) {
      throw new Error(`Cannot propose: channel is ${this.state}`);
    }

    assertSafeU64Number(openerBalance, 'openerBalance');
    assertSafeU64Number(counterpartyBalance, 'counterpartyBalance');
    if (
      BigInt(openerBalance) + BigInt(counterpartyBalance) !==
      BigInt(this.totalDeposit)
    ) {
      throw new Error(
        `Conservation violated: ${openerBalance} + ${counterpartyBalance} != ${this.totalDeposit}`
      );
    }

    const newNonce = this.nonce + 1;
    assertSafeU64Number(newNonce, 'nonce');

    const commitment: StateCommitment = {
      channelId: this.channelId,
      nonce: newNonce,
      openerBalance,
      counterpartyBalance,
      proposerSig: new Uint8Array(64),
      acceptorSig: null,
    };

    return commitment;
  }

  receiveState(commitment: StateCommitment): StateCommitment {
    if (this.state !== ChannelState.Open) {
      throw new Error(`Cannot receive: channel is ${this.state}`);
    }

    if (commitment.channelId !== this.channelId) {
      throw new Error('Channel ID mismatch');
    }

    assertSafeU64Number(commitment.nonce, 'nonce');
    assertSafeU64Number(commitment.openerBalance, 'openerBalance');
    assertSafeU64Number(commitment.counterpartyBalance, 'counterpartyBalance');
    if (commitment.nonce <= this.nonce) {
      throw new Error(`Nonce must increase: got ${commitment.nonce}, current ${this.nonce}`);
    }

    if (
      BigInt(commitment.openerBalance) + BigInt(commitment.counterpartyBalance) !==
      BigInt(this.totalDeposit)
    ) {
      throw new Error('Conservation violated');
    }

    const signed: StateCommitment = {
      ...commitment,
      acceptorSig: new Uint8Array(64),
    };

    this.nonce = commitment.nonce;
    this.openerBalance = commitment.openerBalance;
    this.counterpartyBalance = commitment.counterpartyBalance;
    this.history.push(signed);

    return signed;
  }

  finalizeState(commitment: StateCommitment): void {
    if (!commitment.acceptorSig) {
      throw new Error('Commitment not fully signed');
    }

    assertSafeU64Number(commitment.nonce, 'nonce');
    assertSafeU64Number(commitment.openerBalance, 'openerBalance');
    assertSafeU64Number(commitment.counterpartyBalance, 'counterpartyBalance');
    if (
      BigInt(commitment.openerBalance) + BigInt(commitment.counterpartyBalance) !==
      BigInt(this.totalDeposit)
    ) {
      throw new Error('Conservation violated');
    }

    this.nonce = commitment.nonce;
    this.openerBalance = commitment.openerBalance;
    this.counterpartyBalance = commitment.counterpartyBalance;
    this.history.push(commitment);
  }

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

  dispute(): StateCommitment {
    const latest = [...this.history].reverse().find((c) => c.acceptorSig !== null);
    if (!latest) {
      throw new Error('No fully-signed states available for dispute');
    }
    return latest;
  }

  confirmClosed(): void {
    this.state = ChannelState.Closed;
  }

  static buildOpenBody(
    channelId: Hash256,
    counterparty: Address,
    deposit: number,
    timeoutBlocks: number = 100,
  ): TransactionBody {
    assertSafeU64Number(deposit, 'deposit');
    assertSafeU64Number(timeoutBlocks, 'timeoutBlocks');
    return {
      type: 'ChannelOpen',
      channel_id: channelId,
      counterparty,
      deposit,
      timeout_blocks: timeoutBlocks,
    };
  }

  static buildCloseBody(
    channelId: Hash256,
    openerBalance: number,
    counterpartyBalance: number,
    counterpartySig: number[],
    stateNonce: number,
  ): TransactionBody {
    assertSafeU64Number(openerBalance, 'openerBalance');
    assertSafeU64Number(counterpartyBalance, 'counterpartyBalance');
    assertSafeU64Number(stateNonce, 'stateNonce');
    return {
      type: 'ChannelClose',
      channel_id: channelId,
      opener_balance: openerBalance,
      counterparty_balance: counterpartyBalance,
      counterparty_sig: counterpartySig,
      state_nonce: stateNonce,
    };
  }

  static buildDisputeBody(
    channelId: Hash256,
    openerBalance: number,
    counterpartyBalance: number,
    otherPartySig: number[],
    stateNonce: number,
    challengePeriod: number = 100,
  ): TransactionBody {
    assertSafeU64Number(openerBalance, 'openerBalance');
    assertSafeU64Number(counterpartyBalance, 'counterpartyBalance');
    assertSafeU64Number(stateNonce, 'stateNonce');
    assertSafeU64Number(challengePeriod, 'challengePeriod');
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

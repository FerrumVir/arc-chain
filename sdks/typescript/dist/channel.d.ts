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
export declare enum ChannelState {
    Opening = "Opening",
    Open = "Open",
    Closing = "Closing",
    Disputed = "Disputed",
    Closed = "Closed"
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
export declare class Channel {
    readonly channelId: Hash256;
    readonly role: Role;
    readonly totalDeposit: number;
    state: ChannelState;
    openerBalance: number;
    counterpartyBalance: number;
    nonce: number;
    private history;
    constructor(channelId: Hash256, role: Role, totalDeposit: number);
    /** Mark channel as open after on-chain ChannelOpen confirms. */
    confirmOpen(): void;
    /** Get this party's current balance. */
    myBalance(): number;
    /** Get counterparty's current balance. */
    theirBalance(): number;
    /**
     * Transfer `amount` from this party to the counterparty.
     * Returns a half-signed state commitment to send to the counterparty.
     */
    pay(amount: number): StateCommitment;
    /** Propose a new state with arbitrary balances. */
    proposeState(openerBalance: number, counterpartyBalance: number): StateCommitment;
    /**
     * Receive and validate a state commitment from the counterparty.
     * Updates local state if valid.
     */
    receiveState(commitment: StateCommitment): StateCommitment;
    /** Finalize a state after receiving counterparty's co-signature. */
    finalizeState(commitment: StateCommitment): void;
    /** Initiate cooperative close. */
    close(): StateCommitment;
    /** Get the latest fully-signed state for on-chain dispute submission. */
    dispute(): StateCommitment;
    /** Mark channel as closed after on-chain resolution. */
    confirmClosed(): void;
    /** Build a ChannelOpen transaction body. */
    static buildOpenBody(channelId: Hash256, counterparty: Address, deposit: number, timeoutBlocks?: number): TransactionBody;
    /** Build a ChannelClose transaction body. */
    static buildCloseBody(channelId: Hash256, openerBalance: number, counterpartyBalance: number, counterpartySig: number[], stateNonce: number): TransactionBody;
    /** Build a ChannelDispute transaction body. */
    static buildDisputeBody(channelId: Hash256, openerBalance: number, counterpartyBalance: number, otherPartySig: number[], stateNonce: number, challengePeriod?: number): TransactionBody;
}
//# sourceMappingURL=channel.d.ts.map
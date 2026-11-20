import { Hash256, Address, TransactionBody } from './types';
export declare enum ChannelState {
    Opening = "Opening",
    Open = "Open",
    Closing = "Closing",
    Disputed = "Disputed",
    Closed = "Closed"
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
    confirmOpen(): void;
    myBalance(): number;
    theirBalance(): number;
    pay(amount: number): StateCommitment;
    proposeState(openerBalance: number, counterpartyBalance: number): StateCommitment;
    receiveState(commitment: StateCommitment): StateCommitment;
    finalizeState(commitment: StateCommitment): void;
    close(): StateCommitment;
    dispute(): StateCommitment;
    confirmClosed(): void;
    static buildOpenBody(channelId: Hash256, counterparty: Address, deposit: number, timeoutBlocks?: number): TransactionBody;
    static buildCloseBody(channelId: Hash256, openerBalance: number, counterpartyBalance: number, counterpartySig: number[], stateNonce: number): TransactionBody;
    static buildDisputeBody(channelId: Hash256, openerBalance: number, counterpartyBalance: number, otherPartySig: number[], stateNonce: number, challengePeriod?: number): TransactionBody;
}
//# sourceMappingURL=channel.d.ts.map
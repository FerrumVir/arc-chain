/**
 * ARC Chain SDK — Transaction builder.
 *
 * Constructs unsigned transaction objects matching the ARC Chain RPC format,
 * then signs them with Ed25519 and computes the BLAKE3 transaction hash.
 */
import { KeyPair } from "./crypto";
import type { Transaction } from "./types";
/**
 * Build unsigned ARC Chain transactions.
 *
 * All methods return a Transaction object that can be signed with
 * `TransactionBuilder.sign()` and submitted via `ArcClient.submitTransaction()`.
 */
export declare class TransactionBuilder {
    /**
     * Build an unsigned transfer transaction.
     *
     * @param fromAddr - 64-char hex sender address
     * @param toAddr - 64-char hex recipient address
     * @param amount - Amount in ARC tokens (smallest unit)
     * @param fee - Transaction fee (default 1)
     * @param nonce - Sender nonce for replay protection
     */
    static transfer(fromAddr: string, toAddr: string, amount: number, fee?: number, nonce?: number): Transaction;
    /**
     * Build an unsigned contract deployment transaction.
     *
     * @param fromAddr - 64-char hex sender address
     * @param code - WASM bytecode as Uint8Array
     * @param gasLimit - Maximum gas for deployment
     * @param fee - Transaction fee
     * @param nonce - Sender nonce
     * @param constructorArgs - ABI-encoded constructor arguments
     * @param stateRentDeposit - Pre-paid state rent
     */
    static deployContract(fromAddr: string, code: Uint8Array, gasLimit?: number, fee?: number, nonce?: number, constructorArgs?: Uint8Array, stateRentDeposit?: number): Transaction;
    /**
     * Build an unsigned WASM contract call transaction.
     *
     * @param fromAddr - 64-char hex sender address
     * @param contractAddr - 64-char hex contract address
     * @param calldata - ABI-encoded call data as Uint8Array
     * @param value - ARC tokens to send with the call
     * @param gasLimit - Maximum gas for execution
     * @param func - Function name to call
     * @param fee - Transaction fee
     * @param nonce - Sender nonce
     */
    static callContract(fromAddr: string, contractAddr: string, calldata: Uint8Array, value?: number, gasLimit?: number, func?: string, fee?: number, nonce?: number): Transaction;
    /**
     * Build an unsigned stake/unstake transaction.
     *
     * @param fromAddr - 64-char hex sender address
     * @param amount - Amount to stake or unstake
     * @param isStake - True to stake, false to unstake
     * @param validator - Validator address (defaults to self)
     * @param fee - Transaction fee
     * @param nonce - Sender nonce
     */
    static stake(fromAddr: string, amount: number, isStake?: boolean, validator?: string, fee?: number, nonce?: number): Transaction;
    /**
     * Build an unsigned settlement transaction (zero fee).
     *
     * @param fromAddr - 64-char hex sender address
     * @param agentId - 64-char hex agent address
     * @param serviceHash - 64-char hex service hash
     * @param amount - Settlement amount
     * @param usageUnits - Usage units consumed
     * @param nonce - Sender nonce
     */
    static settle(fromAddr: string, agentId: string, serviceHash: string, amount: number, usageUnits: number, nonce?: number): Transaction;
    /**
     * Sign a transaction with the given key pair.
     *
     * @param tx - Unsigned transaction from any build method
     * @param keypair - Ed25519 key pair whose address matches tx.from
     * @returns A new signed transaction (original is not modified)
     */
    static sign(tx: Transaction, keypair: KeyPair): Promise<Transaction>;
}
//# sourceMappingURL=transaction.d.ts.map
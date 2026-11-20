import { KeyPair } from "./crypto";
import type { Transaction, TransferTransaction, SignedTransferTransaction, Ed25519Signature, U64 } from "./types";
export declare class TransactionBuilder {
    static transfer(fromAddr: string, toAddr: string, amount: U64, fee?: U64, nonce?: U64, transactionDomain?: string | null): TransferTransaction;
    static deployContract(fromAddr: string, code: Uint8Array, gasLimit?: U64, fee?: U64, nonce?: U64, constructorArgs?: Uint8Array, stateRentDeposit?: U64): Transaction;
    static callContract(fromAddr: string, contractAddr: string, calldata: Uint8Array, value?: U64, gasLimit?: U64, func?: string, fee?: U64, nonce?: U64): Transaction;
    static stake(fromAddr: string, amount: U64, isStake?: boolean, validator?: string, fee?: U64, nonce?: U64): Transaction;
    static settle(fromAddr: string, agentId: string, serviceHash: string, amount: U64, usageUnits: U64, nonce?: U64): Transaction;
    static sign(tx: TransferTransaction, keypair: KeyPair): Promise<SignedTransferTransaction>;
    static sign<T extends Transaction>(tx: T, keypair: KeyPair): Promise<T & {
        signature: Ed25519Signature;
    }>;
}
//# sourceMappingURL=transaction.d.ts.map
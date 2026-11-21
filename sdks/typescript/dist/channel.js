"use strict";
Object.defineProperty(exports, "__esModule", { value: true });
exports.Channel = exports.ChannelState = void 0;
function assertSafeU64Number(value, fieldName) {
    if (!Number.isSafeInteger(value) || value < 0) {
        throw new RangeError(`${fieldName} must be a non-negative safe integer`);
    }
}
var ChannelState;
(function (ChannelState) {
    ChannelState["Opening"] = "Opening";
    ChannelState["Open"] = "Open";
    ChannelState["Closing"] = "Closing";
    ChannelState["Disputed"] = "Disputed";
    ChannelState["Closed"] = "Closed";
})(ChannelState || (exports.ChannelState = ChannelState = {}));
class Channel {
    constructor(channelId, role, totalDeposit) {
        this.state = ChannelState.Opening;
        this.nonce = 0;
        this.history = [];
        assertSafeU64Number(totalDeposit, 'totalDeposit');
        this.channelId = channelId;
        this.role = role;
        this.totalDeposit = totalDeposit;
        if (role === 'opener') {
            this.openerBalance = totalDeposit;
            this.counterpartyBalance = 0;
        }
        else {
            this.openerBalance = 0;
            this.counterpartyBalance = totalDeposit;
        }
    }
    confirmOpen() {
        if (this.state !== ChannelState.Opening) {
            throw new Error(`Cannot open: channel is ${this.state}`);
        }
        this.state = ChannelState.Open;
    }
    myBalance() {
        return this.role === 'opener' ? this.openerBalance : this.counterpartyBalance;
    }
    theirBalance() {
        return this.role === 'opener' ? this.counterpartyBalance : this.openerBalance;
    }
    pay(amount) {
        if (this.state !== ChannelState.Open) {
            throw new Error(`Cannot pay: channel is ${this.state}`);
        }
        assertSafeU64Number(amount, 'amount');
        if (amount > this.myBalance()) {
            throw new Error(`Insufficient balance: have ${this.myBalance()}, need ${amount}`);
        }
        let newOpener, newCounter;
        if (this.role === 'opener') {
            newOpener = this.openerBalance - amount;
            newCounter = this.counterpartyBalance + amount;
        }
        else {
            newOpener = this.openerBalance + amount;
            newCounter = this.counterpartyBalance - amount;
        }
        assertSafeU64Number(newOpener, 'openerBalance');
        assertSafeU64Number(newCounter, 'counterpartyBalance');
        return this.proposeState(newOpener, newCounter);
    }
    proposeState(openerBalance, counterpartyBalance) {
        if (this.state !== ChannelState.Open) {
            throw new Error(`Cannot propose: channel is ${this.state}`);
        }
        assertSafeU64Number(openerBalance, 'openerBalance');
        assertSafeU64Number(counterpartyBalance, 'counterpartyBalance');
        if (BigInt(openerBalance) + BigInt(counterpartyBalance) !==
            BigInt(this.totalDeposit)) {
            throw new Error(`Conservation violated: ${openerBalance} + ${counterpartyBalance} != ${this.totalDeposit}`);
        }
        const newNonce = this.nonce + 1;
        assertSafeU64Number(newNonce, 'nonce');
        const commitment = {
            channelId: this.channelId,
            nonce: newNonce,
            openerBalance,
            counterpartyBalance,
            proposerSig: new Uint8Array(64),
            acceptorSig: null,
        };
        return commitment;
    }
    receiveState(commitment) {
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
        if (BigInt(commitment.openerBalance) + BigInt(commitment.counterpartyBalance) !==
            BigInt(this.totalDeposit)) {
            throw new Error('Conservation violated');
        }
        const signed = {
            ...commitment,
            acceptorSig: new Uint8Array(64),
        };
        this.nonce = commitment.nonce;
        this.openerBalance = commitment.openerBalance;
        this.counterpartyBalance = commitment.counterpartyBalance;
        this.history.push(signed);
        return signed;
    }
    finalizeState(commitment) {
        if (!commitment.acceptorSig) {
            throw new Error('Commitment not fully signed');
        }
        assertSafeU64Number(commitment.nonce, 'nonce');
        assertSafeU64Number(commitment.openerBalance, 'openerBalance');
        assertSafeU64Number(commitment.counterpartyBalance, 'counterpartyBalance');
        if (BigInt(commitment.openerBalance) + BigInt(commitment.counterpartyBalance) !==
            BigInt(this.totalDeposit)) {
            throw new Error('Conservation violated');
        }
        this.nonce = commitment.nonce;
        this.openerBalance = commitment.openerBalance;
        this.counterpartyBalance = commitment.counterpartyBalance;
        this.history.push(commitment);
    }
    close() {
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
    dispute() {
        const latest = [...this.history].reverse().find((c) => c.acceptorSig !== null);
        if (!latest) {
            throw new Error('No fully-signed states available for dispute');
        }
        return latest;
    }
    confirmClosed() {
        this.state = ChannelState.Closed;
    }
    static buildOpenBody(channelId, counterparty, deposit, timeoutBlocks = 100) {
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
    static buildCloseBody(channelId, openerBalance, counterpartyBalance, counterpartySig, stateNonce) {
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
    static buildDisputeBody(channelId, openerBalance, counterpartyBalance, otherPartySig, stateNonce, challengePeriod = 100) {
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
exports.Channel = Channel;
//# sourceMappingURL=channel.js.map
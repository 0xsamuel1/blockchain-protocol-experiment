use alloy::consensus::transaction::SignerRecoverable;
use alloy::consensus::{Transaction as TxTrait, TxEnvelope, TxType};
use alloy::eips::Decodable2718;
use alloy::primitives::{Address, B256, Bytes, TxKind, U256};
use primitives::{
    constants::{BLOCK_GAS_LIMIT, CHAIN_ID, GAS_PRICE},
    node::{BlockState, PendingTransaction},
};
use revm::{context_interface::cfg::gas::calculate_initial_tx_gas, primitives::hardfork::SpecId};

/// A signed transaction that has been decoded and checked against the protocol
/// rules, but not yet against chain state.
struct DecodedTransaction {
    hash: B256,
    from: Address,
    to: TxKind,
    input: Bytes,
    value: U256,
    nonce: u64,
    gas: u64,
    gas_price: u128,
    chain_id: u64,
    v: u64,
    r: U256,
    s: U256,
}

/// Decodes a raw signed transaction and applies every rule that can be checked
/// without looking at chain state.
fn decode_and_check(raw: &Bytes) -> Result<DecodedTransaction, anyhow::Error> {
    if raw.is_empty() {
        anyhow::bail!("raw transaction is empty");
    }

    let envelope = TxEnvelope::decode_2718(&mut raw.as_ref())
        .map_err(|e| anyhow::anyhow!("could not RLP-decode the raw transaction: {e}"))?;

    // Breeja predates typed transactions, so it only understands legacy ones.
    // There is no base fee here for an EIP-1559 transaction to bid against.
    if !matches!(envelope.tx_type(), TxType::Legacy) {
        anyhow::bail!(
            "unsupported transaction type {:?}: this node only accepts legacy \
             (pre-EIP-1559) transactions, so sign with --legacy",
            envelope.tx_type()
        );
    }

    // The sender is derived from the signature, never declared.
    let from = envelope
        .recover_signer()
        .map_err(|e| anyhow::anyhow!("invalid signature, could not recover sender: {e}"))?;

    // EIP-155 replay protection: a transaction signed for another chain must not
    // be mineable here. A legacy transaction with no chain id at all predates
    // EIP-155 and is accepted, exactly as on Ethereum.
    let chain_id = match envelope.chain_id() {
        Some(id) if id == CHAIN_ID => id,
        Some(id) => {
            anyhow::bail!("transaction is signed for chain {id}, but this chain is {CHAIN_ID}")
        }
        None => CHAIN_ID,
    };

    let to = envelope.kind();
    let input = envelope.input().clone();

    let gas = envelope.gas_limit();
    if gas > BLOCK_GAS_LIMIT {
        anyhow::bail!("gas limit {gas} exceeds the block gas limit ({BLOCK_GAS_LIMIT})");
    }
    let intrinsic_gas = intrinsic_gas(to, &input);
    if gas < intrinsic_gas {
        anyhow::bail!(
            "gas limit {gas} is below the transaction's intrinsic gas requirement ({intrinsic_gas})"
        );
    }

    let gas_price = envelope.max_fee_per_gas();
    if gas_price < GAS_PRICE {
        anyhow::bail!("gas price {gas_price} is below this chain's fixed price of {GAS_PRICE}");
    }

    let signature = envelope.signature();
    Ok(DecodedTransaction {
        hash: *envelope.tx_hash(),
        from,
        to,
        input,
        value: envelope.value(),
        nonce: envelope.nonce(),
        gas,
        gas_price,
        chain_id,
        v: legacy_v(signature.v(), envelope.chain_id()),
        r: signature.r(),
        s: signature.s(),
    })
}

/// Shanghai intrinsic gas for this legacy call or creation, including calldata
/// zero/non-zero byte costs, creation overhead, and initcode word cost.
fn intrinsic_gas(to: TxKind, input: &Bytes) -> u64 {
    calculate_initial_tx_gas(
        SpecId::SHANGHAI,
        input,
        matches!(to, TxKind::Create),
        0,
        0,
        0,
        None,
    )
    .initial_total_gas()
}

/// Reconstructs the `v` value a legacy transaction reports over JSON-RPC.
///
/// `v` encodes the signature's y-parity, and since EIP-155 it also encodes the
/// chain id, which is how replay protection is folded into the signature itself.
fn legacy_v(y_parity: bool, chain_id: Option<u64>) -> u64 {
    match chain_id {
        Some(id) => y_parity as u64 + 35 + id * 2,
        None => y_parity as u64 + 27,
    }
}

/// Validates a raw transaction against the current chain state and admits it to
/// the mempool, returning its hash.
///
/// The state checks use *pending* values — the nonce and balance including
/// everything already queued — so that a client can fire several transactions
/// back to back without waiting for a block in between.
pub fn admit(state: &mut BlockState, raw: &Bytes) -> Result<B256, anyhow::Error> {
    let tx = decode_and_check(raw)?;

    if state.get_transaction(&tx.hash).is_some() {
        anyhow::bail!("transaction {} has already been mined", tx.hash);
    }
    if state.mempool_contains(&tx.hash) {
        anyhow::bail!("transaction {} is already pending", tx.hash);
    }

    let expected_nonce = state.get_pending_nonce(&tx.from);
    if tx.nonce != expected_nonce {
        anyhow::bail!(
            "nonce too {}: {} expects nonce {expected_nonce}, transaction has {}",
            if tx.nonce < expected_nonce {
                "low"
            } else {
                "high"
            },
            tx.from,
            tx.nonce
        );
    }

    // Ethereum reserves the full gas limit up front and refunds whatever the
    // transaction does not burn. Breeja always burns exactly the intrinsic cost,
    // but it reserves the limit anyway so the accounting matches.
    let gas_reservation = U256::from(tx.gas) * U256::from(tx.gas_price);
    let max_cost = tx
        .value
        .checked_add(gas_reservation)
        .ok_or_else(|| anyhow::anyhow!("transaction maximum cost overflows a U256"))?;
    let available = state.get_pending_balance(&tx.from);
    if available < max_cost {
        anyhow::bail!(
            "insufficient funds: {} has {available} available but the transaction \
             needs up to {max_cost} (value plus gas)",
            tx.from
        );
    }

    state.add_to_mempool(PendingTransaction {
        hash: tx.hash,
        from: tx.from,
        to: tx.to,
        input: tx.input,
        value: tx.value,
        nonce: tx.nonce,
        gas: tx.gas,
        gas_price: tx.gas_price,
        chain_id: tx.chain_id,
        v: tx.v,
        r: tx.r,
        s: tx.s,
    });

    Ok(tx.hash)
}

#[cfg(test)]
mod tests {
    use crate::testing::{
        DEV_PRIVATE_KEY, address_of, one_eth, sign_contract_call, sign_contract_creation,
        sign_transfer,
    };
    use primitives::node::BreejaGenesis;

    use super::*;

    #[test]
    fn a_legacy_v_encodes_parity_and_chain_id() {
        // Pre-EIP-155: 27 or 28.
        assert_eq!(legacy_v(false, None), 27);
        assert_eq!(legacy_v(true, None), 28);
        // EIP-155 on chain 1: 37 or 38.
        assert_eq!(legacy_v(false, Some(1)), 37);
        assert_eq!(legacy_v(true, Some(1)), 38);
    }

    #[test]
    fn garbage_bytes_are_rejected_not_panicked_on() {
        assert!(decode_and_check(&Bytes::new()).is_err());
        assert!(decode_and_check(&Bytes::from_static(&[0xff, 0x00, 0x13])).is_err());
    }

    #[test]
    fn legacy_creation_and_calldata_are_preserved_for_execution() {
        let initcode = Bytes::from_static(&[0x60, 0x00, 0x60, 0x00, 0xfd]);
        let create =
            sign_contract_creation(DEV_PRIVATE_KEY, U256::ZERO, 0, 100_000, initcode.clone())
                .unwrap();
        let decoded_create = decode_and_check(&create).unwrap();
        assert_eq!(decoded_create.to, TxKind::Create);
        assert_eq!(decoded_create.input, initcode);

        let contract = Address::repeat_byte(0x44);
        let calldata = Bytes::from_static(&[0x00, 0x01]);
        let call = sign_contract_call(
            DEV_PRIVATE_KEY,
            contract,
            U256::ZERO,
            0,
            100_000,
            calldata.clone(),
        )
        .unwrap();
        let decoded_call = decode_and_check(&call).unwrap();
        assert_eq!(decoded_call.to, TxKind::Call(contract));
        assert_eq!(decoded_call.input, calldata);
    }

    #[test]
    fn shanghai_intrinsic_gas_counts_zero_nonzero_and_create_input() {
        let empty = Bytes::new();
        let zero = Bytes::from_static(&[0x00]);
        let nonzero = Bytes::from_static(&[0x01]);

        assert_eq!(intrinsic_gas(TxKind::Call(Address::ZERO), &empty), 21_000);
        assert_eq!(intrinsic_gas(TxKind::Call(Address::ZERO), &zero), 21_004);
        assert_eq!(intrinsic_gas(TxKind::Call(Address::ZERO), &nonzero), 21_016);
        assert!(
            intrinsic_gas(TxKind::Create, &zero)
                > intrinsic_gas(TxKind::Call(Address::ZERO), &zero)
        );
    }

    #[test]
    fn transaction_below_intrinsic_gas_or_above_block_limit_is_rejected() {
        let contract = Address::repeat_byte(0x44);
        let too_little = sign_contract_call(
            DEV_PRIVATE_KEY,
            contract,
            U256::ZERO,
            0,
            21_003,
            Bytes::from_static(&[0x00]),
        )
        .unwrap();
        assert!(decode_and_check(&too_little).is_err());

        let too_large = crate::testing::sign_transfer_with(
            DEV_PRIVATE_KEY,
            contract,
            U256::ZERO,
            0,
            GAS_PRICE,
            BLOCK_GAS_LIMIT + 1,
        )
        .unwrap();
        assert!(decode_and_check(&too_large).is_err());
    }

    #[test]
    fn pool_admits_transfers_contract_creation_and_calldata_calls() {
        let sender = address_of(DEV_PRIVATE_KEY).unwrap();
        let mut state = BlockState::new_with_gensis(
            BreejaGenesis {
                accounts: vec![(sender, one_eth() * U256::from(10u64))],
            },
            8,
        );
        let transfer = sign_transfer(DEV_PRIVATE_KEY, Address::ZERO, U256::ZERO, 0).unwrap();
        let create = sign_contract_creation(
            DEV_PRIVATE_KEY,
            U256::ZERO,
            1,
            100_000,
            Bytes::from_static(&[0x60, 0x00]),
        )
        .unwrap();
        let call = sign_contract_call(
            DEV_PRIVATE_KEY,
            Address::repeat_byte(0x44),
            U256::ZERO,
            2,
            100_000,
            Bytes::from_static(&[0x12, 0x34]),
        )
        .unwrap();

        admit(&mut state, &transfer).unwrap();
        admit(&mut state, &create).unwrap();
        admit(&mut state, &call).unwrap();

        assert_eq!(state.mempool_len(), 3);
        assert_eq!(state.get_pending_nonce(&sender), 3);
    }

    #[test]
    fn nonce_and_balance_validation_still_reject_invalid_transactions() {
        let sender = address_of(DEV_PRIVATE_KEY).unwrap();
        let mut state = BlockState::new_with_gensis(
            BreejaGenesis {
                accounts: vec![(sender, one_eth())],
            },
            8,
        );

        let wrong_nonce = sign_transfer(DEV_PRIVATE_KEY, Address::ZERO, U256::ZERO, 1).unwrap();
        assert!(admit(&mut state, &wrong_nonce).is_err());

        let excessive_value = sign_transfer(DEV_PRIVATE_KEY, Address::ZERO, one_eth(), 0).unwrap();
        assert!(admit(&mut state, &excessive_value).is_err());
        assert_eq!(state.mempool_len(), 0);
    }
}

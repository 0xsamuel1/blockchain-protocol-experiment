use std::collections::HashMap;

use alloy::primitives::{Address, B256, Bytes, U256};
use primitives::{
    constants::{BLOCK_GAS_LIMIT, BLOCK_REWARD, CHAIN_ID},
    node::{
        Account, Block, Log, PendingTransaction, Transaction, TransactionReceipt, hash_list_root,
    },
};
use revm::{
    Context, DatabaseCommit, ExecuteEvm, MainBuilder, MainContext,
    context::{BlockEnv, CfgEnv, TxEnv},
    context_interface::result::ExecutionResult,
    primitives::{TxKind, hardfork::SpecId},
};

use crate::evm::BreejaDb;

/// What executing a batch of transactions produced.
pub struct Execution {
    /// Account state after the batch, ready to become the new head state.
    pub accounts: HashMap<Address, Account>,
    /// Content-addressed bytecode after execution.
    pub code_by_hash: HashMap<B256, Bytes>,
    pub transactions: Vec<Transaction>,
    pub receipts: Vec<TransactionReceipt>,
    pub gas_used: u64,
    pub transaction_root: B256,
    pub receipts_root: B256,
    pub state_root: B256,
}

/// Executes one pending legacy transaction through REVM and commits its changes
/// into the owned database snapshot.
pub fn execute_transaction(
    db: &mut BreejaDb,
    tx: &PendingTransaction,
    block: BlockEnv,
) -> Result<ExecutionResult, anyhow::Error> {
    let cfg = CfgEnv::new_with_spec(SpecId::SHANGHAI).with_chain_id(CHAIN_ID);
    let tx_env = TxEnv {
        tx_type: 0,
        caller: tx.from,
        gas_limit: tx.gas,
        gas_price: tx.gas_price,
        kind: tx.to,
        value: tx.value,
        data: tx.input.clone(),
        nonce: tx.nonce,
        chain_id: Some(tx.chain_id),
        ..Default::default()
    };

    let mut evm = Context::mainnet()
        .with_db(&mut *db)
        .with_block(block)
        .with_cfg(cfg)
        .build_mainnet();
    let outcome = evm
        .transact(tx_env)
        .map_err(|error| anyhow::anyhow!("REVM rejected transaction: {error:?}"))?;
    let result = outcome.result;
    let changes = outcome.state;
    drop(evm);

    // REVM finalizes valid reverts and halts with only fee/nonce changes left in
    // its state. Commit that state too: invalid transactions instead return Err.
    db.commit(changes);
    Ok(result)
}

/// Pays the miner for sealing a block.
///
/// This is the only way new ETH enters the supply, and it is what makes mining
/// worth the electricity. Breeja pays Frontier's 5 ETH; Ethereum reduced this
/// twice before dropping it entirely at the merge.
fn pay_block_reward(
    accounts: &mut HashMap<Address, Account>,
    miner: Address,
) -> Result<(), anyhow::Error> {
    accounts.entry(miner).or_default().credit(BLOCK_REWARD)?;
    Ok(())
}

/// Executes a batch of pending transactions, producing the state and receipts the
/// next block will commit to. Transactions REVM rejects as invalid are skipped.
pub fn execute_block(
    mut db: BreejaDb,
    batch: &[PendingTransaction],
    parent: &Block,
    block_number: u64,
    timestamp: u64,
    difficulty: u8,
    miner: Address,
) -> Execution {
    let block_env = BlockEnv {
        number: U256::from(block_number),
        beneficiary: miner,
        timestamp: U256::from(timestamp),
        gas_limit: BLOCK_GAS_LIMIT,
        basefee: 0,
        difficulty: U256::from(difficulty),
        prevrandao: Some(parent.hash),
        ..Default::default()
    };
    let mut transactions = Vec::new();
    let mut receipts = Vec::new();
    let mut gas_used = 0u64;

    for tx in batch {
        // Execute against a throwaway copy so a transaction that would exceed
        // the remaining block gas cannot leak state changes into this candidate.
        let mut candidate_db = db.clone();
        match execute_transaction(&mut candidate_db, tx, block_env.clone()) {
            Ok(result) => {
                let tx_gas = result.tx_gas_used();
                if gas_used.saturating_add(tx_gas) > BLOCK_GAS_LIMIT {
                    continue;
                }
                db = candidate_db;
                let index = transactions.len() as u64;
                gas_used += tx_gas;
                let success = result.is_success();
                let logs = if success {
                    result
                        .logs()
                        .iter()
                        .map(|log| Log {
                            address: log.address,
                            topics: log.data.topics().to_vec(),
                            data: log.data.data.clone(),
                        })
                        .collect()
                } else {
                    // REVM exposes logs produced before a revert in its result;
                    // Ethereum receipts contain no logs for reverted execution.
                    Vec::new()
                };
                let recipient = match tx.to {
                    TxKind::Call(address) => Some(address),
                    TxKind::Create => None,
                };

                transactions.push(Transaction {
                    hash: tx.hash,
                    // The block hash is unknown until the block is sealed, so it
                    // is stamped in afterwards.
                    block_hash: B256::ZERO,
                    block_number,
                    transaction_index: index,
                    chain_id: tx.chain_id,
                    from: tx.from,
                    to: recipient,
                    value: tx.value,
                    nonce: tx.nonce,
                    gas: tx.gas,
                    gas_price: tx.gas_price,
                    input: tx.input.clone(),
                    v: tx.v,
                    r: tx.r,
                    s: tx.s,
                });

                receipts.push(TransactionReceipt {
                    transaction_hash: tx.hash,
                    transaction_index: index,
                    block_hash: B256::ZERO,
                    block_number,
                    from: tx.from,
                    to: recipient,
                    contract_address: result.created_address(),
                    logs,
                    gas_used: tx_gas,
                    cummulative_gas_used: gas_used,
                    effective_gas_price: tx.gas_price,
                    status: u8::from(success),
                });
            }
            Err(error) => {
                tracing::warn!(tx = %tx.hash, %error, "dropping transaction REVM rejected");
            }
        }
    }

    let (mut accounts, code_by_hash) = db.into_parts();
    // The miner's reward is paid after the transactions, so a miner cannot spend
    // it inside the very block that grants it. REVM has already paid tx fees.
    if let Err(error) = pay_block_reward(&mut accounts, miner) {
        tracing::error!(%error, "failed to pay block reward");
    }

    let tx_hashes: Vec<B256> = transactions.iter().map(|tx| tx.hash).collect();
    let receipt_hashes: Vec<B256> = receipts.iter().map(|r| r.transaction_hash).collect();

    Execution {
        transaction_root: hash_list_root(&tx_hashes),
        receipts_root: hash_list_root(&receipt_hashes),
        state_root: primitives::node::state_root_of(&accounts),
        accounts,
        code_by_hash,
        transactions,
        receipts,
        gas_used,
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::Address;
    use primitives::constants::{
        BLOCK_GAS_LIMIT, BLOCK_REWARD, DEV_ADDRESS, DEV_ADDRESS_1, GAS_PRICE, TRANSFER_GAS,
    };
    use revm::context::BlockEnv;

    use super::*;

    fn one_eth() -> U256 {
        U256::from(10u64).pow(U256::from(18u64))
    }

    fn funded(balance: U256) -> BreejaDb {
        let mut db = BreejaDb::default();
        db.accounts.insert(DEV_ADDRESS, Account::new(balance));
        db
    }

    fn transfer(value: U256, nonce: u64) -> PendingTransaction {
        PendingTransaction {
            hash: B256::repeat_byte(0xab),
            from: DEV_ADDRESS,
            to: TxKind::Call(DEV_ADDRESS_1),
            input: Bytes::new(),
            value,
            nonce,
            gas: TRANSFER_GAS,
            gas_price: GAS_PRICE,
            chain_id: CHAIN_ID,
            v: 0,
            r: U256::ZERO,
            s: U256::ZERO,
        }
    }

    fn transfer_block_env(miner: Address) -> BlockEnv {
        BlockEnv {
            number: U256::from(1u64),
            beneficiary: miner,
            timestamp: U256::from(1u64),
            gas_limit: BLOCK_GAS_LIMIT,
            basefee: 0,
            prevrandao: Some(B256::repeat_byte(0x77)),
            ..Default::default()
        }
    }

    fn parent() -> Block {
        Block {
            hash: B256::repeat_byte(0x77),
            ..Default::default()
        }
    }

    #[test]
    fn a_revm_transfer_preserves_plain_transfer_parity() {
        let miner = Address::repeat_byte(0x99);
        let start = one_eth() * U256::from(10u64);
        let fee = U256::from(TRANSFER_GAS) * U256::from(GAS_PRICE);
        let mut db = funded(start);

        let result =
            execute_transaction(&mut db, &transfer(one_eth(), 0), transfer_block_env(miner))
                .unwrap();

        assert!(result.is_success());
        assert_eq!(result.tx_gas_used(), TRANSFER_GAS);
        assert_eq!(result.created_address(), None);
        assert!(result.logs().is_empty());
        assert_eq!(db.accounts[&DEV_ADDRESS].balance, start - one_eth() - fee);
        assert_eq!(db.accounts[&DEV_ADDRESS].nonce, 1);
        assert_eq!(db.accounts[&DEV_ADDRESS_1].balance, one_eth());
        assert_eq!(db.accounts[&miner].balance, fee);

        let total: U256 = db.accounts.values().map(|account| account.balance).sum();
        assert_eq!(total, start);
    }

    #[test]
    fn a_transfer_moves_value_charges_one_fee_and_bumps_the_nonce_once() {
        let miner = Address::repeat_byte(0x99);
        let start = one_eth() * U256::from(10u64);
        let fee = U256::from(TRANSFER_GAS) * U256::from(GAS_PRICE);
        let mut db = funded(start);

        execute_transaction(&mut db, &transfer(one_eth(), 0), transfer_block_env(miner)).unwrap();

        assert_eq!(db.accounts[&DEV_ADDRESS].balance, start - one_eth() - fee);
        assert_eq!(db.accounts[&DEV_ADDRESS].nonce, 1);
        assert_eq!(db.accounts[&DEV_ADDRESS_1].balance, one_eth());
        assert_eq!(db.accounts[&miner].balance, fee);
    }

    #[test]
    fn no_eth_is_created_or_destroyed_by_a_revm_transfer() {
        let start = one_eth() * U256::from(10u64);
        let mut db = funded(start);

        execute_transaction(
            &mut db,
            &transfer(one_eth(), 0),
            transfer_block_env(Address::repeat_byte(0x99)),
        )
        .unwrap();

        let total: U256 = db.accounts.values().map(|account| account.balance).sum();
        assert_eq!(total, start);
    }

    #[test]
    fn a_transfer_the_sender_cannot_afford_is_rejected() {
        let mut db = funded(one_eth());
        assert!(
            execute_transaction(
                &mut db,
                &transfer(one_eth(), 0),
                transfer_block_env(Address::repeat_byte(0x99)),
            )
            .is_err()
        );
    }

    #[test]
    fn a_transfer_with_the_wrong_nonce_is_rejected() {
        let mut db = funded(one_eth() * U256::from(10u64));
        assert!(
            execute_transaction(
                &mut db,
                &transfer(one_eth(), 7),
                transfer_block_env(Address::repeat_byte(0x99)),
            )
            .is_err()
        );
    }

    #[test]
    fn executing_a_block_pays_the_reward_and_records_successful_receipts() {
        let miner = Address::repeat_byte(0x99);
        let start = one_eth() * U256::from(10u64);
        let fee = U256::from(TRANSFER_GAS) * U256::from(GAS_PRICE);
        let execution = execute_block(
            funded(start),
            &[transfer(one_eth(), 0)],
            &parent(),
            1,
            2,
            4,
            miner,
        );

        assert_eq!(execution.transactions.len(), 1);
        assert_eq!(execution.receipts.len(), 1);
        assert_eq!(execution.receipts[0].status, 1);
        assert_eq!(execution.receipts[0].gas_used, TRANSFER_GAS);
        assert_eq!(execution.receipts[0].cummulative_gas_used, TRANSFER_GAS);
        assert_eq!(execution.gas_used, TRANSFER_GAS);
        assert_eq!(execution.accounts[&miner].balance, BLOCK_REWARD + fee);

        // Non-empty roots, because this block committed to a transaction.
        assert_ne!(execution.transaction_root, B256::ZERO);
        assert_ne!(execution.state_root, B256::ZERO);
    }

    #[test]
    fn an_empty_block_still_pays_the_miner_but_commits_to_no_transactions() {
        let miner = Address::repeat_byte(0x99);
        let execution = execute_block(funded(one_eth()), &[], &parent(), 1, 2, 4, miner);

        assert!(execution.transactions.is_empty());
        assert_eq!(execution.gas_used, 0);
        assert_eq!(execution.transaction_root, B256::ZERO);
        assert_eq!(execution.accounts[&miner].balance, BLOCK_REWARD);
    }

    #[test]
    fn a_transaction_that_no_longer_applies_is_skipped_not_mined() {
        let start = one_eth() * U256::from(10u64);
        // Two transactions claiming the same nonce: only the first can apply.
        let batch = vec![transfer(one_eth(), 0), transfer(one_eth(), 0)];

        let execution = execute_block(
            funded(start),
            &batch,
            &parent(),
            1,
            2,
            4,
            Address::repeat_byte(0x99),
        );
        assert_eq!(execution.transactions.len(), 1);
    }

    #[test]
    fn a_batch_never_commits_or_counts_transactions_over_the_block_gas_limit() {
        let miner = Address::repeat_byte(0x99);
        let contract = Address::repeat_byte(0x88);
        let sender_balance = U256::from(BLOCK_GAS_LIMIT) * U256::from(2u64);
        let loop_code = revm::state::Bytecode::new_raw(Bytes::from_static(&[
            0x5b, // JUMPDEST
            0x60, 0x00, // PUSH1 0
            0x56, // JUMP
        ]));
        let code_hash = loop_code.hash_slow();
        let mut db = funded(sender_balance);
        db.accounts.insert(
            contract,
            Account {
                code_hash,
                ..Default::default()
            },
        );
        db.code_by_hash.insert(code_hash, loop_code);

        let gas = BLOCK_GAS_LIMIT / 2 + 100_000;
        let make_call = |nonce, hash_byte| PendingTransaction {
            hash: B256::repeat_byte(hash_byte),
            from: DEV_ADDRESS,
            to: TxKind::Call(contract),
            input: Bytes::new(),
            value: U256::ZERO,
            nonce,
            gas,
            gas_price: 1,
            chain_id: CHAIN_ID,
            v: 0,
            r: U256::ZERO,
            s: U256::ZERO,
        };
        let execution = execute_block(
            db,
            &[make_call(0, 0xaa), make_call(1, 0xbb)],
            &parent(),
            1,
            2,
            4,
            miner,
        );

        assert_eq!(execution.transactions.len(), 1);
        assert_eq!(execution.receipts[0].status, 0);
        assert_eq!(execution.gas_used, gas);
        assert!(execution.gas_used <= BLOCK_GAS_LIMIT);
        assert_eq!(execution.accounts[&DEV_ADDRESS].nonce, 1);
    }
}

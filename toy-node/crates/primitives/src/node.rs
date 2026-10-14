use std::collections::HashMap;

use alloy::primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_rlp::{Encodable, RlpEncodable};
use serde::{Deserialize, Serialize};

use crate::constants::{
    BLOCK_GAS_LIMIT, EXTRA_DATA, GAS_PRICE, MAX_DIFFICULTY_LEADING_ZEROS,
    MIN_DIFFICULTY_LEADING_ZEROS, TRANSFER_GAS,
};

/// Account state used by Breeja, including directly stored contract storage.
/// Contract bytecode is kept in `BlockState` and referenced by `code_hash`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, RlpEncodable)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub nonce: u64,
    pub balance: U256,
    pub storage_root: B256,
    pub code_hash: B256,
    /// Contract storage values keyed by their 256-bit slot.
    ///
    /// This teaching node stores the slots directly. `storage_root` is a
    /// deterministic hash of these entries, not an Ethereum storage trie root.
    #[serde(default)]
    #[rlp(skip)]
    pub storage: HashMap<U256, U256>,
}

/// A sealed block.
///
/// Everything above `hash` is the *header*: the bytes that are hashed to produce
/// `hash`. `nonce` and `hash` are the proof of work — the miner searches for a
/// `nonce` that makes `hash` small enough to satisfy `difficulty`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Block {
    pub number: u64,
    pub parent_hash: B256,
    pub state_root: B256,
    pub transaction_root: B256,
    pub receipts_root: B256,
    /// Number of leading zero bits `hash` must have for this block to be valid.
    pub difficulty: u8,
    /// Sum of every ancestor's difficulty plus this block's. The heaviest chain
    /// wins, which is what makes proof of work a consensus rule and not just a
    /// puzzle.
    pub total_difficulty: u64,
    pub timestamp: u64,
    pub gas_limit: u64,
    pub gas_used: u64,
    pub miner: Address,
    pub extra_data: Bytes,
    /// The proof-of-work solution.
    pub nonce: u64,
    /// `keccak256(rlp(header))`, which is only valid once `nonce` has been found.
    pub hash: B256,
    /// Hashes of the transactions this block executed, in execution order.
    pub transactions: Vec<B256>,
}

/// The part of a [`Block`] that the proof of work commits to.
///
/// This is a separate struct so that the hashed bytes are unambiguous: RLP
/// encode exactly these fields, in exactly this order, and hash the result.
/// Mining is the search for a `nonce` that makes that hash small enough.
#[derive(Debug, Clone, RlpEncodable)]
pub struct BlockHeader {
    pub number: u64,
    pub parent_hash: B256,
    pub state_root: B256,
    pub transaction_root: B256,
    pub receipts_root: B256,
    pub difficulty: u8,
    pub timestamp: u64,
    pub gas_limit: u64,
    pub gas_used: u64,
    pub miner: Address,
    pub extra_data: Bytes,
    pub nonce: u64,
}

/// A transaction that has been included in a block.
///
/// `input` is always empty: Breeja rejects anything with calldata, because it
/// has no EVM to run it with.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transaction {
    pub hash: B256,
    pub block_hash: B256,
    pub block_number: u64,
    pub transaction_index: u64,
    pub chain_id: u64,
    pub from: Address,
    pub to: Address,
    pub value: U256,
    pub nonce: u64,
    pub gas: u64,
    pub gas_price: u128,
    pub input: Bytes,
    pub v: u64,
    pub r: U256,
    pub s: U256,
}

/// The receipt proving what a transaction did.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionReceipt {
    pub transaction_hash: B256,
    pub transaction_index: u64,
    pub block_hash: B256,
    pub block_number: u64,
    pub from: Address,
    pub to: Address,
    pub gas_used: u64,
    pub cummulative_gas_used: u64,
    pub effective_gas_price: u128,
    /// `1` for success. Breeja only ever includes transactions it has already
    /// validated, so a receipt is never a failure.
    pub status: u8,
}

/// A transaction that has been accepted into the mempool but not yet mined.
#[derive(Debug, Clone)]
pub struct PendingTransaction {
    pub hash: B256,
    pub from: Address,
    pub to: Address,
    pub value: U256,
    pub nonce: u64,
    pub gas: u64,
    pub gas_price: u128,
    pub chain_id: u64,
    pub v: u64,
    pub r: U256,
    pub s: U256,
}

/// The accounts a chain starts with, before any block has been mined.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BreejaGenesis {
    pub accounts: Vec<(Address, U256)>,
}

/// Everything the node knows: the current account state, the chain of blocks,
/// the transactions and receipts in them, and the mempool of transactions
/// waiting to be mined.
#[derive(Debug, Clone, Default)]
pub struct BlockState {
    /// Account state as of the chain head.
    accounts: HashMap<Address, Account>,
    /// Bytecode is stored once and accounts refer to it by `code_hash`.
    code_by_hash: HashMap<B256, Bytes>,
    /// Blocks by number; `blocks[n]` is the block with `number == n`.
    blocks: Vec<Block>,
    /// Account state as it was *after* each block, so historical balance and
    /// nonce queries can be answered honestly. A real node would reconstruct
    /// this from a trie; Breeja keeps whole snapshots, which is only affordable
    /// because a teaching chain has a handful of accounts.
    state_history: Vec<HashMap<Address, Account>>,
    /// Block number for a given block hash.
    block_index: HashMap<B256, u64>,
    transactions: HashMap<B256, Transaction>,
    receipts: HashMap<B256, TransactionReceipt>,
    /// Validated transactions waiting to be included in a block, oldest first.
    mempool: Vec<PendingTransaction>,
}

impl Account {
    pub fn new(balance: U256) -> Self {
        Self {
            balance,
            ..Default::default()
        }
    }

    /// Consumes this account's current nonce and advances it, the way sending a
    /// transaction does.
    pub fn increase_nonce(&mut self) -> u64 {
        let used_nonce = self.nonce;
        self.nonce += 1;
        used_nonce
    }

    /// Debits `amount`, failing if the account cannot cover it.
    pub fn debit(&mut self, amount: U256) -> Result<U256, anyhow::Error> {
        match self.balance.checked_sub(amount) {
            Some(remaining) => {
                self.balance = remaining;
                Ok(self.balance)
            }
            None => anyhow::bail!(
                "insufficient balance: have {}, need {}",
                self.balance,
                amount
            ),
        }
    }

    /// Credits `amount`.
    pub fn credit(&mut self, amount: U256) -> Result<U256, anyhow::Error> {
        match self.balance.checked_add(amount) {
            Some(total) => {
                self.balance = total;
                Ok(self.balance)
            }
            None => anyhow::bail!("balance overflow crediting {}", amount),
        }
    }

    /// Hashes the current storage map in sorted slot order.
    pub fn storage_root(&self) -> B256 {
        let mut slots: Vec<_> = self.storage.iter().collect();
        slots.sort_by_key(|(slot, _)| **slot);

        let mut encoded = Vec::new();
        for (slot, value) in slots {
            slot.encode(&mut encoded);
            value.encode(&mut encoded);
        }
        if encoded.is_empty() {
            B256::ZERO
        } else {
            keccak256(encoded)
        }
    }
}

impl BlockHeader {
    /// `keccak256(rlp(header))` — the block hash, and the value proof of work
    /// has to drive below the difficulty target.
    pub fn hash(&self) -> B256 {
        let mut buf = Vec::new();
        self.encode(&mut buf);
        keccak256(buf)
    }
}

impl Block {
    /// Rebuilds the header that was hashed to produce [`Block::hash`], so any
    /// node can re-derive the hash and check the proof of work for itself.
    pub fn header(&self) -> BlockHeader {
        BlockHeader {
            number: self.number,
            parent_hash: self.parent_hash,
            state_root: self.state_root,
            transaction_root: self.transaction_root,
            receipts_root: self.receipts_root,
            difficulty: self.difficulty,
            timestamp: self.timestamp,
            gas_limit: self.gas_limit,
            gas_used: self.gas_used,
            miner: self.miner,
            extra_data: self.extra_data.clone(),
            nonce: self.nonce,
        }
    }
}

/// Counts the leading zero bits of a hash. A hash with more leading zeros is a
/// smaller number, so "at least N leading zero bits" is the same rule as
/// "hash below 2^(256-N)" — just easier to read in a log line.
///
/// The return type is wider than the `u8` a difficulty is stored in because an
/// all-zero hash has 256 leading zero bits, which a `u8` cannot hold.
pub fn leading_zero_bits(hash: &B256) -> u16 {
    let mut bits = 0u16;
    for byte in hash.0.iter() {
        if *byte == 0 {
            bits += 8;
        } else {
            bits += byte.leading_zeros() as u16;
            break;
        }
    }
    bits
}

/// Whether `hash` satisfies a difficulty of `difficulty` leading zero bits.
pub fn meets_difficulty(hash: &B256, difficulty: u8) -> bool {
    leading_zero_bits(hash) >= difficulty as u16
}

/// Picks the difficulty for the next block.
///
/// This is Breeja's difficulty adjustment algorithm: if blocks are coming in too
/// fast, demand one more zero bit; if too slow, demand one fewer.
///
/// The catch is that one bit is a *doubling*. A difficulty of 22 bits and one of
/// 23 bits can easily straddle the target with neither landing on it, so adjusting
/// whenever the time is merely off would oscillate forever. Breeja therefore only
/// moves when the measured time is off by more than the 2x a single bit can
/// correct, which leaves block times inside a factor-of-two band around the
/// target.
///
/// This is exactly why real Ethereum does not count zero bits. It adjusts a
/// 256-bit difficulty *number* in steps of about 1/2048, so it can track a target
/// block time closely instead of bracketing it.
pub fn retarget_difficulty(current: u8, actual_ms: u64, target_ms: u64) -> u8 {
    let next = if actual_ms.saturating_mul(2) < target_ms {
        current.saturating_add(1)
    } else if actual_ms > target_ms.saturating_mul(2) {
        current.saturating_sub(1)
    } else {
        current
    };
    next.clamp(MIN_DIFFICULTY_LEADING_ZEROS, MAX_DIFFICULTY_LEADING_ZEROS)
}

/// Commits to a whole account set in a single hash.
///
/// Addresses are sorted first so that two nodes holding the same accounts always
/// produce the same root, regardless of `HashMap` iteration order. Ethereum gets
/// that ordering for free from its trie.
pub fn state_root_of(accounts: &HashMap<Address, Account>) -> B256 {
    let mut entries: Vec<(&Address, &Account)> = accounts.iter().collect();
    entries.sort_by_key(|(address, _)| **address);

    let mut buf = Vec::new();
    for (address, account) in entries {
        buf.extend_from_slice(address.as_slice());
        let mut account = account.clone();
        account.storage_root = account.storage_root();
        account.encode(&mut buf);
    }
    keccak256(buf)
}

/// Hashes an ordered list of hashes into a single root.
///
/// Ethereum uses a Merkle-Patricia trie here, which lets a light client prove
/// that one item is in the root without downloading the rest. Breeja just hashes
/// the concatenation: it still commits to the full contents, but it cannot
/// produce those proofs.
pub fn hash_list_root(items: &[B256]) -> B256 {
    if items.is_empty() {
        return B256::ZERO;
    }
    let mut buf = Vec::with_capacity(items.len() * 32);
    for item in items {
        buf.extend_from_slice(item.as_slice());
    }
    keccak256(buf)
}

impl BlockState {
    /// Builds the starting state from a genesis definition and seals the genesis
    /// block over it.
    pub fn new_with_gensis(breeja_gen: BreejaGenesis, difficulty: u8) -> Self {
        let mut accounts = HashMap::new();
        for (address, balance) in breeja_gen.accounts {
            accounts.insert(address, Account::new(balance));
        }

        let mut state = Self {
            accounts,
            ..Default::default()
        };

        // The genesis block is the one block nobody mines: it has no parent, no
        // transactions, and its proof of work is accepted by definition.
        let header = BlockHeader {
            number: 0,
            parent_hash: B256::ZERO,
            state_root: state_root_of(&state.accounts),
            transaction_root: B256::ZERO,
            receipts_root: B256::ZERO,
            difficulty,
            timestamp: 0,
            gas_limit: BLOCK_GAS_LIMIT,
            gas_used: 0,
            miner: Address::ZERO,
            extra_data: Bytes::from_static(EXTRA_DATA.as_bytes()),
            nonce: 0,
        };
        let genesis = Block {
            hash: header.hash(),
            total_difficulty: difficulty as u64,
            transactions: Vec::new(),
            number: header.number,
            parent_hash: header.parent_hash,
            state_root: header.state_root,
            transaction_root: header.transaction_root,
            receipts_root: header.receipts_root,
            difficulty: header.difficulty,
            timestamp: header.timestamp,
            gas_limit: header.gas_limit,
            gas_used: header.gas_used,
            miner: header.miner,
            extra_data: header.extra_data.clone(),
            nonce: header.nonce,
        };

        state.block_index.insert(genesis.hash, 0);
        state.blocks.push(genesis);
        state.state_history.push(state.accounts.clone());
        state
    }

    /// Number of the chain head. Genesis is block 0, so a fresh chain is at 0.
    pub fn get_block_number(&self) -> u64 {
        self.blocks.len() as u64 - 1
    }

    pub fn head(&self) -> &Block {
        self.blocks
            .last()
            .expect("chain always contains the genesis block")
    }

    pub fn get_block(&self, number: u64) -> Option<&Block> {
        self.blocks.get(number as usize)
    }

    pub fn get_block_by_hash(&self, hash: &B256) -> Option<&Block> {
        self.block_index
            .get(hash)
            .and_then(|number| self.get_block(*number))
    }

    pub fn get_transaction(&self, hash: &B256) -> Option<&Transaction> {
        self.transactions.get(hash)
    }

    pub fn get_receipt(&self, hash: &B256) -> Option<&TransactionReceipt> {
        self.receipts.get(hash)
    }

    // account queries

    /// Account at the chain head. An address nobody has ever funded reads as an
    /// empty account rather than an error, exactly as on Ethereum.
    pub fn get_account(&self, address: &Address) -> Account {
        self.accounts.get(address).cloned().unwrap_or_default()
    }

    /// Returns bytecode associated with an account. An unknown address returns
    /// `None`; a known account with no code returns empty bytes.
    pub fn get_code(&self, address: &Address) -> Option<Bytes> {
        let account = self.accounts.get(address)?;
        if account.code_hash == B256::ZERO {
            return Some(Bytes::new());
        }
        self.get_code_by_hash(&account.code_hash)
    }

    /// Returns bytecode by its content hash. The zero hash and Ethereum's empty
    /// code hash both represent empty code; an unknown non-empty hash returns
    /// `None`.
    pub fn get_code_by_hash(&self, code_hash: &B256) -> Option<Bytes> {
        if *code_hash == B256::ZERO || *code_hash == keccak256([]) {
            return Some(Bytes::new());
        }
        self.code_by_hash.get(code_hash).cloned()
    }

    /// Stores bytecode once by content hash and associates it with `address`.
    pub fn set_code(&mut self, address: Address, code: Bytes) -> B256 {
        let code_hash = keccak256(&code);
        self.code_by_hash.insert(code_hash, code);
        self.accounts.entry(address).or_default().code_hash = code_hash;
        code_hash
    }

    /// Returns a storage slot's value, or zero for an unknown account or slot.
    pub fn get_storage(&self, address: &Address, slot: &U256) -> U256 {
        self.accounts
            .get(address)
            .and_then(|account| account.storage.get(slot).copied())
            .unwrap_or_default()
    }

    /// Account as it was after block `number`, or `None` if that block does not
    /// exist.
    pub fn get_account_at(&self, address: &Address, number: u64) -> Option<Account> {
        self.state_history
            .get(number as usize)
            .map(|snapshot| snapshot.get(address).cloned().unwrap_or_default())
    }

    /// The nonce the sender's *next* transaction must carry: the mined nonce plus
    /// anything already sitting in the mempool for that sender.
    pub fn get_pending_nonce(&self, address: &Address) -> u64 {
        let mined = self.get_account(address).nonce;
        let queued = self.mempool.iter().filter(|tx| &tx.from == address).count() as u64;
        mined + queued
    }

    /// Balance minus everything the mempool has already committed to spending,
    /// so two transactions cannot each be validated against the same ETH.
    pub fn get_pending_balance(&self, address: &Address) -> U256 {
        let balance = self.get_account(address).balance;
        let committed: U256 = self
            .mempool
            .iter()
            .filter(|tx| &tx.from == address)
            .map(|tx| tx.value + U256::from(tx.gas) * U256::from(tx.gas_price))
            .fold(U256::ZERO, |acc, cost| acc + cost);
        balance.saturating_sub(committed)
    }

    /// Commits to the account set at the chain head in a single hash.
    pub fn state_root(&self) -> B256 {
        state_root_of(&self.accounts)
    }

    /// A copy of the account state at the head, for the miner to execute against
    /// without holding the state lock.
    pub fn accounts_snapshot(&self) -> HashMap<Address, Account> {
        self.accounts.clone()
    }

    /// A copy of the content-addressed bytecode cache for EVM execution.
    pub fn code_snapshot(&self) -> HashMap<B256, Bytes> {
        self.code_by_hash.clone()
    }

    /// Block hashes indexed by block number for REVM's `BLOCKHASH` lookup.
    pub fn block_hashes_snapshot(&self) -> HashMap<u64, B256> {
        self.blocks
            .iter()
            .map(|block| (block.number, block.hash))
            .collect()
    }

    // mempool

    pub fn add_to_mempool(&mut self, tx: PendingTransaction) {
        self.mempool.push(tx);
    }

    pub fn mempool_len(&self) -> usize {
        self.mempool.len()
    }

    /// Whether this exact transaction is already queued, so resubmitting it is
    /// not counted twice.
    pub fn mempool_contains(&self, hash: &B256) -> bool {
        self.mempool.iter().any(|tx| &tx.hash == hash)
    }

    /// The transactions the next block would include: as many as fit in one
    /// block's gas limit, oldest first.
    ///
    /// This does not remove them. The miner may spend a long time failing to seal
    /// a block, and a transaction must stay pending until it is actually mined.
    pub fn peek_mempool_batch(&self) -> Vec<PendingTransaction> {
        let max_txs = (BLOCK_GAS_LIMIT / TRANSFER_GAS) as usize;
        self.mempool.iter().take(max_txs).cloned().collect()
    }

    // block production

    /// Installs a sealed block as the new chain head.
    ///
    /// `accounts` is the state the miner produced by executing `transactions`, and
    /// it replaces the head state wholesale. This is safe because the miner is
    /// the only writer of account state; the JSON-RPC server only ever appends to
    /// the mempool.
    pub fn commit_block(
        &mut self,
        accounts: HashMap<Address, Account>,
        code_by_hash: HashMap<B256, Bytes>,
        block: Block,
        transactions: Vec<Transaction>,
        receipts: Vec<TransactionReceipt>,
    ) {
        let mined: Vec<B256> = transactions.iter().map(|tx| tx.hash).collect();
        self.mempool
            .retain(|pending| !mined.contains(&pending.hash));

        self.accounts = accounts;
        self.code_by_hash.extend(code_by_hash);
        self.block_index.insert(block.hash, block.number);
        for tx in transactions {
            self.transactions.insert(tx.hash, tx);
        }
        for receipt in receipts {
            self.receipts.insert(receipt.transaction_hash, receipt);
        }
        self.blocks.push(block);
        self.state_history.push(self.accounts.clone());
    }

    /// Default gas price for a node with no fee market: a constant.
    pub fn gas_price(&self) -> u128 {
        GAS_PRICE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leading_zero_bits_counts_what_it_says() {
        assert_eq!(leading_zero_bits(&B256::ZERO), 256);
        assert_eq!(leading_zero_bits(&B256::repeat_byte(0xff)), 0);

        // 0x00 0x0f ... = 8 zero bits, then 0b00001111 contributes 4 more.
        let mut bytes = [0u8; 32];
        bytes[1] = 0x0f;
        assert_eq!(leading_zero_bits(&B256::new(bytes)), 12);
    }

    #[test]
    fn difficulty_is_only_retargeted_when_a_whole_bit_is_warranted() {
        // Far too fast (more than 2x) - demand another bit.
        assert_eq!(retarget_difficulty(20, 100, 2_000), 21);
        // Far too slow (more than 2x) - give a bit back.
        assert_eq!(retarget_difficulty(20, 5_000, 2_000), 19);
        // Inside the band a single bit can resolve - leave it alone, so the
        // difficulty settles instead of oscillating.
        assert_eq!(retarget_difficulty(20, 1_500, 2_000), 20);
        assert_eq!(retarget_difficulty(20, 2_000, 2_000), 20);
        assert_eq!(retarget_difficulty(20, 3_900, 2_000), 20);
    }

    #[test]
    fn difficulty_stays_inside_its_clamps() {
        assert_eq!(
            retarget_difficulty(MIN_DIFFICULTY_LEADING_ZEROS, u64::MAX, 1),
            MIN_DIFFICULTY_LEADING_ZEROS
        );
        assert_eq!(
            retarget_difficulty(MAX_DIFFICULTY_LEADING_ZEROS, 0, u64::MAX),
            MAX_DIFFICULTY_LEADING_ZEROS
        );
    }

    #[test]
    fn a_fresh_chain_has_a_genesis_block_and_funded_accounts() {
        let address = Address::repeat_byte(0x11);
        let genesis = BreejaGenesis {
            accounts: vec![(address, U256::from(1_000u64))],
        };
        let state = BlockState::new_with_gensis(genesis, 8);

        assert_eq!(state.get_block_number(), 0);
        assert_eq!(state.head().number, 0);
        assert_eq!(state.head().parent_hash, B256::ZERO);
        assert_eq!(state.get_account(&address).balance, U256::from(1_000u64));
        assert_eq!(state.get_account(&address).nonce, 0);
        // An address nobody funded reads as empty, not missing.
        assert_eq!(state.get_account(&Address::ZERO).balance, U256::ZERO);
    }

    #[test]
    fn the_genesis_block_is_reachable_by_hash_and_by_number() {
        let state = BlockState::new_with_gensis(BreejaGenesis::default(), 8);
        let hash = state.head().hash;
        assert_eq!(state.get_block_by_hash(&hash).unwrap().number, 0);
        assert_eq!(state.get_block(0).unwrap().hash, hash);
        assert!(state.get_block(1).is_none());
    }

    #[test]
    fn the_state_root_commits_to_balances_and_ignores_map_ordering() {
        let a = Address::repeat_byte(0x01);
        let b = Address::repeat_byte(0x02);

        let mut one = HashMap::new();
        one.insert(a, Account::new(U256::from(1u64)));
        one.insert(b, Account::new(U256::from(2u64)));

        // Same accounts inserted in the opposite order must hash identically.
        let mut two = HashMap::new();
        two.insert(b, Account::new(U256::from(2u64)));
        two.insert(a, Account::new(U256::from(1u64)));

        assert_eq!(state_root_of(&one), state_root_of(&two));

        // Changing a single balance must change the root.
        let mut three = one.clone();
        three.insert(a, Account::new(U256::from(99u64)));
        assert_ne!(state_root_of(&one), state_root_of(&three));
    }

    #[test]
    fn the_state_root_commits_to_storage_slots() {
        let address = Address::repeat_byte(0x03);
        let mut empty = HashMap::new();
        empty.insert(address, Account::default());

        let mut with_storage = empty.clone();
        with_storage
            .get_mut(&address)
            .unwrap()
            .storage
            .insert(U256::from(4), U256::from(9));

        assert_ne!(state_root_of(&empty), state_root_of(&with_storage));
    }

    #[test]
    fn code_and_storage_accessors_have_deliberate_missing_value_semantics() {
        let address = Address::repeat_byte(0x04);
        let missing_address = Address::repeat_byte(0x05);
        let mut state = BlockState::new_with_gensis(
            BreejaGenesis {
                accounts: vec![(address, U256::from(1))],
            },
            8,
        );

        assert_eq!(state.get_code(&address), Some(Bytes::new()));
        assert_eq!(state.get_code(&missing_address), None);
        assert_eq!(state.get_storage(&missing_address, &U256::ZERO), U256::ZERO);

        let code = Bytes::from_static(&[0x60, 0x2a, 0x00]);
        let code_hash = state.set_code(address, code.clone());
        assert_eq!(state.get_code(&address), Some(code.clone()));
        assert_eq!(state.get_code_by_hash(&code_hash), Some(code));
        assert_eq!(state.get_code_by_hash(&B256::repeat_byte(0xff)), None);
    }

    #[test]
    fn pending_nonce_and_balance_account_for_what_is_already_queued() {
        let sender = Address::repeat_byte(0x11);
        let one_eth = U256::from(10u64).pow(U256::from(18));
        let genesis = BreejaGenesis {
            accounts: vec![(sender, one_eth * U256::from(10u64))],
        };
        let mut state = BlockState::new_with_gensis(genesis, 8);

        assert_eq!(state.get_pending_nonce(&sender), 0);

        state.add_to_mempool(PendingTransaction {
            hash: B256::repeat_byte(0xaa),
            from: sender,
            to: Address::repeat_byte(0x22),
            value: one_eth,
            nonce: 0,
            gas: TRANSFER_GAS,
            gas_price: GAS_PRICE,
            chain_id: 1,
            v: 0,
            r: U256::ZERO,
            s: U256::ZERO,
        });

        // The next transaction must use nonce 1, and may only spend what the
        // queued one leaves behind.
        assert_eq!(state.get_pending_nonce(&sender), 1);
        assert!(state.get_pending_balance(&sender) < one_eth * U256::from(9u64));
        assert_eq!(state.mempool_len(), 1);
        assert!(state.mempool_contains(&B256::repeat_byte(0xaa)));
    }
}

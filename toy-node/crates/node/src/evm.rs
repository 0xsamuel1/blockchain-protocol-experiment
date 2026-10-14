//! Snapshot-backed adapter between Breeja state and REVM's database traits.
//!
//! REVM needs mutable access to its database while it executes. `BreejaDb` owns
//! copies of the account, code, and block-hash data so execution never holds or
//! mutates the live `BlockState` behind the node's shared lock.

use std::{collections::HashMap, convert::Infallible};

use alloy::primitives::{Address, B256, Bytes};
use primitives::node::{Account as BreejaAccount, BlockState};
use revm::{
    Database, DatabaseCommit,
    primitives::{AddressMap, StorageKey, StorageValue, U256},
    state::{Account as RevmAccount, AccountInfo, Bytecode},
};

/// An owned state snapshot that REVM can read during execution and commit into.
#[derive(Debug, Clone, Default)]
pub struct BreejaDb {
    /// Accounts in this snapshot, including their persistent storage slots.
    pub accounts: HashMap<Address, BreejaAccount>,
    /// Content-addressed bytecode. Accounts refer to these entries by code hash.
    pub code_by_hash: HashMap<B256, Bytecode>,
    /// Known block hashes for the EVM `BLOCKHASH` opcode.
    pub block_hashes: HashMap<u64, B256>,
}

impl BreejaDb {
    /// Takes an owned snapshot of the state needed by REVM.
    pub fn from_state(state: &BlockState) -> Self {
        let code_by_hash = state
            .code_snapshot()
            .into_iter()
            .map(|(hash, code)| (hash, Bytecode::new_raw(code)))
            .collect();

        Self {
            accounts: state.accounts_snapshot(),
            code_by_hash,
            block_hashes: state.block_hashes_snapshot(),
        }
    }

    /// Returns a clone of the resulting account snapshot and bytecode cache.
    /// These can be installed into chain state by a later execution milestone.
    pub fn into_parts(self) -> (HashMap<Address, BreejaAccount>, HashMap<B256, Bytes>) {
        let code_by_hash = self
            .code_by_hash
            .into_iter()
            .map(|(hash, code)| (hash, code.original_bytes()))
            .collect();
        (self.accounts, code_by_hash)
    }
}

impl Database for BreejaDb {
    type Error = Infallible;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let Some(account) = self.accounts.get(&address) else {
            // `None` means the account does not exist; this is REVM's intended
            // representation for an unknown address (as opposed to an empty EOA).
            return Ok(None);
        };

        let empty_code = Bytecode::default();
        let code_hash = if account.code_hash == B256::ZERO {
            empty_code.hash_slow()
        } else {
            account.code_hash
        };
        let code = if code_hash == empty_code.hash_slow() {
            Some(empty_code)
        } else {
            self.code_by_hash.get(&code_hash).cloned()
        };

        Ok(Some(AccountInfo {
            balance: account.balance,
            nonce: account.nonce,
            code_hash,
            code,
            ..Default::default()
        }))
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        Ok(self
            .code_by_hash
            .get(&code_hash)
            .cloned()
            .unwrap_or_default())
    }

    fn storage(
        &mut self,
        address: Address,
        index: StorageKey,
    ) -> Result<StorageValue, Self::Error> {
        Ok(self
            .accounts
            .get(&address)
            .and_then(|account| account.storage.get(&index).copied())
            .unwrap_or(U256::ZERO))
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        Ok(self
            .block_hashes
            .get(&number)
            .copied()
            .unwrap_or(B256::ZERO))
    }
}

impl DatabaseCommit for BreejaDb {
    fn commit(&mut self, changes: AddressMap<RevmAccount>) {
        for (address, changed) in changes {
            // Self-destruction is a deletion transition even if the account was
            // not otherwise marked touched.
            if changed.is_selfdestructed() {
                self.accounts.remove(&address);
                continue;
            }
            if !changed.is_touched() {
                continue;
            }

            let account = self.accounts.entry(address).or_default();
            if changed.is_created() {
                // A newly created account starts with empty storage, even if an
                // older account at this address existed in the snapshot.
                account.storage.clear();
            }

            account.balance = changed.info.balance;
            account.nonce = changed.info.nonce;

            if let Some(code) = changed.info.code {
                account.code_hash = code.hash_slow();
                if !code.is_empty() {
                    self.code_by_hash.insert(account.code_hash, code);
                }
            } else {
                account.code_hash = changed.info.code_hash;
            }

            for (slot, value) in changed.storage {
                if value.present_value.is_zero() {
                    account.storage.remove(&slot);
                } else {
                    account.storage.insert(slot, value.present_value);
                }
            }
            account.storage_root = account.storage_root();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::{
        DatabaseCommit,
        primitives::AddressMap,
        state::{EvmStorageSlot, TransactionId},
    };

    #[test]
    fn basic_returns_funded_account_balance_and_nonce() {
        let address = Address::repeat_byte(0x11);
        let mut db = BreejaDb::default();
        db.accounts.insert(
            address,
            BreejaAccount {
                nonce: 7,
                balance: U256::from(1234),
                ..Default::default()
            },
        );

        let info = Database::basic(&mut db, address).unwrap().unwrap();
        assert_eq!(info.balance, U256::from(1234));
        assert_eq!(info.nonce, 7);
    }

    #[test]
    fn basic_returns_none_for_an_unknown_account() {
        let mut db = BreejaDb::default();

        // A missing account is represented as `None`, preserving Ethereum's
        // distinction between nonexistence and an existing empty account.
        assert!(
            Database::basic(&mut db, Address::repeat_byte(0x22))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn database_storage_returns_zero_for_a_missing_slot() {
        let mut db = BreejaDb::default();
        assert_eq!(
            Database::storage(&mut db, Address::repeat_byte(0x33), U256::from(9)).unwrap(),
            U256::ZERO
        );
    }

    #[test]
    fn database_code_by_hash_returns_cached_bytecode() {
        let code = Bytecode::new_raw(Bytes::from_static(&[0x60, 0x2a, 0x00]));
        let code_hash = code.hash_slow();
        let mut db = BreejaDb::default();
        db.code_by_hash.insert(code_hash, code.clone());

        assert_eq!(Database::code_by_hash(&mut db, code_hash).unwrap(), code);
    }

    #[test]
    fn commit_persists_account_code_and_storage_changes() {
        let address = Address::repeat_byte(0x44);
        let slot = U256::from(5);
        let value = U256::from(42);
        let code = Bytecode::new_raw(Bytes::from_static(&[0x60, 0x2a, 0x00]));
        let code_hash = code.hash_slow();
        let mut changed = RevmAccount::default();
        changed.info.balance = U256::from(900);
        changed.info.nonce = 3;
        changed.info.code_hash = code_hash;
        changed.info.code = Some(code.clone());
        changed.storage.insert(
            slot,
            EvmStorageSlot::new_changed(U256::ZERO, value, TransactionId::ZERO),
        );
        changed.mark_touch();

        let mut changes = AddressMap::default();
        changes.insert(address, changed);
        let mut db = BreejaDb::default();
        DatabaseCommit::commit(&mut db, changes);

        let account = &db.accounts[&address];
        assert_eq!(account.balance, U256::from(900));
        assert_eq!(account.nonce, 3);
        assert_eq!(account.code_hash, code_hash);
        assert_eq!(account.storage.get(&slot), Some(&value));
        assert_eq!(account.storage_root, account.storage_root());
        assert_eq!(Database::code_by_hash(&mut db, code_hash).unwrap(), code);
    }

    #[test]
    fn commit_removes_selfdestructed_accounts() {
        let address = Address::repeat_byte(0x55);
        let mut db = BreejaDb::default();
        db.accounts
            .insert(address, BreejaAccount::new(U256::from(1)));
        let mut destroyed = RevmAccount::default();
        destroyed.mark_selfdestruct();
        let mut changes = AddressMap::default();
        changes.insert(address, destroyed);

        DatabaseCommit::commit(&mut db, changes);

        assert!(!db.accounts.contains_key(&address));
    }
}

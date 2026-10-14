# Breeja EVM Integration Notes

## Baseline

- Branch: `feat/evm`.
- Tests: 48 passed in the workspace baseline run (13 node unit tests, 8 mining integration tests, 14 primitives tests, and 13 JSON-RPC integration tests).
- Release build: passed with `cargo build --release` before the REVM dependency was added.
- Clippy: passed with `cargo clippy --workspace --all-targets -- -D warnings` before the REVM dependency was added.
- Development mining: `cargo run --release -- --dev` starts the local JSON-RPC server with a low initial difficulty and a two-second target. With `RUST_LOG=info`, the node sealed blocks continuously, retargeting difficulty as seal times changed. It shut down cleanly on Ctrl-C.
- REVM: `43.0.3` is added to the `node` crate. This milestone only adds the dependency; Breeja does not use it yet.

## Current architecture

Breeja is a Rust teaching node that supports proof-of-work mining, signed legacy transactions, account balances and nonces, basic ETH transfers, and Ethereum JSON-RPC. Its executor applies transfers and pays transaction fees and block rewards. It cannot deploy contracts or execute contract code. The `eth_call` and `eth_estimateGas` handlers return limited answers for plain transfers and reject requests that require an EVM.

## Orientation findings

### Transaction admission and gas

In `crates/node/src/pool.rs`, `decode_and_check` has two EVM-related rejection checks:

1. It rejects `TxKind::Create`, because the current transaction path cannot create contract accounts.
2. It rejects transactions whose input/calldata is nonempty, because Breeja cannot interpret it.

The same function rejects gas limits below `TRANSFER_GAS` (21,000), the transfer intrinsic cost. That fixed lower bound assumes the only supported transaction is a plain transfer. Once calls and creation are supported, admission will need to account for the transaction's actual intrinsic requirements, including calldata and creation costs, and the execution path will need to determine gas use by running the EVM.

`PendingTransaction` retains the sender, recipient, value, nonce, gas, fee, chain ID, and signature fields, but does not retain transaction input. Both it and the mined `Transaction` model represent `to` as an `Address`; that cannot distinguish a call recipient from contract creation, where Ethereum's `to` is absent. Supporting EVM transactions will require transaction data that preserves calldata and can represent creation.

### Execution outcomes and fees

`executor.rs` currently skips a transaction if `apply_transfer` fails. This is appropriate for a transaction that is invalid when execution is attempted, such as one whose nonce no longer matches or whose sender cannot cover its cost. It is not how a valid contract transaction that reverts should be handled: a revert is an execution result after the transaction has passed validity checks. Ethereum includes such a transaction in the block, charges its consumed gas, and records receipt status `0x0`. The executor will eventually need to distinguish invalid transactions, which should be skipped, from valid executions that revert, which should still be mined with a failed receipt.

Admission reserves `gas_limit × gas_price` (plus value) against the sender's pending balance so queued transactions cannot collectively promise more funds than the account has. Execution instead charges using gas actually used; unused reserved gas is refunded. The current transfer-only executor always uses 21,000 gas, but contract execution can use a different amount, so the reservation and final charge must remain distinct.

### State and history

`BlockState` stores the latest account map and pushes a clone of the complete map into `state_history` after every block. This is simple and supports historical balance and nonce queries, but storage cost grows with both the number of accounts and the number of blocks. Once contracts introduce code and potentially many storage slots per account, copying complete account snapshots for every block becomes increasingly expensive. A state trie or another incremental/versioned storage design will be needed as the project grows.

The current `Account` has nonce, balance, `storage_root`, and `code_hash` fields, but Breeja does not store or expose contract bytecode or storage values. Its state root is a sorted concatenation hash of the current account records, not an Ethereum Merkle-Patricia trie, and the transaction and receipt roots are likewise simplified hashes. These are teaching-model limitations, not completed Ethereum state commitments.

### Mining and RPC flow

The miner snapshots accounts, the chain head, and a mempool batch, executes the candidate transactions without holding the chain lock, then searches for a proof-of-work nonce. In development mode the starting difficulty is low and the target block time is two seconds. A newly admitted transaction changes the mempool generation and can cause the miner to abandon and rebuild its candidate. After sealing, the miner commits the resulting accounts, block, transactions, and receipts.

The JSON-RPC surface provides chain, block, transaction, receipt, balance, nonce, raw-transaction submission, `eth_call`, and `eth_estimateGas` methods. The latter two are intentionally limited: calls needing contract creation or calldata fail because there is no EVM yet. This note records the current limitations and the integration direction; no execution, state model, mempool validation, or RPC behavior has been changed in Milestone 0.

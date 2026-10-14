# Breeja EVM Integration Notes

## Baseline

- Branch: `feat/evm`.
- Tests: 48 passed in the workspace baseline run (13 node unit tests, 8 mining integration tests, 14 primitives tests, and 13 JSON-RPC integration tests).
- Release build: passed with `cargo build --release` before the REVM dependency was added.
- Clippy: passed with `cargo clippy --workspace --all-targets -- -D warnings` before the REVM dependency was added.
- Development mining: `cargo run --release -- --dev` starts the local JSON-RPC server with a low initial difficulty and a two-second target. With `RUST_LOG=info`, the node sealed blocks continuously, retargeting difficulty as seal times changed. It shut down cleanly on Ctrl-C.
- REVM: `43.0.3` is used by the node's snapshot database adapter. Transaction execution is still unchanged.

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

Before Milestone 1, Breeja's `Account` had nonce, balance, `storage_root`, and `code_hash` fields without storing contract bytecode or storage values. Milestone 1 adds those state values, but Breeja still does not expose them through RPC or execute contracts. Its state root is a sorted concatenation hash of account records, not an Ethereum Merkle-Patricia trie, and the transaction and receipt roots are likewise simplified hashes. These are teaching-model limitations, not completed Ethereum state commitments.

### Mining and RPC flow

The miner snapshots accounts, the chain head, and a mempool batch, executes the candidate transactions without holding the chain lock, then searches for a proof-of-work nonce. In development mode the starting difficulty is low and the target block time is two seconds. A newly admitted transaction changes the mempool generation and can cause the miner to abandon and rebuild its candidate. After sealing, the miner commits the resulting accounts, block, transactions, and receipts.

The JSON-RPC surface provides chain, block, transaction, receipt, balance, nonce, raw-transaction submission, `eth_call`, and `eth_estimateGas` methods. The latter two are intentionally limited: calls needing contract creation or calldata fail because there is no EVM yet. This note records the current limitations and the integration direction; no execution, state model, mempool validation, or RPC behavior has been changed in Milestone 0.

## Milestone 1: code and storage state

### Representation

`Account` now owns its storage slots as `HashMap<U256, U256>`. The existing
`storage_root` field is set to a deterministic hash of sorted slot/value pairs
when REVM changes are committed, and the same value is derived for state-root
calculation. This keeps the teaching model simple and deterministic, but it is not Ethereum's storage
Merkle-Patricia trie root. The slot map has a serde default so previously serialized
accounts without a `storage` field still deserialize.

Bytecode is stored once in `BlockState` as `HashMap<B256, Bytes>`, keyed by its
Keccak code hash; each account's existing `code_hash` refers to that content. This
avoids duplicating identical code and matches REVM's `code_by_hash` lookup. A small
in-memory content cache is appropriate for Breeja; a production node would persist
the code store and validate it as part of its state backend. `BreejaDb::from_state`
copies the accounts, converts cached byte strings into REVM `Bytecode`, and copies
known block hashes into an owned execution snapshot.

### REVM database behavior

`BreejaDb` implements REVM 43.0.3's `Database` and `DatabaseCommit` traits. The
adapter owns the snapshot instead of implementing the traits on the live
`BlockState`, so REVM's mutable database access cannot hold the chain's shared lock.
`DatabaseCommit` applies touched balances and nonces, stores newly supplied code by
its computed hash, applies each storage slot's present value, removes zero-valued
slots, refreshes the storage root, and removes accounts marked self-destructed.
Newly created accounts clear any old slots before applying their resulting storage.

An unknown address returns `None` from `basic`, matching REVM's representation of
a nonexistent account. Existing accounts with no bytecode report the empty-code
hash and empty bytecode. `code_by_hash` returns an empty `Bytecode` if its hash is
not in the snapshot cache; the higher-level `BlockState::get_code_by_hash` instead
returns `None` for an unknown non-empty hash. A missing storage slot, including a
slot under an unknown account, reads as zero.

This milestone only adds state access and a database adapter. The miner does not
yet run transactions through REVM, and there is not yet an execution path that
installs an adapter's committed snapshot back into `BlockState`. The custom state
and storage hashes also remain educational substitutes for Ethereum tries.

### Milestone 1 verification

The baseline workspace run passed all 48 tests when run outside the restricted
sandbox; the earlier RPC failures were caused by the sandbox denying local socket
binding. Milestone 1 adds six database adapter tests and two state-model tests. Final
validation also passed outside the sandbox: `cargo fmt --all -- --check`,
`cargo test --workspace` (56 passed, 0 failed), `cargo build --release`, and
`cargo clippy --workspace --all-targets -- -D warnings`.

## Milestone 2: transfers through REVM

### Transaction and block environments

The miner now creates a `BreejaDb` from the current account, bytecode, and block
hash snapshots while it briefly holds the state read lock. It releases that lock
before executing the batch. Each `PendingTransaction` becomes a legacy REVM
`TxEnv`: `from` is the caller, `to` is a call target, and Breeja's value, nonce,
gas limit, gas price, and chain ID are copied into the corresponding fields.
Transaction data remains empty; creation and calldata are still rejected by the
mempool.

The candidate block becomes a `BlockEnv` with its number, monotonic timestamp,
`BLOCK_GAS_LIMIT`, and configured miner as beneficiary. Its base fee is zero because
Breeja has no EIP-1559 fee market; a nonzero base fee would burn part of the fee and
break supply conservation. `prevrandao` uses the parent block hash because the
candidate block hash is not available until after execution and proof of work.
Execution uses `SpecId::SHANGHAI`, a teaching compromise that gives modern EVM
semantics while Breeja's surrounding chain still has its simple Frontier-style
reward and fee model.

### State and accounting

Each transaction is executed with REVM 43.0.3 against the owned database snapshot.
After the EVM object is dropped, successful state changes are committed into that
snapshot. REVM credits the block beneficiary for transaction gas, so Breeja does
not credit that fee a second time. REVM also advances the sender nonce; Breeja does
not increment it manually. Breeja still adds its block reward after the batch,
since that reward is separate from transaction fees. After proof of work, the
updated accounts and code cache are installed with the sealed block. The live state
lock is held only for the short snapshot and commit operations, not EVM execution or
mining.

REVM validity errors and unsuccessful execution results are reported and the
transaction is skipped by this transfer-only executor. The current admission rules
mean no contract code, calldata, or creation is expected to execute. Milestone 3
will need to distinguish invalid transactions, which are skipped, from valid
contract executions that revert, which must be included with receipt status `0x0`.
The existing receipt model still reports successful transfer receipts and does not
yet model contract addresses or logs.

### Milestone 2 verification

The baseline before these changes passed 56 workspace tests. Final validation passed
with 57 tests: 20 node unit tests, 8 mining integration tests, 16 primitives tests,
and 13 JSON-RPC integration tests. The new REVM parity test confirms a transfer
uses exactly 21,000 transaction gas, moves the requested value, charges and credits
the fee once, advances the nonce once, preserves supply, creates no contract
address, and emits no logs. Successful receipt status and cumulative gas are checked
in the block-execution test. The existing supply-conservation integration test
passes without changes.

`cargo fmt --all -- --check`, `cargo test -p node`,
`cargo test -p node --test mining`, `cargo test --workspace`,
`cargo build --release`, and
`cargo clippy --workspace --all-targets -- -D warnings` passed. The full workspace
test run was executed outside the restricted sandbox so its local RPC server tests
could bind sockets.

The release node was run in development mode and its local JSON-RPC endpoint
reported block number `0x1a` (26), confirming that blocks were mined. The process
was stopped with Ctrl-C. `cast` is not installed in this environment, so the
README's interactive `cast mktx` workflow could not be run. The signed-transfer
mining integration test exercises transaction admission, mining, balances, fee,
nonce, and receipt end to end instead.

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

## Milestone 3: contracts

### Transaction representation and admission

The pending transaction now retains an explicit `TxKind`: `TxKind::Create` means
contract creation, and `TxKind::Call(address)` means a call or transfer. Creation
is represented as `to: None` in mined transactions and JSON-RPC transaction/receipt
objects; no zero-address sentinel is used. The pending transaction also retains
its input bytes. The signer helpers share one legacy signing path, with dedicated
helpers for deployment and calls.

The pool accepts creation and non-empty calldata while continuing to require the
legacy transaction type, the configured chain ID and minimum fixed gas price, a
contiguous pending nonce, and enough pending balance for value plus the full
upfront gas reservation. It rejects gas limits above `BLOCK_GAS_LIMIT`. Intrinsic
gas is computed with REVM 43's Shanghai `calculate_initial_tx_gas` helper using the
transaction's actual input and creation kind. This accounts for the 21,000 base,
zero and non-zero calldata byte prices, creation overhead, and Shanghai initcode
word cost. A fixed 21,000 floor would allow some underfunded creations and calls
and ignore calldata cost, so it is no longer used for every transaction kind.

### Execution, state, and receipts

The executor copies the explicit transaction kind to `TxEnv.kind` and input bytes
to `TxEnv.data`: an empty transfer has empty data, a deployment carries initcode,
and a call carries calldata. It continues to use Shanghai, zero base fee, the
parent hash as `prevrandao`, and an owned database snapshot. REVM's created address
is recorded only when the result reports one; REVM's returned state commits
deployed runtime code/code hash and storage together with balances and nonces.
Storage slot writes are persisted through the existing `BreejaDb` commit adapter.

Receipts take status from `ExecutionResult::is_success()`, gas from
`tx_gas_used()`, and the address from `created_address()`. Successful execution
logs are converted to Breeja logs containing emitter address, B256 topics, and raw
data, then serialized in receipt JSON. REVM returns logs in unsuccessful result
objects too, including logs emitted before a revert; those are discarded because
the EVM rolls them back. Receipt logs are therefore populated only for successful
execution. Receipt bloom remains the existing all-zero placeholder and full
Ethereum log metadata/bloom calculation is outside this milestone.

The executor distinguishes validation errors from valid EVM failures. A REVM
transaction-validation error is skipped. A valid `REVERT` or halt returns an
execution result, is included in the block with status `0x0`, and contributes its
actual gas to receipt and cumulative/block gas. REVM's revert journal removes
contract storage, code, and logs from the reverted portion while retaining valid
sender nonce and fee effects; REVM credits the beneficiary fee once. Breeja then
adds the separate block reward. The upfront mempool reservation remains
`gas_limit × gas_price` plus value, while final execution charges actual gas and
returns the unused reservation through REVM's state result.

Each candidate transaction executes on a cloned snapshot, and its state is adopted
only if its actual gas fits the block's remaining limit. Transactions that do not
fit are left unmined for a later candidate. This keeps cumulative gas at or below
the block limit without leaking an excluded transaction's state effects.

### Milestone 3 verification and limitations

New public mining tests cover the supplied deployment initcode and committed
runtime, storage slot zero becoming 42, a single empty LOG0 with the right emitter,
successful deployment/call receipt fields, and a reverting contract call whose
storage and logs are discarded while gas, fee, sender nonce, miner credit, and
supply remain correct. Pool tests cover creation/call admission, zero/non-zero
calldata intrinsic costs, too-low and too-high gas, and continuing nonce/balance
rejection. Receipt serialization has a test for a null `to`, a contract address,
and EVM log contents. The existing exact 21,000-gas transfer and supply
conservation tests remain unchanged and pass.

The release node was also run with `cargo run --release -- --dev`. I queried its
local endpoint with:

```sh
curl --max-time 5 -sS -X POST http://127.0.0.1:8545 \
  -H 'Content-Type: application/json' \
  --data '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}'
```

It returned `0x16`, then `0x1e` after several seconds; Ctrl-C stopped the node
cleanly. `cast` and `forge` are not installed, and Breeja does not
provide a CLI for signing arbitrary creation/call transactions, so an interactive
RPC deployment/call could not be demonstrated. The integration tests submit
signed raw transactions through the node's public submission/mining behavior and
inspect state/receipts, exercising deployment and calls end to end. No
`eth_call`, `eth_estimateGas`, code/storage inspection RPC, or later milestone was
added.

All required final commands passed: `cargo fmt --all`,
`cargo fmt --all -- --check`, `cargo test -p primitives` (17 passed),
`cargo test -p node` (37 passed across node unit and mining integration tests),
`cargo test -p node --test mining` (11 passed), `cargo test --workspace` (67
passed, 0 failed), `cargo build --release`, and
`cargo clippy --workspace --all-targets -- -D warnings`. Workspace counts were 26
node unit tests, 11 mining tests, 17 primitives tests, and 13 JSON-RPC tests; the
test count increased from 57 to 67. The release build first encountered a stale,
zero-byte generated `primitives` artifact from an interrupted build; after removing
that artifact and its fingerprint, the exact workspace release build passed. This
was a build-cache issue, not a source or test failure.

The unchanged `the_chain_never_creates_or_destroys_eth_outside_the_block_reward`
test passed. The exact 21,000-gas transfer parity test passed. The block gas cap
test also passes, including a valid out-of-gas execution that is charged, while the
second transaction is excluded when it would exceed the block limit.

## Milestone 4: read-only calls and gas estimation

### Execution and request handling

The existing JSON-RPC routes now run requests through REVM 43.0.3. `eth_call`
translates `from`, `to`/creation, `value`, `gas`, `gasPrice`, and `data`/`input`
into a Shanghai `TxEnv`. Omitted callers default to the zero address, omitted value
to zero, gas to Breeja's block limit, gas price to the node's fixed price, and nonce
to the caller's current account nonce. Supplying both `data` and `input` is accepted
only when the bytes match. Integer conversions are checked, and requests above the
block gas limit or below transaction intrinsic gas are rejected.

The simulation copies accounts, code, and block hashes into an owned `BreejaDb`
snapshot, releases the chain read lock, executes, and drops the snapshot without
calling `DatabaseCommit`. Successful calls return REVM's output bytes. Reverts and
halts return JSON-RPC execution errors; revert errors carry the returned revert data.
The simulation config disables REVM's nonce and upfront-balance checks, as these
are commonly relaxed for `eth_call` and the resulting state is never committed.
Other transaction and block constraints remain active. Calls still use the current
head's block context, including zero base fee and the configured miner beneficiary.

Breeja retains historical account snapshots, but it does not retain matching
historical code/storage snapshots for REVM. Therefore EVM simulation supports the
current head (`latest`, `pending`, `safe`, and `finalized` resolve there) and rejects
older block requests with a clear error rather than using current contract state for
an old block. `pending` simulation currently means the current committed state; it
does not replay unmined transactions.

### Gas estimation

`eth_estimateGas` computes Shanghai intrinsic gas using the same REVM helper as
mempool validation. It first executes at the request gas limit or block gas limit;
if that execution reverts or halts, estimation returns an execution error. It then
binary-searches from intrinsic gas to that successful upper bound. Every probe
clones the same base `BreejaDb`, so one probe cannot influence another. The final
candidate is executed once more and returned if successful; if GAS-dependent code
makes the candidate fail, the already-proven upper bound is returned. This is a
heuristic rather than a proof because contracts may branch on `GAS`. No blanket
safety margin is added; callers receive the lowest verified successful candidate
under the search's behavior, or the verified cap for a non-monotonic final result.

### Milestone 4 verification

Baseline before changes: the clean `feat/evm` Milestone 3 commit was `fe3aed8`; all
67 workspace tests passed, formatting, release build, and Clippy passed. The initial
RPC integration run added call-output, read-only storage, revert, gas search, and
contract transaction tests. It found and corrected a five-byte REVERT test runtime
that had mistakenly declared a four-byte length. The focused corrected revert test
passed. A first complete run also exposed an assertion race with the background
miner creating unrelated empty blocks. The state-isolation test now stops and joins
the miner before comparing block height; the focused test and subsequent full run
passed.

The RPC integration tests now cover `eth_call` output bytes, data/input alias
handling, revert errors, calldata acceptance, unchanged contract storage, sender
balance/nonce and block height after a simulated write, historical-state rejection,
exact `0x5208` transfer estimation, and a mined contract call using exactly the
estimated gas. The test uses the normal signed-transaction submission, miner, and
receipt interfaces. No code/storage/log RPC queries or other Milestone 5 methods were added.


Final tests, release build, and Clippy passed: `cargo test -p primitives` (17
passed), `cargo test -p node` (37 passed across unit and mining tests),
`cargo test -p node --test mining` (11 passed), `cargo test -p rpc` (15 passed),
`cargo test --workspace` (69 passed, 0 failed), `cargo build --release`, and
`cargo clippy --workspace --all-targets -- -D warnings`. The workspace formatting
check passed while excluding the unrelated empty example described below. On the
final worktree, `cargo fmt --all -- --check` reports formatting for the pre-existing
untracked, zero-byte `crates/node/examples/m4_sign.rs`. I preserved that file
unchanged rather than editing unrelated user data. No tests were removed or ignored.
The test count increased from 67 to 69. The unchanged supply-conservation and exact
21,000-gas transfer tests passed.

The release node was run with `cargo run --release -- --dev` outside the socket-
restricted sandbox and stopped with Ctrl-C after these checks. Its block number
advanced from `0x13` to `0x15` over three seconds. The commands below were sent to
`http://127.0.0.1:8545` while the node was running:

```sh
# Plain transfer estimate
curl -sS -X POST http://127.0.0.1:8545 -H 'Content-Type: application/json' \
  --data '{"jsonrpc":"2.0","method":"eth_estimateGas","params":[{"to":"0x0000000000000000000000000000000000000022"}],"id":1}'
# result: 0x5208

# eth_call against the deployed counter (the signed creation transaction was
# submitted with eth_sendRawTransaction; receipt address: 0x5fbdb2315678afecb367f032d93f642f64180aa3)
curl -sS -X POST http://127.0.0.1:8545 -H 'Content-Type: application/json' \
  --data '{"jsonrpc":"2.0","method":"eth_call","params":[{"to":"0x5fbdb2315678afecb367f032d93f642f64180aa3"},"latest"],"id":2}'
# result: 0x0000000000000000000000000000000000000000000000000000000000000001
# Repeating the same eth_call returned the same value (1), demonstrating that
# the counter's simulated SSTORE did not persist.

# Estimate the state-changing counter call
curl -sS -X POST http://127.0.0.1:8545 -H 'Content-Type: application/json' \
  --data '{"jsonrpc":"2.0","method":"eth_estimateGas","params":[{"from":"0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266","to":"0x5fbdb2315678afecb367f032d93f642f64180aa3"}],"id":3}'
# result: 0xa87a (43,130 gas)
```

The signed counter deployment and call were submitted with these exact raw
transactions; the call used nonce 1 and gas limit exactly `0xa87a`:

```sh
curl -sS -X POST http://127.0.0.1:8545 -H 'Content-Type: application/json' \
  --data '{"jsonrpc":"2.0","method":"eth_sendRawTransaction","params":["0xf86e80843b9aca00830249f080809e6012600c60003960126000f36000546001018060005560005260206000f326a079fd6ebe870f0584995d7646d64d3f02a88a744406cf52d9f8b743d1b6e44d5fa05857ee2307cc83e504091182157695fc35e6139b3ab9921d7f4661f6e7820aed"],"id":10}'
# deployment tx: 0xdd203fd2e7dfcdabf7435beea1284f5567059e8dedbd53ca3ab51dd60e2eabd8

curl -sS -X POST http://127.0.0.1:8545 -H 'Content-Type: application/json' \
  --data '{"jsonrpc":"2.0","method":"eth_sendRawTransaction","params":["0xf86201843b9aca0082a87a945fbdb2315678afecb367f032d93f642f64180aa3808025a0386dfd25396022e61ee2f31ce7a00d5c89a2d31798b2492fcd947c49266a0eb49f8519b3c55cf4eedeae80e525faf544b518de487fc5ef5379375a02e4db9247"],"id":15}'
# mined call tx: 0xcbde4cf451f8947666cd0b2c14b40bf938dbb6e97dd825f42b16a039256d2377
```

The call receipt had status `0x1` and `gasUsed` `0xa87a`. A subsequent `eth_call`
returned the 32-byte value 2: the mined call changed the counter once, while
preceding simulations left it unchanged.

A second signed creation transaction deployed a five-byte REVERT runtime at
`0x9fe46736679d2d9a65f0992f2272de9f3c7fa6e0`:

```sh
curl -sS -X POST http://127.0.0.1:8545 -H 'Content-Type: application/json' \
  --data '{"jsonrpc":"2.0","method":"eth_sendRawTransaction","params":["0xf86102843b9aca00830186a08080916005600c60003960056000f360006000fd26a019c297e776e8cd37cd76a6f0b5d359302637da8c7091f24774f7b021836b9823a01570d21bf3ed1bfb0de82d94bee2d6c9b0848611d642705430adce6e2b632462"],"id":21}'
# revert contract tx: 0xfd57898eac45cb4f536a559b043ae3c9db55c7abafe34883cb936d250d1ca531
```

Calling it produced:

```json
{"jsonrpc":"2.0","id":4,"error":{"code":-32000,"message":"execution reverted","data":"0x"}}
```

The same integration tests exercise deployment, call output, storage neutrality,
revert errors, and exactly-estimated mined execution through the RPC server. No
contract-inspection methods were added, and Milestone 5 was not started.

## Milestone 5: contract inspection RPC

`eth_getCode` resolves the requested block tag against Breeja's retained account
snapshots, reads the code hash that was active for that account, and retrieves the
content-addressed runtime bytecode. Unknown accounts and accounts without code
return `0x`. Because bytecode is retained by hash, historical code queries remain
honest even when an account's active code hash changes.

`eth_getStorageAt` reads the requested 256-bit slot from the account snapshot for
the resolved block. Missing accounts and slots read as zero. The RPC result is
fixed-width 32-byte data rather than a minimal hex quantity, so decimal 42 is
returned as 31 zero bytes followed by `2a`.

`eth_getLogs` performs a direct read-only scan of canonical blocks and their
receipts. The range is inclusive; omitted bounds default to `latest`. It accepts a
single address or an OR-list of addresses. Topic filters are positional: positions
are ANDed, `null` is a wildcard, and an array within one position is an OR-list.
Results are emitted in block/transaction/log order with block hash/number,
transaction hash/index, block-wide log index, and `removed: false`. Breeja has no
reorganizations, and bloom-assisted searching waits for Milestone 6, so a direct
scan is the truthful implementation for this teaching chain. `blockHash` is also
supported and is mutually exclusive with range fields.

`eth_gasPrice` returns Breeja's canonical fixed `GAS_PRICE` as a JSON-RPC quantity.
This keeps the pre-EIP-1559 model while allowing standard `cast send --legacy`
commands to autofill the price.

Tests cover exact runtime code, empty code, slot zero containing 42, missing
storage, inclusive block/address log filtering, address OR semantics, positional
topic matching including wildcard and OR alternatives, complete mined-log
metadata, and the configured gas price. All handlers are read-only.

Milestone 5 was verified in CI with formatting, workspace tests, a release build,
and strict Clippy checks.

## Milestone 6: variable-gas consistency

Mempool admission calculates the Shanghai intrinsic cost of each transaction:
21,000 base gas, calldata byte charges, the contract-creation surcharge, and the
initcode word charge. A transaction whose declared gas limit cannot cover that
cost is rejected before it reaches the miner.

The miner no longer limits a candidate using a fixed number of transfers. It
passes the pending queue to the executor in arrival order. Each transaction is
executed against a temporary state copy, and that copy is committed only when
the transaction's actual gas use still fits under `BLOCK_GAS_LIMIT`. This keeps
the block header's `gasUsed` equal to the cumulative gas in its receipts while
preventing an over-limit transaction from leaking state changes.

Receipt `logsBloom` is now a real 2048-bit Ethereum bloom. Every log address and
topic contributes three bits derived from its Keccak-256 hash. Receipts without
logs retain the canonical all-zero bloom.

The simplified state root commits to balance, nonce, code hash, and a
deterministic storage root. Tests prove that changing only contract code or one
storage slot changes the state root.

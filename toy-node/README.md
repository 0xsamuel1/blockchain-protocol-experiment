# Breeja — a proof-of-work Ethereum teaching node

Breeja is a teaching implementation of an Ethereum execution node from the
proof-of-work era, written in Rust. It mines blocks, validates signed
transactions, maintains account state, and serves the Ethereum JSON-RPC API.

Breeja began without an EVM so the surrounding protocol could be studied first.
It now executes transfers and contracts through REVM while retaining its small,
readable proof-of-work chain, mempool, account state, receipts, and JSON-RPC API.

```text
                   eth_sendRawTransaction
   wallet / cast ──────────────────────────┐
                                           ▼
                                    ┌─────────────┐
                                    │  JSON-RPC   │  crates/rpc
                                    │  (jsonrpsee)│
                                    └──────┬──────┘
                                           │ validate + queue
                                           ▼
                                    ┌─────────────┐
                                    │   mempool   │  crates/node/pool.rs
                                    └──────┬──────┘
                                           │
                                           ▼
                 ┌──────────────────────────────────────────────┐
                 │  miner loop            crates/node/miner.rs  │
                 │                                              │
                 │  1. snapshot state and mempool               │
                 │  2. execute transfers      → executor.rs     │
                 │  3. hash until the nonce works → pow.rs      │
                 │  4. commit the sealed block                  │
                 │  5. retarget difficulty                      │
                 └──────────────────────┬───────────────────────┘
                                        ▼
                                 ┌─────────────┐
                                 │ chain state │  crates/primitives/node.rs
                                 └─────────────┘
```

## Running it

```bash
cargo run --release -- --dev
```

`--dev` starts at a low difficulty and targets 2-second blocks. Release mode
matters: a debug build hashes roughly twenty times slower.

The node prints each block as it is sealed, including how many hashes the proof of
work actually cost:

```text
INFO sealed block number=8 hash=0x000000ab64b3a5… txs=1 difficulty=23
     nonce=1446633 attempts=1446634 hash_rate=1509579 H/s took=0.96s
```

Options:

| Flag | Meaning |
| --- | --- |
| `--dev` | Low starting difficulty, 2-second target blocks |
| `--rpc-address <ADDR>` | JSON-RPC bind address (default `127.0.0.1:8545`) |
| `--miner <ADDRESS>` | Who receives block rewards and fees |
| `--difficulty <BITS>` | Starting difficulty, in leading zero bits |
| `--target-block-time <SECS>` | Block time the miner retargets towards |

Set `RUST_LOG=debug` for more detail.

## Pre-funded accounts

Genesis funds four accounts with 10,000 ETH each. These are the standard
Anvil/Hardhat development accounts — **their private keys are public knowledge and
must never hold real funds.**

| Address | Private key |
| --- | --- |
| `0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266` | `0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80` |
| `0x70997970C51812dc3A010C7d01b50e0d17dc79C8` | `0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d` |
| `0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC` | `0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a` |
| `0x90F79bf6EB2c4f870365E785982E1f101E93b906` | `0x7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6` |

## Sending a transaction

`eth_gasPrice`, transaction-count lookup, and gas estimation let Cast fill the
legacy transaction parameters automatically:

```bash
cast send --legacy --rpc-url http://127.0.0.1:8545 \
  --private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
  --value 1ether 0x70997970C51812dc3A010C7d01b50e0d17dc79C8
```

You can still build and submit the raw bytes explicitly:

```bash
RAW=$(cast mktx --legacy \
  --private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
  --nonce 0 --gas-price 1000000000 --gas-limit 21000 --chain 1 \
  --value 1ether 0x70997970C51812dc3A010C7d01b50e0d17dc79C8)

curl -s -X POST http://localhost:8545 -H 'Content-Type: application/json' \
  --data "{\"jsonrpc\":\"2.0\",\"method\":\"eth_sendRawTransaction\",\"params\":[\"$RAW\"],\"id\":1}"
```

`--legacy` is required. Breeja predates EIP-1559 and rejects typed transactions,
because there is no base fee for one to bid against.

Then poll for the receipt:

```bash
curl -s -X POST http://localhost:8545 -H 'Content-Type: application/json' \
  --data '{"jsonrpc":"2.0","method":"eth_getTransactionReceipt","params":["<TX_HASH>"],"id":1}'
```

A `null` receipt means "not mined yet" — which is exactly how every Ethereum
client waits for confirmation.

## The JSON-RPC API

The original assignment methods plus EVM execution and contract inspection.

| Method | Behaviour |
| --- | --- |
| `eth_chainId` | `0x1` |
| `eth_blockNumber` | Height of the chain head |
| `eth_getBlockByNumber` | By number or tag (`latest`, `earliest`, `pending`, `safe`, `finalized`) |
| `eth_getBlockByHash` | By hash; `null` if unknown |
| `eth_getTransactionByHash` | Mined transactions; `null` if unknown |
| `eth_getTransactionReceipt` | Receipts for mined transactions; `null` while pending |
| `eth_getBalance` | Balance at any historical block, or `pending` |
| `eth_getTransactionCount` | Nonce; `pending` includes queued transactions |
| `eth_sendRawTransaction` | Validates and queues signed transfers, deployments, and calls |
| `eth_call` | Executes against a throwaway state snapshot and returns output bytes |
| `eth_estimateGas` | Simulates and searches for a sufficient gas limit |
| `eth_getCode` | Runtime code at an address and block tag; `0x` if none |
| `eth_getStorageAt` | One fixed-width 32-byte storage word |
| `eth_getLogs` | Logs filtered by block range/hash, address, and positional topics |
| `eth_gasPrice` | Breeja's fixed legacy gas price |

Anything else returns `-32601 Method not found`.

Contract inspection examples (replace the address with your deployed contract):

```bash
CONTRACT=0x5fbdb2315678afecb367f032d93f642f64180aa3

cast code --rpc-url http://127.0.0.1:8545 "$CONTRACT"
cast storage --rpc-url http://127.0.0.1:8545 "$CONTRACT" 0

curl -sS -X POST http://127.0.0.1:8545 \
  -H 'Content-Type: application/json' \
  --data "{\"jsonrpc\":\"2.0\",\"method\":\"eth_getLogs\",\"params\":[{\"fromBlock\":\"earliest\",\"toBlock\":\"latest\",\"address\":\"$CONTRACT\"}],\"id\":1}"

cast rpc --rpc-url http://127.0.0.1:8545 eth_gasPrice
```

## Chain parameters

Deliberately Frontier-era, in `crates/primitives/src/constants.rs`:

| Parameter | Value | Note |
| --- | --- | --- |
| Chain ID | 1 | |
| Gas price | 1 gwei, fixed | No fee market; no EIP-1559 |
| Transfer cost | 21,000 gas | The only cost there is |
| Block gas limit | 3,141,592 | Frontier's "pi million" — 149 transfers per block |
| Block reward | 5 ETH | As on Frontier and Homestead |
| Target block time | 15s (2s with `--dev`) | |


## Course documents

- [ASSIGNMENT.md](./ASSIGNMENT.md) — the JSON-RPC exercise this node grew out of.
- [FINAL-PROJECT.md](./FINAL-PROJECT.md) — the final project: give Breeja an EVM,
  using [REVM](https://github.com/bluealloy/revm).

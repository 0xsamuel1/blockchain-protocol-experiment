use std::time::Duration;

use alloy::primitives::{Address, Bytes, U256};
use jsonrpsee::{
    core::client::ClientT,
    http_client::{HttpClient, HttpClientBuilder},
    rpc_params,
};
use node::{
    Chain, ChainConfig, miner,
    testing::{
        DEV_PRIVATE_KEY, DEV_PRIVATE_KEY_1, address_of, one_eth, sign_contract_call,
        sign_contract_creation, sign_transfer,
    },
};
use primitives::node::BreejaGenesis;
use rpc::{RPCContext, start_server};
use serde_json::Value;

/// A running node and a client pointed at it.
struct Harness {
    client: HttpClient,
    chain: Chain,
    _server: jsonrpsee::server::ServerHandle,
    miner: Option<std::thread::JoinHandle<()>>,
}

impl Harness {
    /// Starts a node with the given funded accounts, mining on a dedicated thread.
    async fn start(funded: &[(Address, U256)], mine: bool) -> Self {
        let config = ChainConfig {
            difficulty: 4,
            target_block_time: Duration::from_millis(50),
            miner: Address::repeat_byte(0x99),
        };
        let chain = Chain::new(
            BreejaGenesis {
                accounts: funded.to_vec(),
            },
            config,
        );

        // Port 0 lets the OS pick a free port, so tests can run in parallel.
        let (server, address) = start_server(RPCContext::new(chain.clone()), "127.0.0.1:0")
            .await
            .expect("server should start");
        let url = format!("http://{address}");
        let client = HttpClientBuilder::default().build(url).unwrap();

        let miner = mine.then(|| miner::spawn(chain.clone()));

        Self {
            client,
            chain,
            _server: server,
            miner,
        }
    }

    /// Polls `predicate` until it holds, or panics.
    async fn wait_for(&self, label: &str, mut predicate: impl FnMut(&Chain) -> bool) {
        for _ in 0..1_000 {
            if predicate(&self.chain) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {label}");
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.chain.shutdown();
        if let Some(handle) = self.miner.take() {
            let _ = handle.join();
        }
    }
}

/// Parses a hex quantity the way a client would.
fn from_quantity(raw: &str) -> u128 {
    u128::from_str_radix(raw.trim_start_matches("0x"), 16).expect("a valid hex quantity")
}

#[tokio::test]
async fn eth_chain_id_returns_the_configured_chain_id() {
    let harness = Harness::start(&[], false).await;
    let id: String = harness
        .client
        .request("eth_chainId", rpc_params![])
        .await
        .unwrap();
    assert_eq!(id, "0x1");
}

#[tokio::test]
async fn eth_block_number_starts_at_genesis_and_climbs_as_blocks_are_mined() {
    let harness = Harness::start(&[], true).await;

    harness
        .wait_for("blocks to be mined", |c| c.read().get_block_number() >= 3)
        .await;

    let number: String = harness
        .client
        .request("eth_blockNumber", rpc_params![])
        .await
        .unwrap();
    assert!(
        number.starts_with("0x"),
        "must be a hex quantity, got {number}"
    );
    assert!(from_quantity(&number) >= 3);
}

#[tokio::test]
async fn eth_get_block_by_number_accepts_tags_and_numbers() {
    let harness = Harness::start(&[], true).await;
    harness
        .wait_for("a block", |c| c.read().get_block_number() >= 2)
        .await;

    // By tag, with the fullTransactions flag.
    let latest: Value = harness
        .client
        .request("eth_getBlockByNumber", rpc_params!["latest", false])
        .await
        .unwrap();
    assert!(latest["hash"].as_str().unwrap().starts_with("0x"));

    // By tag, with the flag omitted entirely — also valid JSON-RPC.
    let genesis: Value = harness
        .client
        .request("eth_getBlockByNumber", rpc_params!["earliest"])
        .await
        .unwrap();
    assert_eq!(genesis["number"], "0x0");
    assert_eq!(
        genesis["parentHash"],
        format!("0x{}", "0".repeat(64)),
        "genesis has no parent"
    );

    // By number.
    let first: Value = harness
        .client
        .request("eth_getBlockByNumber", rpc_params!["0x1", false])
        .await
        .unwrap();
    assert_eq!(first["number"], "0x1");

    // Every field a client expects is present and hex-encoded.
    for field in [
        "number",
        "hash",
        "parentHash",
        "stateRoot",
        "transactionsRoot",
        "receiptsRoot",
        "difficulty",
        "totalDifficulty",
        "timestamp",
        "gasLimit",
        "gasUsed",
        "miner",
        "extraData",
        "nonce",
        "size",
    ] {
        let value = first[field]
            .as_str()
            .unwrap_or_else(|| panic!("{field} missing"));
        assert!(value.starts_with("0x"), "{field} is not hex: {value}");
    }
    // The proof-of-work nonce is a fixed-width 8-byte field, not a quantity.
    assert_eq!(first["nonce"].as_str().unwrap().len(), 18);
}

#[tokio::test]
async fn eth_get_block_by_number_rejects_a_block_that_does_not_exist() {
    let harness = Harness::start(&[], false).await;
    let result: Result<Value, _> = harness
        .client
        .request("eth_getBlockByNumber", rpc_params!["0xffffff", false])
        .await;
    assert!(
        result.is_err(),
        "a future block should be an error, not null"
    );
}

#[tokio::test]
async fn eth_get_block_by_hash_finds_the_same_block_as_by_number() {
    let harness = Harness::start(&[], true).await;
    harness
        .wait_for("a block", |c| c.read().get_block_number() >= 1)
        .await;

    let by_number: Value = harness
        .client
        .request("eth_getBlockByNumber", rpc_params!["0x1", false])
        .await
        .unwrap();
    let hash = by_number["hash"].as_str().unwrap().to_owned();

    let by_hash: Value = harness
        .client
        .request("eth_getBlockByHash", rpc_params![hash, false])
        .await
        .unwrap();
    assert_eq!(by_hash, by_number);

    // An unknown hash is null, not an error.
    let missing: Value = harness
        .client
        .request(
            "eth_getBlockByHash",
            rpc_params![format!("0x{}", "11".repeat(32)), false],
        )
        .await
        .unwrap();
    assert!(missing.is_null());
}

#[tokio::test]
async fn eth_get_balance_reads_current_pending_and_historical_state() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let bob = address_of(DEV_PRIVATE_KEY_1).unwrap();
    let harness = Harness::start(&[(alice, one_eth() * U256::from(100u64))], true).await;

    // Latest, and the one-parameter form.
    let balance: String = harness
        .client
        .request("eth_getBalance", rpc_params![alice, "latest"])
        .await
        .unwrap();
    assert_eq!(balance, "0x56bc75e2d63100000");
    let without_tag: String = harness
        .client
        .request("eth_getBalance", rpc_params![alice])
        .await
        .unwrap();
    assert_eq!(without_tag, balance);

    // An address nobody funded is zero, not an error.
    let empty: String = harness
        .client
        .request(
            "eth_getBalance",
            rpc_params![Address::repeat_byte(0x77), "latest"],
        )
        .await
        .unwrap();
    assert_eq!(empty, "0x0");

    // Send 5 ETH and wait for it to land.
    let raw = sign_transfer(DEV_PRIVATE_KEY, bob, one_eth() * U256::from(5u64), 0).unwrap();
    let tx_hash: String = harness
        .client
        .request("eth_sendRawTransaction", rpc_params![raw.to_string()])
        .await
        .unwrap();
    harness
        .wait_for("the transfer to be mined", |c| {
            c.read().get_account(&bob).balance > U256::ZERO
        })
        .await;

    let bob_now: String = harness
        .client
        .request("eth_getBalance", rpc_params![bob, "latest"])
        .await
        .unwrap();
    assert_eq!(from_quantity(&bob_now), 5 * 10u128.pow(18));

    // At genesis Bob had nothing, and the chain still remembers that.
    let bob_at_genesis: String = harness
        .client
        .request("eth_getBalance", rpc_params![bob, "0x0"])
        .await
        .unwrap();
    assert_eq!(bob_at_genesis, "0x0");

    let _ = tx_hash;
}

#[tokio::test]
async fn eth_get_transaction_count_tracks_mined_and_pending_nonces() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let bob = address_of(DEV_PRIVATE_KEY_1).unwrap();
    // No miner: the transactions stay pending, which is what this test is about.
    let harness = Harness::start(&[(alice, one_eth() * U256::from(100u64))], false).await;

    let mined: String = harness
        .client
        .request("eth_getTransactionCount", rpc_params![alice, "latest"])
        .await
        .unwrap();
    assert_eq!(mined, "0x0");

    for nonce in 0..2 {
        let raw = sign_transfer(DEV_PRIVATE_KEY, bob, one_eth(), nonce).unwrap();
        let _: String = harness
            .client
            .request("eth_sendRawTransaction", rpc_params![raw.to_string()])
            .await
            .unwrap();
    }

    // "latest" still sees no mined transactions...
    let mined: String = harness
        .client
        .request("eth_getTransactionCount", rpc_params![alice, "latest"])
        .await
        .unwrap();
    assert_eq!(mined, "0x0");

    // ...but "pending" includes the queued ones, which is the nonce a wallet must
    // use for its next transaction.
    let pending: String = harness
        .client
        .request("eth_getTransactionCount", rpc_params![alice, "pending"])
        .await
        .unwrap();
    assert_eq!(pending, "0x2");
}

#[tokio::test]
async fn a_transfer_produces_a_transaction_and_a_receipt() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let bob = address_of(DEV_PRIVATE_KEY_1).unwrap();
    let harness = Harness::start(&[(alice, one_eth() * U256::from(100u64))], true).await;

    let raw = sign_transfer(DEV_PRIVATE_KEY, bob, one_eth(), 0).unwrap();
    let tx_hash: String = harness
        .client
        .request("eth_sendRawTransaction", rpc_params![raw.to_string()])
        .await
        .unwrap();
    assert_eq!(tx_hash.len(), 66, "a transaction hash is 32 bytes of hex");

    // Before it is mined, there is a transaction but no receipt. That difference
    // is exactly how a client knows to keep waiting.
    harness
        .wait_for("the transfer to be mined", |c| {
            c.read().get_receipt(&tx_hash.parse().unwrap()).is_some()
        })
        .await;

    let tx: Value = harness
        .client
        .request("eth_getTransactionByHash", rpc_params![tx_hash.clone()])
        .await
        .unwrap();
    assert_eq!(tx["from"], format!("{alice:#x}"));
    assert_eq!(tx["to"], format!("{bob:#x}"));
    assert_eq!(tx["value"], "0xde0b6b3a7640000");
    assert_eq!(tx["nonce"], "0x0");
    assert_eq!(tx["input"], "0x");
    assert_eq!(tx["type"], "0x0", "legacy transaction");
    assert_eq!(tx["chainId"], "0x1");

    let receipt: Value = harness
        .client
        .request("eth_getTransactionReceipt", rpc_params![tx_hash.clone()])
        .await
        .unwrap();
    assert_eq!(receipt["status"], "0x1");
    assert_eq!(receipt["gasUsed"], "0x5208");
    assert_eq!(receipt["from"], format!("{alice:#x}"));
    assert_eq!(receipt["to"], format!("{bob:#x}"));
    assert!(
        receipt["contractAddress"].is_null(),
        "no contracts without an EVM"
    );
    assert_eq!(receipt["logs"].as_array().unwrap().len(), 0);
    assert_eq!(receipt["blockHash"], tx["blockHash"]);

    // The block it landed in lists it, and can inline it.
    let block: Value = harness
        .client
        .request(
            "eth_getBlockByHash",
            rpc_params![receipt["blockHash"].as_str().unwrap(), true],
        )
        .await
        .unwrap();
    let inlined = block["transactions"].as_array().unwrap();
    assert_eq!(inlined.len(), 1);
    assert_eq!(inlined[0]["hash"], tx_hash);

    // With the flag off, the same block lists bare hashes instead.
    let hashes_only: Value = harness
        .client
        .request(
            "eth_getBlockByHash",
            rpc_params![receipt["blockHash"].as_str().unwrap(), false],
        )
        .await
        .unwrap();
    assert_eq!(hashes_only["transactions"][0], tx_hash);
}

#[tokio::test]
async fn an_unknown_transaction_or_receipt_is_null() {
    let harness = Harness::start(&[], false).await;
    let unknown = format!("0x{}", "11".repeat(32));

    let tx: Value = harness
        .client
        .request("eth_getTransactionByHash", rpc_params![unknown.clone()])
        .await
        .unwrap();
    assert!(tx.is_null());

    let receipt: Value = harness
        .client
        .request("eth_getTransactionReceipt", rpc_params![unknown])
        .await
        .unwrap();
    assert!(receipt.is_null());
}

#[tokio::test]
async fn eth_send_raw_transaction_rejects_what_the_node_cannot_execute() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let harness = Harness::start(&[(alice, one_eth())], false).await;

    // Not hex at all.
    let result: Result<String, _> = harness
        .client
        .request("eth_sendRawTransaction", rpc_params!["not hex"])
        .await;
    assert!(result.is_err());

    // Hex, but not a transaction.
    let result: Result<String, _> = harness
        .client
        .request("eth_sendRawTransaction", rpc_params!["0xdeadbeef"])
        .await;
    assert!(result.is_err());

    // A real transaction for more ETH than the sender has.
    let raw = sign_transfer(
        DEV_PRIVATE_KEY,
        Address::repeat_byte(0x22),
        one_eth() * U256::from(1_000u64),
        0,
    )
    .unwrap();
    let result: Result<String, _> = harness
        .client
        .request("eth_sendRawTransaction", rpc_params![raw.to_string()])
        .await;
    let error = result.expect_err("an unaffordable transfer must be rejected");
    assert!(
        error.to_string().contains("insufficient funds"),
        "the error should say why: {error}"
    );
}

#[tokio::test]
async fn eth_call_executes_contracts_and_discards_storage_changes() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let mut harness = Harness::start(&[(alice, one_eth() * U256::from(10u64))], true).await;
    let initcode = Bytes::from_static(&[
        0x60, 0x0b, 0x60, 0x0c, 0x60, 0x00, 0x39, 0x60, 0x0b, 0x60, 0x00, 0xf3, 0x60, 0x2a, 0x60,
        0x00, 0x55, 0x60, 0x00, 0x60, 0x00, 0xa0, 0x00,
    ]);
    let raw = sign_contract_creation(DEV_PRIVATE_KEY, U256::ZERO, 0, 100_000, initcode).unwrap();
    let hash: String = harness
        .client
        .request("eth_sendRawTransaction", rpc_params![raw.to_string()])
        .await
        .unwrap();
    harness
        .wait_for("deployment receipt", |chain| {
            chain.read().get_receipt(&hash.parse().unwrap()).is_some()
        })
        .await;
    let receipt: Value = harness
        .client
        .request("eth_getTransactionReceipt", rpc_params![hash])
        .await
        .unwrap();
    assert_eq!(receipt["status"], "0x1");
    let contract: Address = receipt["contractAddress"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();

    // Stop the background miner so its unrelated empty blocks cannot race the
    // read-only state assertions below.
    harness.chain.shutdown();
    if let Some(handle) = harness.miner.take() {
        handle.join().unwrap();
    }
    let before_block = harness.chain.read().get_block_number();
    let before_balance = harness
        .chain
        .read()
        .get_account_at(&alice, before_block)
        .unwrap()
        .balance;
    let before_nonce = harness
        .chain
        .read()
        .get_account_at(&alice, before_block)
        .unwrap()
        .nonce;
    let result: String = harness
        .client
        .request(
            "eth_call",
            rpc_params![
                serde_json::json!({
                    "from": alice,
                    "to": contract,
                    "gas": "0x186a0",
                }),
                "latest"
            ],
        )
        .await
        .unwrap();
    assert_eq!(result, "0x");
    assert_eq!(
        harness.chain.read().get_storage(&contract, &U256::ZERO),
        U256::ZERO
    );
    assert_eq!(harness.chain.read().get_block_number(), before_block);
    assert_eq!(
        harness
            .chain
            .read()
            .get_account_at(&alice, before_block)
            .unwrap()
            .balance,
        before_balance
    );
    assert_eq!(
        harness
            .chain
            .read()
            .get_account_at(&alice, before_block)
            .unwrap()
            .nonce,
        before_nonce
    );
}

#[tokio::test]
async fn eth_call_returns_contract_output_and_errors_on_revert() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let harness = Harness::start(&[(alice, one_eth() * U256::from(10u64))], true).await;
    // Return the 32-byte word 42.
    let initcode = Bytes::from_static(&[
        0x60, 0x0a, 0x60, 0x0c, 0x60, 0x00, 0x39, 0x60, 0x0a, 0x60, 0x00, 0xf3, 0x60, 0x2a, 0x60,
        0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3,
    ]);
    let raw = sign_contract_creation(DEV_PRIVATE_KEY, U256::ZERO, 0, 100_000, initcode).unwrap();
    let hash: String = harness
        .client
        .request("eth_sendRawTransaction", rpc_params![raw.to_string()])
        .await
        .unwrap();
    harness
        .wait_for("deployment receipt", |chain| {
            chain.read().get_receipt(&hash.parse().unwrap()).is_some()
        })
        .await;
    let receipt: Value = harness
        .client
        .request("eth_getTransactionReceipt", rpc_params![hash])
        .await
        .unwrap();
    let contract: Address = receipt["contractAddress"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();

    let result: String = harness
        .client
        .request(
            "eth_call",
            rpc_params![
                serde_json::json!({"to": contract, "input": "0x1234"}),
                "latest"
            ],
        )
        .await
        .unwrap();
    assert_eq!(result, format!("0x{}{}", "0".repeat(62), "2a"));

    let revert_initcode = Bytes::from_static(&[
        0x60, 0x05, 0x60, 0x0c, 0x60, 0x00, 0x39, 0x60, 0x05, 0x60, 0x00, 0xf3, 0x60, 0x00, 0x60,
        0x00, 0xfd,
    ]);
    let raw =
        sign_contract_creation(DEV_PRIVATE_KEY, U256::ZERO, 1, 100_000, revert_initcode).unwrap();
    let hash: String = harness
        .client
        .request("eth_sendRawTransaction", rpc_params![raw.to_string()])
        .await
        .unwrap();
    harness
        .wait_for("reverting contract deployment", |chain| {
            chain.read().get_receipt(&hash.parse().unwrap()).is_some()
        })
        .await;
    let receipt: Value = harness
        .client
        .request("eth_getTransactionReceipt", rpc_params![hash])
        .await
        .unwrap();
    let reverting: Address = receipt["contractAddress"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let result: Result<String, _> = harness
        .client
        .request(
            "eth_call",
            rpc_params![serde_json::json!({"to": reverting}), "latest"],
        )
        .await;
    let error = result.expect_err("a REVERT must be reported");
    assert!(
        error.to_string().contains("execution reverted"),
        "got: {error}"
    );
}

#[tokio::test]
async fn eth_estimate_gas_returns_transfer_cost_and_sufficient_contract_gas() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let harness = Harness::start(&[(alice, one_eth() * U256::from(10u64))], true).await;
    let estimate: String = harness
        .client
        .request(
            "eth_estimateGas",
            rpc_params![serde_json::json!({
                "from": alice,
                "to": Address::repeat_byte(0x22),
                "value": "0xde0b6b3a7640000"
            })],
        )
        .await
        .unwrap();
    assert_eq!(estimate, "0x5208", "21000 gas");

    let initcode = Bytes::from_static(&[
        0x60, 0x0b, 0x60, 0x0c, 0x60, 0x00, 0x39, 0x60, 0x0b, 0x60, 0x00, 0xf3, 0x60, 0x2a, 0x60,
        0x00, 0x55, 0x60, 0x00, 0x60, 0x00, 0xa0, 0x00,
    ]);
    let raw = sign_contract_creation(DEV_PRIVATE_KEY, U256::ZERO, 0, 100_000, initcode).unwrap();
    let hash: String = harness
        .client
        .request("eth_sendRawTransaction", rpc_params![raw.to_string()])
        .await
        .unwrap();
    harness
        .wait_for("contract deployment", |chain| {
            chain.read().get_receipt(&hash.parse().unwrap()).is_some()
        })
        .await;
    let receipt: Value = harness
        .client
        .request("eth_getTransactionReceipt", rpc_params![hash])
        .await
        .unwrap();
    let contract: Address = receipt["contractAddress"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();

    let estimate: String = harness
        .client
        .request(
            "eth_estimateGas",
            rpc_params![serde_json::json!({"from": alice, "to": contract})],
        )
        .await
        .unwrap();
    let gas = u64::from_str_radix(estimate.trim_start_matches("0x"), 16).unwrap();
    assert!(
        gas > 21_000,
        "contract execution adds gas beyond intrinsic cost: {gas}"
    );
    let call =
        sign_contract_call(DEV_PRIVATE_KEY, contract, U256::ZERO, 1, gas, Bytes::new()).unwrap();
    let hash: String = harness
        .client
        .request("eth_sendRawTransaction", rpc_params![call.to_string()])
        .await
        .unwrap();
    harness
        .wait_for("estimated-gas contract call", |chain| {
            chain.read().get_receipt(&hash.parse().unwrap()).is_some()
        })
        .await;
    let receipt: Value = harness
        .client
        .request("eth_getTransactionReceipt", rpc_params![hash])
        .await
        .unwrap();
    assert_eq!(
        receipt["status"], "0x1",
        "transaction with estimated gas must succeed"
    );
}

#[tokio::test]
async fn eth_call_rejects_invalid_gas_and_historical_evm_snapshots() {
    let harness = Harness::start(&[], true).await;
    let result: Result<String, _> = harness
        .client
        .request(
            "eth_call",
            rpc_params![serde_json::json!({
                "to": Address::repeat_byte(0x22), "gas": "0x1"
            })],
        )
        .await;
    assert!(result.is_err(), "gas below intrinsic must fail");
    harness
        .wait_for("a mined block", |chain| chain.read().get_block_number() > 0)
        .await;
    let result: Result<String, _> = harness
        .client
        .request(
            "eth_call",
            rpc_params![
                serde_json::json!({"to": Address::repeat_byte(0x22)}),
                "earliest"
            ],
        )
        .await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("historical EVM snapshots are unavailable")
    );
    let result: Result<String, _> = harness.client.request(
        "eth_call",
        rpc_params![serde_json::json!({ "to": Address::repeat_byte(0x22), "data": "0x01", "input": "0x02" })],
    ).await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("data and input fields disagree")
    );
}

#[tokio::test]
async fn an_unsupported_method_gets_the_standard_method_not_found_error() {
    let harness = Harness::start(&[], false).await;

    // `eth_gasPrice` is a real Ethereum method this node does not implement, so it
    // must come back as -32601 rather than something invented.
    let result: Result<Value, _> = harness.client.request("eth_gasPrice", rpc_params![]).await;
    let error = result.expect_err("an unregistered method must be an error");
    assert!(
        error.to_string().contains("Method not found") || error.to_string().contains("-32601"),
        "got: {error}"
    );
}

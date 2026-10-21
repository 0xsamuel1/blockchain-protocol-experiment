use alloy::primitives::{Address, B256, Bytes, TxKind, U256};
use jsonrpsee::{
    core::RpcResult,
    types::{ErrorObjectOwned, Params, error::ErrorCode},
};
use node::{
    evm::BreejaDb,
    executor::{SimulationError, execution_cfg, simulate_transaction},
    pool::intrinsic_gas,
};
use primitives::{
    constants::{BLOCK_GAS_LIMIT, CHAIN_ID, GAS_PRICE},
    node::BlockState,
    params::{AddressFilter, BlockTag, CallRequest, LogFilter, TopicFilter},
    response::{
        BlockResponse, LogResponse, TransactionReceiptResponse, TransactionResponse, data_bytes,
        data_hash, quantity,
    },
};
use revm::{
    context::{BlockEnv, CfgEnv, TxEnv},
    context_interface::result::ExecutionResult,
    primitives::hardfork::SpecId,
};
use serde_json::Value;

use crate::RPCContext;

/// JSON-RPC error code Ethereum clients use for "your request was understood but
/// is not valid", as opposed to a malformed one.
const INVALID_INPUT: i32 = -32000;

/// Turns an internal error into a JSON-RPC error object the client can read.
fn invalid_input(error: impl std::fmt::Display) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(INVALID_INPUT, error.to_string(), None::<()>)
}

fn invalid_params(error: impl std::fmt::Display) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(
        ErrorCode::InvalidParams.code(),
        error.to_string(),
        None::<()>,
    )
}

/// Parses a 32-byte hash parameter.
fn parse_hash(raw: &str) -> Result<B256, ErrorObjectOwned> {
    raw.parse::<B256>()
        .map_err(|e| invalid_params(format!("invalid 32-byte hash {raw:?}: {e}")))
}

/// Resolves a block tag against the chain head, erroring on a block that does not
/// exist rather than silently substituting the latest one.
fn resolve_tag(state: &BlockState, tag: &BlockTag) -> Result<u64, ErrorObjectOwned> {
    tag.resolve(state.get_block_number()).map_err(invalid_input)
}

/// The chain's id, used by wallets for EIP-155 replay protection.
pub fn chain_id_handler() -> String {
    quantity(CHAIN_ID)
}

/// The number of the chain head. A freshly started node sits at `0x0`, the genesis
/// block, and climbs from there as the miner seals blocks.
pub fn block_number_handler(context: &RPCContext) -> String {
    quantity(context.chain.read().get_block_number())
}

/// Renders a block, inlining full transaction objects if the caller asked for them.
fn render_block(state: &BlockState, number: u64, full_transactions: bool) -> Value {
    let Some(block) = state.get_block(number) else {
        return Value::Null;
    };

    let transactions: Vec<Value> = if full_transactions {
        block
            .transactions
            .iter()
            .filter_map(|hash| state.get_transaction(hash))
            .map(|tx| {
                serde_json::to_value(TransactionResponse::from(tx))
                    .expect("a transaction response always serializes")
            })
            .collect()
    } else {
        block
            .transactions
            .iter()
            .map(|hash| Value::String(data_hash(hash)))
            .collect()
    };

    serde_json::to_value(BlockResponse::from_block(block, transactions))
        .expect("a block response always serializes")
}

pub fn get_block_by_number_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<Value> {
    // Ethereum's optional trailing parameters have to be read one at a time: a
    // client may send just the block, or the block and the `fullTransactions`
    // flag, and both are valid.
    let mut sequence = params.sequence();
    let tag: BlockTag = sequence.next()?;
    let full_transactions = sequence.optional_next::<bool>()?.unwrap_or(false);

    let state = context.chain.read();
    let number = resolve_tag(&state, &tag)?;
    Ok(render_block(&state, number, full_transactions))
}

pub fn get_block_by_hash_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<Value> {
    let mut sequence = params.sequence();
    let hash = parse_hash(&sequence.next::<String>()?)?;
    let full_transactions = sequence.optional_next::<bool>()?.unwrap_or(false);

    let state = context.chain.read();
    // An unknown hash is `null`, not an error: the caller asked a legitimate
    // question and the answer is "no such block".
    let Some(number) = state.get_block_by_hash(&hash).map(|block| block.number) else {
        return Ok(Value::Null);
    };
    Ok(render_block(&state, number, full_transactions))
}

pub fn get_transaction_by_hash_handler(
    params: Params<'_>,
    context: &RPCContext,
) -> RpcResult<Value> {
    let hash = parse_hash(&params.one::<String>()?)?;

    let state = context.chain.read();
    match state.get_transaction(&hash) {
        Some(tx) => Ok(serde_json::to_value(TransactionResponse::from(tx))
            .expect("a transaction response always serializes")),
        None => Ok(Value::Null),
    }
}

/// A receipt exists only once a transaction has been mined. A transaction still
/// sitting in the mempool correctly returns `null` here, which is exactly how a
/// client polls for confirmation.
pub fn get_transaction_receipt_handler(
    params: Params<'_>,
    context: &RPCContext,
) -> RpcResult<Value> {
    let hash = parse_hash(&params.one::<String>()?)?;

    let state = context.chain.read();
    match state.get_receipt(&hash) {
        Some(receipt) => Ok(
            serde_json::to_value(TransactionReceiptResponse::from(receipt))
                .expect("a receipt response always serializes"),
        ),
        None => Ok(Value::Null),
    }
}

pub fn get_balance_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<String> {
    let mut sequence = params.sequence();
    let address: Address = sequence.next()?;
    let tag = sequence
        .optional_next::<BlockTag>()?
        .unwrap_or(BlockTag::Latest);

    let state = context.chain.read();
    let balance = match tag {
        // "pending" means "after everything in the mempool lands", which is what a
        // wallet wants before it builds the next transaction.
        BlockTag::Pending => state.get_pending_balance(&address),
        _ => {
            let number = resolve_tag(&state, &tag)?;
            state
                .get_account_at(&address, number)
                .map(|account| account.balance)
                .unwrap_or(U256::ZERO)
        }
    };

    Ok(quantity(balance))
}

/// The account's nonce: how many transactions it has sent.
pub fn get_transaction_count_handler(
    params: Params<'_>,
    context: &RPCContext,
) -> RpcResult<String> {
    let mut sequence = params.sequence();
    let address: Address = sequence.next()?;
    let tag = sequence
        .optional_next::<BlockTag>()?
        .unwrap_or(BlockTag::Latest);

    let state = context.chain.read();
    let nonce = match tag {
        BlockTag::Pending => state.get_pending_nonce(&address),
        _ => {
            let number = resolve_tag(&state, &tag)?;
            state
                .get_account_at(&address, number)
                .map(|account| account.nonce)
                .unwrap_or(0)
        }
    };

    Ok(quantity(nonce))
}

/// Returns runtime bytecode at a block, or `0x` for an EOA/unknown account.
pub fn get_code_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<String> {
    let mut sequence = params.sequence();
    let address: Address = sequence.next()?;
    let tag = sequence
        .optional_next::<BlockTag>()?
        .unwrap_or(BlockTag::Latest);

    let state = context.chain.read();
    let number = resolve_tag(&state, &tag)?;
    Ok(data_bytes(
        &state.get_code_at(&address, number).unwrap_or_default(),
    ))
}

/// Returns one EVM storage word as fixed-width 32-byte data.
pub fn get_storage_at_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<String> {
    let mut sequence = params.sequence();
    let address: Address = sequence.next()?;
    let raw_slot: String = sequence.next()?;
    let slot = raw_slot
        .parse::<U256>()
        .map_err(|error| invalid_params(format!("invalid storage slot {raw_slot:?}: {error}")))?;
    let tag = sequence
        .optional_next::<BlockTag>()?
        .unwrap_or(BlockTag::Latest);

    let state = context.chain.read();
    let number = resolve_tag(&state, &tag)?;
    let value = state
        .get_storage_at(&address, &slot, number)
        .unwrap_or_default();
    Ok(format!("{value:#066x}"))
}

fn address_matches(filter: &Option<AddressFilter>, address: &Address) -> bool {
    match filter {
        None => true,
        Some(AddressFilter::One(expected)) => expected == address,
        Some(AddressFilter::Many(expected)) => expected.contains(address),
    }
}

fn topics_match(filter: &[Option<TopicFilter>], topics: &[B256]) -> bool {
    filter.iter().enumerate().all(|(index, expected)| {
        let Some(expected) = expected else {
            return true;
        };
        let Some(actual) = topics.get(index) else {
            return false;
        };
        match expected {
            TopicFilter::One(expected) => actual == expected,
            TopicFilter::AnyOf(expected) => expected.contains(actual),
        }
    })
}

/// Scans canonical receipts in block order and applies Ethereum's positional
/// address/topic filter semantics. Breeja is small enough not to need a bloom
/// index yet; bloom construction belongs to Milestone 6.
pub fn get_logs_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<Vec<LogResponse>> {
    let filter: LogFilter = params.one()?;
    let state = context.chain.read();
    let head = state.get_block_number();

    let (from, to) = if let Some(hash) = filter.block_hash {
        if filter.from_block.is_some() || filter.to_block.is_some() {
            return Err(invalid_params(
                "blockHash cannot be combined with fromBlock or toBlock",
            ));
        }
        let Some(block) = state.get_block_by_hash(&hash) else {
            return Ok(Vec::new());
        };
        (block.number, block.number)
    } else {
        let from = resolve_tag(&state, &filter.from_block.unwrap_or(BlockTag::Latest))?;
        let to = resolve_tag(&state, &filter.to_block.unwrap_or(BlockTag::Latest))?;
        if from > to {
            return Err(invalid_params(format!(
                "fromBlock ({from}) is after toBlock ({to})"
            )));
        }
        (from, to.min(head))
    };

    let mut matches = Vec::new();
    for number in from..=to {
        let Some(block) = state.get_block(number) else {
            continue;
        };
        let mut block_log_index = 0u64;
        for (transaction_index, transaction_hash) in block.transactions.iter().enumerate() {
            let Some(receipt) = state.get_receipt(transaction_hash) else {
                continue;
            };
            for log in &receipt.logs {
                if address_matches(&filter.address, &log.address)
                    && topics_match(&filter.topics, &log.topics)
                {
                    matches.push(LogResponse::from_mined(
                        log,
                        block,
                        transaction_hash,
                        transaction_index as u64,
                        block_log_index,
                    ));
                }
                block_log_index += 1;
            }
        }
    }
    Ok(matches)
}

/// Breeja has a fixed pre-EIP-1559 gas price and no fee market.
pub fn gas_price_handler() -> String {
    quantity(GAS_PRICE)
}

/// Accepts a signed transaction, validates it, and queues it for mining.
///
/// The response is the transaction hash, returned the moment the transaction is
/// *accepted* — not when it is mined. The client then polls
/// `eth_getTransactionReceipt` until a receipt appears, which is the standard
/// Ethereum confirmation flow.
pub fn send_raw_transaction_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<String> {
    let raw: String = params.one()?;
    let raw: Bytes = raw
        .parse()
        .map_err(|e| invalid_params(format!("raw transaction is not valid hex: {e}")))?;

    let hash = context
        .chain
        .submit_raw_transaction(&raw)
        .map_err(invalid_input)?;

    Ok(data_hash(&hash))
}

struct PreparedCall {
    db: BreejaDb,
    block: BlockEnv,
    cfg: CfgEnv<SpecId>,
    base_tx: TxEnv,
    intrinsic: u64,
    requested_gas: Option<u64>,
}

fn prepare_call(
    request: CallRequest,
    tag: Option<BlockTag>,
    context: &RPCContext,
) -> Result<PreparedCall, ErrorObjectOwned> {
    let input = match (&request.data, &request.input) {
        (Some(data), Some(input)) if data != input => {
            return Err(invalid_params("data and input fields disagree"));
        }
        (Some(data), _) => data.clone(),
        (_, Some(input)) => input.clone(),
        _ => Bytes::new(),
    };
    let gas = request
        .gas
        .map(|gas| u64::try_from(gas).map_err(|_| invalid_params("gas exceeds u64")))
        .transpose()?;
    if gas.is_some_and(|gas| gas > BLOCK_GAS_LIMIT) {
        return Err(invalid_params(format!(
            "gas exceeds Breeja's block gas limit ({BLOCK_GAS_LIMIT})"
        )));
    }
    let gas_price = request
        .gas_price
        .map(|price| u128::try_from(price).map_err(|_| invalid_params("gasPrice exceeds u128")))
        .transpose()?
        .unwrap_or(GAS_PRICE);
    let value = request.value.unwrap_or(U256::ZERO);
    let to = request.to.map(TxKind::Call).unwrap_or(TxKind::Create);
    let intrinsic = intrinsic_gas(to, &input);

    let state = context.chain.read();
    let head = state
        .get_block(state.get_block_number())
        .expect("head block exists");
    let number = tag
        .as_ref()
        .map(|tag| resolve_tag(&state, tag))
        .transpose()?
        .unwrap_or(head.number);
    if number != head.number {
        return Err(invalid_input(
            "historical EVM snapshots are unavailable; use the current block",
        ));
    }
    let caller = request.from.unwrap_or(Address::ZERO);
    let nonce = state
        .get_account_at(&caller, head.number)
        .map(|account| account.nonce)
        .unwrap_or(0);
    let db = BreejaDb::from_state(&state);
    let block = BlockEnv {
        number: U256::from(head.number),
        beneficiary: context.chain.config().miner,
        timestamp: U256::from(head.timestamp),
        gas_limit: BLOCK_GAS_LIMIT,
        basefee: 0,
        difficulty: U256::from(head.difficulty),
        prevrandao: Some(head.hash),
        ..Default::default()
    };
    drop(state);

    let mut cfg = execution_cfg();
    // Calls ignore sender nonce and upfront balance requirements; their state is discarded.
    cfg.disable_nonce_check = true;
    cfg.disable_balance_check = true;
    let base_tx = TxEnv {
        tx_type: 0,
        caller,
        gas_limit: gas.unwrap_or(BLOCK_GAS_LIMIT),
        gas_price,
        kind: to,
        value,
        data: input,
        nonce,
        chain_id: Some(CHAIN_ID),
        ..Default::default()
    };
    Ok(PreparedCall {
        db,
        block,
        cfg,
        base_tx,
        intrinsic,
        requested_gas: gas,
    })
}

fn simulate(prepared: &PreparedCall, gas: u64) -> Result<ExecutionResult, SimulationError> {
    let mut tx = prepared.base_tx.clone();
    tx.gas_limit = gas;
    simulate_transaction(
        prepared.db.clone(),
        prepared.cfg.clone(),
        tx,
        prepared.block.clone(),
    )
}

fn execution_error(result: ExecutionResult) -> ErrorObjectOwned {
    match result {
        ExecutionResult::Revert { output, .. } => ErrorObjectOwned::owned(
            INVALID_INPUT,
            "execution reverted",
            Some(Value::String(format!("0x{}", alloy::hex::encode(output)))),
        ),
        ExecutionResult::Halt { reason, .. } => {
            invalid_input(format!("execution halted: {reason}"))
        }
        ExecutionResult::Success { .. } => unreachable!("success is not an error"),
    }
}

fn simulation_error(error: SimulationError) -> ErrorObjectOwned {
    match error {
        SimulationError::Invalid(message) => invalid_input(message),
        SimulationError::Internal(message) => {
            tracing::error!(%message, "EVM simulation failed internally");
            ErrorObjectOwned::owned(
                ErrorCode::InternalError.code(),
                "internal EVM simulation error",
                None::<()>,
            )
        }
    }
}

/// Executes a read-only call against the current chain snapshot.
pub fn call_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<String> {
    let mut sequence = params.sequence();
    let request: CallRequest = sequence.next()?;
    let tag = sequence.optional_next::<BlockTag>()?;
    let prepared = prepare_call(request, tag, context)?;
    let gas = prepared.requested_gas.unwrap_or(BLOCK_GAS_LIMIT);
    if gas < prepared.intrinsic {
        return Err(invalid_input(format!(
            "gas {gas} is below intrinsic gas ({})",
            prepared.intrinsic
        )));
    }
    match simulate(&prepared, gas).map_err(simulation_error)? {
        ExecutionResult::Success { output, .. } => {
            Ok(format!("0x{}", alloy::hex::encode(output.data())))
        }
        result => Err(execution_error(result)),
    }
}

/// Finds the least successful gas limit with independent snapshot probes.
pub fn estimate_gas_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<String> {
    let mut sequence = params.sequence();
    let request: CallRequest = sequence.next()?;
    let tag = sequence.optional_next::<BlockTag>()?;
    let prepared = prepare_call(request, tag, context)?;
    let upper = prepared.requested_gas.unwrap_or(BLOCK_GAS_LIMIT);
    if prepared.intrinsic > upper {
        return Err(invalid_input(format!(
            "gas limit {upper} is below intrinsic gas ({})",
            prepared.intrinsic
        )));
    }
    match simulate(&prepared, upper).map_err(simulation_error)? {
        ExecutionResult::Success { .. } => (),
        result => return Err(execution_error(result)),
    }
    let (mut low, mut high) = (prepared.intrinsic, upper);
    while low < high {
        let mid = low + (high - low) / 2;
        match simulate(&prepared, mid).map_err(simulation_error)? {
            ExecutionResult::Success { .. } => high = mid,
            ExecutionResult::Revert { .. } | ExecutionResult::Halt { .. } => low = mid + 1,
        }
    }
    // GAS-dependent bytecode makes this heuristic; verify the final bound and
    // use the proven successful cap if the minimum unexpectedly fails.
    let estimate = match simulate(&prepared, low).map_err(simulation_error)? {
        ExecutionResult::Success { .. } => low,
        ExecutionResult::Revert { .. } | ExecutionResult::Halt { .. } => upper,
    };
    Ok(quantity(estimate))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_filters_are_positional_with_wildcards_and_or_alternatives() {
        let a = B256::repeat_byte(0xaa);
        let b = B256::repeat_byte(0xbb);
        let c = B256::repeat_byte(0xcc);
        let actual = [a, b];

        assert!(topics_match(&[], &actual));
        assert!(topics_match(&[Some(TopicFilter::One(a))], &actual));
        assert!(!topics_match(&[Some(TopicFilter::One(b))], &actual));
        assert!(topics_match(&[None, Some(TopicFilter::One(b))], &actual));
        assert!(topics_match(
            &[Some(TopicFilter::AnyOf(vec![c, a]))],
            &actual
        ));
        assert!(!topics_match(
            &[Some(TopicFilter::AnyOf(Vec::new()))],
            &actual
        ));
        assert!(!topics_match(
            &[None, None, Some(TopicFilter::One(c))],
            &actual
        ));
    }

    #[test]
    fn address_arrays_use_or_semantics_and_empty_arrays_match_nothing() {
        let a = Address::repeat_byte(0xaa);
        let b = Address::repeat_byte(0xbb);
        assert!(address_matches(&None, &a));
        assert!(address_matches(&Some(AddressFilter::One(a)), &a));
        assert!(address_matches(&Some(AddressFilter::Many(vec![b, a])), &a));
        assert!(!address_matches(&Some(AddressFilter::Many(Vec::new())), &a));
    }
}

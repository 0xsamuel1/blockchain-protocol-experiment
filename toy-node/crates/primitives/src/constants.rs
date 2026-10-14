use alloy::primitives::{Address, U256, address};

/// Chain id used for EIP-155 replay protection. Signed transactions must carry
/// this id (or be a pre-EIP-155 legacy transaction with no id at all).
pub const CHAIN_ID: u64 = 1;

/// Address the JSON-RPC server binds to.
pub const RPC_ADDRESS: &str = "127.0.0.1:8545";

/// Starting difficulty, expressed as the number of leading zero *bits* the block
/// hash must have. Each extra bit doubles the expected work.
pub const DIFFICULTY_LEADING_ZEROS: u8 = 7;

/// Starting difficulty in `--dev` mode.
pub const DEV_DIFFICULTY_LEADING_ZEROS: u8 = 3;

/// How long the network wants to spend finding each block. The miner retargets
/// difficulty after every block to converge on this.
pub const TARGET_BLOCK_TIME_SECS: u64 = 15;

/// Target block time in `--dev` mode, so a lecture demo does not need patience.
pub const DEV_TARGET_BLOCK_TIME_SECS: u64 = 2;

/// Difficulty is clamped to this range so the chain can never become unmineable
/// or completely free.
pub const MIN_DIFFICULTY_LEADING_ZEROS: u8 = 1;
pub const MAX_DIFFICULTY_LEADING_ZEROS: u8 = 32;

/// Breeja predates EIP-1559, so gas price is a single fixed constant (1 gwei)
/// rather than a per-block base fee.
pub const GAS_PRICE: u128 = 1_000_000_000;

/// Intrinsic cost of a plain ETH transfer. Without an EVM this is the cost of
/// *every* transaction Breeja accepts.
pub const TRANSFER_GAS: u64 = 21_000;

/// Frontier's famous "pi million" block gas limit, which caps a block at
/// `3_141_592 / 21_000 = 149` transfers.
pub const BLOCK_GAS_LIMIT: u64 = 3_141_592;

/// Reward paid to the miner for sealing a block, on top of the transaction fees
/// it collects. 5 ETH, as on Frontier and Homestead.
pub const BLOCK_REWARD: U256 = U256::from_limbs([5_000_000_000_000_000_000, 0, 0, 0]);

/// Free-form bytes the miner stamps into each block it seals.
pub const EXTRA_DATA: &str = "breeja";

/// First well-known development account. This is Anvil/Hardhat account first dev account
pub const DEV_ADDRESS: Address = address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

/// The remaining pre-funded development accounts (Anvil/Hardhat accounts 1 - 3).
pub const DEV_ADDRESS_1: Address = address!("0x70997970C51812dc3A010C7d01b50e0d17dc79C8");
pub const DEV_ADDRESS_2: Address = address!("0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC");
pub const DEV_ADDRESS_3: Address = address!("0x90F79bf6EB2c4f870365E785982E1f101E93b906");

/// Balance every development account starts with: 10,000 ETH. (doing it this way so I can declare this as a const and not a const fn)
pub const DEV_BALANCE: U256 = U256::from_limbs([1_864_712_049_423_024_128, 542, 0, 0]);

/// The all-zero hash, used for the genesis block's parent and for empty roots.
pub const ZERO_HASH: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";

#[cfg(test)]
mod tests {
    use super::*;

    /// `U256` can only be built from raw limbs in a const, so pin the two money
    /// constants down with a test rather than trusting the hand-written limbs.
    #[test]
    fn money_constants_have_the_values_the_docs_claim() {
        let one_eth = U256::from(10u64).pow(U256::from(18));
        assert_eq!(BLOCK_REWARD, U256::from(5u64) * one_eth);
        assert_eq!(DEV_BALANCE, U256::from(10_000u64) * one_eth);
    }
}

use anchor_lang::prelude::*;
use anyhow::{anyhow, ensure, Result};
use pyth_solana_receiver_sdk::price_update::Price;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use spl_token_2022::{
    extension::transfer_fee::{TransferFeeConfig, MAX_FEE_BASIS_POINTS},
};

use byreal_clmm::libraries::{
    dynamic_fee_math::{
        calculate_dynamic_fee_rate as calculate_onchain_dynamic_fee_rate, calculate_imbalance_fee,
        calculate_total_dynamic_fee, calculate_trade_slippage_fee, normalize_trade_size,
        price_from_sqrt_price_x64, quote_amount_from_base, DynamicFeeInputs, DynamicFeeResult,
    },
    MulDiv,
};
use byreal_clmm::states::TICK_ARRAY_SEED;
use byreal_clmm::util::pyth::calculate_price_index;
pub use byreal_clmm::{
    libraries::{liquidity_math, swap_math, tick_math, MAX_SQRT_PRICE_X64, MIN_SQRT_PRICE_X64},
    states::{
        AmmConfig, DynTickArrayState, PoolState, PoolStatusBitIndex, TickArrayBitmapExtension,
        TickArrayState, TickState, TickUtils,
    },
};
use std::collections::BTreeSet;

// Program IDs
#[cfg(feature = "mainnet")]
pub const BYREAL_CLMM_PROGRAM: Pubkey =
    solana_sdk::pubkey!("REALQqNEomY6cQGZJUGwywTBD2UmDT32rZcNnfxQ5N2");

#[cfg(feature = "devnet")]
pub const BYREAL_CLMM_PROGRAM: Pubkey =
    solana_sdk::pubkey!("45iBNkaENereLKMjLm2LHkF3hpDapf6mnvrM5HWFg9cY");

#[cfg(not(any(feature = "mainnet", feature = "devnet")))]
pub const BYREAL_CLMM_PROGRAM: Pubkey =
    solana_sdk::pubkey!("REALQqNEomY6cQGZJUGwywTBD2UmDT32rZcNnfxQ5N2");

// Constants
pub const TICK_ARRAY_SIZE: i32 = 60;
const DYNAMIC_MAX_PYTH_AGE_SECONDS: i64 = 3600;

// The pinned math requires a price index. A zero index skips only arbitrage fee,
// using the same slippage/imbalance functions and fee cap as the program.
fn calculate_dynamic_fee_rate(inputs: &DynamicFeeInputs) -> Result<DynamicFeeResult> {
    if inputs.p_index != 0 {
        return Ok(calculate_onchain_dynamic_fee_rate(inputs)?);
    }
    let trade_slippage_fee = calculate_trade_slippage_fee(
        inputs.trade_size,
        inputs.trade_slippage_fee_base_milli_bp,
        inputs.trade_slippage_fee_trade_size_threshold,
    )?;
    let imbalance_fee = calculate_imbalance_fee(
        inputs.quote_value_of_base,
        inputs.quote_balance,
        inputs.imbalance_fee_base_tenths_of_bp,
        inputs.imbalance_fee_x,
        inputs.is_buying_base,
    )?;
    let swap_dynamic_fee = calculate_total_dynamic_fee(0, trade_slippage_fee, imbalance_fee)?;
    let total_fee_rate = inputs
        .fee_base
        .checked_add(swap_dynamic_fee)
        .ok_or_else(|| anyhow!("dynamic fee total rate overflow"))?;
    ensure!(
        total_fee_rate < byreal_clmm::states::config::FEE_RATE_DENOMINATOR_VALUE,
        "dynamic fee total rate exceeds fee denominator"
    );
    Ok(DynamicFeeResult {
        arbitrage_fee: 0,
        trade_slippage_fee,
        imbalance_fee,
        swap_dynamic_fee,
        total_fee_rate,
    })
}

/// exact-output trade_size cap multiplier; mirrors the contract's private
/// `EXACT_OUTPUT_TRADE_SIZE_CAP_MULTIPLIER` in programs/amm/src/states/pool.rs.
const EXACT_OUTPUT_TRADE_SIZE_CAP_MULTIPLIER: u128 = 3;
/// Fast-path iterations for the exact-output dynamic-fee fixed-point. The common
/// case (typical pools) converges in 2-3 naive steps; if it has not converged
/// within this many, we fall back to a bounded exponential-bracket + binary
/// search (step count bounded by the u64 bit width — no unproved iteration cap).
const EXACT_OUT_FEE_FAST_ITERS: u32 = 4;

#[derive(Clone)]
pub enum DynamicTickArrayState {
    Dynamic((DynTickArrayState, Vec<TickState>)),
    Fixed(TickArrayState),
}

impl DynamicTickArrayState {
    /// Decode a dynamic tick array from raw bytes into header + tick slice views.
    fn decode_dyn_tick_array(data: &[u8]) -> Option<(DynTickArrayState, Vec<TickState>)> {
        if data.len() < 8 {
            return None;
        }
        if &data[0..8] != DynTickArrayState::DISCRIMINATOR {
            return None;
        }
        if data.len() < DynTickArrayState::HEADER_LEN {
            return None;
        }

        let header_bytes = &data[8..(DynTickArrayState::HEADER_LEN)];
        let header: &DynTickArrayState = bytemuck::from_bytes(header_bytes);
        let ticks_bytes = &data[DynTickArrayState::HEADER_LEN..];
        // Safety: TickState derives AnyBitPattern in the CLMM crate
        let ticks: &[TickState] = bytemuck::try_cast_slice(ticks_bytes).ok()?;
        Some((*header, ticks.to_vec()))
    }

    /// Decode a fixed tick array from raw bytes using Anchor deserialization.
    fn decode_fixed_tick_array(data: &[u8]) -> Option<TickArrayState> {
        TickArrayState::try_deserialize(&mut data.to_vec().as_slice()).ok()
    }

    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < 8 {
            return None;
        }

        let discriminator = &data[0..8];

        if discriminator == DynTickArrayState::DISCRIMINATOR {
            Self::decode_dyn_tick_array(data).map(|(header, ticks)| Self::Dynamic((header, ticks)))
        } else if discriminator == TickArrayState::DISCRIMINATOR {
            Self::decode_fixed_tick_array(data).map(Self::Fixed)
        } else {
            None
        }
    }

    /// Get the start tick index for this tick array
    pub fn start_tick_index(&self) -> i32 {
        match self {
            DynamicTickArrayState::Dynamic((header, _)) => header.start_tick_index,
            DynamicTickArrayState::Fixed(ta) => ta.start_tick_index,
        }
    }

    pub fn next_initialized_tick(
        &self,
        cur_tick: i32,
        spacing: u16,
        zero_for_one: bool,
    ) -> Option<i32> {
        match self {
            DynamicTickArrayState::Dynamic((header, ticks)) => {
                if let Ok(Some(local_idx)) =
                    header.next_initialized_tick_index(ticks, cur_tick, spacing, zero_for_one)
                {
                    let idx = local_idx as usize;
                    return Some(ticks[idx].tick);
                }

                None
            }
            DynamicTickArrayState::Fixed(ta) => {
                let mut ta = ta.clone();
                if let Ok(Some(ts)) = ta.next_initialized_tick(cur_tick, spacing, zero_for_one) {
                    return Some(ts.tick);
                }

                None
            }
        }
    }

    pub fn first_initialized_tick(&self, zero_for_one: bool) -> Option<i32> {
        match self {
            DynamicTickArrayState::Dynamic((header, ticks)) => {
                if let Ok(local_idx) = header.first_initialized_tick_index(ticks, zero_for_one) {
                    let idx = local_idx as usize;
                    return Some(ticks[idx].tick);
                }
                None
            }
            DynamicTickArrayState::Fixed(ta) => {
                let mut ta = ta.clone();
                if let Ok(ts) = ta.first_initialized_tick(zero_for_one) {
                    return Some(ts.tick);
                }

                None
            }
        }
    }

    /// Get liquidity_net for a given tick index
    pub fn get_tick_liquidity_net(&self, tick_index: i32, spacing: u16) -> Option<i128> {
        match self {
            DynamicTickArrayState::Dynamic((header, ticks)) => {
                if let Ok(i) = header.get_tick_index_in_array(tick_index, spacing) {
                    return Some(ticks[i as usize].liquidity_net);
                }
                None
            }
            DynamicTickArrayState::Fixed(ta) => {
                if let Ok(offset) = ta.get_tick_offset_in_array(tick_index, spacing) {
                    return Some(ta.ticks[offset].liquidity_net);
                }
                None
            }
        }
    }
}

#[derive(Clone)]
pub struct ByrealClmmAmm {
    /// Pool account key
    pub key: Pubkey,
    /// Pool state
    pub pool_state: PoolState,
    /// AMM config
    pub amm_config: AmmConfig,
    /// Bitmap extension
    pub bitmap_extension: Option<TickArrayBitmapExtension>,
    pub max_one_side_tick_arrays: usize,
    pub dynamic_tick_arrays: HashMap<Pubkey, DynamicTickArrayState>,
    pub token0_vault_amount: u64,
    pub token1_vault_amount: u64,
    pub token0_pyth_price: Option<Price>,
    pub token1_pyth_price: Option<Price>,
    pub token0_transfer_fee_config: Option<TransferFeeConfig>,
    pub token1_transfer_fee_config: Option<TransferFeeConfig>,
}

impl ByrealClmmAmm {
    /// Find next initialized tick using a navigation state that mirrors the
    /// on-chain `swap_internal` logic: walk within the current tick array
    /// using `next_initialized_tick`, optionally fall back to
    /// `first_initialized_tick`, and then advance across arrays via
    /// `next_initialized_tick_array_start_index` when necessary.
    pub fn find_next_initialized_tick_with_nav(
        &self,
        current_tick: i32,
        zero_for_one: bool,
        nav: &mut TickNavState,
    ) -> Result<i32> {
        let spacing = self.pool_state.tick_spacing as u16;

        loop {
            let start_index = nav.current_valid_tick_array_start_index;
            let addr = self.get_tick_array_address(start_index);

            let tick_array = self.dynamic_tick_arrays.get(&addr).ok_or_else(|| {
                anyhow!("Missing tick array data for start_index {}", start_index)
            })?;

            // 1. Try next_initialized_tick within the current array.
            if let Some(t) = tick_array.next_initialized_tick(current_tick, spacing, zero_for_one) {
                return Ok(t);
            }

            // 2. If nothing found and we haven't yet matched pool_current_tick_array,
            // fall back to first_initialized_tick on this array (mirrors on-chain).
            if !nav.is_match_pool_current_tick_array {
                nav.is_match_pool_current_tick_array = true;
                if let Some(t) = tick_array.first_initialized_tick(zero_for_one) {
                    return Ok(t);
                }
            }

            // 3. Still nothing: advance to the next initialized tick array and
            // immediately take its first_initialized_tick (exactly like the
            // on-chain `swap_internal` implementation).
            let next_arr = self.pool_state.next_initialized_tick_array_start_index(
                &self.bitmap_extension,
                nav.current_valid_tick_array_start_index,
                zero_for_one,
            )?;
            let next_start = match next_arr {
                Some(s) => s,
                None => {
                    return Err(anyhow!(
                        "Liquidity insufficient: no further initialized tick arrays"
                    ))
                }
            };
            nav.current_valid_tick_array_start_index = next_start;
            let next_addr = self.get_tick_array_address(next_start);

            let next_tick_array = self.dynamic_tick_arrays.get(&next_addr).ok_or_else(|| {
                anyhow!(
                    "Missing tick array data for advanced start_index {}",
                    next_start
                )
            })?;

            if let Some(t) = next_tick_array.first_initialized_tick(zero_for_one) {
                return Ok(t);
            }
        }
    }

    /// Initialize tick navigation state using the same helper as on-chain
    /// `swap_internal`, i.e. starting from the first initialized tick array
    /// in the given direction and tracking whether that array matches the
    /// pool's current tick array.
    pub fn init_tick_nav_state(&self, zero_for_one: bool) -> Result<TickNavState> {
        let (is_match, first_start) = self
            .pool_state
            .get_first_initialized_tick_array(&self.bitmap_extension, zero_for_one)?;
        Ok(TickNavState {
            is_match_pool_current_tick_array: is_match,
            current_valid_tick_array_start_index: first_start,
        })
    }

    /// Get liquidity_net for a given tick index from the cached tick arrays.
    pub fn get_tick_liquidity_net(&self, tick_index: i32) -> Option<i128> {
        let spacing = self.pool_state.tick_spacing as u16;
        let start = TickUtils::get_array_start_index(tick_index, spacing);
        let addr = self.get_tick_array_address(start);
        self.dynamic_tick_arrays
            .get(&addr)?
            .get_tick_liquidity_net(tick_index, spacing)
    }

    /// Get the tick array PDA address
    pub fn get_tick_array_address(&self, start_index: i32) -> Pubkey {
        Pubkey::find_program_address(
            &[
                TICK_ARRAY_SEED.as_bytes(),
                self.key.as_ref(),
                &start_index.to_be_bytes(),
            ],
            &BYREAL_CLMM_PROGRAM,
        )
        .0
    }

    /// Get tick array addresses around current price using bitmap navigation (both directions).
    /// Fallback to adjacent offsets if bitmap helpers are unavailable.
    pub fn get_all_tick_array_addresses(&self) -> Vec<Pubkey> {
        let mut start_indexes: BTreeSet<i32> = BTreeSet::new();

        let mut collect_dir = |zero_for_one: bool, limit: usize| {
            if limit == 0 {
                return;
            }
            if let Ok((_, mut start)) = self
                .pool_state
                .get_first_initialized_tick_array(&self.bitmap_extension, zero_for_one)
            {
                start_indexes.insert(start);
                for _ in 1..limit {
                    match self.pool_state.next_initialized_tick_array_start_index(
                        &self.bitmap_extension,
                        start,
                        zero_for_one,
                    ) {
                        Ok(Some(next)) => {
                            start_indexes.insert(next);
                            start = next;
                        }
                        _ => break,
                    }
                }
            }
        };
        let overflow_default = self
            .pool_state
            .is_overflow_default_tickarray_bitmap(vec![self.pool_state.tick_current]);
        let can_use_bitmap_helpers = self.bitmap_extension.is_some() || !overflow_default;
        if can_use_bitmap_helpers {
            collect_dir(true, self.max_one_side_tick_arrays);
            collect_dir(false, self.max_one_side_tick_arrays);
        }

        let tick_spacing = self.pool_state.tick_spacing;
        let current_tick = self.pool_state.tick_current;
        let current_start_index = TickUtils::get_array_start_index(current_tick, tick_spacing);
        start_indexes.insert(current_start_index);
        for i in 1..self.max_one_side_tick_arrays {
            let offset = (TICK_ARRAY_SIZE * i as i32) * i32::from(tick_spacing);
            start_indexes.insert(current_start_index.saturating_sub(offset));
            start_indexes.insert(current_start_index.saturating_add(offset));
        }

        start_indexes
            .into_iter()
            .map(|s| self.get_tick_array_address(s))
            .collect()
    }

    /// Check if decay fee is enabled
    pub fn is_decay_fee_enabled(&self) -> bool {
        self.pool_state.decay_fee_flag & (1 << 0) != 0
    }

    /// Check if decay fee is enabled for selling mint0
    pub fn is_decay_fee_on_sell_mint0(&self) -> bool {
        self.pool_state.decay_fee_flag & (1 << 1) != 0
    }

    /// Check if decay fee is enabled for selling mint1
    pub fn is_decay_fee_on_sell_mint1(&self) -> bool {
        self.pool_state.decay_fee_flag & (1 << 2) != 0
    }

    /// Calculate decay fee rate based on current timestamp
    /// Returns fee rate in hundredths of a bip (10^-6)
    pub fn get_decay_fee_rate(&self, current_timestamp: u64) -> Option<u32> {
        if !self.is_decay_fee_enabled() {
            return Some(0u32);
        }

        // Not open yet
        if current_timestamp < self.pool_state.open_time {
            return Some(0u32);
        }

        // Check for zero interval to avoid division by zero
        if self.pool_state.decay_fee_decrease_interval == 0 {
            return Some(0u32);
        }

        let interval_count = (current_timestamp - self.pool_state.open_time)
            / self.pool_state.decay_fee_decrease_interval as u64;
        let decay_fee_decrease_rate = self.pool_state.decay_fee_decrease_rate as u64 * 10_000;

        // 10^6 (FEE_RATE_DENOMINATOR_VALUE)
        let hundredths_of_a_bip = 1_000_000u64;
        let mut rate = hundredths_of_a_bip;

        // Fast power calculation: (1 - x)^c
        // x = decay_fee_decrease_rate / 10^6
        // c = interval_count
        {
            let mut exp = interval_count;
            let mut base = hundredths_of_a_bip.saturating_sub(decay_fee_decrease_rate);

            while exp > 0 {
                if exp % 2 == 1 {
                    rate = rate.mul_div_ceil(base, hundredths_of_a_bip)?;
                }
                base = base.mul_div_ceil(base, hundredths_of_a_bip)?;
                exp /= 2;
            }
        }

        // Convert from percentage to hundredths of a bip
        rate = rate.mul_div_ceil(self.pool_state.decay_fee_init_fee_rate as u64, 100u64)?;

        Some(rate as u32)
    }

    fn load_dynamic_pyth_prices(&self, current_timestamp: i64) -> Result<Option<(Price, Price)>> {
        if self.pool_state.token0_pyth_feed_id == [0; 32]
            || self.pool_state.token1_pyth_feed_id == [0; 32]
        {
            return Ok(None);
        }
        let token0_price = *self
            .token0_pyth_price
            .as_ref()
            .ok_or_else(|| anyhow!("dynamic fee token0 pyth price missing"))?;
        let token1_price = *self
            .token1_pyth_price
            .as_ref()
            .ok_or_else(|| anyhow!("dynamic fee token1 pyth price missing"))?;

        if token0_price.price <= 0 || token1_price.price <= 0 {
            return Ok(None);
        }

        let oldest_allowed = current_timestamp - DYNAMIC_MAX_PYTH_AGE_SECONDS;
        if token0_price.publish_time < oldest_allowed || token1_price.publish_time < oldest_allowed
        {
            return Ok(None);
        }

        Ok(Some((token0_price, token1_price)))
    }

    fn calculate_transfer_fee(
        config: Option<&TransferFeeConfig>,
        epoch: u64,
        pre_fee_amount: u64,
    ) -> Result<u64> {
        match config {
            Some(config) => config
                .calculate_epoch_fee(epoch, pre_fee_amount)
                .ok_or_else(|| anyhow!("transfer fee calculation overflow")),
            None => Ok(0),
        }
    }

    fn calculate_transfer_inverse_fee(
        config: Option<&TransferFeeConfig>,
        epoch: u64,
        post_fee_amount: u64,
    ) -> Result<u64> {
        let config = match config {
            Some(config) => config,
            None => return Ok(0),
        };

        let transfer_fee = config.get_epoch_fee(epoch);
        if u16::from(transfer_fee.transfer_fee_basis_points) == MAX_FEE_BASIS_POINTS {
            return Ok(u64::from(transfer_fee.maximum_fee));
        }

        let fee = config
            .calculate_inverse_epoch_fee(epoch, post_fee_amount)
            .ok_or_else(|| anyhow!("transfer inverse fee calculation overflow"))?;
        let check_fee = config
            .calculate_epoch_fee(
                epoch,
                post_fee_amount
                    .checked_add(fee)
                    .ok_or_else(|| anyhow!("transfer fee gross amount overflow"))?,
            )
            .ok_or_else(|| anyhow!("transfer inverse fee verification overflow"))?;
        if fee != check_fee {
            return Err(anyhow!("transfer fee calculate not match"));
        }
        Ok(fee)
    }

    fn compute_trade_fee_rate(
        &self,
        zero_for_one: bool,
        amount_specified: u64,
        is_base_input: bool,
        current_timestamp: i64,
    ) -> Result<u32> {
        self.dynamic_fee_rate(zero_for_one, amount_specified, is_base_input, None, current_timestamp)
    }

    /// Dynamic fee rate. For exact-output, `exact_out_amount_in_max` is the swap ix's
    /// `other_amount_threshold`; it drives trade_size = min(amount_in_max_quote, output*3),
    /// exactly as the on-chain contract does. Exact-input ignores it.
    fn dynamic_fee_rate(
        &self,
        zero_for_one: bool,
        amount_specified: u64,
        is_base_input: bool,
        exact_out_amount_in_max: Option<u64>,
        current_timestamp: i64,
    ) -> Result<u32> {
        let fee_rate = self
            .pool_state
            .calculate_base_trade_fee_rate(&self.amm_config, zero_for_one, current_timestamp as u64)
            .map_err(|e| anyhow!("base trade fee computation failed: {e}"))?;

        if !self.pool_state.is_swap_dynamic_fee_enabled() {
            return Ok(fee_rate);
        }

        let p_index = match self.load_dynamic_pyth_prices(current_timestamp)? {
            Some((token0_price, token1_price)) => calculate_price_index(
                &token0_price,
                &token1_price,
                self.pool_state.mint_decimals_0,
                self.pool_state.mint_decimals_1,
            )?,
            None => 0,
        };
        let p_0 = price_from_sqrt_price_x64(self.pool_state.sqrt_price_x64)?;

        let token1_as_quote = self.pool_state.is_token1_quote();
        let input_is_quote = if token1_as_quote {
            !zero_for_one
        } else {
            zero_for_one
        };
        let is_buying_base = input_is_quote;

        let quote_amount = if is_base_input {
            if input_is_quote {
                amount_specified as u128
            } else {
                quote_amount_from_base(amount_specified as u128, p_0, token1_as_quote)?
            }
        } else {
            // exact-output: mirror the contract (pool.rs::calculate_dynamic_fee_rate) —
            // trade_size = min(amount_in_max_quote, output_quote * CAP), where amount_in_max is
            // the SAME value the swap ix carries as other_amount_threshold (for Jupiter that is
            // the reported in_amount, no slippage).
            let amount_in_max = exact_out_amount_in_max
                .ok_or_else(|| anyhow!("exact-output dynamic fee requires amount_in_max"))?
                as u128;
            let output_quote = if input_is_quote {
                quote_amount_from_base(amount_specified as u128, p_0, token1_as_quote)?
            } else {
                amount_specified as u128
            };
            let input_max_quote = if input_is_quote {
                amount_in_max
            } else {
                quote_amount_from_base(amount_in_max, p_0, token1_as_quote)?
            };
            let cap = output_quote
                .checked_mul(EXACT_OUTPUT_TRADE_SIZE_CAP_MULTIPLIER)
                .unwrap_or(u128::MAX);
            std::cmp::min(input_max_quote, cap)
        };

        let quote_decimals = if token1_as_quote {
            self.pool_state.mint_decimals_1
        } else {
            self.pool_state.mint_decimals_0
        };
        let trade_size = normalize_trade_size(quote_amount, quote_decimals)?;

        let token0_vault_amount = self.token0_vault_amount as u128;
        let token1_vault_amount = self.token1_vault_amount as u128;
        let (quote_value_of_base, quote_balance) = if token1_as_quote {
            let base_amount = token0_vault_amount;
            let quote_balance = token1_vault_amount;
            (
                quote_amount_from_base(base_amount, p_0, true)?,
                quote_balance,
            )
        } else {
            let base_amount = token1_vault_amount;
            let quote_balance = token0_vault_amount;
            (
                quote_amount_from_base(base_amount, p_0, false)?,
                quote_balance,
            )
        };

        let dynamic = calculate_dynamic_fee_rate(&DynamicFeeInputs {
            p_0,
            p_index,
            trade_size,
            quote_value_of_base,
            quote_balance,
            is_buying_base,
            fee_base: fee_rate,
            arbitrage_fee_buffer_ppm: self.pool_state.arbitrage_fee_buffer_ppm,
            trade_slippage_fee_base_milli_bp: self.pool_state.trade_slippage_fee_base_milli_bp,
            trade_slippage_fee_trade_size_threshold: self
                .pool_state
                .trade_slippage_fee_trade_size_threshold,
            imbalance_fee_base_tenths_of_bp: self.pool_state.imbalance_fee_base_tenths_of_bp,
            imbalance_fee_x: self.pool_state.imbalance_fee_x,
        })?;

        Ok(dynamic.total_fee_rate)
    }

    /// Compute swap for the given parameters
    pub fn compute_swap(
        &self,
        zero_for_one: bool,
        amount_specified: u64,
        is_base_input: bool,
        sqrt_price_limit_x64: Option<u128>,
        current_timestamp: i64,
        current_epoch: u64,
    ) -> Result<SwapResult> {
        if amount_specified == 0 {
            return Err(anyhow!("zero amount specified"));
        }
        if current_timestamp < 0 {
            return Err(anyhow!("invalid negative timestamp"));
        }
        if current_timestamp as u64 <= self.pool_state.open_time {
            return Err(anyhow!("Pool is not open yet"));
        }
        if !self.pool_state.get_status_by_bit(PoolStatusBitIndex::Swap) {
            return Err(anyhow!("Pool swap is not approved"));
        }

        let sqrt_price_limit = sqrt_price_limit_x64.unwrap_or_else(|| {
            if zero_for_one {
                MIN_SQRT_PRICE_X64 + 1
            } else {
                MAX_SQRT_PRICE_X64 - 1
            }
        });

        let (input_transfer_config, output_transfer_config) = if zero_for_one {
            (
                self.token0_transfer_fee_config.as_ref(),
                self.token1_transfer_fee_config.as_ref(),
            )
        } else {
            (
                self.token1_transfer_fee_config.as_ref(),
                self.token0_transfer_fee_config.as_ref(),
            )
        };

        let (amount_calculate_specified, specified_transfer_fee) = if is_base_input {
            let transfer_fee = Self::calculate_transfer_fee(
                input_transfer_config,
                current_epoch,
                amount_specified,
            )?;
            (
                amount_specified
                    .checked_sub(transfer_fee)
                    .ok_or_else(|| anyhow!("transfer fee exceeds specified amount"))?,
                transfer_fee,
            )
        } else {
            let transfer_fee = Self::calculate_transfer_inverse_fee(
                output_transfer_config,
                current_epoch,
                amount_specified,
            )?;
            (
                amount_specified
                    .checked_add(transfer_fee)
                    .ok_or_else(|| anyhow!("transfer fee adjusted amount overflow"))?,
                transfer_fee,
            )
        };

        let result = if !is_base_input && self.pool_state.is_swap_dynamic_fee_enabled() {
            // exact-output + dynamic fee: on-chain trade_size = min(other_amount_threshold_quote,
            // output*3), and the swap ix carries other_amount_threshold = the reported in_amount
            // (no slippage; route-level slippage is applied outside the AMM). in_amount depends on
            // the fee rate and the fee rate depends on in_amount, so resolve a MONOTONIC
            // LEAST-FIXED-POINT: start the threshold at 0 (=> slippage_fee 0) and iterate; in_amount
            // is non-decreasing and bounded, so it converges to the self-consistent value where the
            // reported in_amount equals the threshold the fee was computed with.
            // f(t) = simulate(fee(min(t_quote, output*3))).amount_in, where t is the candidate
            // other_amount_threshold (= reported in_amount). f is NON-DECREASING in t, and one can
            // show f(t) >= t for all t <= t* (the least fixed point), with equality at t*. We solve
            // for t* with a guaranteed-terminating search.
            let eval = |amount_in_max: u64| -> Result<SwapResult> {
                let fee_rate = self.dynamic_fee_rate(
                    zero_for_one,
                    amount_specified,
                    false,
                    Some(amount_in_max),
                    current_timestamp,
                )?;
                self.simulate_swap_steps(
                    fee_rate,
                    amount_calculate_specified,
                    sqrt_price_limit,
                    is_base_input,
                    zero_for_one,
                    current_timestamp,
                    current_epoch,
                    input_transfer_config,
                    output_transfer_config,
                    specified_transfer_fee,
                )
            };

            // Fast path: naive monotonic iteration from 0 (converges in 2-3 steps for typical pools).
            let mut lo = 0u64;
            let mut converged: Option<SwapResult> = None;
            for _ in 0..EXACT_OUT_FEE_FAST_ITERS {
                let r = eval(lo)?;
                if r.amount_in == lo {
                    converged = Some(r);
                    break;
                }
                lo = r.amount_in; // monotonic: f(lo) >= lo, strictly increasing toward t*
            }
            match converged {
                Some(r) => r,
                None => {
                    // Slow path (rare, high-fee pools): `lo` is a lower bound with f(lo) > lo.
                    // Grow an upper bracket `hi` until f(hi) <= hi OR f(hi) errors — both mean
                    // hi >= t*. A fee-cap error means the threshold is too high, NOT that the swap is
                    // invalid, so it must NOT be propagated during the search; the doubling saturates
                    // at u64::MAX so it always terminates. Then binary search [lo, hi] for the least
                    // fixed point, treating an eval error as "mid > t*" (search lower). Only the final
                    // t* eval propagates (a genuine rejection the contract would share).
                    let mut hi = lo.max(1);
                    while matches!(eval(hi), Ok(r) if r.amount_in > hi) {
                        if hi == u64::MAX {
                            break;
                        }
                        hi = hi.saturating_mul(2);
                    }
                    while lo < hi {
                        let mid = lo + (hi - lo) / 2;
                        let above = matches!(eval(mid), Ok(r) if r.amount_in > mid);
                        if above {
                            lo = mid + 1;
                        } else {
                            hi = mid;
                        }
                    }
                    let r = eval(lo)?;
                    // For a non-decreasing f, the least t with f(t) <= t is necessarily a fixed
                    // point; verify to guard the monotonicity assumption.
                    if r.amount_in != lo {
                        return Err(anyhow!(
                            "exact-out dynamic-fee fixed point not self-consistent (in_amount={}, threshold={})",
                            r.amount_in,
                            lo
                        ));
                    }
                    r
                }
            }
        } else {
            let fee_rate = self.compute_trade_fee_rate(
                zero_for_one,
                amount_specified,
                is_base_input,
                current_timestamp,
            )?;
            self.simulate_swap_steps(
                fee_rate,
                amount_calculate_specified,
                sqrt_price_limit,
                is_base_input,
                zero_for_one,
                current_timestamp,
                current_epoch,
                input_transfer_config,
                output_transfer_config,
                specified_transfer_fee,
            )?
        };
        Ok(result)
    }

    /// Run the swap-step loop for a GIVEN fee_rate and finalize amounts (including transfer
    /// fees). Factored out of `compute_swap` so the exact-output dynamic-fee fixed-point can
    /// re-run it with successive fee rates.
    #[allow(clippy::too_many_arguments)]
    fn simulate_swap_steps(
        &self,
        fee_rate: u32,
        amount_calculate_specified: u64,
        sqrt_price_limit: u128,
        is_base_input: bool,
        zero_for_one: bool,
        current_timestamp: i64,
        current_epoch: u64,
        input_transfer_config: Option<&TransferFeeConfig>,
        output_transfer_config: Option<&TransferFeeConfig>,
        specified_transfer_fee: u64,
    ) -> Result<SwapResult> {
        // Initialize swap state
        let mut state = SwapState {
            amount_specified_remaining: amount_calculate_specified,
            amount_calculated: 0,
            sqrt_price_x64: self.pool_state.sqrt_price_x64,
            tick: self.pool_state.tick_current,
            liquidity: self.pool_state.liquidity,
            fee_amount: 0,
        };

        // Initialize tick-array navigation state so that tick discovery mirrors
        // the on-chain `swap_internal` helper logic.
        let mut nav = self.init_tick_nav_state(zero_for_one)?;

        // Simulate swap steps
        while state.amount_specified_remaining != 0 && state.sqrt_price_x64 != sqrt_price_limit {
            // Find next initialized tick
            let next_tick =
                self.find_next_initialized_tick_with_nav(state.tick, zero_for_one, &mut nav)?;

            let sqrt_price_next = tick_math::get_sqrt_price_at_tick(next_tick)
                .map_err(|e| anyhow!("Failed to get sqrt price at tick {}: {}", next_tick, e))?;

            let target_price = if (zero_for_one && sqrt_price_next < sqrt_price_limit)
                || (!zero_for_one && sqrt_price_next > sqrt_price_limit)
            {
                sqrt_price_limit
            } else {
                sqrt_price_next
            };

            // Compute swap step
            let step = swap_math::compute_swap_step(
                state.sqrt_price_x64,
                target_price,
                state.liquidity,
                state.amount_specified_remaining,
                fee_rate,
                is_base_input,
                zero_for_one,
                current_timestamp as u32,
            )
            .map_err(|e| anyhow!("Swap step computation failed: {:?}", e))?;

            // Update state
            state.sqrt_price_x64 = step.sqrt_price_next_x64;
            if state.liquidity > 0 {
                state.fee_amount = state
                    .fee_amount
                    .checked_add(step.fee_amount)
                    .ok_or_else(|| anyhow!("compute_swap: fee_amount overflow"))?;
            }

            if is_base_input {
                let step_amount_in_with_fee =
                    step.amount_in
                        .checked_add(step.fee_amount)
                        .ok_or_else(|| {
                            anyhow!(
                                "compute_swap: step.amount_in + fee_amount overflow (exact in)"
                            )
                        })?;
                state.amount_specified_remaining = state
                    .amount_specified_remaining
                    .checked_sub(step_amount_in_with_fee)
                    .ok_or_else(|| {
                        anyhow!(
                            "compute_swap: step.amount_in + fee_amount exceeds remaining (exact in)"
                        )
                    })?;
                state.amount_calculated = state
                    .amount_calculated
                    .checked_add(step.amount_out)
                    .ok_or_else(|| {
                        anyhow!(
                            "compute_swap: amount_calculated overflow when adding amount_out (exact in)"
                        )
                    })?;
            } else {
                state.amount_specified_remaining = state
                    .amount_specified_remaining
                    .checked_sub(step.amount_out)
                    .ok_or_else(|| {
                        anyhow!("compute_swap: step.amount_out exceeds remaining (exact out)")
                    })?;
                let step_amount_in_with_fee =
                    step.amount_in
                        .checked_add(step.fee_amount)
                        .ok_or_else(|| {
                            anyhow!(
                                "compute_swap: step.amount_in + fee_amount overflow (exact out)"
                            )
                        })?;
                state.amount_calculated = state
                    .amount_calculated
                    .checked_add(step_amount_in_with_fee)
                    .ok_or_else(|| {
                        anyhow!(
                            "compute_swap: amount_calculated overflow when adding amount_in + fee (exact out)"
                        )
                    })?;
            }

            // Update tick/liquidity if we've crossed an initialized tick boundary
            if state.sqrt_price_x64 == sqrt_price_next {
                // Adjust liquidity on crossing initialized tick. If the
                // required tick array data is missing, treat this as a
                // hard error instead of silently assuming zero liquidity.
                let mut liq_net = self
                    .get_tick_liquidity_net(next_tick)
                    .ok_or_else(|| anyhow!("Missing tick array data for tick {}", next_tick))?;
                if zero_for_one {
                    liq_net = -liq_net;
                }
                state.liquidity =
                    liquidity_math::add_delta(state.liquidity, liq_net).map_err(|e| {
                        anyhow!("Failed to adjust liquidity at tick {}: {:?}", next_tick, e)
                    })?;
                state.tick = if zero_for_one {
                    next_tick - 1
                } else {
                    next_tick
                };
            } else {
                state.tick = tick_math::get_tick_at_sqrt_price(state.sqrt_price_x64)
                    .map_err(|e| anyhow!("Failed to get tick at sqrt price: {:?}", e))?;
            }
        }

        let raw_amount_in = if is_base_input {
            amount_calculate_specified
                .checked_sub(state.amount_specified_remaining)
                .ok_or_else(|| anyhow!("compute_swap: raw input underflow"))?
        } else {
            state.amount_calculated
        };
        let raw_amount_out = if is_base_input {
            state.amount_calculated
        } else {
            amount_calculate_specified
                .checked_sub(state.amount_specified_remaining)
                .ok_or_else(|| anyhow!("compute_swap: raw output underflow"))?
        };
        if raw_amount_in == 0 || raw_amount_out == 0 {
            return Err(anyhow!(
                "swap produced zero amount; chain would reject TooSmallInputOrOutputAmount"
            ));
        }

        Ok(SwapResult {
            amount_in: if is_base_input {
                let transfer_fee = if raw_amount_in == amount_calculate_specified {
                    specified_transfer_fee
                } else {
                    Self::calculate_transfer_inverse_fee(
                        input_transfer_config,
                        current_epoch,
                        raw_amount_in,
                    )?
                };
                raw_amount_in
                    .checked_add(transfer_fee)
                    .ok_or_else(|| anyhow!("input amount transfer fee overflow"))?
            } else {
                let transfer_fee = Self::calculate_transfer_inverse_fee(
                    input_transfer_config,
                    current_epoch,
                    raw_amount_in,
                )?;
                raw_amount_in
                    .checked_add(transfer_fee)
                    .ok_or_else(|| anyhow!("input amount transfer fee overflow"))?
            },
            amount_out: if is_base_input {
                let transfer_fee = Self::calculate_transfer_fee(
                    output_transfer_config,
                    current_epoch,
                    raw_amount_out,
                )?;
                raw_amount_out
                    .checked_sub(transfer_fee)
                    .ok_or_else(|| anyhow!("output transfer fee exceeds amount"))?
            } else {
                let transfer_fee = if raw_amount_out == amount_calculate_specified {
                    specified_transfer_fee
                } else {
                    Self::calculate_transfer_fee(
                        output_transfer_config,
                        current_epoch,
                        raw_amount_out,
                    )?
                };
                raw_amount_out
                    .checked_sub(transfer_fee)
                    .ok_or_else(|| anyhow!("output transfer fee exceeds amount"))?
            },
            fee_amount: state.fee_amount,
            fee_rate,
        })
    }

    // (removed: replaced by dynamic-aware implementation above)

    /// Get tick arrays needed for a swap, starting from the first initialized tick array
    /// according to the direction, then following in that direction. Falls back to adjacent offsets.
    pub fn get_swap_tick_arrays(&self, zero_for_one: bool) -> Vec<Pubkey> {
        let mut addrs: Vec<Pubkey> = Vec::new();

        // Preferred: bitmap-guided discovery from the first initialized tick array
        if let Ok((_, first_start)) = self
            .pool_state
            .get_first_initialized_tick_array(&self.bitmap_extension, zero_for_one)
        {
            addrs.push(self.get_tick_array_address(first_start));
            let mut cur = first_start;
            for _ in 1..self.max_one_side_tick_arrays {
                match self.pool_state.next_initialized_tick_array_start_index(
                    &self.bitmap_extension,
                    cur,
                    zero_for_one,
                ) {
                    Ok(Some(next)) => {
                        addrs.push(self.get_tick_array_address(next));
                        cur = next;
                    }
                    _ => break,
                }
            }
            return addrs;
        }

        // Fallback: adjacent offsets from current array in the swap direction
        let tick_spacing = self.pool_state.tick_spacing as u16;
        let current_tick = self.pool_state.tick_current;
        let current_start_index = TickUtils::get_array_start_index(current_tick, tick_spacing);
        addrs.push(self.get_tick_array_address(current_start_index));
        for i in 1..self.max_one_side_tick_arrays {
            let offset = (TICK_ARRAY_SIZE * i as i32) * i32::from(tick_spacing);
            let s = if zero_for_one {
                current_start_index.saturating_sub(offset)
            } else {
                current_start_index.saturating_add(offset)
            };
            addrs.push(self.get_tick_array_address(s));
        }
        addrs
    }
}

// Helper structures
#[derive(Debug)]
pub struct SwapState {
    pub amount_specified_remaining: u64,
    pub amount_calculated: u64,
    pub sqrt_price_x64: u128,
    pub tick: i32,
    pub liquidity: u128,
    pub fee_amount: u64,
}

#[derive(Debug)]
pub struct SwapResult {
    pub amount_in: u64,
    pub amount_out: u64,
    pub fee_amount: u64,
    pub fee_rate: u32,
}

/// Internal navigation state for walking initialized tick arrays in the same
/// way as the on-chain `swap_internal` implementation.
pub struct TickNavState {
    pub is_match_pool_current_tick_array: bool,
    pub current_valid_tick_array_start_index: i32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use spl_token_2022::extension::transfer_fee::TransferFee;

    fn build_dynamic_amm() -> ByrealClmmAmm {
        let mut pool_state = PoolState::default();
        pool_state.sqrt_price_x64 = tick_math::get_sqrt_price_at_tick(0).unwrap();
        pool_state.tick_current = 0;
        pool_state.tick_spacing = 1;
        pool_state.liquidity = 1;
        pool_state.mint_decimals_0 = 6;
        pool_state.mint_decimals_1 = 6;
        pool_state.set_quote_token_flag(true);
        pool_state.set_swap_dynamic_fee_enabled(true);
        pool_state.token0_pyth_feed_id = [1u8; 32];
        pool_state.token1_pyth_feed_id = [2u8; 32];
        pool_state.arbitrage_fee_buffer_ppm = 10_000;
        pool_state.trade_slippage_fee_base_milli_bp = 50;
        pool_state.trade_slippage_fee_trade_size_threshold = 1;
        pool_state.imbalance_fee_base_tenths_of_bp = 20;
        pool_state.imbalance_fee_x = 50;

        let mut amm_config = AmmConfig::default();
        amm_config.trade_fee_rate = 1_200;

        ByrealClmmAmm {
            key: Pubkey::new_unique(),
            pool_state,
            amm_config,
            bitmap_extension: Some(TickArrayBitmapExtension::default()),
            max_one_side_tick_arrays: 3,
            dynamic_tick_arrays: HashMap::new(),
            token0_vault_amount: 5_000_000_000,
            token1_vault_amount: 500_000_000,
            token0_pyth_price: Some(Price {
                price: 100_000_000,
                conf: 1,
                exponent: -8,
                publish_time: 100,
            }),
            token1_pyth_price: Some(Price {
                price: 1_000_000,
                conf: 1,
                exponent: -6,
                publish_time: 100,
            }),
            token0_transfer_fee_config: None,
            token1_transfer_fee_config: None,
        }
    }

    fn build_neutral_dynamic_amm(token1_as_quote: bool) -> ByrealClmmAmm {
        let mut amm = build_dynamic_amm();
        amm.pool_state.mint_decimals_0 = 6;
        amm.pool_state.mint_decimals_1 = 6;
        amm.pool_state.set_quote_token_flag(token1_as_quote);
        amm.pool_state.arbitrage_fee_buffer_ppm = 0;
        amm.pool_state.trade_slippage_fee_base_milli_bp = 0;
        amm.pool_state.trade_slippage_fee_trade_size_threshold = 1;
        amm.pool_state.imbalance_fee_base_tenths_of_bp = 0;
        amm.pool_state.imbalance_fee_x = 10;
        amm.amm_config.trade_fee_rate = 0;
        amm.pool_state.trade_fee_rate = 0;
        amm.token0_pyth_price = Some(Price {
            price: 1_000_000,
            conf: 1,
            exponent: -6,
            publish_time: 100,
        });
        amm.token1_pyth_price = Some(Price {
            price: 1_000_000,
            conf: 1,
            exponent: -6,
            publish_time: 100,
        });
        amm
    }

    fn build_fee_policy_amm() -> ByrealClmmAmm {
        let mut amm = build_neutral_dynamic_amm(true);
        amm.token0_pyth_price.as_mut().unwrap().price = 2_000_000;
        amm.amm_config.trade_fee_rate = 1_000;
        amm.pool_state.trade_slippage_fee_base_milli_bp = 10;
        amm.pool_state.imbalance_fee_base_tenths_of_bp = 5;
        amm.pool_state.imbalance_fee_x = 0;
        amm.token0_vault_amount = 150_000_000;
        amm.token1_vault_amount = 50_000_000;
        amm
    }

    // Exact-output fee parity: the on-chain exact-out trade_size is
    // min(amount_in_max_quote, output_quote * 3) (contract pool.rs). With token1 as quote and
    // zero_for_one=false the input is quote, so input_max_quote == amount_in_max (no folding) and
    // exact-out(output, A) must equal exact-in(A) below the cap, drop the slippage at A=0, and
    // saturate above the cap. This is the formula the exact-out fixed-point converges on.
    #[test]
    fn dynamic_fee_exact_out_uses_capped_amount_in_max_as_trade_size() {
        let mut amm = build_neutral_dynamic_amm(true);
        amm.pool_state.trade_slippage_fee_base_milli_bp = 10;
        amm.pool_state.trade_slippage_fee_trade_size_threshold = 0;
        let zfo = false; // token1_as_quote=true => input_is_quote = !zfo = true
        let output = 1_000_000_000_000u64; // large => cap (>=3*output_quote) far above `a`
        let a = 1_000_000_000u64; // well below the cap

        // Below the cap: exact-out uses amount_in_max as trade_size, == exact-in at the same amount.
        let exact_out = amm.dynamic_fee_rate(zfo, output, false, Some(a), 200).unwrap();
        let exact_in = amm.dynamic_fee_rate(zfo, a, true, None, 200).unwrap();
        assert_eq!(
            exact_out, exact_in,
            "exact-out below the cap should use amount_in_max as trade_size"
        );

        // amount_in_max = 0 => trade_size 0 => no slippage component, strictly less than above.
        let exact_out_zero = amm.dynamic_fee_rate(zfo, output, false, Some(0), 200).unwrap();
        assert!(
            exact_out_zero < exact_out,
            "threshold 0 must drop the slippage component"
        );

        // Far above the cap: trade_size saturates at output_quote*3, so two huge thresholds match.
        let huge1 = output.saturating_mul(1_000);
        let huge2 = output.saturating_mul(2_000);
        let cap1 = amm.dynamic_fee_rate(zfo, output, false, Some(huge1), 200).unwrap();
        let cap2 = amm.dynamic_fee_rate(zfo, output, false, Some(huge2), 200).unwrap();
        assert_eq!(cap1, cap2, "exact-out trade_size must cap at output_quote*3");
    }

    fn transfer_fee_config(basis_points: u16, maximum_fee: u64) -> TransferFeeConfig {
        let transfer_fee = TransferFee {
            epoch: 0.into(),
            maximum_fee: maximum_fee.into(),
            transfer_fee_basis_points: basis_points.into(),
        };
        TransferFeeConfig {
            older_transfer_fee: transfer_fee,
            newer_transfer_fee: transfer_fee,
            ..TransferFeeConfig::default()
        }
    }

    #[test]
    fn test_dynamic_fee_without_pyth_keeps_other_fees() {
        for (feed0, feed1) in [([0; 32], [0; 32]), ([0; 32], [2; 32]), ([1; 32], [0; 32])] {
            let mut amm = build_fee_policy_amm();
            amm.pool_state.token0_pyth_feed_id = feed0;
            amm.pool_state.token1_pyth_feed_id = feed1;
            amm.token0_pyth_price = None;
            amm.token1_pyth_price = None;

            let fee = amm
                .compute_trade_fee_rate(true, 200_000_000, true, 200)
                .unwrap();
            // Base 1000 ppm + slippage 10 ppm + imbalance 25 ppm; no arbitrage fee.
            assert_eq!(fee, 1_035);
        }
    }

    #[test]
    fn test_dynamic_fee_stale_prices_disable_only_arbitrage_fee() {
        let mut amm = build_fee_policy_amm();
        let current_timestamp = 100 + DYNAMIC_MAX_PYTH_AGE_SECONDS + 1;
        amm.token0_pyth_price.as_mut().unwrap().publish_time = 100;
        amm.token1_pyth_price.as_mut().unwrap().publish_time = current_timestamp;

        let fee = amm
            .compute_trade_fee_rate(true, 200_000_000, true, current_timestamp)
            .unwrap();
        assert_eq!(fee, 1_035);

        amm.token0_pyth_price.as_mut().unwrap().publish_time = current_timestamp;
        amm.token1_pyth_price.as_mut().unwrap().publish_time = 100;

        let fee = amm
            .compute_trade_fee_rate(true, 200_000_000, true, current_timestamp)
            .unwrap();
        assert_eq!(fee, 1_035);
    }

    #[test]
    fn test_dynamic_fee_non_positive_prices_disable_only_arbitrage_fee() {
        for token in [0, 1] {
            for price in [0, -1] {
                let mut amm = build_fee_policy_amm();
                if token == 0 {
                    amm.token0_pyth_price.as_mut().unwrap().price = price;
                } else {
                    amm.token1_pyth_price.as_mut().unwrap().price = price;
                }
                assert_eq!(
                    amm.compute_trade_fee_rate(true, 200_000_000, true, 200)
                        .unwrap(),
                    1_035
                );
            }
        }
    }

    #[test]
    fn test_dynamic_fee_accepts_pyth_price_at_staleness_boundary() {
        let mut amm = build_fee_policy_amm();
        amm.token0_pyth_price.as_mut().unwrap().publish_time = 100;
        amm.token1_pyth_price.as_mut().unwrap().publish_time = 100;

        let got = amm
            .compute_trade_fee_rate(true, 200_000_000, true, 100 + DYNAMIC_MAX_PYTH_AGE_SECONDS)
            .unwrap();
        assert_eq!(got, 500_035);
    }

    #[test]
    fn test_dynamic_fee_zero_price_index_disables_only_arbitrage_fee() {
        let mut amm = build_fee_policy_amm();
        amm.token0_pyth_price.as_mut().unwrap().price = 1;
        amm.token0_pyth_price.as_mut().unwrap().exponent = -30;
        assert_eq!(
            amm.compute_trade_fee_rate(true, 200_000_000, true, 200)
                .unwrap(),
            1_035
        );
        amm.token0_pyth_price.as_mut().unwrap().exponent = -100;
        assert!(amm
            .compute_trade_fee_rate(true, 200_000_000, true, 200)
            .is_err());
    }

    #[test]
    fn test_dynamic_fee_without_pyth_still_enforces_total_fee_cap() {
        let mut amm = build_fee_policy_amm();
        amm.pool_state.token0_pyth_feed_id = [0; 32];
        amm.amm_config.trade_fee_rate = 999_965;
        assert!(amm
            .compute_trade_fee_rate(true, 200_000_000, true, 200)
            .is_err());
        amm.amm_config.trade_fee_rate = 999_964;
        assert_eq!(
            amm.compute_trade_fee_rate(true, 200_000_000, true, 200)
                .unwrap(),
            999_999
        );
    }

    #[test]
    fn test_dynamic_fee_direction_mapping_when_token1_is_quote() {
        let mut amm = build_neutral_dynamic_amm(true);
        amm.pool_state.imbalance_fee_base_tenths_of_bp = 5;
        amm.pool_state.imbalance_fee_x = 10;
        amm.token0_vault_amount = 150;
        amm.token1_vault_amount = 50;

        let selling_overweight_base = amm
            .compute_trade_fee_rate(true, 1, true, 200)
            .unwrap();
        let buying_overweight_base = amm
            .compute_trade_fee_rate(false, 1, true, 200)
            .unwrap();

        assert_eq!(selling_overweight_base, 20);
        assert_eq!(buying_overweight_base, 0);
    }

    #[test]
    fn test_dynamic_fee_direction_mapping_when_token0_is_quote() {
        let mut amm = build_neutral_dynamic_amm(false);
        amm.pool_state.imbalance_fee_base_tenths_of_bp = 5;
        amm.pool_state.imbalance_fee_x = 10;
        amm.token0_vault_amount = 50;
        amm.token1_vault_amount = 150;

        let buying_overweight_base = amm
            .compute_trade_fee_rate(true, 1, true, 200)
            .unwrap();
        let selling_overweight_base = amm
            .compute_trade_fee_rate(false, 1, true, 200)
            .unwrap();

        assert_eq!(buying_overweight_base, 0);
        assert_eq!(selling_overweight_base, 20);
    }

    #[test]
    fn test_dynamic_fee_trade_size_mapping_uses_normalized_quote_amount() {
        let mut amm = build_neutral_dynamic_amm(true);
        amm.pool_state.trade_slippage_fee_base_milli_bp = 15;
        amm.pool_state.trade_slippage_fee_trade_size_threshold = 50;

        let just_above_threshold = amm
            .compute_trade_fee_rate(false, 5_001_000_000, true, 200)
            .unwrap();
        let below_one_quote_token = amm
            .compute_trade_fee_rate(false, 500_999, true, 200)
            .unwrap();

        assert_eq!(just_above_threshold, 2);
        assert_eq!(below_one_quote_token, 0);
    }

    #[test]
    fn test_dynamic_fee_base_vault_value_mapping_uses_quote_token_side() {
        let mut token1_quote = build_neutral_dynamic_amm(true);
        token1_quote.pool_state.imbalance_fee_base_tenths_of_bp = 5;
        token1_quote.pool_state.imbalance_fee_x = 10;
        token1_quote.token0_vault_amount = 150;
        token1_quote.token1_vault_amount = 50;

        let mut token0_quote = build_neutral_dynamic_amm(false);
        token0_quote.pool_state.imbalance_fee_base_tenths_of_bp = 5;
        token0_quote.pool_state.imbalance_fee_x = 10;
        token0_quote.token0_vault_amount = 50;
        token0_quote.token1_vault_amount = 150;

        assert_eq!(
            token1_quote
                .compute_trade_fee_rate(true, 1, true, 200)
                .unwrap(),
            20
        );
        assert_eq!(
            token0_quote
                .compute_trade_fee_rate(false, 1, true, 200)
                .unwrap(),
            20
        );
    }

    #[test]
    fn test_compute_swap_rejects_contract_precondition_failures() {
        let mut amm = build_dynamic_amm();

        let zero_err = amm.compute_swap(true, 0, true, None, 200, 0).unwrap_err();
        assert!(format!("{zero_err:#}").contains("zero amount specified"));

        amm.pool_state.open_time = 200;
        let open_err = amm.compute_swap(true, 1, true, None, 200, 0).unwrap_err();
        assert!(format!("{open_err:#}").contains("Pool is not open yet"));

        amm.pool_state.open_time = 0;
        amm.pool_state.set_status(1 << (PoolStatusBitIndex::Swap as u8));
        let status_err = amm.compute_swap(true, 1, true, None, 200, 0).unwrap_err();
        assert!(format!("{status_err:#}").contains("Pool swap is not approved"));
    }

    #[test]
    fn test_transfer_fee_helpers_match_contract_rounding() {
        let config = transfer_fee_config(100, 10_000);

        assert_eq!(
            ByrealClmmAmm::calculate_transfer_fee(Some(&config), 0, 1_000).unwrap(),
            10
        );
        assert_eq!(
            ByrealClmmAmm::calculate_transfer_inverse_fee(Some(&config), 0, 990).unwrap(),
            10
        );

        let max_config = transfer_fee_config(10_000, 7);
        assert_eq!(
            ByrealClmmAmm::calculate_transfer_inverse_fee(Some(&max_config), 0, 990).unwrap(),
            7
        );
    }

    #[test]
    fn test_dynamic_fee_matches_onchain_formula_inputs() {
        let amm = build_dynamic_amm();
        let zero_for_one = true;
        let amount_specified = 2_000_000u64;
        let is_base_input = true;
        let current_timestamp = 200i64;

        let got = amm
            .compute_trade_fee_rate(
                zero_for_one,
                amount_specified,
                is_base_input,
                current_timestamp,
            )
            .unwrap();

        let p_0 = price_from_sqrt_price_x64(amm.pool_state.sqrt_price_x64).unwrap();
        let p_index = calculate_price_index(
            &amm.token0_pyth_price.unwrap(),
            &amm.token1_pyth_price.unwrap(),
            amm.pool_state.mint_decimals_0,
            amm.pool_state.mint_decimals_1,
        )
        .unwrap();

        let token1_as_quote = amm.pool_state.is_token1_quote();
        let input_is_quote = if token1_as_quote {
            !zero_for_one
        } else {
            zero_for_one
        };
        let output_is_quote = !input_is_quote;
        let quote_amount = if is_base_input {
            if input_is_quote {
                amount_specified as u128
            } else {
                quote_amount_from_base(amount_specified as u128, p_0, token1_as_quote).unwrap()
            }
        } else if output_is_quote {
            amount_specified as u128
        } else {
            quote_amount_from_base(amount_specified as u128, p_0, token1_as_quote).unwrap()
        };

        let quote_decimals = if token1_as_quote {
            amm.pool_state.mint_decimals_1
        } else {
            amm.pool_state.mint_decimals_0
        };
        let trade_size = normalize_trade_size(quote_amount, quote_decimals).unwrap();

        let token0_vault_amount = amm.token0_vault_amount as u128;
        let token1_vault_amount = amm.token1_vault_amount as u128;
        let (quote_value_of_base, quote_balance) = if token1_as_quote {
            let base_amount = token0_vault_amount;
            let quote_balance = token1_vault_amount;
            (
                quote_amount_from_base(base_amount, p_0, true).unwrap(),
                quote_balance,
            )
        } else {
            let base_amount = token1_vault_amount;
            let quote_balance = token0_vault_amount;
            (
                quote_amount_from_base(base_amount, p_0, false).unwrap(),
                quote_balance,
            )
        };

        let expected = calculate_dynamic_fee_rate(&DynamicFeeInputs {
            p_0,
            p_index,
            trade_size,
            quote_value_of_base,
            quote_balance,
            is_buying_base: input_is_quote,
            fee_base: amm
                .pool_state
                .calculate_base_trade_fee_rate(
                    &amm.amm_config,
                    zero_for_one,
                    current_timestamp as u64,
                )
                .unwrap(),
            arbitrage_fee_buffer_ppm: amm.pool_state.arbitrage_fee_buffer_ppm,
            trade_slippage_fee_base_milli_bp: amm.pool_state.trade_slippage_fee_base_milli_bp,
            trade_slippage_fee_trade_size_threshold: amm
                .pool_state
                .trade_slippage_fee_trade_size_threshold,
            imbalance_fee_base_tenths_of_bp: amm.pool_state.imbalance_fee_base_tenths_of_bp,
            imbalance_fee_x: amm.pool_state.imbalance_fee_x,
        })
        .unwrap()
        .total_fee_rate;

        assert_eq!(
            got, expected,
            "mismatch: p_0={p_0}, p_index={p_index}, trade_size={trade_size}, quote_value_of_base={quote_value_of_base}, quote_balance={quote_balance}, token1_as_quote={token1_as_quote}, input_is_quote={input_is_quote}, amount_specified={amount_specified}"
        );
        assert!(got >= amm.amm_config.trade_fee_rate);
    }

    #[test]
    fn test_dynamic_fee_price_index_uses_mint_decimals() {
        let mut amm = build_dynamic_amm();
        amm.pool_state.mint_decimals_0 = 9;
        amm.pool_state.mint_decimals_1 = 6;
        amm.token0_pyth_price = Some(Price {
            price: 100_000_000_000,
            conf: 1,
            exponent: -8,
            publish_time: 100,
        });

        let p_index = calculate_price_index(
            &amm.token0_pyth_price.unwrap(),
            &amm.token1_pyth_price.unwrap(),
            amm.pool_state.mint_decimals_0,
            amm.pool_state.mint_decimals_1,
        )
        .unwrap();

        assert_eq!(p_index, 1u128 << 64);
        assert!(amm
            .compute_trade_fee_rate(true, 1_000_000, true, 200)
            .is_ok());
    }

    #[test]
    fn test_dynamic_fee_rejects_total_fee_above_cap() {
        let q64 = 1u128 << 64;
        let inputs = DynamicFeeInputs {
            p_0: q64 * 3,
            p_index: q64,
            trade_size: 0,
            quote_value_of_base: 100,
            quote_balance: 100,
            is_buying_base: false,
            fee_base: 500_000,
            arbitrage_fee_buffer_ppm: 0,
            trade_slippage_fee_base_milli_bp: 0,
            trade_slippage_fee_trade_size_threshold: 1,
            imbalance_fee_base_tenths_of_bp: 0,
            imbalance_fee_x: 10,
        };

        assert!(calculate_dynamic_fee_rate(&inputs).is_err());
    }

    #[test]
    fn test_pool_trade_fee_override_falls_back_to_amm_config_when_zero() {
        let amm = build_dynamic_amm();
        let base_fee = amm
            .pool_state
            .calculate_base_trade_fee_rate(&amm.amm_config, true, 200)
            .unwrap();
        let got = amm
            .compute_trade_fee_rate(true, 1_000_000, true, 200)
            .unwrap();
        assert_eq!(base_fee, amm.amm_config.trade_fee_rate);
        assert!(got >= amm.amm_config.trade_fee_rate);
    }

    #[test]
    fn test_pool_trade_fee_override_changes_fee_base() {
        let mut amm = build_dynamic_amm();
        amm.pool_state.trade_fee_rate = 2_500;
        let base_fee = amm
            .pool_state
            .calculate_base_trade_fee_rate(&amm.amm_config, true, 200)
            .unwrap();

        let got = amm
            .compute_trade_fee_rate(true, 1_000_000, true, 200)
            .unwrap();
        assert_eq!(base_fee, 2_500);
        assert!(got >= 2_500);
    }
}

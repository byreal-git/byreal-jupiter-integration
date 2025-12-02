use anchor_lang::prelude::*;
use anyhow::{anyhow, Result};
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;

use byreal_clmm::libraries::MulDiv;
use byreal_clmm::states::TICK_ARRAY_SEED;
pub use byreal_clmm::{
    libraries::{liquidity_math, swap_math, tick_math, MAX_SQRT_PRICE_X64, MIN_SQRT_PRICE_X64},
    states::{
        AmmConfig, DynTickArrayState, PoolState, TickArrayBitmapExtension, TickArrayState,
        TickState, TickUtils,
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
    /// Cache of start_index -> tick array PDA to avoid repeated PDA grinding on hot paths
    pub tick_array_pda_cache: HashMap<i32, Pubkey>,
    /// Cached list of tick array addresses gathered during the last update
    pub cached_tick_array_addresses: Vec<Pubkey>,
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
        if let Some(addr) = self.tick_array_pda_cache.get(&start_index) {
            return *addr;
        }
        let addr = Pubkey::find_program_address(
            &[
                TICK_ARRAY_SEED.as_bytes(),
                self.key.as_ref(),
                &start_index.to_be_bytes(),
            ],
            &BYREAL_CLMM_PROGRAM,
        )
        .0;
        addr
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

        // Fallback to naive neighbors if nothing collected
        if start_indexes.is_empty() {
            let tick_spacing = self.pool_state.tick_spacing;
            let current_tick = self.pool_state.tick_current;
            let current_start_index = TickUtils::get_array_start_index(current_tick, tick_spacing);
            start_indexes.insert(current_start_index);
            for i in 1..self.max_one_side_tick_arrays {
                let offset = (TICK_ARRAY_SIZE * i as i32) * i32::from(tick_spacing);
                start_indexes.insert(current_start_index.saturating_sub(offset));
            }
        }

        start_indexes
            .into_iter()
            .map(|s| self.get_tick_array_address(s))
            .collect()
    }

    /// Recompute and cache tick array PDAs (addresses) for both directions.
    /// Populates `tick_array_pda_cache` and `cached_tick_array_addresses`.
    pub fn refresh_tick_array_cache(&mut self) {
        self.tick_array_pda_cache.clear();
        let mut addrs: Vec<Pubkey> = Vec::new();

        let mut start_indexes: BTreeSet<i32> = BTreeSet::new();

        let mut collect_dir = |zero_for_one: bool, limit: usize, start_indexes: &mut BTreeSet<i32>| {
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
            collect_dir(true, self.max_one_side_tick_arrays, &mut start_indexes);
            collect_dir(false, self.max_one_side_tick_arrays, &mut start_indexes);
        }

        if start_indexes.is_empty() {
            let tick_spacing = self.pool_state.tick_spacing;
            let current_tick = self.pool_state.tick_current;
            let current_start_index = TickUtils::get_array_start_index(current_tick, tick_spacing);
            start_indexes.insert(current_start_index);
            for i in 1..self.max_one_side_tick_arrays {
                let offset = (TICK_ARRAY_SIZE * i as i32) * i32::from(tick_spacing);
                start_indexes.insert(current_start_index.saturating_sub(offset));
            }
        }

        for s in start_indexes.into_iter() {
            let addr = self.get_tick_array_address(s);
            self.tick_array_pda_cache.insert(s, addr);
            addrs.push(addr);
        }

        self.cached_tick_array_addresses = addrs;
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

    /// Compute swap for the given parameters
    pub fn compute_swap(
        &self,
        zero_for_one: bool,
        amount_specified: u64,
        is_base_input: bool,
        sqrt_price_limit_x64: Option<u128>,
        current_timestamp: i64,
    ) -> Result<SwapResult> {
        let sqrt_price_limit = sqrt_price_limit_x64.unwrap_or_else(|| {
            if zero_for_one {
                MIN_SQRT_PRICE_X64 + 1
            } else {
                MAX_SQRT_PRICE_X64 - 1
            }
        });

        // Initialize swap state
        let mut state = SwapState {
            amount_specified_remaining: amount_specified,
            amount_calculated: 0,
            sqrt_price_x64: self.pool_state.sqrt_price_x64,
            tick: self.pool_state.tick_current,
            liquidity: self.pool_state.liquidity,
            fee_amount: 0,
        };

        // Calculate fee rate considering decay fee
        let mut fee_rate = self.amm_config.trade_fee_rate;

        if self.is_decay_fee_enabled() {
            if let Some(decay_fee_rate) = if zero_for_one && self.is_decay_fee_on_sell_mint0() {
                self.get_decay_fee_rate(current_timestamp as u64)
            } else if !zero_for_one && self.is_decay_fee_on_sell_mint1() {
                self.get_decay_fee_rate(current_timestamp as u64)
            } else {
                None
            } {
                // Use decay fee if it's higher than the base fee
                if decay_fee_rate > fee_rate {
                    fee_rate = decay_fee_rate;
                }
            }
        }

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
            state.fee_amount += step.fee_amount;

            if is_base_input {
                state.amount_specified_remaining = state
                    .amount_specified_remaining
                    .checked_sub(step.amount_in + step.fee_amount)
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
                state.amount_calculated = state
                    .amount_calculated
                    .checked_add(step.amount_in + step.fee_amount)
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

        Ok(SwapResult {
            amount_in: if is_base_input {
                amount_specified - state.amount_specified_remaining
            } else {
                state.amount_calculated
            },
            amount_out: if is_base_input {
                state.amount_calculated
            } else {
                amount_specified - state.amount_specified_remaining
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

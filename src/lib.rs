use anyhow::{anyhow, Context, Result};
use anchor_lang::prelude::*;
use solana_sdk::{instruction::AccountMeta, pubkey::Pubkey};
use solana_program::program_pack::Pack;
use std::collections::{HashMap, HashSet};

use jupiter_amm_interface::{
    AccountMap, Amm, AmmContext, KeyedAccount, Quote, QuoteParams,
    SwapAndAccountMetas, SwapParams, Swap, SwapMode, ClockRef
};

use byreal_clmm::{
    states::{
        AmmConfig, PoolState, TickArrayState, TickArrayBitmapExtension, ObservationState, TickUtils, DynTickArrayState, TickState,
    },
    libraries::{
        tick_math, swap_math, liquidity_math,
        MAX_SQRT_PRICE_X64, MIN_SQRT_PRICE_X64
    },
};
use byreal_clmm::libraries::MulDiv;
use byreal_clmm::states::{POOL_TICK_ARRAY_BITMAP_SEED, TICK_ARRAY_SEED};

// Program IDs
#[cfg(feature = "mainnet")]
pub const BYREAL_CLMM_PROGRAM: Pubkey = solana_sdk::pubkey!("REALQqNEomY6cQGZJUGwywTBD2UmDT32rZcNnfxQ5N2");

#[cfg(feature = "devnet")]
pub const BYREAL_CLMM_PROGRAM: Pubkey = solana_sdk::pubkey!("45iBNkaENereLKMjLm2LHkF3hpDapf6mnvrM5HWFg9cY");

#[cfg(not(any(feature = "mainnet", feature = "devnet")))]
pub const BYREAL_CLMM_PROGRAM: Pubkey = solana_sdk::pubkey!("REALQqNEomY6cQGZJUGwywTBD2UmDT32rZcNnfxQ5N2");

// Constants
const TICK_ARRAY_SIZE: i32 = 60;
const MAX_TICK_ARRAY_CROSSINGS: usize = 10;

#[derive(Clone)]
    pub struct ByrealClmmAmm {
    /// Pool account key
    key: Pubkey,
    /// Label for display
    label: String,
    /// Program ID
    program_id: Pubkey,
    /// Pool state
    pool_state: PoolState,
    /// AMM config
    amm_config: AmmConfig,
    /// Tick arrays cache
    tick_arrays: HashMap<Pubkey, TickArrayState>,
    /// Bitmap extension
    bitmap_extension: Option<TickArrayBitmapExtension>,
    /// Observation state
    observation_state: Option<ObservationState>,
    /// Vaults balance
    vault_a_amount: u64,
    vault_b_amount: u64,
    /// Clock reference
    clock_ref: ClockRef,
    /// Raw tick array bytes cache (fixed or dynamic)
    tick_arrays_raw: HashMap<Pubkey, Vec<u8>>,
}

impl ByrealClmmAmm {
    /// Decode a dynamic tick array from raw bytes into header + tick slice views.
    fn decode_dyn_tick_array<'a>(&self, data: &'a [u8]) -> Option<(&'a DynTickArrayState, &'a [TickState])> {
        if data.len() < 8 { return None; }
        if &data[0..8] != DynTickArrayState::DISCRIMINATOR { return None; }
        if data.len() < DynTickArrayState::HEADER_LEN { return None; }

        let header_bytes = &data[8..(DynTickArrayState::HEADER_LEN)];
        let header: &DynTickArrayState = bytemuck::from_bytes(header_bytes);
        let ticks_bytes = &data[DynTickArrayState::HEADER_LEN..];
        // Safety: TickState derives AnyBitPattern in the CLMM crate
        let ticks: &[TickState] = bytemuck::try_cast_slice(ticks_bytes).ok()?;
        Some((header, ticks))
    }

    /// Decode a fixed tick array from raw bytes using Anchor deserialization.
    fn decode_fixed_tick_array(&self, data: &[u8]) -> Option<TickArrayState> {
        TickArrayState::try_deserialize(&mut data.to_vec().as_slice()).ok()
    }

    /// Initialize tick navigation state using the same helper as on-chain
    /// `swap_internal`, i.e. starting from the first initialized tick array
    /// in the given direction and tracking whether that array matches the
    /// pool's current tick array.
    fn init_tick_nav_state(&self, zero_for_one: bool) -> Result<TickNavState> {
        let (is_match, first_start) = self
            .pool_state
            .get_first_initialized_tick_array(&self.bitmap_extension, zero_for_one)?;
        Ok(TickNavState {
            is_match_pool_current_tick_array: is_match,
            current_valid_tick_array_start_index: first_start,
        })
    }

    /// Find next initialized tick using a navigation state that mirrors the
    /// on-chain `swap_internal` logic: walk within the current tick array
    /// using `next_initialized_tick`, optionally fall back to
    /// `first_initialized_tick`, and then advance across arrays via
    /// `next_initialized_tick_array_start_index` when necessary.
    fn find_next_initialized_tick_with_nav(
        &self,
        current_tick: i32,
        zero_for_one: bool,
        nav: &mut TickNavState,
    ) -> Result<i32> {
        let spacing = self.pool_state.tick_spacing as u16;

        loop {
            let start_index = nav.current_valid_tick_array_start_index;
            let addr = self.get_tick_array_address(start_index);
            let bytes = self
                .tick_arrays_raw
                .get(&addr)
                .ok_or_else(|| anyhow!("Missing tick array data for start_index {}", start_index))?;

            if bytes.len() < 8 {
                return Err(anyhow!(
                    "Tick array account {} too small; missing discriminator",
                    addr
                ));
            }

            // 1. Try next_initialized_tick within the current array.
            if &bytes[0..8] == DynTickArrayState::DISCRIMINATOR {
                let (header, ticks) = self
                    .decode_dyn_tick_array(bytes)
                    .ok_or_else(|| anyhow!("Failed to decode dynamic tick array {}", addr))?;
                if let Ok(Some(local_idx)) =
                    header.next_initialized_tick_index(ticks, current_tick, spacing, zero_for_one)
                {
                    let idx = local_idx as usize;
                    return Ok(ticks[idx].tick);
                }
            } else if &bytes[0..8] == TickArrayState::DISCRIMINATOR {
                if let Some(mut ta) = self.decode_fixed_tick_array(bytes) {
                    if let Ok(Some(ts)) =
                        ta.next_initialized_tick(current_tick, spacing, zero_for_one)
                    {
                        return Ok(ts.tick);
                    }
                }
            } else {
                return Err(anyhow!(
                    "Account {} is not a tick array (unexpected discriminator)",
                    addr
                ));
            }

            // 2. If nothing found and we haven't yet matched pool_current_tick_array,
            // fall back to first_initialized_tick on this array (mirrors on-chain).
            if !nav.is_match_pool_current_tick_array {
                nav.is_match_pool_current_tick_array = true;
                if &bytes[0..8] == DynTickArrayState::DISCRIMINATOR {
                    let (header, ticks) = self
                        .decode_dyn_tick_array(bytes)
                        .ok_or_else(|| anyhow!("Failed to decode dynamic tick array {}", addr))?;
                    if let Ok(local_idx) =
                        header.first_initialized_tick_index(ticks, zero_for_one)
                    {
                        let idx = local_idx as usize;
                        return Ok(ticks[idx].tick);
                    }
                } else if &bytes[0..8] == TickArrayState::DISCRIMINATOR {
                    if let Some(mut ta) = self.decode_fixed_tick_array(bytes) {
                        if let Ok(ts) = ta.first_initialized_tick(zero_for_one) {
                            return Ok(ts.tick);
                        }
                    }
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
            let next_bytes = self
                .tick_arrays_raw
                .get(&next_addr)
                .ok_or_else(|| {
                    anyhow!(
                        "Missing tick array data for advanced start_index {}",
                        next_start
                    )
                })?;

            if &next_bytes[0..8] == DynTickArrayState::DISCRIMINATOR {
                let (header, ticks) = self
                    .decode_dyn_tick_array(next_bytes)
                    .ok_or_else(|| anyhow!("Failed to decode dynamic tick array {}", next_addr))?;
                if let Ok(local_idx) =
                    header.first_initialized_tick_index(ticks, zero_for_one)
                {
                    let idx = local_idx as usize;
                    return Ok(ticks[idx].tick);
                }
            } else if &next_bytes[0..8] == TickArrayState::DISCRIMINATOR {
                if let Some(mut ta) = self.decode_fixed_tick_array(next_bytes) {
                    if let Ok(ts) = ta.first_initialized_tick(zero_for_one) {
                        return Ok(ts.tick);
                    }
                }
            } else {
                return Err(anyhow!(
                    "Account {} is not a tick array (unexpected discriminator)",
                    next_addr
                ));
            }
        }
    }

    /// Get liquidity_net for a given tick index from the cached tick arrays.
    fn get_tick_liquidity_net(&self, tick_index: i32) -> Option<i128> {
        let spacing = self.pool_state.tick_spacing as u16;
        let start = TickUtils::get_array_start_index(tick_index, spacing);
        let addr = self.get_tick_array_address(start);
        let data = self.tick_arrays_raw.get(&addr)?;
        if data.len() < 8 { return None; }
        if &data[0..8] == DynTickArrayState::DISCRIMINATOR {
            let (header, ticks) = self.decode_dyn_tick_array(data)?;
            if let Ok(i) = header.get_tick_index_in_array(tick_index, spacing) {
                return Some(ticks[i as usize].liquidity_net);
            }
            None
        } else if &data[0..8] == TickArrayState::DISCRIMINATOR {
            if let Some(ta) = self.decode_fixed_tick_array(data) {
                if let Ok(offset) = ta.get_tick_offset_in_array(tick_index, spacing) {
                    return Some(ta.ticks[offset].liquidity_net);
                }
            }
            None
        } else {
            None
        }
    }
    /// Get the tick array PDA address
    fn get_tick_array_address(&self, start_index: i32) -> Pubkey {
        Pubkey::find_program_address(
            &[
                TICK_ARRAY_SEED.as_bytes(),
                self.key.as_ref(),
                &start_index.to_be_bytes(),
            ],
            &self.program_id,
        ).0
    }

    /// Get tick array addresses around current price using bitmap navigation (both directions).
    /// Fallback to adjacent offsets if bitmap helpers are unavailable.
    fn get_all_tick_array_addresses(&self) -> Vec<Pubkey> {
        use std::collections::BTreeSet;

        let mut start_indexes: BTreeSet<i32> = BTreeSet::new();

        let mut collect_dir = |zero_for_one: bool, limit: usize| {
            if limit == 0 { return; }
            if let Ok((_, mut start)) = self.pool_state.get_first_initialized_tick_array(&self.bitmap_extension, zero_for_one) {
                start_indexes.insert(start);
                for _ in 1..limit {
                    match self.pool_state.next_initialized_tick_array_start_index(&self.bitmap_extension, start, zero_for_one) {
                        Ok(Some(next)) => { start_indexes.insert(next); start = next; }
                        _ => break,
                    }
                }
            }
        };

        // Try bitmap-guided discovery (10 each direction) when it is safe to
        // call the on-chain helpers. If the current tick lies outside the
        // default bitmap range *and* we have not yet decoded a bitmap
        // extension for this pool, calling get_first_initialized_tick_array
        // would panic on an internal unwrap. In that case we deliberately
        // skip bitmap navigation here and fall back to naive neighbors.
        let overflow_default =
            self.pool_state
                .is_overflow_default_tickarray_bitmap(vec![self.pool_state.tick_current]);
        let can_use_bitmap_helpers = self.bitmap_extension.is_some() || !overflow_default;
        if can_use_bitmap_helpers {
            collect_dir(true, 10);
            collect_dir(false, 10);
        }

        // Fallback to naive neighbors if nothing collected
        if start_indexes.is_empty() {
            let tick_spacing = self.pool_state.tick_spacing;
            let current_tick = self.pool_state.tick_current;
            let current_start_index = TickUtils::get_array_start_index(current_tick, tick_spacing);
            start_indexes.insert(current_start_index);
            for i in 1..=12 {
                let offset = (TICK_ARRAY_SIZE * i as i32) * i32::from(tick_spacing);
                start_indexes.insert(current_start_index.saturating_sub(offset));
                start_indexes.insert(current_start_index.saturating_add(offset));
            }
        }

        start_indexes
            .into_iter()
            .map(|s| self.get_tick_array_address(s))
            .collect()
    }

    /// Check if decay fee is enabled
    fn is_decay_fee_enabled(&self) -> bool {
        self.pool_state.decay_fee_flag & (1 << 0) != 0
    }

    /// Check if decay fee is enabled for selling mint0
    fn is_decay_fee_on_sell_mint0(&self) -> bool {
        self.pool_state.decay_fee_flag & (1 << 1) != 0
    }

    /// Check if decay fee is enabled for selling mint1
    fn is_decay_fee_on_sell_mint1(&self) -> bool {
        self.pool_state.decay_fee_flag & (1 << 2) != 0
    }

    /// Calculate decay fee rate based on current timestamp
    /// Returns fee rate in hundredths of a bip (10^-6)
    fn get_decay_fee_rate(&self, current_timestamp: u64) -> u32 {
        if !self.is_decay_fee_enabled() {
            return 0u32;
        }

        // Not open yet
        if current_timestamp < self.pool_state.open_time {
            return 0u32;
        }

        // Check for zero interval to avoid division by zero
        if self.pool_state.decay_fee_decrease_interval == 0 {
            return 0u32;
        }

        let interval_count = (current_timestamp - self.pool_state.open_time) / self.pool_state.decay_fee_decrease_interval as u64;
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
                    rate = rate.mul_div_ceil(base, hundredths_of_a_bip).unwrap();
                }
                base = base.mul_div_ceil(base, hundredths_of_a_bip).unwrap();
                exp /= 2;
            }
        }

        // Convert from percentage to hundredths of a bip
        rate = rate.mul_div_ceil(self.pool_state.decay_fee_init_fee_rate as u64, 100u64).unwrap();

        rate as u32
    }

    /// Compute swap for the given parameters
    fn compute_swap(
        &self,
        zero_for_one: bool,
        amount_specified: u64,
        is_base_input: bool,
        sqrt_price_limit_x64: Option<u128>,
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
        let current_timestamp = self.clock_ref.unix_timestamp.load(std::sync::atomic::Ordering::Relaxed) as i64;
        let mut fee_rate = self.amm_config.trade_fee_rate;
        
        if self.is_decay_fee_enabled() {
            let mut decay_fee_rate = 0u32;
            
            if zero_for_one && self.is_decay_fee_on_sell_mint0() {
                decay_fee_rate = self.get_decay_fee_rate(current_timestamp as u64);
            } else if !zero_for_one && self.is_decay_fee_on_sell_mint1() {
                decay_fee_rate = self.get_decay_fee_rate(current_timestamp as u64);
            }
            
            // Use decay fee if it's higher than the base fee
            if decay_fee_rate > fee_rate {
                fee_rate = decay_fee_rate;
            }
        }

        #[cfg(feature = "debug-swap-steps")]
        println!(
            "SDK_SWAP_INIT zero_for_one={} is_base_input={} amount_specified={} tick_current={} sqrt_price={} liquidity={} fee_rate={}",
            zero_for_one,
            is_base_input,
            amount_specified,
            state.tick,
            state.sqrt_price_x64,
            state.liquidity,
            fee_rate,
        );

        // Initialize tick-array navigation state so that tick discovery mirrors
        // the on-chain `swap_internal` helper logic.
        let mut nav = self.init_tick_nav_state(zero_for_one)?;

        // Simulate swap steps. We bound the number of *tick array* crossings
        // (not individual ticks) to avoid simulating paths that require more
        // arrays than the snapshot or account set is expected to cover.
        let spacing = self.pool_state.tick_spacing as u16;
        let mut current_array_start =
            TickUtils::get_array_start_index(state.tick, spacing);
        let mut array_crossings: usize = 0;
        while state.amount_specified_remaining != 0
            && state.sqrt_price_x64 != sqrt_price_limit
            && array_crossings < MAX_TICK_ARRAY_CROSSINGS
        {
            // Find next initialized tick
            let next_tick =
                self.find_next_initialized_tick_with_nav(state.tick, zero_for_one, &mut nav)?;

            let sqrt_price_next = tick_math::get_sqrt_price_at_tick(next_tick)
                .map_err(|e| anyhow!("Failed to get sqrt price at tick {}: {}", next_tick, e))?;

            let target_price = if (zero_for_one && sqrt_price_next < sqrt_price_limit) ||
                                 (!zero_for_one && sqrt_price_next > sqrt_price_limit) {
                sqrt_price_limit
            } else {
                sqrt_price_next
            };

            // Compute swap step
            let block_timestamp = self.clock_ref.unix_timestamp.load(std::sync::atomic::Ordering::Relaxed) as u32;
            let step = swap_math::compute_swap_step(
                state.sqrt_price_x64,
                target_price,
                state.liquidity,
                state.amount_specified_remaining,
                fee_rate,
                is_base_input,
                zero_for_one,
                block_timestamp,
            ).map_err(|e| anyhow!("Swap step computation failed: {:?}", e))?;

            #[cfg(feature = "debug-swap-steps")]
            println!(
                "SDK_SWAP_STEP tick={} next_tick={} sqrt_price_current={} sqrt_price_next={} target_price={} liq={} amount_rem={} step_in={} step_out={} step_fee={} fee_accumulated={}",
                state.tick,
                next_tick,
                state.sqrt_price_x64,
                step.sqrt_price_next_x64,
                target_price,
                state.liquidity,
                state.amount_specified_remaining,
                step.amount_in,
                step.amount_out,
                step.fee_amount,
                state.fee_amount + step.fee_amount,
            );

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
                        anyhow!(
                            "compute_swap: step.amount_out exceeds remaining (exact out)"
                        )
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
                // If the next initialized tick lies in a different tick array,
                // count this as a tick-array crossing. This gives us a bound
                // on how many arrays the math simulation is allowed to touch.
                let next_array_start =
                    TickUtils::get_array_start_index(next_tick, spacing);
                if next_array_start != current_array_start {
                    array_crossings += 1;
                    current_array_start = next_array_start;
                }

                // Adjust liquidity on crossing initialized tick. If the
                // required tick array data is missing, treat this as a
                // hard error instead of silently assuming zero liquidity.
                let mut liq_net = self
                    .get_tick_liquidity_net(next_tick)
                    .ok_or_else(|| anyhow!("Missing tick array data for tick {}", next_tick))?;
                if zero_for_one { liq_net = -liq_net; }
                state.liquidity = liquidity_math::add_delta(state.liquidity, liq_net)
                    .map_err(|e| anyhow!("Failed to adjust liquidity at tick {}: {:?}", next_tick, e))?;
                state.tick = if zero_for_one { next_tick - 1 } else { next_tick };
            } else {
                state.tick = tick_math::get_tick_at_sqrt_price(state.sqrt_price_x64)
                    .map_err(|e| anyhow!("Failed to get tick at sqrt price: {:?}", e))?;
            }

            #[cfg(feature = "debug-swap-steps")]
            println!(
                "SDK_SWAP_STATE tick={} sqrt_price={} liq={} amount_rem={} amount_calc={} fee_accumulated={} array_crossings={}",
                state.tick,
                state.sqrt_price_x64,
                state.liquidity,
                state.amount_specified_remaining,
                state.amount_calculated,
                state.fee_amount,
                array_crossings,
            );
        }

        // If we exit the loop because we've hit the maximum number of tick
        // array crossings but still have remaining amount to swap, this
        // indicates that not enough tick arrays were provided to complete
        // the simulation. Surface this as an error instead of returning
        // a partial quote.
        if array_crossings >= MAX_TICK_ARRAY_CROSSINGS
            && state.amount_specified_remaining > 0
        {
            return Err(anyhow!(
                "Not enough tick arrays to simulate swap: crossed {} arrays, remaining amount {}",
                array_crossings,
                state.amount_specified_remaining
            ));
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
    fn get_swap_tick_arrays(&self, zero_for_one: bool) -> Vec<Pubkey> {
        let mut addrs: Vec<Pubkey> = Vec::new();

        // Preferred: bitmap-guided discovery from the first initialized tick array
        if let Ok((_, first_start)) = self.pool_state.get_first_initialized_tick_array(&self.bitmap_extension, zero_for_one) {
            addrs.push(self.get_tick_array_address(first_start));
            let mut cur = first_start;
            for _ in 1..=10 {
                match self.pool_state.next_initialized_tick_array_start_index(&self.bitmap_extension, cur, zero_for_one) {
                    Ok(Some(next)) => { addrs.push(self.get_tick_array_address(next)); cur = next; }
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
        for i in 1..=10 {
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

impl Amm for ByrealClmmAmm {
    fn from_keyed_account(keyed_account: &KeyedAccount, amm_context: &AmmContext) -> Result<Self> {
        let mut data_ref = keyed_account.account.data.as_slice();
        let pool_state = PoolState::try_deserialize(&mut data_ref)
            .context("Failed to deserialize pool state")?;
        
        Ok(Self {
            key: keyed_account.key,
            label: "Byreal".to_string(),
            program_id: keyed_account.account.owner,
            pool_state,
            // set a default amm config, will update later
            amm_config: AmmConfig::default(),
            tick_arrays: HashMap::new(),
            bitmap_extension: None,
            observation_state: None,
            vault_a_amount: 0,
            vault_b_amount: 0,
            clock_ref: amm_context.clock_ref.clone(),
            tick_arrays_raw: HashMap::new(),
        })
    }

    fn label(&self) -> String {
        self.label.clone()
    }

    fn program_id(&self) -> Pubkey {
        self.program_id
    }

    fn key(&self) -> Pubkey {
        self.key
    }

    fn get_reserve_mints(&self) -> Vec<Pubkey> {
        vec![self.pool_state.token_mint_0, self.pool_state.token_mint_1]
    }

    fn get_accounts_to_update(&self) -> Vec<Pubkey> {
        let mut accounts = vec![
            self.key, // Pool state itself
            self.pool_state.token_vault_0,
            self.pool_state.token_vault_1,
            self.pool_state.amm_config,
            self.pool_state.observation_key,
        ];

        // Add bitmap extension if exists
        let bitmap_key = TickArrayBitmapExtension::key(self.key);
        accounts.push(bitmap_key);

        // Add tick arrays
        accounts.extend(self.get_all_tick_array_addresses());

        accounts
    }

    fn update(&mut self, account_map: &AccountMap) -> Result<()> {
        // Update pool state
        if let Some(pool_data) = account_map.get(&self.key) {
            let mut buf = pool_data.data.as_slice();
            self.pool_state = PoolState::try_deserialize(&mut buf)?;
        }

        // Update AMM config
        if let Some(config_data) = account_map.get(&self.pool_state.amm_config) {
            self.amm_config = AmmConfig::try_deserialize(&mut config_data.data.as_slice())?;
        }

        // Update vault balances
        if let Some(vault_0_data) = account_map.get(&self.pool_state.token_vault_0) {
            let vault_0_account = anchor_spl::token::spl_token::state::Account::unpack(&vault_0_data.data)?;
            self.vault_a_amount = vault_0_account.amount;
        }

        if let Some(vault_1_data) = account_map.get(&self.pool_state.token_vault_1) {
            let vault_1_account = anchor_spl::token::spl_token::state::Account::unpack(&vault_1_data.data)?;
            self.vault_b_amount = vault_1_account.amount;
        }

        // Update tick arrays (raw bytes + fixed decode if possible)
        self.tick_arrays_raw.clear();
        for address in self.get_all_tick_array_addresses() {
            if let Some(tick_array_data) = account_map.get(&address) {
                self.tick_arrays_raw.insert(address, tick_array_data.data.clone());
                if let Ok(tick_array) = TickArrayState::try_deserialize(&mut tick_array_data.data.as_slice()) {
                    self.tick_arrays.insert(address, tick_array);
                }
            }
        }

        // Update bitmap extension
        let bitmap_key = Pubkey::find_program_address(
            &[
                POOL_TICK_ARRAY_BITMAP_SEED.as_bytes(),
                self.key.as_ref(),
            ],
            &self.program_id,
        ).0;
        if let Some(bitmap_data) = account_map.get(&bitmap_key) {
            if let Ok(bitmap) = TickArrayBitmapExtension::try_deserialize(&mut bitmap_data.data.as_slice()) {
                self.bitmap_extension = Some(bitmap);
            }
        }

        // Update observation state
        if let Some(observation_data) = account_map.get(&self.pool_state.observation_key) {
            if let Ok(observation) = ObservationState::try_deserialize(&mut observation_data.data.as_slice()) {
                self.observation_state = Some(observation);
            }
        }

        Ok(())
    }

    fn quote(&self, quote_params: &QuoteParams) -> Result<Quote> {
        let zero_for_one = quote_params.input_mint == self.pool_state.token_mint_0;
        
        // Verify input mint is valid
        if !zero_for_one && quote_params.input_mint != self.pool_state.token_mint_1 {
            return Err(anyhow!(
                "Input mint {} does not match either mint in pool",
                quote_params.input_mint
            ));
        }

        // Compute swap - determine if base_input from quote_params
        let is_base_input = quote_params.swap_mode == SwapMode::ExactIn;
        
        let swap_result = self.compute_swap(
            zero_for_one,
            quote_params.amount,
            is_base_input,
            None, // No price limit for quotes
        )?;

        Ok(Quote {
            in_amount: swap_result.amount_in,
            out_amount: swap_result.amount_out,
            fee_amount: swap_result.fee_amount,
            fee_mint: quote_params.input_mint,
            fee_pct: swap_result.fee_rate.into(),
        })
    }

    fn get_swap_and_account_metas(&self, swap_params: &SwapParams) -> Result<SwapAndAccountMetas> {
        let zero_for_one = swap_params.source_mint == self.pool_state.token_mint_0;

        // Build account metas for swap instruction (must match on-chain order)
        let mut account_metas = vec![
            // Signer (payer)
            AccountMeta::new_readonly(swap_params.token_transfer_authority, true),
            // AMM Config
            AccountMeta::new_readonly(self.pool_state.amm_config, false),
            // Pool state
            AccountMeta::new(self.key, false),
            // User token accounts (input, output)
            AccountMeta::new(swap_params.source_token_account, false),
            AccountMeta::new(swap_params.destination_token_account, false),
            // Vaults (input, output)
            if zero_for_one {
                AccountMeta::new(self.pool_state.token_vault_0, false)
            } else {
                AccountMeta::new(self.pool_state.token_vault_1, false)
            },
            if zero_for_one {
                AccountMeta::new(self.pool_state.token_vault_1, false)
            } else {
                AccountMeta::new(self.pool_state.token_vault_0, false)
            },
            // Observation state
            AccountMeta::new(self.pool_state.observation_key, false),
            // Token program
            AccountMeta::new_readonly(anchor_spl::token::ID, false),
        ];

        // Tick arrays for this swap:
        // - start from directional candidates from get_swap_tick_arrays;
        // - keep only those for which we actually have account bytes loaded
        //   (so they are present in tick_arrays_raw and thus in LiteSVM);
        // - if none of the directional candidates are present, fall back to
        //   *any* tick arrays we have in tick_arrays_raw.
        let candidate_tick_arrays = self.get_swap_tick_arrays(zero_for_one);
        let mut live_tick_arrays: Vec<Pubkey> = candidate_tick_arrays
            .into_iter()
            .filter(|addr| self.tick_arrays_raw.contains_key(addr))
            .collect();

        if live_tick_arrays.is_empty() {
            live_tick_arrays.extend(self.tick_arrays_raw.keys().copied());
        }

        if live_tick_arrays.is_empty() {
            return Err(anyhow!(
                "No tick array accounts available for swap; cannot build account metas"
            ));
        }

        // Primary tick array (named account in Anchor context)
        let primary_tick_array = live_tick_arrays[0];
        account_metas.push(AccountMeta::new(primary_tick_array, false));

        // Bitmap extension (readonly) to support out-of-bound bitmaps
        let bitmap_key = TickArrayBitmapExtension::key(self.key);
        account_metas.push(AccountMeta::new_readonly(bitmap_key, false));

        // Additional tick arrays as remaining accounts
        for addr in live_tick_arrays.iter().skip(1) {
            account_metas.push(AccountMeta::new(*addr, false));
        }

        Ok(SwapAndAccountMetas {
            swap: Swap::RaydiumClmm {},
            account_metas,
        })
    }

    // Override to indicate tick arrays can change dynamically
    fn has_dynamic_accounts(&self) -> bool {
        true // Tick arrays can change as price moves
    }

    fn supports_exact_out(&self) -> bool {
        true
    }

    fn clone_amm(&self) -> Box<dyn Amm + Send + Sync> {
        Box::new(self.clone())
    }

    // Optional: Indicate this pool doesn't share liquidity with others
    fn underlying_liquidities(&self) -> Option<HashSet<Pubkey>> {
        None
    }
}

// Helper structures
#[derive(Debug)]
struct SwapState {
    amount_specified_remaining: u64,
    amount_calculated: u64,
    sqrt_price_x64: u128,
    tick: i32,
    liquidity: u128,
    fee_amount: u64,
}

#[derive(Debug)]
struct SwapResult {
    amount_in: u64,
    amount_out: u64,
    fee_amount: u64,
    fee_rate: u32,
}

/// Internal navigation state for walking initialized tick arrays in the same
/// way as the on-chain `swap_internal` implementation.
struct TickNavState {
    is_match_pool_current_tick_array: bool,
    current_valid_tick_array_start_index: i32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_client::rpc_client::RpcClient;
    use std::str::FromStr;

    #[test]
    fn test_jupiter_integration() {
        // Skip if not in integration test mode
        if std::env::var("RUN_INTEGRATION_TESTS").is_err() {
            println!("Skipping integration test. Set RUN_INTEGRATION_TESTS=1 to run.");
            return;
        }

        // Example pool addresses (replace with actual pools)
        // SOL-USDC pool on mainnet
        let pool_address = Pubkey::from_str("J4jiEPEu8c8nLdpkiMa7k1P8rL1HCJSNxCvzA5DsmYds").unwrap();
        
        let rpc = RpcClient::new("https://api.mainnet-beta.solana.com");
        
        // Fetch pool account
        let account = rpc.get_account(&pool_address).unwrap();
        
        let keyed_account = KeyedAccount {
            key: pool_address,
            account: account.into(),
            params: None,
        };
        
        // Create AMM context with default clock ref
        let amm_context = AmmContext {
            clock_ref: ClockRef::default(),
        };
        
        // Create AMM instance
        let mut amm = ByrealClmmAmm::from_keyed_account(&keyed_account, &amm_context).unwrap();
        
        println!("Pool: {}", amm.key());
        println!("Label: {}", amm.label());
        println!("Program: {}", amm.program_id());
        
        // Get accounts to update
        let accounts_to_update = amm.get_accounts_to_update();
        println!("Accounts to update: {}", accounts_to_update.len());
        
        // Fetch all accounts  
        let accounts = rpc.get_multiple_accounts(&accounts_to_update).unwrap();
        let account_map: AccountMap = accounts_to_update
            .iter()
            .enumerate()
            .filter_map(|(i, key)| {
                accounts[i].as_ref().map(|account| (*key, account.clone().into()))
            })
            .collect();
        
        // Update AMM state
        amm.update(&account_map).unwrap();

        // Debug: inspect discriminators for CLMM-owned accounts and our local
        // type discriminators, to ensure we are selecting the correct
        // tick_array accounts (fixed or dynamic).
        println!(
            "TickArrayState discriminator (SDK): {:?}",
            TickArrayState::DISCRIMINATOR
        );
        println!(
            "DynTickArrayState discriminator (SDK): {:?}",
            DynTickArrayState::DISCRIMINATOR
        );
        for (addr, acc) in account_map.iter() {
            if acc.owner != BYREAL_CLMM_PROGRAM || acc.data.len() < 8 {
                continue;
            }
            let disc = &acc.data[0..8];
            let kind = if disc == TickArrayState::DISCRIMINATOR {
                "fixed_tick_array"
            } else if disc == DynTickArrayState::DISCRIMINATOR {
                "dyn_tick_array"
            } else {
                "other_clmm_account"
            };
            println!("CLMM account {} kind={} disc_bytes={:?}", addr, kind, disc);
        }
        
        // Test quotes
        let mints = amm.get_reserve_mints();
        println!("Reserve mints: {:?}", mints);
        
        // Test exact in quote
        let quote_in = amm.quote(&QuoteParams {
            amount: 100_000, // 0.0001 SOL
            input_mint: mints[0],
            output_mint: mints[1],
            swap_mode: SwapMode::ExactIn,
        }).unwrap();
        
        println!("Quote (exact in):");
        println!("  Input: {} {}", quote_in.in_amount, mints[0]);
        println!("  Output: {} {}", quote_in.out_amount, mints[1]);
        println!("  Fee: {}", quote_in.fee_amount);
        println!("  Fee %: {}", quote_in.fee_pct);
        
        // Test exact out quote
        let quote_out = amm.quote(&QuoteParams {
            amount: quote_in.out_amount, // Same amount as output from previous quote
            input_mint: mints[0],
            output_mint: mints[1],
            swap_mode: SwapMode::ExactOut,
        }).unwrap();
        
        println!("Quote (exact out):");
        println!("  Input: {} {}", quote_out.in_amount, mints[0]);
        println!("  Output: {} {}", quote_out.out_amount, mints[1]);
        println!("  Fee: {}", quote_out.fee_amount);
        
        // Test swap account metas
        let jupiter_program = Pubkey::new_unique();
        let swap_params = SwapParams {
            source_mint: mints[0],
            destination_mint: mints[1],
            source_token_account: Pubkey::new_unique(),
            destination_token_account: Pubkey::new_unique(),
            token_transfer_authority: Pubkey::new_unique(),
            quote_mint_to_referrer: None,
            jupiter_program_id: &jupiter_program,
            in_amount: quote_in.in_amount,
            out_amount: quote_in.out_amount,
            missing_dynamic_accounts_as_default: false,
            swap_mode: SwapMode::ExactIn,
        };
        
        let swap_and_metas = amm.get_swap_and_account_metas(&swap_params).unwrap();
        println!("Swap accounts: {}", swap_and_metas.account_metas.len());
        
        // Verify dynamic accounts
        assert!(amm.has_dynamic_accounts());
    }

    #[test]
    fn test_tick_array_address_calculation() {
        let pool_key = Pubkey::new_unique();
        let program_id = BYREAL_CLMM_PROGRAM;
        
        // Create a mock AMM
        let amm = ByrealClmmAmm {
            key: pool_key,
            label: "Test".to_string(),
            program_id,
            pool_state: PoolState::default(),
            amm_config: AmmConfig::default(),
            tick_arrays: HashMap::new(),
            bitmap_extension: None,
            observation_state: None,
            vault_a_amount: 0,
            vault_b_amount: 0,
            clock_ref: ClockRef::default(),
            tick_arrays_raw: HashMap::new(),
        };
        
        // Test tick array address generation
        let tick_array_addr = amm.get_tick_array_address(0);
        assert_ne!(tick_array_addr, Pubkey::default());
        
        // Test different start indices
        let addr1 = amm.get_tick_array_address(100);
        let addr2 = amm.get_tick_array_address(200);
        assert_ne!(addr1, addr2);
    }

    #[test] 
    fn test_swap_direction_tick_arrays() {
        let pool_key = Pubkey::new_unique();
        let program_id = BYREAL_CLMM_PROGRAM;
        
        let mut pool_state = PoolState::default();
        pool_state.tick_current = 1000;
        pool_state.tick_spacing = 10;
        
        let amm = ByrealClmmAmm {
            key: pool_key,
            label: "Test".to_string(),
            program_id,
            pool_state,
            amm_config: AmmConfig::default(),
            tick_arrays: HashMap::new(),
            bitmap_extension: None,
            observation_state: None,
            vault_a_amount: 0,
            vault_b_amount: 0,
            clock_ref: ClockRef::default(),
            tick_arrays_raw: HashMap::new(),
        };
        
        // Test zero_for_one (price decreasing)
        let arrays_down = amm.get_swap_tick_arrays(true);
        assert_eq!(arrays_down.len(), 11);
        
        // Test one_for_zero (price increasing)
        let arrays_up = amm.get_swap_tick_arrays(false);
        assert_eq!(arrays_up.len(), 11);
        
        // Verify they're different
        assert_ne!(arrays_down[1], arrays_up[1]);
    }

    #[test]
    fn test_decay_fee_calculation() {
        let pool_key = Pubkey::new_unique();
        let program_id = BYREAL_CLMM_PROGRAM;
        
        let mut pool_state = PoolState::default();
        pool_state.tick_current = 1000;
        pool_state.tick_spacing = 10;
        pool_state.open_time = 0; // Pool opened at timestamp 0
        pool_state.decay_fee_flag = 0b111; // Enable decay fee for both directions
        pool_state.decay_fee_init_fee_rate = 80; // 80% initial fee
        pool_state.decay_fee_decrease_rate = 10; // 10% decrease per interval
        pool_state.decay_fee_decrease_interval = 10; // 10 seconds per interval
        
        let mut amm_config = AmmConfig::default();
        amm_config.trade_fee_rate = 2500; // 0.25% base fee (2500 / 10^6)
        
        let amm = ByrealClmmAmm {
            key: pool_key,
            label: "Test".to_string(),
            program_id,
            pool_state,
            amm_config,
            tick_arrays: HashMap::new(),
            bitmap_extension: None,
            observation_state: None,
            vault_a_amount: 0,
            vault_b_amount: 0,
            clock_ref: ClockRef::default(),
            tick_arrays_raw: HashMap::new(),
        };
        
        // Test decay fee enabled
        assert!(amm.is_decay_fee_enabled());
        assert!(amm.is_decay_fee_on_sell_mint0());
        assert!(amm.is_decay_fee_on_sell_mint1());
        
        // Test decay fee at different intervals
        // Interval 0: timestamp 0-9, fee = 80%
        let fee_rate = amm.get_decay_fee_rate(0);
        assert_eq!(fee_rate, 800_000); // 80% = 800,000 / 10^6
        
        let fee_rate = amm.get_decay_fee_rate(9);
        assert_eq!(fee_rate, 800_000); // Still 80%
        
        // Interval 1: timestamp 10-19, fee = 72%
        let fee_rate = amm.get_decay_fee_rate(10);
        assert_eq!(fee_rate, 720_000); // 72% = 720,000 / 10^6
        
        let fee_rate = amm.get_decay_fee_rate(19);
        assert_eq!(fee_rate, 720_000); // Still 72%
        
        // Interval 2: timestamp 20-29, fee = 64.8%
        let fee_rate = amm.get_decay_fee_rate(20);
        assert_eq!(fee_rate, 648_000); // 64.8% = 648,000 / 10^6
        
        // Interval 3: timestamp 30-39, fee = 58.32%
        let fee_rate = amm.get_decay_fee_rate(30);
        assert_eq!(fee_rate, 583_200); // 58.32% = 583,200 / 10^6
        
        // Test after many intervals (should approach 0)
        let fee_rate = amm.get_decay_fee_rate(1000);
        assert!(fee_rate < 100); // Should be very small after 100 intervals
    }

    #[test]
    fn test_decay_fee_disabled() {
        let pool_key = Pubkey::new_unique();
        let program_id = BYREAL_CLMM_PROGRAM;
        
        let mut pool_state = PoolState::default();
        pool_state.decay_fee_flag = 0; // Decay fee disabled
        
        let amm = ByrealClmmAmm {
            key: pool_key,
            label: "Test".to_string(),
            program_id,
            pool_state,
            amm_config: AmmConfig::default(),
            tick_arrays: HashMap::new(),
            bitmap_extension: None,
            observation_state: None,
            vault_a_amount: 0,
            vault_b_amount: 0,
            clock_ref: ClockRef::default(),
            tick_arrays_raw: HashMap::new(),
        };
        
        assert!(!amm.is_decay_fee_enabled());
        assert_eq!(amm.get_decay_fee_rate(100), 0);
    }

    #[test]
    fn test_decay_fee_before_open_time() {
        let pool_key = Pubkey::new_unique();
        let program_id = BYREAL_CLMM_PROGRAM;
        
        let mut pool_state = PoolState::default();
        pool_state.open_time = 1000; // Pool opens at timestamp 1000
        pool_state.decay_fee_flag = 0b111; // Enable decay fee
        pool_state.decay_fee_init_fee_rate = 50;
        pool_state.decay_fee_decrease_interval = 10; // Set interval to avoid division by zero
        
        let amm = ByrealClmmAmm {
            key: pool_key,
            label: "Test".to_string(),
            program_id,
            pool_state,
            amm_config: AmmConfig::default(),
            tick_arrays: HashMap::new(),
            bitmap_extension: None,
            observation_state: None,
            vault_a_amount: 0,
            vault_b_amount: 0,
            clock_ref: ClockRef::default(),
            tick_arrays_raw: HashMap::new(),
        };
        
        // Before open time, fee should be 0
        assert_eq!(amm.get_decay_fee_rate(999), 0);
        
        // At open time, fee should be initial rate
        assert_eq!(amm.get_decay_fee_rate(1000), 500_000); // 50% = 500,000 / 10^6
    }

    #[test]
    fn test_decode_dyn_tick_array_and_next_tick() {
        // Helper to build a minimal dynamic tick array bytes blob
        fn build_dyn_bytes(start: i32, spacing: u16, offsets: &[usize]) -> Vec<u8> {
            let mut header = DynTickArrayState::default();
            header.start_tick_index = start;
            header.alloc_tick_count = offsets.len() as u8;
            // Map offsets to 1-based indices
            for (i, off) in offsets.iter().enumerate() {
                header.tick_offset_index[*off] = (i as u8) + 1;
            }
            let mut ticks: Vec<TickState> = Vec::with_capacity(offsets.len());
            for off in offsets.iter() {
                let mut t = TickState::default();
                t.tick = start + (*off as i32) * (spacing as i32);
                t.liquidity_gross = 1; // mark initialized
                ticks.push(t);
            }
            let mut data = Vec::new();
            data.extend_from_slice(&DynTickArrayState::DISCRIMINATOR);
            data.extend_from_slice(bytemuck::bytes_of(&header));
            data.extend_from_slice(bytemuck::cast_slice(&ticks));
            data
        }

        // Synthetic dynamic tick array with initialized ticks at offsets
        // 0 (600), 2 (620) and 5 (650) for spacing=10.
        let spacing: u16 = 10;
        let start: i32 = 600;
        let bytes = build_dyn_bytes(start, spacing, &[0, 2, 5]);

        let pool_key = Pubkey::new_unique();
        let program_id = BYREAL_CLMM_PROGRAM;
        let mut pool_state = PoolState::default();
        pool_state.tick_spacing = spacing;

        let amm = ByrealClmmAmm {
            key: pool_key,
            label: "Test".to_string(),
            program_id,
            pool_state,
            amm_config: AmmConfig::default(),
            tick_arrays: HashMap::new(),
            bitmap_extension: None,
            observation_state: None,
            vault_a_amount: 0,
            vault_b_amount: 0,
            clock_ref: ClockRef::default(),
            tick_arrays_raw: HashMap::new(),
        };

        let (header, ticks) = amm.decode_dyn_tick_array(&bytes).unwrap();
        let header_start = header.start_tick_index;
        assert_eq!(header_start, start);
        assert_eq!(header.alloc_tick_count, 3);
        assert_eq!(ticks.len(), 3);
        let t0 = ticks[0].tick;
        let t1 = ticks[1].tick;
        let t2 = ticks[2].tick;
        assert_eq!(t0, start);
        assert_eq!(t1, start + 2 * i32::from(spacing));
        assert_eq!(t2, start + 5 * i32::from(spacing));

        // zero_for_one=true (price decreasing): from tick just above 650, the
        // next initialized tick inside this array should be 650.
        let current_tick_down = start + 5 * i32::from(spacing) + 1; // 651
        let idx_down = header
            .next_initialized_tick_index(ticks, current_tick_down, spacing, true)
            .unwrap()
            .unwrap();
        let tick_down = ticks[idx_down as usize].tick;
        assert_eq!(tick_down, start + 5 * i32::from(spacing));

        // zero_for_one=false (price increasing): the first initialized tick in
        // this array should be 600.
        let idx_up = header.first_initialized_tick_index(ticks, false).unwrap();
        let tick_up = ticks[idx_up as usize].tick;
        assert_eq!(tick_up, start);
    }

    #[test]
    fn test_compute_swap_errors_when_not_enough_tick_arrays() {
        let pool_key = Pubkey::new_unique();
        let program_id = BYREAL_CLMM_PROGRAM;

        // Minimal pool state with some liquidity so that compute_swap would
        // attempt to walk ticks, but we deliberately do not provide any
        // tick array account data.
        let mut pool_state = PoolState::default();
        pool_state.tick_spacing = 10;
        pool_state.tick_current = 0;
        pool_state.sqrt_price_x64 = tick_math::get_sqrt_price_at_tick(0).unwrap();
        pool_state.liquidity = 1_000_000u128;

        let amm = ByrealClmmAmm {
            key: pool_key,
            label: "Test".to_string(),
            program_id,
            pool_state,
            amm_config: AmmConfig::default(),
            tick_arrays: HashMap::new(),
            bitmap_extension: None,
            observation_state: None,
            vault_a_amount: 0,
            vault_b_amount: 0,
            clock_ref: ClockRef::default(),
            tick_arrays_raw: HashMap::new(),
        };

        // Attempting to compute a swap without tick array data should now
        // surface an error instead of silently falling back.
        let res = amm.compute_swap(true, 1_000u64, true, None);
        assert!(res.is_err());
    }

    /// LiteSVM vs SDK quote test for the Byreal JUP/USDC CLMM pool.
    ///
    /// This test:
    /// - fetches pool + tick-array snapshot from mainnet,
    /// - computes SDK quote via `Amm::quote`,
    /// - sets the same accounts into a LiteSVM VM with the byreal_clmm BPF binary,
    /// - simulates a `swap` instruction,
    /// - compares the user output amount between LiteSVM and SDK math.
    #[cfg(feature = "with-litesvm")]
    #[test]
    #[ignore]
    fn test_litesvm_vs_sdk_byreal_jup_usdc() {
        use litesvm::LiteSVM;
        use solana_clock::Clock as RawClock;
        use solana_account::Account as RawAccount;
        use solana_pubkey::Pubkey as RawPubkey;
        use solana_instruction::{account_meta::AccountMeta as RawAccountMeta, Instruction as RawInstruction};
        use solana_message::Message as RawMessage;
        use solana_transaction::Transaction as RawTransaction;
        use solana_client::rpc_request::TokenAccountsFilter;

        // 1. Build SDK AMM from mainnet snapshot
        let rpc = RpcClient::new(
            "https://api.mainnet-beta.solana.com",
        );
        // Byreal (cbBTC-USDC) pool used by aggregator
        let pool_address =
            Pubkey::from_str("A5vkCw1VXPNXq5VFbffPm6Bo4kVKAP1UUoRrEn3gyVey").unwrap();
        // USDC is fixed; cbBTC mint is derived from pool and cross‑checked against the known mint.
        let usdc_mint = Pubkey::from_str("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v").unwrap();

        let account = rpc.get_account(&pool_address).unwrap();
        let keyed_account = KeyedAccount {
            key: pool_address,
            account: account.into(),
            params: None,
        };
        let amm_context = AmmContext {
            clock_ref: ClockRef::default(),
        };
        let mut amm = ByrealClmmAmm::from_keyed_account(&keyed_account, &amm_context).unwrap();

        // Build the full account list the SDK believes is needed for this pool,
        // including pool state, vaults, config, observation, bitmap extension
        // and all candidate tick array PDAs.
        let accounts_to_update = amm.get_accounts_to_update();
        let accounts = rpc.get_multiple_accounts(&accounts_to_update).unwrap();
        let mut account_map: AccountMap = accounts_to_update
            .iter()
            .enumerate()
            .filter_map(|(i, key)| {
                accounts[i]
                    .as_ref()
                    .map(|account| (*key, account.clone().into()))
                })
                .collect();
        amm.update(&account_map).unwrap();

        // Derive cbBTC mint from pool state (the non-USDC side) and sanity‑check
        // against the expected cbBTC mint used by the aggregator.
        let expected_cb_btc_mint =
            Pubkey::from_str("cbbtcf3aa214zXHbiAZQwf4122FBYbraNdFqgw4iMij").unwrap();
        let cb_btc_mint = if amm.pool_state.token_mint_0 == usdc_mint {
            amm.pool_state.token_mint_1
        } else if amm.pool_state.token_mint_1 == usdc_mint {
            amm.pool_state.token_mint_0
        } else {
            panic!(
                "Pool {} is not a USDC-cbBTC pool (mints: {}, {})",
                pool_address,
                amm.pool_state.token_mint_0,
                amm.pool_state.token_mint_1
            );
        };
        assert_eq!(
            cb_btc_mint, expected_cb_btc_mint,
            "Pool {} cbBTC mint mismatch (pool side vs expected)",
            pool_address
        );

        // Ensure we have *all* tick array accounts that the on-chain program
        // may touch, based on the bitmap navigation helpers, not just the
        // subset returned by get_accounts_to_update(). Otherwise the program
        // can legitimately throw NotEnoughTickArrayAccount while the SDK
        // math succeeds.
        let mut full_tick_addrs: HashSet<Pubkey> = HashSet::new();
        for &dir in &[true, false] {
            if let Ok((_, mut start)) =
                amm.pool_state.get_first_initialized_tick_array(&amm.bitmap_extension, dir)
            {
                loop {
                    full_tick_addrs.insert(amm.get_tick_array_address(start));
                    match amm.pool_state.next_initialized_tick_array_start_index(
                        &amm.bitmap_extension,
                        start,
                        dir,
                    ) {
                        Ok(Some(next)) => {
                            start = next;
                        }
                        _ => break,
                    }
                }
            }
        }
        for addr in full_tick_addrs.into_iter() {
            if !account_map.contains_key(&addr) {
                if let Ok(acc) = rpc.get_account(&addr) {
                    account_map.insert(addr, acc.into());
                }
            }
        }
        // Re-sync AMM with the enriched account map so that both SDK math and
        // LiteSVM simulation see the same complete tick array set.
        amm.update(&account_map).unwrap();

        // Log decay-fee related configuration for this pool so we can see
        // whether dynamic fee is enabled and on which side it applies.
        println!(
            "Pool decay_fee_flag={}, init_rate={}, decrease_rate={}, interval={}",
            amm.pool_state.decay_fee_flag,
            amm.pool_state.decay_fee_init_fee_rate,
            amm.pool_state.decay_fee_decrease_rate,
            amm.pool_state.decay_fee_decrease_interval,
        );
        println!(
            "Decay fee enabled={}, on_sell_mint0={}, on_sell_mint1={}",
            amm.is_decay_fee_enabled(),
            amm.is_decay_fee_on_sell_mint0(),
            amm.is_decay_fee_on_sell_mint1(),
        );
        println!(
            "Base trade_fee_rate={}, protocol_fee_rate={}, fund_fee_rate={}",
            amm.amm_config.trade_fee_rate,
            amm.amm_config.protocol_fee_rate,
            amm.amm_config.fund_fee_rate,
        );

        // Debug: print SDK-side discriminators and CLMM-owned accounts'
        // first 8 bytes to verify which accounts are actually tick arrays
        // from the program's perspective (fixed or dynamic).
        println!(
            "TickArrayState discriminator (SDK): {:?}",
            TickArrayState::DISCRIMINATOR
        );
        println!(
            "DynTickArrayState discriminator (SDK): {:?}",
            DynTickArrayState::DISCRIMINATOR
        );
        for (addr, acc) in account_map.iter() {
            if acc.owner != BYREAL_CLMM_PROGRAM || acc.data.len() < 8 {
                continue;
            }
            let disc = &acc.data[0..8];
            let kind = if disc == TickArrayState::DISCRIMINATOR {
                "fixed_tick_array"
            } else if disc == DynTickArrayState::DISCRIMINATOR {
                "dyn_tick_array"
            } else {
                "other_clmm_account"
            };
            println!("CLMM account {} kind={} disc_bytes={:?}", addr, kind, disc);
        }

        // Discover user's cbBTC/USDC token accounts for this test case
        let user = Pubkey::from_str("DdZR6zRFiUt4S5mg7AV1uKB2z1f1WzcNYCaTEEWPAuby").unwrap();
        let cb_btc_accounts = rpc
            .get_token_accounts_by_owner(&user, TokenAccountsFilter::Mint(cb_btc_mint))
            .unwrap();
        let usdc_accounts = rpc
            .get_token_accounts_by_owner(&user, TokenAccountsFilter::Mint(usdc_mint))
            .unwrap();
        if cb_btc_accounts.is_empty() || usdc_accounts.is_empty() {
            println!("User missing cbBTC or USDC ATA; skipping LiteSVM test.");
            return;
        }
        let cb_btc_ata = Pubkey::from_str(&cb_btc_accounts[0].pubkey).unwrap();
        let usdc_ata = Pubkey::from_str(&usdc_accounts[0].pubkey).unwrap();

        // Use the same input amount as the aggregator scenario: 1_250_000 cbBTC base units.
        let amount_in: u64 = 1_250_000;
        if amount_in == 0 {
            println!(
                    "User USDC ATA {} has insufficient balance; skipping LiteSVM test.",
                    usdc_ata
            );
            return;
        }

        // SDK quote baseline: cbBTC -> USDC (ExactIn)
        let sdk_quote = amm
            .quote(&QuoteParams {
                amount: amount_in,
                input_mint: cb_btc_mint,
                output_mint: usdc_mint,
                swap_mode: SwapMode::ExactIn,
            })
            .unwrap();
        println!(
            "SDK quote: in={}, out={}, fee={}",
            sdk_quote.in_amount, sdk_quote.out_amount, sdk_quote.fee_amount
        );

        // 2. Build LiteSVM VM and load Byreal CLMM BPF program
        let mut svm = LiteSVM::new()
            .with_sysvars()
            .with_builtins()
            .with_default_programs()
            .with_sigverify(false)
            .with_blockhash_check(false);

        // Path is relative to this crate's src/, use two levels up.
        let program_bytes =
            include_bytes!("/Users/sh01426ml/Projects/rust/byreal-clmm/target/sbf-solana-solana/release/byreal_clmm.so");
        let clmm_program = RawPubkey::new_from_array(BYREAL_CLMM_PROGRAM.to_bytes());
        svm.add_program(clmm_program, program_bytes).unwrap();

        // 3. Write the same pool/tick/aux accounts into LiteSVM
        for (addr, acc) in account_map.iter() {
            let raw_addr = RawPubkey::new_from_array(addr.to_bytes());
            let raw_acc = RawAccount {
                lamports: acc.lamports,
                data: acc.data.clone(),
                owner: RawPubkey::new_from_array(acc.owner.to_bytes()),
                executable: acc.executable,
                rent_epoch: acc.rent_epoch,
            };
            svm.set_account(raw_addr, raw_acc).unwrap();
        }

        // Also write user cbBTC/USDC token accounts from mainnet snapshot.
        // For cbBTC ATA, ensure the simulated balance is sufficient to
        // cover the input amount, so we don't fail with "insufficient
        // funds" while validating math.
        {
            let acc = rpc.get_account(&cb_btc_ata).unwrap();
            let mut cbbtc_data = acc.data.clone();
            if let Ok(mut token_acc) =
                anchor_spl::token::spl_token::state::Account::unpack(&cbbtc_data)
            {
                if token_acc.amount < amount_in {
                    token_acc.amount = amount_in.saturating_mul(2);
                    let mut new_data = vec![0u8; cbbtc_data.len()];
                    anchor_spl::token::spl_token::state::Account::pack(
                        token_acc,
                        &mut new_data,
                    )
                    .unwrap();
                    cbbtc_data = new_data;
                }
            }
            let raw_addr = RawPubkey::new_from_array(cb_btc_ata.to_bytes());
            let raw_acc = RawAccount {
                lamports: acc.lamports,
                data: cbbtc_data,
                owner: RawPubkey::new_from_array(acc.owner.to_bytes()),
                executable: acc.executable,
                rent_epoch: acc.rent_epoch,
            };
            svm.set_account(raw_addr, raw_acc).unwrap();
        }
        {
            let acc = rpc.get_account(&usdc_ata).unwrap();
            let raw_addr = RawPubkey::new_from_array(usdc_ata.to_bytes());
            let raw_acc = RawAccount {
                lamports: acc.lamports,
                data: acc.data,
                owner: RawPubkey::new_from_array(acc.owner.to_bytes()),
                executable: acc.executable,
                rent_epoch: acc.rent_epoch,
            };
            svm.set_account(raw_addr, raw_acc).unwrap();
        }

        // Ensure user lamport account exists in LiteSVM via airdrop
        let user_raw = RawPubkey::new_from_array(user.to_bytes());
        svm.airdrop(&user_raw, 1_000_000_000).unwrap();

        // Align LiteSVM clock so that swap pool is considered "open".
        // The on-chain program requires block_timestamp > pool_state.open_time.
        let mut clock_sysvar: RawClock = svm.get_sysvar();
        // pool_state.open_time is u64 seconds; set VM clock just after that.
        clock_sysvar.unix_timestamp = (amm.pool_state.open_time as i64).saturating_add(1);
        svm.set_sysvar(&clock_sysvar);

        // 4. Construct swap instruction accounts directly from the snapshot.
        //    First, follow the on-chain bitmap navigation to compute the
        //    ordered tick_array start indices in the swap direction, so that
        //    the tick_array_states queue matches the program's expectations
        //    and avoids NotEnoughTickArrayAccount due to ordering issues.
        //    Here we simulate cbBTC -> USDC, so zero_for_one is decided by cbBTC mint.
        let zero_for_one = cb_btc_mint == amm.pool_state.token_mint_0;
        let (input_vault, output_vault) = if zero_for_one {
            (amm.pool_state.token_vault_0, amm.pool_state.token_vault_1)
        } else {
            (amm.pool_state.token_vault_1, amm.pool_state.token_vault_0)
        };

        // All accounts in the snapshot that look like tick arrays (used as fallback)
        let mut all_tick_arrays: Vec<Pubkey> = Vec::new();
        for (addr, acc) in account_map.iter() {
            if acc.owner != BYREAL_CLMM_PROGRAM || acc.data.len() < 8 {
                continue;
            }
            let disc = &acc.data[0..8];
            if disc == DynTickArrayState::DISCRIMINATOR
                || disc == TickArrayState::DISCRIMINATOR
            {
                all_tick_arrays.push(*addr);
            }
        }
        if all_tick_arrays.is_empty() {
            panic!("No tick array accounts with valid discriminator found in snapshot");
        }

        // Traverse all initialized tick_array start indices in the current
        // swap direction using pool_state + bitmap_extension to derive an
        // ordered list of tick_array addresses.
        let mut ordered_tick_arrays: Vec<Pubkey> = Vec::new();
        if let Ok((_, mut start)) =
            amm.pool_state
                .get_first_initialized_tick_array(&amm.bitmap_extension, zero_for_one)
        {
            loop {
                let addr = amm.get_tick_array_address(start);
                if account_map.contains_key(&addr) {
                    ordered_tick_arrays.push(addr);
                }
                match amm.pool_state.next_initialized_tick_array_start_index(
                    &amm.bitmap_extension,
                    start,
                    zero_for_one,
                ) {
                    Ok(Some(next)) => {
                        start = next;
                    }
                    _ => break,
                }
            }
        }

        // If bitmap navigation yields nothing (e.g. missing bitmap_extension),
        // fall back to the full set of known tick_array accounts to ensure
        // the set is never empty.
        if ordered_tick_arrays.is_empty() {
            ordered_tick_arrays.extend(all_tick_arrays.iter().copied());
        }
        if ordered_tick_arrays.is_empty() {
            panic!("No ordered tick array accounts available for swap");
        }

        println!("Selected tick_array candidates (ordered_tick_arrays):");
        for addr in ordered_tick_arrays.iter() {
            if let Some(acc) = account_map.get(addr) {
                let disc = if acc.data.len() >= 8 {
                    Some(&acc.data[0..8])
                } else {
                    None
                };
                println!("- {} disc_bytes={:?}", addr, disc);
            }
        }

        #[derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)]
        struct SwapIxArgs {
            amount: u64,
            other_amount_threshold: u64,
            sqrt_price_limit_x64: u128,
            is_base_input: bool,
        }

        // Discriminator for `swap` from byreal_clmm IDL
        let mut data = vec![248u8, 198, 158, 145, 225, 117, 135, 200];
        data.extend(
            SwapIxArgs {
                amount: amount_in,
                other_amount_threshold: 0,
                sqrt_price_limit_x64: 0,
                is_base_input: true,
            }
            .try_to_vec()
            .unwrap(),
        );

        // Build accounts following byreal_clmm `swap` IDL:
        // payer, amm_config, pool_state, input_token_account, output_token_account,
        // input_vault, output_vault, observation_state, token_program, tick_array,
        // then remaining_accounts: bitmap extension (if any) + other tick arrays,
        // in the exact order computed in ordered_tick_arrays.
        let mut accounts: Vec<RawAccountMeta> = Vec::new();

        // payer (signer)
        accounts.push(RawAccountMeta {
            pubkey: RawPubkey::new_from_array(user.to_bytes()),
            is_signer: true,
            is_writable: false,
        });
        // amm_config
        accounts.push(RawAccountMeta {
            pubkey: RawPubkey::new_from_array(amm.pool_state.amm_config.to_bytes()),
            is_signer: false,
            is_writable: false,
        });
        // pool_state
        accounts.push(RawAccountMeta {
            pubkey: RawPubkey::new_from_array(amm.key.to_bytes()),
            is_signer: false,
            is_writable: true,
        });
        // input / output user token accounts (cbBTC -> USDC)
        accounts.push(RawAccountMeta {
            pubkey: RawPubkey::new_from_array(cb_btc_ata.to_bytes()),
            is_signer: false,
            is_writable: true,
        });
        accounts.push(RawAccountMeta {
            pubkey: RawPubkey::new_from_array(usdc_ata.to_bytes()),
            is_signer: false,
            is_writable: true,
        });
        // input / output vaults
        accounts.push(RawAccountMeta {
            pubkey: RawPubkey::new_from_array(input_vault.to_bytes()),
            is_signer: false,
            is_writable: true,
        });
        accounts.push(RawAccountMeta {
            pubkey: RawPubkey::new_from_array(output_vault.to_bytes()),
            is_signer: false,
            is_writable: true,
        });
        // observation_state
        accounts.push(RawAccountMeta {
            pubkey: RawPubkey::new_from_array(amm.pool_state.observation_key.to_bytes()),
            is_signer: false,
            is_writable: true,
        });
        // token_program
        accounts.push(RawAccountMeta {
            pubkey: RawPubkey::new_from_array(anchor_spl::token::ID.to_bytes()),
            is_signer: false,
            is_writable: false,
        });

        // Primary tick array (named account in SwapSingle)
        let primary_tick_array = ordered_tick_arrays[0];
        accounts.push(RawAccountMeta {
            pubkey: RawPubkey::new_from_array(primary_tick_array.to_bytes()),
            is_signer: false,
            is_writable: true,
        });

        // Remaining accounts: bitmap extension (if present) then other tick arrays
        let bitmap_key = TickArrayBitmapExtension::key(amm.key);
        if account_map.contains_key(&bitmap_key) {
            accounts.push(RawAccountMeta {
                pubkey: RawPubkey::new_from_array(bitmap_key.to_bytes()),
                is_signer: false,
                is_writable: false,
            });
        }
        for addr in ordered_tick_arrays.iter().skip(1) {
            accounts.push(RawAccountMeta {
                pubkey: RawPubkey::new_from_array(addr.to_bytes()),
                is_signer: false,
                is_writable: true,
            });
        }

        let raw_ix = RawInstruction {
            program_id: clmm_program,
            accounts,
            data,
        };

        // Use the real user as fee payer in the message so LiteSVM's
        // fee-payer validation passes (we already airdropped lamports
        // to `user_raw` above).
        let msg = RawMessage::new(&[raw_ix], Some(&user_raw));
        // Construct an unsigned transaction; LiteSVM is configured with
        // `with_sigverify(false)` and `with_blockhash_check(false)`, so we
        // don't need real signatures here, only the correct signer layout.
        let tx = RawTransaction::new_unsigned(msg);

        // 5. Simulate transaction in LiteSVM
        let sim = svm
            .simulate_transaction(tx)
            .expect("LiteSVM simulate_transaction should succeed");

        #[cfg(feature = "debug-swap-steps")]
        println!("LiteSVM logs: {:?}", sim.meta.logs);

        // Find post-simulated USDC ATA and compute out amount
        let usdc_raw = RawPubkey::new_from_array(usdc_ata.to_bytes());
        let mut post_usdc_amount: Option<u64> = None;
        for (pk, acc) in sim.post_accounts.iter() {
            if *pk == usdc_raw {
                let raw: RawAccount = (*acc).clone().into();
                if let Ok(token_acc) =
                    anchor_spl::token::spl_token::state::Account::unpack(&raw.data)
                {
                    post_usdc_amount = Some(token_acc.amount);
                }
            }
        }

        if post_usdc_amount.is_none() {
            println!("LiteSVM did not modify USDC ATA; logs: {:?}", sim.meta.logs);
            return;
        }

        let pre_usdc_acc = rpc.get_account(&usdc_ata).unwrap();
        let pre_usdc_token =
            anchor_spl::token::spl_token::state::Account::unpack(&pre_usdc_acc.data).unwrap();
        let pre_amount = pre_usdc_token.amount;
        let post_amount = post_usdc_amount.unwrap();
        let litesvm_out = post_amount.saturating_sub(pre_amount);

        println!(
            "LiteSVM out={}, diff (sdk_math - litesvm)={}",
            litesvm_out,
            sdk_quote.out_amount.saturating_sub(litesvm_out)
        );

        // In ideal case we expect exact match; for now just print diff for inspection.
    }

    /// LiteSVM vs SDK quote test (ExactOut) for the Byreal cbBTC/USDC CLMM pool.
    ///
    /// Scenario:
    /// - Same pool / snapshot as `test_litesvm_vs_sdk_byreal_jup_usdc`;
    /// - Direction: cbBTC -> USDC, user wants EXACT_OUT of 2000 USDC;
    /// - SDK computes the required cbBTC input via `SwapMode::ExactOut`;
    /// - LiteSVM simulates the on-chain `swap` with `is_base_input = false`;
    /// - We compare the required cbBTC input between SDK math and LiteSVM.
    #[cfg(feature = "with-litesvm")]
    #[test]
    #[ignore]
    fn test_litesvm_vs_sdk_byreal_jup_usdc_exact_out() {
        use litesvm::LiteSVM;
        use solana_clock::Clock as RawClock;
        use solana_account::Account as RawAccount;
        use solana_pubkey::Pubkey as RawPubkey;
        use solana_instruction::{account_meta::AccountMeta as RawAccountMeta, Instruction as RawInstruction};
        use solana_message::Message as RawMessage;
        use solana_transaction::Transaction as RawTransaction;
        use solana_client::rpc_request::TokenAccountsFilter;

        // 1. Build SDK AMM from mainnet snapshot
        let rpc = RpcClient::new(
            "https://api.mainnet-beta.solana.com",
        );
        let pool_address =
            Pubkey::from_str("A5vkCw1VXPNXq5VFbffPm6Bo4kVKAP1UUoRrEn3gyVey").unwrap();
        let usdc_mint = Pubkey::from_str("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v").unwrap();

        let account = rpc.get_account(&pool_address).unwrap();
        let keyed_account = KeyedAccount {
            key: pool_address,
            account: account.into(),
            params: None,
        };
        let amm_context = AmmContext {
            clock_ref: ClockRef::default(),
        };
        let mut amm = ByrealClmmAmm::from_keyed_account(&keyed_account, &amm_context).unwrap();

        // Pull all relevant accounts (pool, vaults, config, observation, bitmap, tick arrays).
        let accounts_to_update = amm.get_accounts_to_update();
        let accounts = rpc.get_multiple_accounts(&accounts_to_update).unwrap();
        let mut account_map: AccountMap = accounts_to_update
            .iter()
            .enumerate()
            .filter_map(|(i, key)| {
                accounts[i]
                    .as_ref()
                    .map(|account| (*key, account.clone().into()))
            })
            .collect();
        amm.update(&account_map).unwrap();

        // Derive cbBTC mint from pool state (the non-USDC side) and sanity-check
        // against the expected cbBTC mint used by the aggregator.
        let expected_cb_btc_mint =
            Pubkey::from_str("cbbtcf3aa214zXHbiAZQwf4122FBYbraNdFqgw4iMij").unwrap();
        let cb_btc_mint = if amm.pool_state.token_mint_0 == usdc_mint {
            amm.pool_state.token_mint_1
        } else if amm.pool_state.token_mint_1 == usdc_mint {
            amm.pool_state.token_mint_0
        } else {
            panic!(
                "Pool {} is not a USDC-cbBTC pool (mints: {}, {})",
                pool_address,
                amm.pool_state.token_mint_0,
                amm.pool_state.token_mint_1
            );
        };
        assert_eq!(
            cb_btc_mint, expected_cb_btc_mint,
            "Pool {} cbBTC mint mismatch (pool side vs expected)",
            pool_address
        );

        // Enrich tick-array snapshot via bitmap navigation (both directions).
        let mut full_tick_addrs: HashSet<Pubkey> = HashSet::new();
        for &dir in &[true, false] {
            if let Ok((_, mut start)) =
                amm.pool_state.get_first_initialized_tick_array(&amm.bitmap_extension, dir)
            {
                loop {
                    full_tick_addrs.insert(amm.get_tick_array_address(start));
                    match amm.pool_state.next_initialized_tick_array_start_index(
                        &amm.bitmap_extension,
                        start,
                        dir,
                    ) {
                        Ok(Some(next)) => {
                            start = next;
                        }
                        _ => break,
                    }
                }
            }
        }
        for addr in full_tick_addrs.into_iter() {
            if !account_map.contains_key(&addr) {
                if let Ok(acc) = rpc.get_account(&addr) {
                    account_map.insert(addr, acc.into());
                }
            }
        }
        amm.update(&account_map).unwrap();

        // Log decay-fee configuration for this pool.
        println!(
            "Pool decay_fee_flag={}, init_rate={}, decrease_rate={}, interval={}",
            amm.pool_state.decay_fee_flag,
            amm.pool_state.decay_fee_init_fee_rate,
            amm.pool_state.decay_fee_decrease_rate,
            amm.pool_state.decay_fee_decrease_interval,
        );
        println!(
            "Decay fee enabled={}, on_sell_mint0={}, on_sell_mint1={}",
            amm.is_decay_fee_enabled(),
            amm.is_decay_fee_on_sell_mint0(),
            amm.is_decay_fee_on_sell_mint1(),
        );
        println!(
            "Base trade_fee_rate={}, protocol_fee_rate={}, fund_fee_rate={}",
            amm.amm_config.trade_fee_rate,
            amm.amm_config.protocol_fee_rate,
            amm.amm_config.fund_fee_rate,
        );

        // CLMM-owned accounts and their discriminators (for debugging).
        println!(
            "TickArrayState discriminator (SDK): {:?}",
            TickArrayState::DISCRIMINATOR
        );
        println!(
            "DynTickArrayState discriminator (SDK): {:?}",
            DynTickArrayState::DISCRIMINATOR
        );
        for (addr, acc) in account_map.iter() {
            if acc.owner != BYREAL_CLMM_PROGRAM || acc.data.len() < 8 {
                continue;
            }
            let disc = &acc.data[0..8];
            let kind = if disc == TickArrayState::DISCRIMINATOR {
                "fixed_tick_array"
            } else if disc == DynTickArrayState::DISCRIMINATOR {
                "dyn_tick_array"
            } else {
                "other_clmm_account"
            };
            println!("CLMM account {} kind={} disc_bytes={:?}", addr, kind, disc);
        }

        // Discover user's cbBTC/USDC token accounts (reuse the same authority as the ExactIn test).
        let user = Pubkey::from_str("DdZR6zRFiUt4S5mg7AV1uKB2z1f1WzcNYCaTEEWPAuby").unwrap();
        let cb_btc_accounts = rpc
            .get_token_accounts_by_owner(&user, TokenAccountsFilter::Mint(cb_btc_mint))
            .unwrap();
        let usdc_accounts = rpc
            .get_token_accounts_by_owner(&user, TokenAccountsFilter::Mint(usdc_mint))
            .unwrap();
        if cb_btc_accounts.is_empty() || usdc_accounts.is_empty() {
            println!(
                "User {} missing cbBTC or USDC ATA; skipping ExactOut LiteSVM test.",
                user
            );
            return;
        }
        let cb_btc_ata = Pubkey::from_str(&cb_btc_accounts[0].pubkey).unwrap();
        let usdc_ata = Pubkey::from_str(&usdc_accounts[0].pubkey).unwrap();

        // Desired exact-out amount: 2000 USDC (6 decimals).
        let desired_out: u64 = 2_000_000_000;

        // SDK quote baseline: cbBTC -> USDC (ExactOut)
        let sdk_quote = amm
            .quote(&QuoteParams {
                amount: desired_out,
                input_mint: cb_btc_mint,
                output_mint: usdc_mint,
                swap_mode: SwapMode::ExactOut,
            })
            .unwrap();
        println!(
            "SDK exact-out quote: in={} (cbBTC), out={} (USDC), fee={}",
            sdk_quote.in_amount, sdk_quote.out_amount, sdk_quote.fee_amount
        );

        // 2. Build LiteSVM VM and load Byreal CLMM BPF program
        let mut svm = LiteSVM::new()
            .with_sysvars()
            .with_builtins()
            .with_default_programs()
            .with_sigverify(false)
            .with_blockhash_check(false);

        let program_bytes =
            include_bytes!("/Users/sh01426ml/Projects/rust/byreal-clmm/target/sbf-solana-solana/release/byreal_clmm.so");
        let clmm_program = RawPubkey::new_from_array(BYREAL_CLMM_PROGRAM.to_bytes());
        svm.add_program(clmm_program, program_bytes).unwrap();

        // 3. Write pool/tick/aux accounts into LiteSVM
        for (addr, acc) in account_map.iter() {
            let raw_addr = RawPubkey::new_from_array(addr.to_bytes());
            let raw_acc = RawAccount {
                lamports: acc.lamports,
                data: acc.data.clone(),
                owner: RawPubkey::new_from_array(acc.owner.to_bytes()),
                executable: acc.executable,
                rent_epoch: acc.rent_epoch,
            };
            svm.set_account(raw_addr, raw_acc).unwrap();
        }

        // Also write user cbBTC/USDC token accounts. For cbBTC ATA, ensure the
        // simulated balance is sufficient to cover the SDK-computed required
        // input amount, so that we do not fail with "insufficient funds" while
        // validating the math.
        {
            // cbBTC ATA with topped-up balance in the LiteSVM VM
            let acc = rpc.get_account(&cb_btc_ata).unwrap();
            let mut cbbtc_data = acc.data.clone();
            if let Ok(mut token_acc) =
                anchor_spl::token::spl_token::state::Account::unpack(&cbbtc_data)
            {
                if token_acc.amount < sdk_quote.in_amount {
                    token_acc.amount = sdk_quote.in_amount.saturating_mul(2);
                    let mut new_data = vec![0u8; cbbtc_data.len()];
                    anchor_spl::token::spl_token::state::Account::pack(
                        token_acc,
                        &mut new_data,
                    )
                    .unwrap();
                    cbbtc_data = new_data;
                }
            }
            let raw_addr = RawPubkey::new_from_array(cb_btc_ata.to_bytes());
            let raw_acc = RawAccount {
                lamports: acc.lamports,
                data: cbbtc_data,
                owner: RawPubkey::new_from_array(acc.owner.to_bytes()),
                executable: acc.executable,
                rent_epoch: acc.rent_epoch,
            };
            svm.set_account(raw_addr, raw_acc).unwrap();
        }
        {
            // USDC ATA unchanged
            let acc = rpc.get_account(&usdc_ata).unwrap();
            let raw_addr = RawPubkey::new_from_array(usdc_ata.to_bytes());
            let raw_acc = RawAccount {
                lamports: acc.lamports,
                data: acc.data,
                owner: RawPubkey::new_from_array(acc.owner.to_bytes()),
                executable: acc.executable,
                rent_epoch: acc.rent_epoch,
            };
            svm.set_account(raw_addr, raw_acc).unwrap();
        }

        // Ensure user lamport account exists in LiteSVM via airdrop
        let user_raw = RawPubkey::new_from_array(user.to_bytes());
        svm.airdrop(&user_raw, 1_000_000_000).unwrap();

        // Align LiteSVM clock so that swap pool is considered "open".
        let mut clock_sysvar: RawClock = svm.get_sysvar();
        clock_sysvar.unix_timestamp = (amm.pool_state.open_time as i64).saturating_add(1);
        svm.set_sysvar(&clock_sysvar);

        // 4. Construct swap instruction accounts + data (ExactOut).
        let zero_for_one = cb_btc_mint == amm.pool_state.token_mint_0;
        let (input_vault, output_vault) = if zero_for_one {
            (amm.pool_state.token_vault_0, amm.pool_state.token_vault_1)
        } else {
            (amm.pool_state.token_vault_1, amm.pool_state.token_vault_0)
        };

        // Ordered tick arrays in swap direction.
        let mut all_tick_arrays: Vec<Pubkey> = Vec::new();
        for (addr, acc) in account_map.iter() {
            if acc.owner != BYREAL_CLMM_PROGRAM || acc.data.len() < 8 {
                continue;
            }
            let disc = &acc.data[0..8];
            if disc == DynTickArrayState::DISCRIMINATOR
                || disc == TickArrayState::DISCRIMINATOR
            {
                all_tick_arrays.push(*addr);
            }
        }
        if all_tick_arrays.is_empty() {
            panic!("No tick array accounts with valid discriminator found in snapshot");
        }

        let mut ordered_tick_arrays: Vec<Pubkey> = Vec::new();
        if let Ok((_, mut start)) =
            amm.pool_state
                .get_first_initialized_tick_array(&amm.bitmap_extension, zero_for_one)
        {
            loop {
                let addr = amm.get_tick_array_address(start);
                if account_map.contains_key(&addr) {
                    ordered_tick_arrays.push(addr);
                }
                match amm.pool_state.next_initialized_tick_array_start_index(
                    &amm.bitmap_extension,
                    start,
                    zero_for_one,
                ) {
                    Ok(Some(next)) => {
                        start = next;
                    }
                    _ => break,
                }
            }
        }
        if ordered_tick_arrays.is_empty() {
            ordered_tick_arrays.extend(all_tick_arrays.iter().copied());
        }
        if ordered_tick_arrays.is_empty() {
            panic!("No ordered tick array accounts available for swap");
        }
        println!("Selected tick_array candidates (ordered_tick_arrays):");
        for addr in ordered_tick_arrays.iter() {
            if let Some(acc) = account_map.get(addr) {
                let disc = if acc.data.len() >= 8 {
                    Some(&acc.data[0..8])
                } else {
                    None
                };
                println!("- {} disc_bytes={:?}", addr, disc);
            }
        }

        #[derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)]
        struct SwapIxArgs {
            amount: u64,
            other_amount_threshold: u64,
            sqrt_price_limit_x64: u128,
            is_base_input: bool,
        }

        // Use ExactOut semantics: amount = desired USDC out, is_base_input = false.
        // Set other_amount_threshold to sdk_quote.in_amount so that the on-chain
        // slippage check matches the SDK's computed min input.
        let mut data = vec![248u8, 198, 158, 145, 225, 117, 135, 200];
        data.extend(
            SwapIxArgs {
                amount: desired_out,
                other_amount_threshold: sdk_quote.in_amount,
                sqrt_price_limit_x64: 0,
                is_base_input: false,
            }
            .try_to_vec()
            .unwrap(),
        );

        // Accounts: payer, amm_config, pool_state, input/output token accounts,
        // vaults, observation, token_program, primary tick_array, bitmap extension, remaining arrays.
        let mut accounts: Vec<RawAccountMeta> = Vec::new();

        // payer
        accounts.push(RawAccountMeta {
            pubkey: RawPubkey::new_from_array(user.to_bytes()),
            is_signer: true,
            is_writable: false,
        });
        // amm_config
        accounts.push(RawAccountMeta {
            pubkey: RawPubkey::new_from_array(amm.pool_state.amm_config.to_bytes()),
            is_signer: false,
            is_writable: false,
        });
        // pool_state
        accounts.push(RawAccountMeta {
            pubkey: RawPubkey::new_from_array(amm.key.to_bytes()),
            is_signer: false,
            is_writable: true,
        });
        // input/output user token accounts (cbBTC -> USDC)
        accounts.push(RawAccountMeta {
            pubkey: RawPubkey::new_from_array(cb_btc_ata.to_bytes()),
            is_signer: false,
            is_writable: true,
        });
        accounts.push(RawAccountMeta {
            pubkey: RawPubkey::new_from_array(usdc_ata.to_bytes()),
            is_signer: false,
            is_writable: true,
        });
        // input / output vaults
        accounts.push(RawAccountMeta {
            pubkey: RawPubkey::new_from_array(input_vault.to_bytes()),
            is_signer: false,
            is_writable: true,
        });
        accounts.push(RawAccountMeta {
            pubkey: RawPubkey::new_from_array(output_vault.to_bytes()),
            is_signer: false,
            is_writable: true,
        });
        // observation_state
        accounts.push(RawAccountMeta {
            pubkey: RawPubkey::new_from_array(amm.pool_state.observation_key.to_bytes()),
            is_signer: false,
            is_writable: true,
        });
        // token_program
        accounts.push(RawAccountMeta {
            pubkey: RawPubkey::new_from_array(anchor_spl::token::ID.to_bytes()),
            is_signer: false,
            is_writable: false,
        });

        // primary tick_array
        let primary_tick_array = ordered_tick_arrays[0];
        accounts.push(RawAccountMeta {
            pubkey: RawPubkey::new_from_array(primary_tick_array.to_bytes()),
            is_signer: false,
            is_writable: true,
        });

        // bitmap extension + remaining tick arrays
        let bitmap_key = TickArrayBitmapExtension::key(amm.key);
        if account_map.contains_key(&bitmap_key) {
            accounts.push(RawAccountMeta {
                pubkey: RawPubkey::new_from_array(bitmap_key.to_bytes()),
                is_signer: false,
                is_writable: false,
            });
        }
        for addr in ordered_tick_arrays.iter().skip(1) {
            accounts.push(RawAccountMeta {
                pubkey: RawPubkey::new_from_array(addr.to_bytes()),
                is_signer: false,
                is_writable: true,
            });
        }

        let raw_ix = RawInstruction {
            program_id: clmm_program,
            accounts,
            data,
        };

        let msg = RawMessage::new(&[raw_ix], Some(&user_raw));
        let tx = RawTransaction::new_unsigned(msg);

        // 5. Simulate transaction in LiteSVM
        let sim = svm
            .simulate_transaction(tx)
            .expect("LiteSVM simulate_transaction (ExactOut) should succeed");

        #[cfg(feature = "debug-swap-steps")]
        println!("LiteSVM exact-out logs: {:?}", sim.meta.logs);

        // Compute cbBTC input from pre/post cbBTC ATA.
        let cb_btc_raw = RawPubkey::new_from_array(cb_btc_ata.to_bytes());
        let mut post_cb_btc_amount: Option<u64> = None;
        for (pk, acc) in sim.post_accounts.iter() {
            if *pk == cb_btc_raw {
                let raw: RawAccount = (*acc).clone().into();
                if let Ok(token_acc) =
                    anchor_spl::token::spl_token::state::Account::unpack(&raw.data)
                {
                    post_cb_btc_amount = Some(token_acc.amount);
                }
            }
        }
        if post_cb_btc_amount.is_none() {
            println!(
                "LiteSVM (ExactOut) did not modify cbBTC ATA; logs: {:?}",
                sim.meta.logs
            );
            return;
        }

        let pre_cb_btc_acc = rpc.get_account(&cb_btc_ata).unwrap();
        let pre_cb_btc_token =
            anchor_spl::token::spl_token::state::Account::unpack(&pre_cb_btc_acc.data).unwrap();
        let pre_in = pre_cb_btc_token.amount;
        let post_in = post_cb_btc_amount.unwrap();
        let litesvm_in = pre_in.saturating_sub(post_in);

        println!(
            "LiteSVM ExactOut in_cbBTC={}, diff (sdk_in - litesvm_in)={}",
            litesvm_in,
            sdk_quote.in_amount.saturating_sub(litesvm_in)
        );
    }
}

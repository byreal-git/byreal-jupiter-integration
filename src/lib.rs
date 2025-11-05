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

    /// Extract the start_tick_index from either a fixed or dynamic tick array account data.
    fn get_tick_array_start_index_from_bytes(&self, data: &[u8]) -> Option<i32> {
        if data.len() >= 8 {
            if &data[0..8] == DynTickArrayState::DISCRIMINATOR {
                let (h, _) = self.decode_dyn_tick_array(data)?;
                return Some(h.start_tick_index);
            } else if &data[0..8] == TickArrayState::DISCRIMINATOR {
                let ta = self.decode_fixed_tick_array(data)?;
                return Some(ta.start_tick_index);
            }
        }
        None
    }

    /// Find next initialized tick across tick arrays in the given direction.
    /// This mirrors the on-chain navigation: search within the current array, otherwise
    /// advance to the next initialized array and take its first initialized tick.
    fn find_next_initialized_tick(&self, current_tick: i32, zero_for_one: bool) -> Result<i32> {
        let spacing = self.pool_state.tick_spacing as u16;
        let arrays = self.get_swap_tick_arrays(zero_for_one);
        if arrays.is_empty() {
            // fallback to arithmetic next grid if nothing available
            let step = i32::from(spacing);
            return Ok(if zero_for_one { ((current_tick / step) - 1) * step } else { ((current_tick / step) + 1) * step });
        }

        // Determine the array corresponding to current_tick
        let current_start = TickUtils::get_array_start_index(current_tick, spacing);
        let mut idx = 0usize;
        let mut matched_current = false;
        for (i, addr) in arrays.iter().enumerate() {
            if let Some(bytes) = self.tick_arrays_raw.get(addr) {
                if let Some(start) = self.get_tick_array_start_index_from_bytes(bytes) {
                    if start == current_start { idx = i; matched_current = true; break; }
                }
            }
        }

        // Helper to fetch next initialized tick within an array
        let search_in_array = |addr: &Pubkey, cur_tick: i32, allow_first: bool| -> Option<i32> {
            let bytes = self.tick_arrays_raw.get(addr)?;
            if bytes.len() < 8 { return None; }
            if &bytes[0..8] == DynTickArrayState::DISCRIMINATOR {
                let (header, _ticks) = self.decode_dyn_tick_array(bytes)?;
                // Compute within-array search without mutating header
                let start = header.start_tick_index;
                let mut found_pos: Option<usize> = None;
                if !allow_first {
                    // search relative to current tick
                    if TickUtils::get_array_start_index(cur_tick, spacing) == start {
                        let mut offset_in_array = ((cur_tick - start) / (spacing as i32)) as i32;
                        if zero_for_one {
                            while offset_in_array >= 0 {
                                if header.tick_offset_index[offset_in_array as usize] > 0 {
                                    found_pos = Some(offset_in_array as usize); break;
                                }
                                offset_in_array -= 1;
                            }
                        } else {
                            offset_in_array += 1;
                            while offset_in_array < TICK_ARRAY_SIZE {
                                if header.tick_offset_index[offset_in_array as usize] > 0 {
                                    found_pos = Some(offset_in_array as usize); break;
                                }
                                offset_in_array += 1;
                            }
                        }
                    }
                }
                if found_pos.is_none() && allow_first {
                    if zero_for_one {
                        let mut i = TICK_ARRAY_SIZE - 1;
                        while i >= 0 {
                            if header.tick_offset_index[i as usize] > 0 { found_pos = Some(i as usize); break; }
                            i -= 1;
                        }
                    } else {
                        let mut i: usize = 0;
                        while i < TICK_ARRAY_SIZE as usize {
                            if header.tick_offset_index[i] > 0 { found_pos = Some(i); break; }
                            i += 1;
                        }
                    }
                }
                if let Some(off) = found_pos {
                    return Some(start + (off as i32) * (spacing as i32));
                }
            } else if &bytes[0..8] == TickArrayState::DISCRIMINATOR {
                if let Some(mut ta) = self.decode_fixed_tick_array(bytes) {
                    if let Ok(Some(ts)) = ta.next_initialized_tick(cur_tick, spacing, zero_for_one) {
                        return Some(ts.tick);
                    }
                    if allow_first {
                        if let Ok(ts) = ta.first_initialized_tick(zero_for_one) {
                            return Some(ts.tick);
                        }
                    }
                }
            }
            None
        };

        // If current array matches, try within it using current_tick
        if matched_current {
            if let Some(t) = search_in_array(&arrays[idx], current_tick, false) {
                return Ok(t);
            }
            // Otherwise advance in direction
            let iter: Box<dyn Iterator<Item = &Pubkey>> = if zero_for_one {
                Box::new(arrays[..idx].iter().rev())
            } else {
                Box::new(arrays[idx + 1..].iter())
            };
            for addr in iter {
                if let Some(t) = search_in_array(addr, current_tick, true) { return Ok(t); }
            }
        } else {
            // Not matched: start from the first array and take its first/next
            let iter: Box<dyn Iterator<Item = &Pubkey>> = if zero_for_one { Box::new(arrays.iter().rev()) } else { Box::new(arrays.iter()) };
            for addr in iter {
                if let Some(t) = search_in_array(addr, current_tick, true) { return Ok(t); }
            }
        }

        // Fallback grid if nothing found
        let step = i32::from(spacing);
        Ok(if zero_for_one { ((current_tick / step) - 1) * step } else { ((current_tick / step) + 1) * step })
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

        // Try bitmap-guided discovery (10 each direction)
        collect_dir(true, 10);
        collect_dir(false, 10);

        // Fallback to naive neighbors if nothing collected
        if start_indexes.is_empty() {
            let tick_spacing = self.pool_state.tick_spacing as u16;
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

        // Simulate swap steps
        let mut tick_crossings = 0;
        while state.amount_specified_remaining != 0 && 
              state.sqrt_price_x64 != sqrt_price_limit &&
              tick_crossings < MAX_TICK_ARRAY_CROSSINGS {
            
            // Find next initialized tick
            let next_tick = self.find_next_initialized_tick(state.tick, zero_for_one)?;
            
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

            // Update state
            state.sqrt_price_x64 = step.sqrt_price_next_x64;
            state.fee_amount += step.fee_amount;

            if is_base_input {
                state.amount_specified_remaining = state.amount_specified_remaining
                    .saturating_sub(step.amount_in + step.fee_amount);
                state.amount_calculated = state.amount_calculated
                    .saturating_add(step.amount_out);
            } else {
                state.amount_specified_remaining = state.amount_specified_remaining
                    .saturating_sub(step.amount_out);
                state.amount_calculated = state.amount_calculated
                    .saturating_add(step.amount_in + step.fee_amount);
            }

            // Update tick/liquidity if we've crossed an initialized tick boundary
            if state.sqrt_price_x64 == sqrt_price_next {
                // Adjust liquidity on crossing initialized tick
                if let Some(mut liq_net) = self.get_tick_liquidity_net(next_tick) {
                    if zero_for_one { liq_net = -liq_net; }
                    state.liquidity = liquidity_math::add_delta(state.liquidity, liq_net)
                        .map_err(|e| anyhow!("Failed to adjust liquidity at tick {}: {:?}", next_tick, e))?;
                }
                state.tick = if zero_for_one { next_tick - 1 } else { next_tick };
                tick_crossings += 1;
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

        // Tick arrays for this swap: first is the named `tick_array`, others + bitmap extension go as remaining accounts
        let tick_arrays = self.get_swap_tick_arrays(zero_for_one);
        if let Some((&first, rest)) = tick_arrays.split_first() {
            // Named tick_array account
            account_metas.push(AccountMeta::new(first, false));
            // Bitmap extension (readonly) to support out-of-bound bitmaps
            let bitmap_key = TickArrayBitmapExtension::key(self.key);
            account_metas.push(AccountMeta::new_readonly(bitmap_key, false));
            // Additional tick arrays as remaining accounts
            for ta in rest {
                account_metas.push(AccountMeta::new(*ta, false));
            }
        } else {
            // Fallback: still include current tick array and bitmap extension
            let tick_spacing = self.pool_state.tick_spacing as u16;
            let current_start = TickUtils::get_array_start_index(self.pool_state.tick_current, tick_spacing);
            account_metas.push(AccountMeta::new(self.get_tick_array_address(current_start), false));
            let bitmap_key = TickArrayBitmapExtension::key(self.key);
            account_metas.push(AccountMeta::new_readonly(bitmap_key, false));
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

        let pool_key = Pubkey::new_unique();
        let program_id = BYREAL_CLMM_PROGRAM;
        let mut pool_state = PoolState::default();
        pool_state.tick_spacing = 10;
        pool_state.tick_current = 55; // current tick sits in the first array (start 0)

        let mut amm = ByrealClmmAmm {
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

        // First array: start=0 has initialized ticks at offsets 3 (30) and 5 (50)
        let start0 = TickUtils::get_array_start_index(55, 10);
        let bytes0 = build_dyn_bytes(start0, 10, &[3, 5]);
        let addr0 = amm.get_tick_array_address(start0);
        amm.tick_arrays_raw.insert(addr0, bytes0);

        // Second array: start=600 has ticks at 0 (600) and 2 (620)
        let start1 = start0 + (TICK_ARRAY_SIZE as i32) * 10;
        let bytes1 = build_dyn_bytes(start1, 10, &[0, 2]);
        let addr1 = amm.get_tick_array_address(start1);
        amm.tick_arrays_raw.insert(addr1, bytes1);

        // zero_for_one=false (price increasing): next initialized >= current should be 600 in the next array
        let next_up = amm.find_next_initialized_tick(amm.pool_state.tick_current, false).unwrap();
        assert_eq!(next_up, start1);

        // zero_for_one=true (price decreasing): next initialized <= current should be 50 in current array
        let next_down = amm.find_next_initialized_tick(amm.pool_state.tick_current, true).unwrap();
        assert_eq!(next_down, 50);
    }
}

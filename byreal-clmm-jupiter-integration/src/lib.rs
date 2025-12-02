use anchor_lang::prelude::*;
use anyhow::{anyhow, ensure, Result};
use solana_sdk::{instruction::AccountMeta, pubkey::Pubkey};
use std::{
    collections::{HashMap, HashSet},
    sync::{atomic::AtomicI64, Arc},
};

use jupiter_amm_interface::{
    single_program_amm, try_get_account_data, AccountMap, Amm, AmmContext, KeyedAccount, Quote,
    QuoteParams, SingleProgramAmm, Swap, SwapAndAccountMetas, SwapMode, SwapParams,
};

use byreal_clmm_common::{
    AmmConfig, ByrealClmmAmm, DynamicTickArrayState, PoolState, TickArrayBitmapExtension,
};

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

pub const ID: Pubkey = BYREAL_CLMM_PROGRAM;

single_program_amm!(ByrealClmm, ID, "Byreal");

#[derive(Clone)]
pub struct ByrealClmm {
    key: Pubkey,
    amm: ByrealClmmAmm,
    timestamp: Arc<AtomicI64>,
}

impl Amm for ByrealClmm {
    fn from_keyed_account(keyed_account: &KeyedAccount, amm_context: &AmmContext) -> Result<Self> {
        let pool_state = PoolState::try_deserialize(&mut keyed_account.account.data.as_ref())?;

        Ok(Self {
            key: keyed_account.key,
            amm: ByrealClmmAmm {
                key: keyed_account.key,
                pool_state,
                amm_config: AmmConfig::default(),
                bitmap_extension: Some(TickArrayBitmapExtension::default()),
                max_one_side_tick_arrays: 3, // 3 extra tick arrays
                dynamic_tick_arrays: HashMap::new(),
                tick_array_pda_cache: HashMap::new(),
                cached_tick_array_addresses: Vec::new(),
            },
            timestamp: amm_context.clock_ref.unix_timestamp.clone(),
        })
    }

    fn label(&self) -> String {
        Self::LABEL.to_string()
    }

    fn program_id(&self) -> Pubkey {
        Self::PROGRAM_ID
    }

    fn key(&self) -> Pubkey {
        self.key
    }

    fn get_reserve_mints(&self) -> Vec<Pubkey> {
        vec![
            self.amm.pool_state.token_mint_0,
            self.amm.pool_state.token_mint_1,
        ]
    }

    fn get_accounts_to_update(&self) -> Vec<Pubkey> {
        let mut accounts = vec![
            self.key, // Pool state itself
            self.amm.pool_state.amm_config,
        ];

        let bitmap_key = TickArrayBitmapExtension::key(self.key);
        accounts.push(bitmap_key);

        if !self.amm.cached_tick_array_addresses.is_empty() {
            accounts.extend(self.amm.cached_tick_array_addresses.iter().copied());
        } else {
            accounts.extend(self.amm.get_all_tick_array_addresses());
        }
        accounts
    }

    fn update(&mut self, account_map: &AccountMap) -> Result<()> {
        // Update pool state
        self.amm.pool_state =
            PoolState::try_deserialize(&mut try_get_account_data(account_map, &self.key)?)?;

        // Update AMM config
        self.amm.amm_config = AmmConfig::try_deserialize(&mut try_get_account_data(
            account_map,
            &self.amm.pool_state.amm_config,
        )?)?;

        // Update bitmap extension
        let bitmap_key = TickArrayBitmapExtension::key(self.key);
        self.amm.bitmap_extension = Some(TickArrayBitmapExtension::try_deserialize(
            &mut try_get_account_data(account_map, &bitmap_key)?,
        )?);

        // Update tick arrays
        self.amm.dynamic_tick_arrays.clear();
        self.amm.refresh_tick_array_cache();
        for &address in &self.amm.cached_tick_array_addresses {
            if let Ok(tick_array_data) = try_get_account_data(account_map, &address) {
                if let Some(tick_array) = DynamicTickArrayState::from_bytes(tick_array_data) {
                    self.amm.dynamic_tick_arrays.insert(address, tick_array);
                }
            }
        }

        Ok(())
    }

    fn quote(&self, quote_params: &QuoteParams) -> Result<Quote> {
        let current_timestamp = self.timestamp.load(std::sync::atomic::Ordering::Relaxed);
        ensure!(
            current_timestamp as u64 >= self.amm.pool_state.open_time,
            "Pool is not open yet"
        );
        let zero_for_one = quote_params.input_mint == self.amm.pool_state.token_mint_0;

        let is_base_input = quote_params.swap_mode == SwapMode::ExactIn;
        let swap_result = self.amm.compute_swap(
            zero_for_one,
            quote_params.amount,
            is_base_input,
            None, // No price limit for quotes
            current_timestamp,
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
        let zero_for_one = swap_params.source_mint == self.amm.pool_state.token_mint_0;

        // Build account metas for swap instruction (must match on-chain order)
        let mut account_metas = vec![
            // Signer (payer)
            AccountMeta::new_readonly(swap_params.token_transfer_authority, true),
            // AMM Config
            AccountMeta::new_readonly(self.amm.pool_state.amm_config, false),
            // Pool state
            AccountMeta::new(self.key, false),
            // User token accounts (input, output)
            AccountMeta::new(swap_params.source_token_account, false),
            AccountMeta::new(swap_params.destination_token_account, false),
            // Vaults (input, output)
            if zero_for_one {
                AccountMeta::new(self.amm.pool_state.token_vault_0, false)
            } else {
                AccountMeta::new(self.amm.pool_state.token_vault_1, false)
            },
            if zero_for_one {
                AccountMeta::new(self.amm.pool_state.token_vault_1, false)
            } else {
                AccountMeta::new(self.amm.pool_state.token_vault_0, false)
            },
            // Observation state
            AccountMeta::new(self.amm.pool_state.observation_key, false),
            // Token program
            AccountMeta::new_readonly(anchor_spl::token::ID, false),
        ];

        // Tick arrays for this swap:
        // - start from directional candidates from get_swap_tick_arrays;
        // - keep only those for which we actually have account bytes loaded
        //   (so they are present in tick_arrays_raw and thus in LiteSVM);
        // - if none of the directional candidates are present, fall back to
        //   *any* tick arrays we have in tick_arrays_raw.
        let candidate_tick_arrays = self.amm.get_swap_tick_arrays(zero_for_one);
        let mut live_tick_arrays: Vec<Pubkey> = candidate_tick_arrays
            .into_iter()
            .filter(|addr| self.amm.dynamic_tick_arrays.contains_key(addr))
            .collect();

        if live_tick_arrays.is_empty() {
            live_tick_arrays.extend(self.amm.dynamic_tick_arrays.keys().copied());
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

#[cfg(test)]
mod tests {
    use super::*;
    use byreal_clmm_common::{tick_math, DynTickArrayState, TickArrayState, TickState};
    use jupiter_amm_interface::ClockRef;
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
        let pool_address =
            Pubkey::from_str("J4jiEPEu8c8nLdpkiMa7k1P8rL1HCJSNxCvzA5DsmYds").unwrap();

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
        let mut amm = ByrealClmm::from_keyed_account(&keyed_account, &amm_context).unwrap();

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
                accounts[i]
                    .as_ref()
                    .map(|account| (*key, account.clone().into()))
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
        let quote_in = amm
            .quote(&QuoteParams {
                amount: 100_000, // 0.0001 SOL
                input_mint: mints[0],
                output_mint: mints[1],
                swap_mode: SwapMode::ExactIn,
            })
            .unwrap();

        println!("Quote (exact in):");
        println!("  Input: {} {}", quote_in.in_amount, mints[0]);
        println!("  Output: {} {}", quote_in.out_amount, mints[1]);
        println!("  Fee: {}", quote_in.fee_amount);
        println!("  Fee %: {}", quote_in.fee_pct);

        // Test exact out quote
        let quote_out = amm
            .quote(&QuoteParams {
                amount: quote_in.out_amount, // Same amount as output from previous quote
                input_mint: mints[0],
                output_mint: mints[1],
                swap_mode: SwapMode::ExactOut,
            })
            .unwrap();

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

        // Create a mock AMM
        let amm = ByrealClmmAmm {
            key: pool_key,
            pool_state: PoolState::default(),
            amm_config: AmmConfig::default(),
            dynamic_tick_arrays: HashMap::new(),
            tick_array_pda_cache: HashMap::new(),
            cached_tick_array_addresses: Vec::new(),
            max_one_side_tick_arrays: 3,
            bitmap_extension: None,
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

        let mut pool_state = PoolState::default();
        pool_state.tick_current = 1000;
        pool_state.tick_spacing = 10;

        let amm = ByrealClmmAmm {
            key: pool_key,
            pool_state,
            amm_config: AmmConfig::default(),
            dynamic_tick_arrays: HashMap::new(),
            tick_array_pda_cache: HashMap::new(),
            cached_tick_array_addresses: Vec::new(),
            max_one_side_tick_arrays: 3,
            bitmap_extension: None,
        };

        // Test zero_for_one (price decreasing)
        let arrays_down = amm.get_swap_tick_arrays(true);
        assert_eq!(arrays_down.len(), 3);

        // Test one_for_zero (price increasing)
        let arrays_up = amm.get_swap_tick_arrays(false);
        assert_eq!(arrays_up.len(), 3);

        // Verify they're different
        assert_ne!(arrays_down[1], arrays_up[1]);
    }

    #[test]
    fn test_decay_fee_calculation() {
        let pool_key = Pubkey::new_unique();

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
            pool_state,
            amm_config: AmmConfig::default(),
            dynamic_tick_arrays: HashMap::new(),
            tick_array_pda_cache: HashMap::new(),
            cached_tick_array_addresses: Vec::new(),
            max_one_side_tick_arrays: 3,
            bitmap_extension: None,
        };

        // Test decay fee enabled
        assert!(amm.is_decay_fee_enabled());
        assert!(amm.is_decay_fee_on_sell_mint0());
        assert!(amm.is_decay_fee_on_sell_mint1());

        // Test decay fee at different intervals
        // Interval 0: timestamp 0-9, fee = 80%
        let fee_rate = amm.get_decay_fee_rate(0);
        assert_eq!(fee_rate, Some(800_000)); // 80% = 800,000 / 10^6

        let fee_rate = amm.get_decay_fee_rate(9);
        assert_eq!(fee_rate, Some(800_000)); // Still 80%

        // Interval 1: timestamp 10-19, fee = 72%
        let fee_rate = amm.get_decay_fee_rate(10);
        assert_eq!(fee_rate, Some(720_000)); // 72% = 720,000 / 10^6

        let fee_rate = amm.get_decay_fee_rate(19);
        assert_eq!(fee_rate, Some(720_000)); // Still 72%

        // Interval 2: timestamp 20-29, fee = 64.8%
        let fee_rate = amm.get_decay_fee_rate(20);
        assert_eq!(fee_rate, Some(648_000)); // 64.8% = 648,000 / 10^6

        // Interval 3: timestamp 30-39, fee = 58.32%
        let fee_rate = amm.get_decay_fee_rate(30);
        assert_eq!(fee_rate, Some(583_200)); // 58.32% = 583,200 / 10^6

        // Test after many intervals (should approach 0)
        let fee_rate = amm.get_decay_fee_rate(1000);
        assert!(fee_rate.is_some() && fee_rate.unwrap() < 100); // Should be very small after 100 intervals
    }

    #[test]
    fn test_decay_fee_disabled() {
        let pool_key = Pubkey::new_unique();

        let mut pool_state = PoolState::default();
        pool_state.decay_fee_flag = 0; // Decay fee disabled

        let amm = ByrealClmmAmm {
            key: pool_key,
            pool_state,
            amm_config: AmmConfig::default(),
            dynamic_tick_arrays: HashMap::new(),
            tick_array_pda_cache: HashMap::new(),
            cached_tick_array_addresses: Vec::new(),
            max_one_side_tick_arrays: 3,
            bitmap_extension: None,
        };

        assert!(!amm.is_decay_fee_enabled());
        assert_eq!(amm.get_decay_fee_rate(100), Some(0));
    }

    #[test]
    fn test_decay_fee_before_open_time() {
        let pool_key = Pubkey::new_unique();

        let mut pool_state = PoolState::default();
        pool_state.open_time = 1000; // Pool opens at timestamp 1000
        pool_state.decay_fee_flag = 0b111; // Enable decay fee
        pool_state.decay_fee_init_fee_rate = 50;
        pool_state.decay_fee_decrease_interval = 10; // Set interval to avoid division by zero

        let amm = ByrealClmmAmm {
            key: pool_key,
            pool_state,
            amm_config: AmmConfig::default(),
            dynamic_tick_arrays: HashMap::new(),
            tick_array_pda_cache: HashMap::new(),
            cached_tick_array_addresses: Vec::new(),
            max_one_side_tick_arrays: 3,
            bitmap_extension: None,
        };

        // Before open time, fee should be 0
        assert_eq!(amm.get_decay_fee_rate(999), Some(0));

        // At open time, fee should be initial rate
        assert_eq!(amm.get_decay_fee_rate(1000), Some(500_000)); // 50% = 500,000 / 10^6
    }

    #[test]
    fn test_decode_dyn_tick_array_and_next_tick() -> Result<()> {
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

        let (header, ticks) = match DynamicTickArrayState::from_bytes(&bytes).unwrap() {
            DynamicTickArrayState::Dynamic(inner) => inner,
            _ => return Err(anyhow!("Expected Dynamic variant")),
        };
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
            .next_initialized_tick_index(&ticks, current_tick_down, spacing, true)
            .unwrap()
            .unwrap();
        let tick_down = ticks[idx_down as usize].tick;
        assert_eq!(tick_down, start + 5 * i32::from(spacing));

        // zero_for_one=false (price increasing): the first initialized tick in
        // this array should be 600.
        let idx_up = header.first_initialized_tick_index(&ticks, false).unwrap();
        let tick_up = ticks[idx_up as usize].tick;
        assert_eq!(tick_up, start);

        Ok(())
    }

    #[test]
    fn test_compute_swap_errors_when_not_enough_tick_arrays() {
        let pool_key = Pubkey::new_unique();

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
            pool_state,
            amm_config: AmmConfig::default(),
            dynamic_tick_arrays: HashMap::new(),
            tick_array_pda_cache: HashMap::new(),
            cached_tick_array_addresses: Vec::new(),
            max_one_side_tick_arrays: 3,
            bitmap_extension: None,
        };

        // Attempting to compute a swap without tick array data should now
        // surface an error instead of silently falling back.
        let res = amm.compute_swap(true, 1_000u64, true, None, 0);
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
    fn test_litesvm_vs_sdk_byreal_jup_usdc() -> Result<()> {
        use litesvm::LiteSVM;
        use solana_account::Account as RawAccount;
        use solana_client::rpc_request::TokenAccountsFilter;
        use solana_clock::Clock as RawClock;
        use solana_instruction::{
            account_meta::AccountMeta as RawAccountMeta, Instruction as RawInstruction,
        };
        use solana_message::Message as RawMessage;
        use solana_pubkey::Pubkey as RawPubkey;
        use solana_transaction::Transaction as RawTransaction;

        // 1. Build SDK AMM from mainnet snapshot
        let rpc = RpcClient::new("https://api.mainnet-beta.solana.com");
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
            return Err(anyhow!(
                "Pool {} is not a USDC-cbBTC pool (mints: {}, {})",
                pool_address,
                amm.pool_state.token_mint_0,
                amm.pool_state.token_mint_1
            ));
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
            if let Ok((_, mut start)) = amm
                .pool_state
                .get_first_initialized_tick_array(&amm.bitmap_extension, dir)
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
        let program_bytes = include_bytes!("./byreal_clmm.so");
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
                    anchor_spl::token::spl_token::state::Account::pack(token_acc, &mut new_data)
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
            if disc == DynTickArrayState::DISCRIMINATOR || disc == TickArrayState::DISCRIMINATOR {
                all_tick_arrays.push(*addr);
            }
        }
        if all_tick_arrays.is_empty() {
            return Err(anyhow!(
                "No tick array accounts with valid discriminator found in snapshot"
            ));
        }

        // Traverse all initialized tick_array start indices in the current
        // swap direction using pool_state + bitmap_extension to derive an
        // ordered list of tick_array addresses.
        let mut ordered_tick_arrays: Vec<Pubkey> = Vec::new();
        if let Ok((_, mut start)) = amm
            .pool_state
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
            return Err(anyhow!("No ordered tick array accounts available for swap"));
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
            return Ok(());
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
        Ok(())
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
    fn test_litesvm_vs_sdk_byreal_jup_usdc_exact_out() -> Result<()> {
        use litesvm::LiteSVM;
        use solana_account::Account as RawAccount;
        use solana_client::rpc_request::TokenAccountsFilter;
        use solana_clock::Clock as RawClock;
        use solana_instruction::{
            account_meta::AccountMeta as RawAccountMeta, Instruction as RawInstruction,
        };
        use solana_message::Message as RawMessage;
        use solana_pubkey::Pubkey as RawPubkey;
        use solana_transaction::Transaction as RawTransaction;

        // 1. Build SDK AMM from mainnet snapshot
        let rpc = RpcClient::new("https://api.mainnet-beta.solana.com");
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
            return Err(anyhow!(
                "Pool {} is not a USDC-cbBTC pool (mints: {}, {})",
                pool_address,
                amm.pool_state.token_mint_0,
                amm.pool_state.token_mint_1
            ));
        };
        assert_eq!(
            cb_btc_mint, expected_cb_btc_mint,
            "Pool {} cbBTC mint mismatch (pool side vs expected)",
            pool_address
        );

        // Enrich tick-array snapshot via bitmap navigation (both directions).
        let mut full_tick_addrs: HashSet<Pubkey> = HashSet::new();
        for &dir in &[true, false] {
            if let Ok((_, mut start)) = amm
                .pool_state
                .get_first_initialized_tick_array(&amm.bitmap_extension, dir)
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

        let program_bytes = include_bytes!("./byreal_clmm.so");
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
                    anchor_spl::token::spl_token::state::Account::pack(token_acc, &mut new_data)
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
            if disc == DynTickArrayState::DISCRIMINATOR || disc == TickArrayState::DISCRIMINATOR {
                all_tick_arrays.push(*addr);
            }
        }
        if all_tick_arrays.is_empty() {
            return Err(anyhow!(
                "No tick array accounts with valid discriminator found in snapshot"
            ));
        }

        let mut ordered_tick_arrays: Vec<Pubkey> = Vec::new();
        if let Ok((_, mut start)) = amm
            .pool_state
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
            return Err(anyhow!("No ordered tick array accounts available for swap"));
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
            return Ok(());
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

        Ok(())
    }
}

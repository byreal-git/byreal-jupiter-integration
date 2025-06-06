use anyhow::{anyhow, Context, Result};
use anchor_lang::prelude::*;
use solana_sdk::{instruction::AccountMeta, pubkey::Pubkey, program_pack::Pack};
use std::collections::{HashMap, HashSet};

use jupiter_amm_interface::{
    AccountMap, Amm, AmmContext, KeyedAccount, Quote, QuoteParams,
    SwapAndAccountMetas, SwapParams, Swap, SwapMode, ClockRef
};

use byreal_clmm::{
    states::{AmmConfig, PoolState, TickArrayState, TickArrayBitmapExtension, ObservationState},
    libraries::{
        tick_math, swap_math,
        MAX_SQRT_PRICE_X64, MIN_SQRT_PRICE_X64
    },
};
use byreal_clmm::states::{POOL_TICK_ARRAY_BITMAP_SEED, TICK_ARRAY_SEED};

// Program IDs
#[cfg(feature = "mainnet")]
pub const BYREAL_CLMM_PROGRAM: Pubkey = solana_sdk::pubkey!("45iBNkaENereLKMjLm2LHkF3hpDapf6mnvrM5HWFg9cY");

#[cfg(feature = "devnet")]
pub const BYREAL_CLMM_PROGRAM: Pubkey = solana_sdk::pubkey!("45iBNkaENereLKMjLm2LHkF3hpDapf6mnvrM5HWFg9cY");

#[cfg(not(any(feature = "mainnet", feature = "devnet")))]
pub const BYREAL_CLMM_PROGRAM: Pubkey = solana_sdk::pubkey!("45iBNkaENereLKMjLm2LHkF3hpDapf6mnvrM5HWFg9cY");

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
}

impl ByrealClmmAmm {
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

    /// Get all tick array addresses that might be needed for a swap
    fn get_all_tick_array_addresses(&self) -> Vec<Pubkey> {
        let mut addresses = Vec::new();
        let tick_spacing = self.pool_state.tick_spacing as i32;
        let current_tick = self.pool_state.tick_current;
        
        // Get the current tick array
        let current_start_index = TickArrayState::get_array_start_index(current_tick, tick_spacing as u16);
        addresses.push(self.get_tick_array_address(current_start_index));
        
        // Add adjacent tick arrays (up to 12 on each side for ~24 total)
        for i in 1..=12 {
            addresses.push(self.get_tick_array_address(current_start_index - i * TICK_ARRAY_SIZE * tick_spacing));
            addresses.push(self.get_tick_array_address(current_start_index + i * TICK_ARRAY_SIZE * tick_spacing));
        }
        
        addresses
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

        // Get fee rate from AMM config
        let fee_rate = self.amm_config.trade_fee_rate;

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

            // Update tick if we've crossed
            if state.sqrt_price_x64 == sqrt_price_next {
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
            sqrt_price_x64: state.sqrt_price_x64,
            tick: state.tick,
        })
    }

    /// Find next initialized tick in the swap direction
    fn find_next_initialized_tick(&self, current_tick: i32, zero_for_one: bool) -> Result<i32> {
        // This is a simplified version - in production you'd need to check tick arrays
        let tick_spacing = self.pool_state.tick_spacing as i32;
        
        if zero_for_one {
            // Price decreasing, tick decreasing
            Ok(((current_tick / tick_spacing) - 1) * tick_spacing)
        } else {
            // Price increasing, tick increasing
            Ok(((current_tick / tick_spacing) + 1) * tick_spacing)
        }
    }
    
    /// Get tick arrays needed for a swap in the given direction
    fn get_swap_tick_arrays(&self, zero_for_one: bool) -> Vec<Pubkey> {
        let mut addresses = Vec::new();
        let tick_spacing = self.pool_state.tick_spacing as i32;
        let current_tick = self.pool_state.tick_current;
        
        // Get the current tick array
        let current_start_index = TickArrayState::get_array_start_index(current_tick, tick_spacing as u16);
        addresses.push(self.get_tick_array_address(current_start_index));
        
        // Add adjacent tick arrays in the swap direction (up to 10 arrays total)
        for i in 1..=10 {
            let offset = i * TICK_ARRAY_SIZE * tick_spacing;
            if zero_for_one {
                addresses.push(self.get_tick_array_address(current_start_index - offset));
            } else {
                addresses.push(self.get_tick_array_address(current_start_index + offset));
            }
        }
        
        addresses
    }
}

impl Amm for ByrealClmmAmm {
    fn from_keyed_account(keyed_account: &KeyedAccount, amm_context: &AmmContext) -> Result<Self> {
        let pool_state = PoolState::try_deserialize(&mut keyed_account.account.data.as_slice())
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
            self.pool_state = PoolState::try_deserialize(&mut pool_data.data.as_slice())?;
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

        // Update tick arrays
        for address in self.get_all_tick_array_addresses() {
            if let Some(tick_array_data) = account_map.get(&address) {
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
            fee_pct: self.amm_config.trade_fee_rate.into(),
        })
    }

    fn get_swap_and_account_metas(&self, swap_params: &SwapParams) -> Result<SwapAndAccountMetas> {
        let zero_for_one = swap_params.source_mint == self.pool_state.token_mint_0;

        // Build account metas for swap instruction
        let mut account_metas = vec![
            // Signer
            AccountMeta::new_readonly(swap_params.token_transfer_authority, true),
            // AMM Config
            AccountMeta::new_readonly(self.pool_state.amm_config, false),
            // Pool state
            AccountMeta::new(self.key, false),
            // Input/Output vaults
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
            // User token accounts
            AccountMeta::new(swap_params.source_token_account, false),
            AccountMeta::new(swap_params.destination_token_account, false),
            // Token program
            AccountMeta::new_readonly(anchor_spl::token::ID, false),
        ];

        // Add tick arrays needed for this specific swap
        let tick_arrays = self.get_swap_tick_arrays(zero_for_one);
        for tick_array in tick_arrays {
            account_metas.push(AccountMeta::new(tick_array, false));
        }

        Ok(SwapAndAccountMetas {
            swap: Swap::RaydiumClmm {
                // Can add custom params here if needed
            },
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
    #[allow(dead_code)]
    sqrt_price_x64: u128,
    #[allow(dead_code)]
    tick: i32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_client::rpc_client::RpcClient;
    use std::str::FromStr;

    #[test]
    fn test_jupiter_integration() {
        // Skip if not in integration test mode
        // if std::env::var("RUN_INTEGRATION_TESTS").is_err() {
        //     println!("Skipping integration test. Set RUN_INTEGRATION_TESTS=1 to run.");
        //     return;
        // }

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
        };
        
        // Test zero_for_one (price decreasing)
        let arrays_down = amm.get_swap_tick_arrays(true);
        assert_eq!(arrays_down.len(), 5);
        
        // Test one_for_zero (price increasing)
        let arrays_up = amm.get_swap_tick_arrays(false);
        assert_eq!(arrays_up.len(), 5);
        
        // Verify they're different
        assert_ne!(arrays_down[1], arrays_up[1]);
    }
}
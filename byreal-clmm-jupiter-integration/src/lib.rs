use anchor_lang::prelude::*;
use anyhow::{anyhow, ensure, Result};
use pyth_solana_receiver_sdk::price_update::{Price, PriceUpdateV2};
use solana_sdk::{instruction::AccountMeta, pubkey::Pubkey};
use std::{
    collections::{HashMap, HashSet},
    sync::{atomic::AtomicI64, Arc},
};

use jupiter_amm_interface::{
    single_program_amm, try_get_account_data, try_get_account_data_and_owner, AccountMap, Amm,
    AmmContext, KeyedAccount, Quote, QuoteParams, SingleProgramAmm, Swap, SwapAndAccountMetas,
    SwapMode, SwapParams,
};
use spl_token::solana_program::program_pack::Pack;
use spl_token_2022::{
    extension::{transfer_fee::TransferFeeConfig, BaseStateWithExtensions, StateWithExtensions},
    state::{Account as Token2022Account, Mint as Token2022Mint},
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
const PYTH_RECEIVER_PROGRAM_ID: Pubkey = pyth_solana_receiver_sdk::ID;
const PYTH_PRICE_SHARD_ID: u16 = 0;

single_program_amm!(ByrealClmm, ID, "Byreal");

fn dynamic_enabled() -> bool {
    cfg!(feature = "dynamic-pool")
}

fn get_price_feed_account_address(shard_id: u16, feed_id: &[u8; 32]) -> Pubkey {
    let shard_bytes = shard_id.to_le_bytes();
    Pubkey::find_program_address(
        &[&shard_bytes, feed_id],
        &pyth_solana_receiver_sdk::PYTH_PUSH_ORACLE_ID,
    )
    .0
}

fn get_dynamic_pyth_oracle_addresses(pool_state: &PoolState) -> Option<(Pubkey, Pubkey)> {
    if pool_state.token0_pyth_feed_id == [0; 32] || pool_state.token1_pyth_feed_id == [0; 32] {
        return None;
    }
    Some((
        get_price_feed_account_address(PYTH_PRICE_SHARD_ID, &pool_state.token0_pyth_feed_id),
        get_price_feed_account_address(PYTH_PRICE_SHARD_ID, &pool_state.token1_pyth_feed_id),
    ))
}

fn decode_vault_amount(owner: &Pubkey, data: &[u8]) -> Result<u64> {
    if *owner == spl_token::id() {
        let account = spl_token::state::Account::unpack(data)
            .map_err(|e| anyhow!("decode SPL token vault failed: {e}"))?;
        return Ok(account.amount);
    }
    if *owner == spl_token_2022::id() {
        let account = StateWithExtensions::<Token2022Account>::unpack(data)
            .map_err(|e| anyhow!("decode SPL token-2022 vault failed: {e}"))?;
        return Ok(account.base.amount);
    }
    Err(anyhow!("unsupported token vault owner: {owner}"))
}

fn decode_pyth_price(data: &[u8], expected_feed_id: &[u8; 32]) -> Result<Price> {
    let mut data_ref = data;
    let account = PriceUpdateV2::try_deserialize(&mut data_ref)
        .map_err(|e| anyhow!("decode pyth account failed: {e}"))?;
    account
        .get_price_unchecked(expected_feed_id)
        .map_err(|e| anyhow!("pyth feed id mismatch: {e}"))
}

fn decode_transfer_fee_config(owner: &Pubkey, data: &[u8]) -> Result<Option<TransferFeeConfig>> {
    if *owner == spl_token::id() {
        return Ok(None);
    }
    if *owner == spl_token_2022::id() {
        let mint = StateWithExtensions::<Token2022Mint>::unpack(data)
            .map_err(|e| anyhow!("decode token-2022 mint failed: {e}"))?;
        return Ok(mint.get_extension::<TransferFeeConfig>().ok().copied());
    }
    Err(anyhow!("unsupported token mint owner: {owner}"))
}

#[derive(Clone)]
pub struct ByrealClmm {
    key: Pubkey,
    amm: ByrealClmmAmm,
    timestamp: Arc<AtomicI64>,
    epoch: Arc<std::sync::atomic::AtomicU64>,
}

impl ByrealClmm {
    fn swap_direction_for_mints(
        &self,
        source_mint: Pubkey,
        destination_mint: Pubkey,
    ) -> Result<bool> {
        match (
            source_mint == self.amm.pool_state.token_mint_0,
            source_mint == self.amm.pool_state.token_mint_1,
            destination_mint == self.amm.pool_state.token_mint_0,
            destination_mint == self.amm.pool_state.token_mint_1,
        ) {
            (true, false, false, true) => Ok(true),
            (false, true, true, false) => Ok(false),
            _ => Err(anyhow!("swap mints do not match pool reserves")),
        }
    }

    fn live_directional_tick_arrays(&self, zero_for_one: bool) -> Result<Vec<Pubkey>> {
        let candidate_tick_arrays = self.amm.get_swap_tick_arrays(zero_for_one);
        let mut live_tick_arrays = Vec::new();

        for address in candidate_tick_arrays {
            if self.amm.dynamic_tick_arrays.contains_key(&address) {
                live_tick_arrays.push(address);
            } else if live_tick_arrays.is_empty() {
                return Err(anyhow!(
                    "directional first tick array account missing for dynamic swap: {address}"
                ));
            }
        }

        ensure!(
            !live_tick_arrays.is_empty(),
            "No directional tick array accounts available for dynamic swap"
        );
        Ok(live_tick_arrays)
    }

    pub fn build_swap_v3_dyn_account_metas(
        &self,
        swap_params: &SwapParams,
    ) -> Result<Vec<AccountMeta>> {
        let mut account_metas = self.build_swap_v2_account_metas(swap_params)?;

        if self.amm.pool_state.is_swap_dynamic_fee_enabled() {
            if let Some((token0_pyth_oracle, token1_pyth_oracle)) =
                get_dynamic_pyth_oracle_addresses(&self.amm.pool_state)
            {
                account_metas.push(AccountMeta::new_readonly(token0_pyth_oracle, false));
                account_metas.push(AccountMeta::new_readonly(token1_pyth_oracle, false));
            }
        }

        Ok(account_metas)
    }

    fn build_swap_v2_account_metas(&self, swap_params: &SwapParams) -> Result<Vec<AccountMeta>> {
        let zero_for_one =
            self.swap_direction_for_mints(swap_params.source_mint, swap_params.destination_mint)?;
        let live_tick_arrays = self.live_directional_tick_arrays(zero_for_one)?;

        let mut account_metas = vec![
            AccountMeta::new_readonly(swap_params.token_transfer_authority, true),
            AccountMeta::new_readonly(self.amm.pool_state.amm_config, false),
            AccountMeta::new(self.key, false),
            AccountMeta::new(swap_params.source_token_account, false),
            AccountMeta::new(swap_params.destination_token_account, false),
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
            AccountMeta::new(self.amm.pool_state.observation_key, false),
            AccountMeta::new_readonly(anchor_spl::token::ID, false),
            AccountMeta::new_readonly(spl_token_2022::ID, false),
            AccountMeta::new_readonly(anchor_spl::memo::ID, false),
            if zero_for_one {
                AccountMeta::new_readonly(self.amm.pool_state.token_mint_0, false)
            } else {
                AccountMeta::new_readonly(self.amm.pool_state.token_mint_1, false)
            },
            if zero_for_one {
                AccountMeta::new_readonly(self.amm.pool_state.token_mint_1, false)
            } else {
                AccountMeta::new_readonly(self.amm.pool_state.token_mint_0, false)
            },
        ];

        account_metas.push(AccountMeta::new_readonly(
            TickArrayBitmapExtension::key(self.key),
            false,
        ));
        for address in live_tick_arrays {
            account_metas.push(AccountMeta::new(address, false));
        }

        Ok(account_metas)
    }
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
                token0_vault_amount: 0,
                token1_vault_amount: 0,
                token0_pyth_price: None,
                token1_pyth_price: None,
                token0_transfer_fee_config: None,
                token1_transfer_fee_config: None,
            },
            timestamp: amm_context.clock_ref.unix_timestamp.clone(),
            epoch: amm_context.clock_ref.epoch.clone(),
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
            self.amm.pool_state.token_mint_0,
            self.amm.pool_state.token_mint_1,
        ];

        let bitmap_key = TickArrayBitmapExtension::key(self.key);
        accounts.push(bitmap_key);

        accounts.extend(self.amm.get_all_tick_array_addresses());

        if dynamic_enabled() && self.amm.pool_state.is_swap_dynamic_fee_enabled() {
            accounts.push(self.amm.pool_state.token_vault_0);
            accounts.push(self.amm.pool_state.token_vault_1);
            if let Some((token0_pyth_oracle, token1_pyth_oracle)) =
                get_dynamic_pyth_oracle_addresses(&self.amm.pool_state)
            {
                accounts.push(token0_pyth_oracle);
                accounts.push(token1_pyth_oracle);
            }
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

        let (mint0_data, mint0_owner) =
            try_get_account_data_and_owner(account_map, &self.amm.pool_state.token_mint_0)?;
        self.amm.token0_transfer_fee_config =
            decode_transfer_fee_config(mint0_owner, mint0_data)?;
        let (mint1_data, mint1_owner) =
            try_get_account_data_and_owner(account_map, &self.amm.pool_state.token_mint_1)?;
        self.amm.token1_transfer_fee_config =
            decode_transfer_fee_config(mint1_owner, mint1_data)?;

        // Update bitmap extension
        let bitmap_key = TickArrayBitmapExtension::key(self.key);
        self.amm.bitmap_extension = match try_get_account_data(account_map, &bitmap_key) {
            Ok(mut data) => Some(TickArrayBitmapExtension::try_deserialize(&mut data)?),
            Err(_) => None,
        };

        // Update tick arrays
        self.amm.dynamic_tick_arrays.clear();
        for address in self.amm.get_all_tick_array_addresses() {
            if let Ok(tick_array_data) = try_get_account_data(account_map, &address) {
                if let Some(tick_array) = DynamicTickArrayState::from_bytes(tick_array_data) {
                    self.amm.dynamic_tick_arrays.insert(address, tick_array);
                }
            }
        }

        if dynamic_enabled() && self.amm.pool_state.is_swap_dynamic_fee_enabled() {
            let (vault0_data, vault0_owner) =
                try_get_account_data_and_owner(account_map, &self.amm.pool_state.token_vault_0)?;
            self.amm.token0_vault_amount = decode_vault_amount(vault0_owner, vault0_data)?;
            let (vault1_data, vault1_owner) =
                try_get_account_data_and_owner(account_map, &self.amm.pool_state.token_vault_1)?;
            self.amm.token1_vault_amount = decode_vault_amount(vault1_owner, vault1_data)?;

            if let Some((token0_pyth_oracle, token1_pyth_oracle)) =
                get_dynamic_pyth_oracle_addresses(&self.amm.pool_state)
            {
                let (pyth0_data, pyth0_owner) =
                    try_get_account_data_and_owner(account_map, &token0_pyth_oracle)?;
                if *pyth0_owner != PYTH_RECEIVER_PROGRAM_ID {
                    return Err(anyhow!("token0 pyth oracle owner mismatch"));
                }
                let (pyth1_data, pyth1_owner) =
                    try_get_account_data_and_owner(account_map, &token1_pyth_oracle)?;
                if *pyth1_owner != PYTH_RECEIVER_PROGRAM_ID {
                    return Err(anyhow!("token1 pyth oracle owner mismatch"));
                }
                self.amm.token0_pyth_price = Some(decode_pyth_price(
                    pyth0_data,
                    &self.amm.pool_state.token0_pyth_feed_id,
                )?);
                self.amm.token1_pyth_price = Some(decode_pyth_price(
                    pyth1_data,
                    &self.amm.pool_state.token1_pyth_feed_id,
                )?);
            } else {
                self.amm.token0_pyth_price = None;
                self.amm.token1_pyth_price = None;
            }
        } else {
            self.amm.token0_vault_amount = 0;
            self.amm.token1_vault_amount = 0;
            self.amm.token0_pyth_price = None;
            self.amm.token1_pyth_price = None;
        }

        Ok(())
    }

    fn quote(&self, quote_params: &QuoteParams) -> Result<Quote> {
        if self.amm.pool_state.is_swap_dynamic_fee_enabled() {
            if !dynamic_enabled() {
                return Err(anyhow!("dynamic pool disabled by compile-time feature"));
            }
        }

        let current_timestamp = self.timestamp.load(std::sync::atomic::Ordering::Relaxed);
        let current_epoch = self.epoch.load(std::sync::atomic::Ordering::Relaxed);
        let zero_for_one =
            self.swap_direction_for_mints(quote_params.input_mint, quote_params.output_mint)?;

        let is_base_input = quote_params.swap_mode == SwapMode::ExactIn;
        let swap_result = self.amm.compute_swap(
            zero_for_one,
            quote_params.amount,
            is_base_input,
            None, // No price limit for quotes
            current_timestamp,
            current_epoch,
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
        if self.amm.pool_state.is_swap_dynamic_fee_enabled() {
            if !dynamic_enabled() {
                return Err(anyhow!("dynamic pool disabled by compile-time feature"));
            }
            return Ok(SwapAndAccountMetas {
                swap: Swap::RaydiumClmmV2,
                account_metas: self.build_swap_v3_dyn_account_metas(swap_params)?,
            });
        }

        if dynamic_enabled() {
            return Ok(SwapAndAccountMetas {
                swap: Swap::RaydiumClmmV2,
                account_metas: self.build_swap_v3_dyn_account_metas(swap_params)?,
            });
        }

        Ok(SwapAndAccountMetas {
            swap: Swap::RaydiumClmmV2,
            account_metas: self.build_swap_v2_account_metas(swap_params)?,
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
    use byreal_clmm_common::TickArrayState;

    fn build_dynamic_test_amm() -> ByrealClmm {
        let pool_key = Pubkey::new_unique();
        let mut pool_state = PoolState::default();
        pool_state.amm_config = Pubkey::new_unique();
        pool_state.token_mint_0 = Pubkey::new_unique();
        pool_state.token_mint_1 = Pubkey::new_unique();
        pool_state.token_vault_0 = Pubkey::new_unique();
        pool_state.token_vault_1 = Pubkey::new_unique();
        pool_state.observation_key = Pubkey::new_unique();
        pool_state.tick_spacing = 1;
        pool_state.token0_pyth_feed_id = [1u8; 32];
        pool_state.token1_pyth_feed_id = [2u8; 32];
        pool_state.set_swap_dynamic_fee_enabled(true);

        ByrealClmm {
            key: pool_key,
            amm: ByrealClmmAmm {
                key: pool_key,
                pool_state,
                amm_config: AmmConfig::default(),
                bitmap_extension: Some(TickArrayBitmapExtension::default()),
                max_one_side_tick_arrays: 3,
                dynamic_tick_arrays: HashMap::new(),
                token0_vault_amount: 0,
                token1_vault_amount: 0,
                token0_pyth_price: None,
                token1_pyth_price: None,
                token0_transfer_fee_config: None,
                token1_transfer_fee_config: None,
            },
            timestamp: Arc::new(AtomicI64::new(0)),
            epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    #[cfg(feature = "dynamic-pool")]
    fn pool_state_account_data(pool_state: &PoolState) -> Vec<u8> {
        let mut data = Vec::with_capacity(PoolState::LEN);
        data.extend_from_slice(&PoolState::DISCRIMINATOR);
        data.extend_from_slice(bytemuck::bytes_of(pool_state));
        data
    }

    #[cfg(feature = "dynamic-pool")]
    fn amm_config_account_data(config: &AmmConfig) -> Vec<u8> {
        let mut data = Vec::new();
        anchor_lang::AccountSerialize::try_serialize(config, &mut data).unwrap();
        data
    }

    #[cfg(feature = "dynamic-pool")]
    fn spl_vault_account_data(amount: u64) -> Vec<u8> {
        let mut vault = spl_token::state::Account::default();
        vault.state = spl_token::state::AccountState::Initialized;
        vault.amount = amount;
        let mut data = vec![0u8; spl_token::state::Account::LEN];
        spl_token::state::Account::pack(vault, &mut data).unwrap();
        data
    }

    #[cfg(feature = "dynamic-pool")]
    fn pyth_price_account_data(feed_id: [u8; 32]) -> Vec<u8> {
        let account = PriceUpdateV2 {
            write_authority: Pubkey::new_unique(),
            verification_level: pyth_solana_receiver_sdk::price_update::VerificationLevel::Full,
            price_message: pyth_solana_receiver_sdk::price_update::PriceFeedMessage {
                feed_id,
                price: 1_000_000,
                conf: 1,
                exponent: -6,
                publish_time: 100,
                prev_publish_time: 99,
                ema_price: 1_000_000,
                ema_conf: 1,
            },
            posted_slot: 1,
        };
        let mut data = Vec::new();
        anchor_lang::AccountSerialize::try_serialize(&account, &mut data).unwrap();
        data
    }

    #[cfg(feature = "dynamic-pool")]
    fn base_update_account_map(pool_key: Pubkey, pool_state: &PoolState) -> AccountMap {
        let mut account_map = AccountMap::default();
        account_map.insert(
            pool_key,
            solana_sdk::account::Account {
                lamports: 1_000_000,
                data: pool_state_account_data(pool_state),
                owner: BYREAL_CLMM_PROGRAM,
                executable: false,
                rent_epoch: 0,
            },
        );
        account_map.insert(
            pool_state.amm_config,
            solana_sdk::account::Account {
                lamports: 1_000_000,
                data: amm_config_account_data(&AmmConfig::default()),
                owner: BYREAL_CLMM_PROGRAM,
                executable: false,
                rent_epoch: 0,
            },
        );
        account_map.insert(
            pool_state.token_mint_0,
            solana_sdk::account::Account {
                lamports: 1_000_000,
                data: Vec::new(),
                owner: spl_token::id(),
                executable: false,
                rent_epoch: 0,
            },
        );
        account_map.insert(
            pool_state.token_mint_1,
            solana_sdk::account::Account {
                lamports: 1_000_000,
                data: Vec::new(),
                owner: spl_token::id(),
                executable: false,
                rent_epoch: 0,
            },
        );
        account_map
    }

    #[cfg(feature = "dynamic-pool")]
    fn dynamic_update_account_map(
        pool_key: Pubkey,
        pool_state: &PoolState,
        pyth0_owner: Pubkey,
        pyth0_feed_id: [u8; 32],
        pyth1_owner: Pubkey,
        pyth1_feed_id: [u8; 32],
    ) -> AccountMap {
        let mut account_map = base_update_account_map(pool_key, pool_state);
        account_map.insert(
            pool_state.token_vault_0,
            solana_sdk::account::Account {
                lamports: 1_000_000,
                data: spl_vault_account_data(1_000),
                owner: spl_token::id(),
                executable: false,
                rent_epoch: 0,
            },
        );
        account_map.insert(
            pool_state.token_vault_1,
            solana_sdk::account::Account {
                lamports: 1_000_000,
                data: spl_vault_account_data(1_000),
                owner: spl_token::id(),
                executable: false,
                rent_epoch: 0,
            },
        );

        let (token0_pyth_oracle, token1_pyth_oracle) =
            get_dynamic_pyth_oracle_addresses(pool_state).unwrap();
        account_map.insert(
            token0_pyth_oracle,
            solana_sdk::account::Account {
                lamports: 1_000_000,
                data: pyth_price_account_data(pyth0_feed_id),
                owner: pyth0_owner,
                executable: false,
                rent_epoch: 0,
            },
        );
        account_map.insert(
            token1_pyth_oracle,
            solana_sdk::account::Account {
                lamports: 1_000_000,
                data: pyth_price_account_data(pyth1_feed_id),
                owner: pyth1_owner,
                executable: false,
                rent_epoch: 0,
            },
        );
        account_map
    }

    #[cfg(not(feature = "dynamic-pool"))]
    #[test]
    fn test_dynamic_pool_disabled_rejects_quote_and_swap_metas() {
        let mut pool_state = PoolState::default();
        pool_state.token_mint_0 = Pubkey::new_unique();
        pool_state.token_mint_1 = Pubkey::new_unique();
        pool_state.set_swap_dynamic_fee_enabled(true);

        let pool_key = Pubkey::new_unique();
        let amm = ByrealClmm {
            key: pool_key,
            amm: ByrealClmmAmm {
                key: pool_key,
                pool_state,
                amm_config: AmmConfig::default(),
                bitmap_extension: Some(TickArrayBitmapExtension::default()),
                max_one_side_tick_arrays: 3,
                dynamic_tick_arrays: HashMap::new(),
                token0_vault_amount: 0,
                token1_vault_amount: 0,
                token0_pyth_price: None,
                token1_pyth_price: None,
                token0_transfer_fee_config: None,
                token1_transfer_fee_config: None,
            },
            timestamp: Arc::new(AtomicI64::new(0)),
            epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };

        let quote_err = amm
            .quote(&QuoteParams {
                amount: 1_000,
                input_mint: amm.amm.pool_state.token_mint_0,
                output_mint: amm.amm.pool_state.token_mint_1,
                swap_mode: SwapMode::ExactIn,
            })
            .unwrap_err();
        assert!(format!("{quote_err:#}").contains("dynamic pool disabled by compile-time feature"));

        let jupiter_program = Pubkey::new_unique();
        let swap_err = amm
            .get_swap_and_account_metas(&SwapParams {
                source_mint: amm.amm.pool_state.token_mint_0,
                destination_mint: amm.amm.pool_state.token_mint_1,
                source_token_account: Pubkey::new_unique(),
                destination_token_account: Pubkey::new_unique(),
                token_transfer_authority: Pubkey::new_unique(),
                quote_mint_to_referrer: None,
                jupiter_program_id: &jupiter_program,
                in_amount: 1_000,
                out_amount: 1,
                missing_dynamic_accounts_as_default: false,
                swap_mode: SwapMode::ExactIn,
            })
            .err()
            .expect("dynamic pool disabled should fail before producing swap metas");
        assert!(format!("{swap_err:#}").contains("dynamic pool disabled by compile-time feature"));
    }

    #[test]
    fn test_dynamic_pyth_oracle_addresses_omit_zero_feed_id() {
        let mut amm = build_dynamic_test_amm();
        amm.amm.pool_state.token0_pyth_feed_id = [0u8; 32];

        assert!(get_dynamic_pyth_oracle_addresses(&amm.amm.pool_state).is_none());
    }

    #[test]
    fn test_dynamic_pyth_oracle_addresses_omit_token1_zero_feed_id() {
        let mut amm = build_dynamic_test_amm();
        amm.amm.pool_state.token1_pyth_feed_id = [0u8; 32];

        assert!(get_dynamic_pyth_oracle_addresses(&amm.amm.pool_state).is_none());
    }

    #[cfg(feature = "dynamic-pool")]
    #[test]
    fn test_dynamic_pool_accounts_to_update_omit_invalid_zero_feed_pyth_accounts() {
        let mut amm = build_dynamic_test_amm();
        amm.amm.pool_state.token0_pyth_feed_id = [0u8; 32];

        let accounts = amm.get_accounts_to_update();

        assert!(accounts.contains(&amm.amm.pool_state.token_vault_0));
        assert!(accounts.contains(&amm.amm.pool_state.token_vault_1));
        assert!(!accounts.contains(&get_price_feed_account_address(
            PYTH_PRICE_SHARD_ID,
            &[0u8; 32]
        )));
        assert!(!accounts.contains(&get_price_feed_account_address(
            PYTH_PRICE_SHARD_ID,
            &amm.amm.pool_state.token1_pyth_feed_id
        )));
    }

    #[cfg(feature = "dynamic-pool")]
    #[test]
    fn test_dynamic_pool_update_loads_valid_pyth_prices_and_vault_amounts() {
        let mut amm = build_dynamic_test_amm();
        let account_map = dynamic_update_account_map(
            amm.key,
            &amm.amm.pool_state,
            PYTH_RECEIVER_PROGRAM_ID,
            amm.amm.pool_state.token0_pyth_feed_id,
            PYTH_RECEIVER_PROGRAM_ID,
            amm.amm.pool_state.token1_pyth_feed_id,
        );

        amm.update(&account_map).unwrap();

        assert_eq!(amm.amm.token0_vault_amount, 1_000);
        assert_eq!(amm.amm.token1_vault_amount, 1_000);
        assert_eq!(amm.amm.token0_pyth_price.unwrap().price, 1_000_000);
        assert_eq!(amm.amm.token1_pyth_price.unwrap().price, 1_000_000);
    }

    #[cfg(feature = "dynamic-pool")]
    #[test]
    fn test_dynamic_pool_update_without_pyth_clears_cached_prices_and_keeps_vaults() {
        for (feed0, feed1) in [([0; 32], [0; 32]), ([0; 32], [2; 32]), ([1; 32], [0; 32])] {
            let mut amm = build_dynamic_test_amm();
            let (pyth0, pyth1) = get_dynamic_pyth_oracle_addresses(&amm.amm.pool_state).unwrap();
            let mut account_map = dynamic_update_account_map(
                amm.key,
                &amm.amm.pool_state,
                PYTH_RECEIVER_PROGRAM_ID,
                [1; 32],
                PYTH_RECEIVER_PROGRAM_ID,
                [2; 32],
            );
            amm.update(&account_map).unwrap();
            assert!(amm.amm.token0_pyth_price.is_some());
            assert!(amm.amm.token1_pyth_price.is_some());

            let mut pool = amm.amm.pool_state;
            pool.token0_pyth_feed_id = feed0;
            pool.token1_pyth_feed_id = feed1;
            account_map.get_mut(&amm.key).unwrap().data = pool_state_account_data(&pool);
            account_map.remove(&pyth0);
            account_map.remove(&pyth1);
            amm.update(&account_map).unwrap();

            assert_eq!(amm.amm.token0_vault_amount, 1_000);
            assert_eq!(amm.amm.token1_vault_amount, 1_000);
            assert!(amm.amm.token0_pyth_price.is_none());
            assert!(amm.amm.token1_pyth_price.is_none());
            let required = amm.get_accounts_to_update();
            assert!(!required.contains(&pyth0));
            assert!(!required.contains(&pyth1));
        }
    }

    #[cfg(feature = "dynamic-pool")]
    #[test]
    fn test_dynamic_pool_update_rejects_pyth_owner_mismatch() {
        let mut amm = build_dynamic_test_amm();
        let account_map = dynamic_update_account_map(
            amm.key,
            &amm.amm.pool_state,
            Pubkey::new_unique(),
            amm.amm.pool_state.token0_pyth_feed_id,
            PYTH_RECEIVER_PROGRAM_ID,
            amm.amm.pool_state.token1_pyth_feed_id,
        );

        let err = amm.update(&account_map).unwrap_err();
        assert!(format!("{err:#}").contains("token0 pyth oracle owner mismatch"));

        let mut amm = build_dynamic_test_amm();
        let account_map = dynamic_update_account_map(
            amm.key,
            &amm.amm.pool_state,
            PYTH_RECEIVER_PROGRAM_ID,
            amm.amm.pool_state.token0_pyth_feed_id,
            Pubkey::new_unique(),
            amm.amm.pool_state.token1_pyth_feed_id,
        );

        let err = amm.update(&account_map).unwrap_err();
        assert!(format!("{err:#}").contains("token1 pyth oracle owner mismatch"));
    }

    #[cfg(feature = "dynamic-pool")]
    #[test]
    fn test_dynamic_pool_update_rejects_pyth_feed_id_mismatch() {
        let mut amm = build_dynamic_test_amm();
        let account_map = dynamic_update_account_map(
            amm.key,
            &amm.amm.pool_state,
            PYTH_RECEIVER_PROGRAM_ID,
            [9u8; 32],
            PYTH_RECEIVER_PROGRAM_ID,
            amm.amm.pool_state.token1_pyth_feed_id,
        );

        let err = amm.update(&account_map).unwrap_err();
        assert!(format!("{err:#}").contains("pyth feed id mismatch"));

        let mut amm = build_dynamic_test_amm();
        let account_map = dynamic_update_account_map(
            amm.key,
            &amm.amm.pool_state,
            PYTH_RECEIVER_PROGRAM_ID,
            amm.amm.pool_state.token0_pyth_feed_id,
            PYTH_RECEIVER_PROGRAM_ID,
            [9u8; 32],
        );

        let err = amm.update(&account_map).unwrap_err();
        assert!(format!("{err:#}").contains("pyth feed id mismatch"));
    }

    #[test]
    fn test_quote_rejects_mints_outside_pool_reserves() {
        let mut amm = build_dynamic_test_amm();
        amm.amm.pool_state.set_swap_dynamic_fee_enabled(false);

        let err = amm
            .quote(&QuoteParams {
                amount: 1,
                input_mint: Pubkey::new_unique(),
                output_mint: amm.amm.pool_state.token_mint_1,
                swap_mode: SwapMode::ExactIn,
            })
            .unwrap_err();
        assert!(format!("{err:#}").contains("swap mints do not match pool reserves"));
    }

    #[test]
    fn test_non_dynamic_swap_metas_reject_mints_outside_pool_reserves() {
        let mut amm = build_dynamic_test_amm();
        amm.amm.pool_state.set_swap_dynamic_fee_enabled(false);
        let jupiter_program = Pubkey::new_unique();

        let err = match amm.get_swap_and_account_metas(&SwapParams {
            source_mint: Pubkey::new_unique(),
            destination_mint: amm.amm.pool_state.token_mint_1,
            source_token_account: Pubkey::new_unique(),
                destination_token_account: Pubkey::new_unique(),
                token_transfer_authority: Pubkey::new_unique(),
                quote_mint_to_referrer: None,
                jupiter_program_id: &jupiter_program,
                in_amount: 1,
            out_amount: 1,
            missing_dynamic_accounts_as_default: false,
            swap_mode: SwapMode::ExactIn,
        }) {
            Ok(_) => panic!("swap metas should reject mints outside pool reserves"),
            Err(err) => err,
        };
        assert!(format!("{err:#}").contains("swap mints do not match pool reserves"));
    }

    #[test]
    fn test_non_dynamic_pool_returns_v2_compatible_metas() {
        let mut amm = build_dynamic_test_amm();
        amm.amm.pool_state.set_swap_dynamic_fee_enabled(false);
        let first_tick_array = amm.amm.get_swap_tick_arrays(true)[0];
        amm.amm.dynamic_tick_arrays.insert(
            first_tick_array,
            DynamicTickArrayState::Fixed(TickArrayState::default()),
        );

        let jupiter_program = Pubkey::new_unique();
        let swap_params = SwapParams {
            source_mint: amm.amm.pool_state.token_mint_0,
            destination_mint: amm.amm.pool_state.token_mint_1,
            source_token_account: Pubkey::new_unique(),
            destination_token_account: Pubkey::new_unique(),
            token_transfer_authority: Pubkey::new_unique(),
            quote_mint_to_referrer: None,
            jupiter_program_id: &jupiter_program,
            in_amount: 1_000,
            out_amount: 1,
            missing_dynamic_accounts_as_default: false,
            swap_mode: SwapMode::ExactIn,
        };

        let swap = amm.get_swap_and_account_metas(&swap_params).unwrap();

        assert_eq!(swap.swap, Swap::RaydiumClmmV2);
        assert_eq!(swap.account_metas[9].pubkey, spl_token_2022::ID);
        assert_eq!(swap.account_metas[10].pubkey, anchor_spl::memo::ID);
        assert_eq!(swap.account_metas[11].pubkey, amm.amm.pool_state.token_mint_0);
        assert_eq!(swap.account_metas[12].pubkey, amm.amm.pool_state.token_mint_1);
        assert_eq!(swap.account_metas[13].pubkey, TickArrayBitmapExtension::key(amm.key));
        assert_eq!(swap.account_metas[14].pubkey, first_tick_array);
    }

    #[cfg(feature = "dynamic-pool")]
    #[test]
    fn test_non_dynamic_pool_feature_on_does_not_require_pyth_metas() {
        let mut amm = build_dynamic_test_amm();
        amm.amm.pool_state.set_swap_dynamic_fee_enabled(false);
        amm.amm.pool_state.token0_pyth_feed_id = [0u8; 32];
        amm.amm.pool_state.token1_pyth_feed_id = [0u8; 32];
        let first_tick_array = amm.amm.get_swap_tick_arrays(true)[0];
        amm.amm.dynamic_tick_arrays.insert(
            first_tick_array,
            DynamicTickArrayState::Fixed(TickArrayState::default()),
        );

        let jupiter_program = Pubkey::new_unique();
        let swap = amm
            .get_swap_and_account_metas(&SwapParams {
                source_mint: amm.amm.pool_state.token_mint_0,
                destination_mint: amm.amm.pool_state.token_mint_1,
                source_token_account: Pubkey::new_unique(),
                destination_token_account: Pubkey::new_unique(),
                token_transfer_authority: Pubkey::new_unique(),
                quote_mint_to_referrer: None,
                jupiter_program_id: &jupiter_program,
                in_amount: 1_000,
                out_amount: 1,
                missing_dynamic_accounts_as_default: false,
                swap_mode: SwapMode::ExactIn,
            })
            .unwrap();

        assert_eq!(swap.swap, Swap::RaydiumClmmV2);
        assert_eq!(swap.account_metas.len(), 15);
        assert_eq!(swap.account_metas[14].pubkey, first_tick_array);
    }

    #[test]
    fn test_pyth_price_feed_address_uses_push_oracle_program() {
        let jup_feed_id =
            hex_feed_id("0a0408d619e9380abad35060f9192039ed5042fa6f82301d0e48bb52be830996");
        let usdc_feed_id =
            hex_feed_id("eaa020c61cc479712813461ce153894a96a6c00b21ed0cfc2798d1f9a9e9c94a");

        assert_eq!(
            get_price_feed_account_address(PYTH_PRICE_SHARD_ID, &jup_feed_id),
            solana_sdk::pubkey!("7dbob1psH1iZBS7qPsm3Kwbf5DzSXK8Jyg31CTgTnxH5")
        );
        assert_eq!(
            get_price_feed_account_address(PYTH_PRICE_SHARD_ID, &usdc_feed_id),
            solana_sdk::pubkey!("Dpw1EAVrSB1ibxiDQyTAW6Zip3J4Btk2x4SgApQCeFbX")
        );
    }

    #[test]
    fn test_swap_v3_dyn_metas_match_expected_order() {
        let mut amm = build_dynamic_test_amm();
        let first_tick_array = amm.amm.get_swap_tick_arrays(true)[0];
        amm.amm.dynamic_tick_arrays.insert(
            first_tick_array,
            DynamicTickArrayState::Fixed(TickArrayState::default()),
        );

        let jupiter_program = Pubkey::new_unique();
        let swap_params = SwapParams {
            source_mint: amm.amm.pool_state.token_mint_0,
            destination_mint: amm.amm.pool_state.token_mint_1,
            source_token_account: Pubkey::new_unique(),
            destination_token_account: Pubkey::new_unique(),
            token_transfer_authority: Pubkey::new_unique(),
            quote_mint_to_referrer: None,
            jupiter_program_id: &jupiter_program,
            in_amount: 1_000,
            out_amount: 1,
            missing_dynamic_accounts_as_default: false,
            swap_mode: SwapMode::ExactIn,
        };

        let metas = amm.build_swap_v3_dyn_account_metas(&swap_params).unwrap();
        let (token0_pyth_oracle, token1_pyth_oracle) =
            get_dynamic_pyth_oracle_addresses(&amm.amm.pool_state).unwrap();

        assert_eq!(metas[0].pubkey, swap_params.token_transfer_authority);
        assert!(metas[0].is_signer);
        assert_eq!(metas[5].pubkey, amm.amm.pool_state.token_vault_0);
        assert_eq!(metas[6].pubkey, amm.amm.pool_state.token_vault_1);
        assert_eq!(metas[9].pubkey, spl_token_2022::ID);
        assert_eq!(metas[10].pubkey, anchor_spl::memo::ID);
        assert_eq!(metas[11].pubkey, amm.amm.pool_state.token_mint_0);
        assert_eq!(metas[12].pubkey, amm.amm.pool_state.token_mint_1);
        assert_eq!(metas[13].pubkey, TickArrayBitmapExtension::key(amm.key));
        assert!(!metas[13].is_writable);
        assert_eq!(metas[14].pubkey, first_tick_array);
        assert!(metas[14].is_writable);
        assert_eq!(metas[metas.len() - 2].pubkey, token0_pyth_oracle);
        assert_eq!(metas[metas.len() - 1].pubkey, token1_pyth_oracle);
        assert!(!metas[metas.len() - 2].is_writable);
        assert!(!metas[metas.len() - 1].is_writable);
    }

    #[test]
    fn test_swap_v3_dyn_metas_reverse_vault_and_mint_order() {
        let mut amm = build_dynamic_test_amm();
        let first_tick_array = amm.amm.get_swap_tick_arrays(false)[0];
        amm.amm.dynamic_tick_arrays.insert(
            first_tick_array,
            DynamicTickArrayState::Fixed(TickArrayState::default()),
        );

        let jupiter_program = Pubkey::new_unique();
        let swap_params = SwapParams {
            source_mint: amm.amm.pool_state.token_mint_1,
            destination_mint: amm.amm.pool_state.token_mint_0,
            source_token_account: Pubkey::new_unique(),
            destination_token_account: Pubkey::new_unique(),
            token_transfer_authority: Pubkey::new_unique(),
            quote_mint_to_referrer: None,
            jupiter_program_id: &jupiter_program,
            in_amount: 1_000,
            out_amount: 1,
            missing_dynamic_accounts_as_default: false,
            swap_mode: SwapMode::ExactIn,
        };

        let metas = amm.build_swap_v3_dyn_account_metas(&swap_params).unwrap();

        assert_eq!(metas[5].pubkey, amm.amm.pool_state.token_vault_1);
        assert_eq!(metas[6].pubkey, amm.amm.pool_state.token_vault_0);
        assert_eq!(metas[11].pubkey, amm.amm.pool_state.token_mint_1);
        assert_eq!(metas[12].pubkey, amm.amm.pool_state.token_mint_0);
        assert_eq!(metas[14].pubkey, first_tick_array);
    }

    #[test]
    fn test_swap_v3_dyn_metas_reject_missing_directional_first_tick_array() {
        let amm = build_dynamic_test_amm();
        let jupiter_program = Pubkey::new_unique();
        let swap_params = SwapParams {
            source_mint: amm.amm.pool_state.token_mint_0,
            destination_mint: amm.amm.pool_state.token_mint_1,
            source_token_account: Pubkey::new_unique(),
            destination_token_account: Pubkey::new_unique(),
            token_transfer_authority: Pubkey::new_unique(),
            quote_mint_to_referrer: None,
            jupiter_program_id: &jupiter_program,
            in_amount: 1_000,
            out_amount: 1,
            missing_dynamic_accounts_as_default: false,
            swap_mode: SwapMode::ExactIn,
        };

        let err = amm.build_swap_v3_dyn_account_metas(&swap_params).unwrap_err();
        assert!(format!("{err:#}")
            .contains("directional first tick array account missing for dynamic swap"));
    }

    #[cfg(feature = "dynamic-pool")]
    #[test]
    fn test_dynamic_pool_feature_on_returns_quote_and_swap_v3_dyn_route() {
        let mut amm = build_dynamic_test_amm();
        amm.amm.pool_state.sqrt_price_x64 =
            byreal_clmm_common::tick_math::get_sqrt_price_at_tick(0).unwrap();
        amm.amm.pool_state.tick_current = 0;
        amm.amm.pool_state.tick_spacing = 1;
        amm.amm.pool_state.liquidity = 1_000_000_000;
        amm.amm.pool_state.mint_decimals_0 = 6;
        amm.amm.pool_state.mint_decimals_1 = 6;
        amm.amm.pool_state.set_quote_token_flag(true);
        amm.amm.pool_state.open_time = 0;
        amm.amm.pool_state.arbitrage_fee_buffer_ppm = 10_000;
        amm.amm.pool_state.trade_slippage_fee_base_milli_bp = 50;
        amm.amm.pool_state.trade_slippage_fee_trade_size_threshold = 1;
        amm.amm.pool_state.imbalance_fee_base_tenths_of_bp = 20;
        amm.amm.pool_state.imbalance_fee_x = 50;
        amm.amm.amm_config.trade_fee_rate = 1_200;
        amm.amm.token0_vault_amount = 5_000_000_000;
        amm.amm.token1_vault_amount = 500_000_000;
        amm.amm.token0_pyth_price = Some(Price {
            price: 100_000_000,
            conf: 1,
            exponent: -8,
            publish_time: 100,
        });
        amm.amm.token1_pyth_price = Some(Price {
            price: 1_000_000,
            conf: 1,
            exponent: -6,
            publish_time: 100,
        });
        amm.timestamp.store(200, std::sync::atomic::Ordering::Relaxed);

        let current_start = 0;
        amm.amm
            .pool_state
            .flip_tick_array_bit_internal(current_start)
            .unwrap();
        let mut tick_array = TickArrayState::default();
        tick_array.pool_id = amm.key;
        tick_array.start_tick_index = current_start;
        tick_array.ticks[1].tick = 1;
        tick_array.ticks[1].liquidity_gross = 1;
        tick_array.initialized_tick_count = 1;
        let current_tick_array = amm.amm.get_tick_array_address(current_start);
        amm.amm
            .dynamic_tick_arrays
            .insert(current_tick_array, DynamicTickArrayState::Fixed(tick_array));

        let quote = amm
            .quote(&QuoteParams {
                amount: 1_000,
                input_mint: amm.amm.pool_state.token_mint_1,
                output_mint: amm.amm.pool_state.token_mint_0,
                swap_mode: SwapMode::ExactIn,
            })
            .unwrap();
        assert_eq!(quote.in_amount, 1_000);
        assert!(quote.out_amount > 0);
        assert!(quote.fee_amount > 0);

        let jupiter_program = Pubkey::new_unique();
        let swap = amm
            .get_swap_and_account_metas(&SwapParams {
                source_mint: amm.amm.pool_state.token_mint_1,
                destination_mint: amm.amm.pool_state.token_mint_0,
                source_token_account: Pubkey::new_unique(),
                destination_token_account: Pubkey::new_unique(),
                token_transfer_authority: Pubkey::new_unique(),
                quote_mint_to_referrer: None,
                jupiter_program_id: &jupiter_program,
                in_amount: quote.in_amount,
                out_amount: quote.out_amount,
                missing_dynamic_accounts_as_default: false,
                swap_mode: SwapMode::ExactIn,
            })
            .unwrap();
        assert_eq!(swap.swap, Swap::RaydiumClmmV2);
        assert_eq!(swap.account_metas[14].pubkey, current_tick_array);
        let (token0_pyth_oracle, token1_pyth_oracle) =
            get_dynamic_pyth_oracle_addresses(&amm.amm.pool_state).unwrap();
        assert_eq!(swap.account_metas[swap.account_metas.len() - 2].pubkey, token0_pyth_oracle);
        assert_eq!(swap.account_metas[swap.account_metas.len() - 1].pubkey, token1_pyth_oracle);
    }

    #[test]
    fn test_decode_vault_amount_supports_spl_and_token_2022() {
        let mut spl_vault = spl_token::state::Account::default();
        spl_vault.state = spl_token::state::AccountState::Initialized;
        spl_vault.amount = 123_456;
        let mut spl_bytes = vec![0u8; spl_token::state::Account::LEN];
        spl_token::state::Account::pack(spl_vault, &mut spl_bytes).unwrap();
        assert_eq!(
            decode_vault_amount(&spl_token::id(), &spl_bytes).unwrap(),
            123_456
        );

        let mut token2022_vault = Token2022Account::default();
        token2022_vault.state = spl_token_2022::state::AccountState::Initialized;
        token2022_vault.amount = 654_321;
        let mut token2022_bytes = vec![0u8; Token2022Account::LEN];
        Token2022Account::pack(token2022_vault, &mut token2022_bytes).unwrap();
        assert_eq!(
            decode_vault_amount(&spl_token_2022::id(), &token2022_bytes).unwrap(),
            654_321
        );

        let err = decode_vault_amount(&Pubkey::new_unique(), &spl_bytes).unwrap_err();
        assert!(format!("{err:#}").contains("unsupported token vault owner"));
    }

    fn hex_feed_id(value: &str) -> [u8; 32] {
        let mut feed_id = [0u8; 32];
        for i in 0..32 {
            feed_id[i] = u8::from_str_radix(&value[i * 2..i * 2 + 2], 16).unwrap();
        }
        feed_id
    }
}

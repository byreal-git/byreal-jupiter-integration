use anchor_lang::{AccountDeserialize, Discriminator};
use byreal_clmm_common::PoolState;
use byreal_clmm_jupiter_integration::{ByrealClmm, BYREAL_CLMM_PROGRAM};
use jupiter_amm_interface::{
    Amm, AmmContext, ClockRef, KeyedAccount, QuoteParams, SwapMode, SwapParams,
};
use solana_sdk::{account::Account, pubkey::Pubkey};
use std::sync::atomic::Ordering;

#[cfg(feature = "dynamic-pool")]
#[test]
fn dynamic_pool_accounts_to_update_include_vaults_and_pyth_oracles() {
    let (amm, pool_state, _) = dynamic_test_amm();
    let accounts = amm.get_accounts_to_update();

    assert!(accounts.contains(&pool_state.token_vault_0));
    assert!(accounts.contains(&pool_state.token_vault_1));
    assert!(accounts.contains(&byreal_clmm_common::TickArrayBitmapExtension::key(amm.key())));
    assert!(accounts.contains(&pyth_price_feed_address(&pool_state.token0_pyth_feed_id)));
    assert!(accounts.contains(&pyth_price_feed_address(&pool_state.token1_pyth_feed_id)));
}

#[cfg(not(feature = "dynamic-pool"))]
#[test]
fn dynamic_pool_accounts_to_update_omit_vaults_and_pyth_oracles_when_flag_off() {
    let (amm, pool_state, _) = dynamic_test_amm();
    let accounts = amm.get_accounts_to_update();

    assert!(!accounts.contains(&pool_state.token_vault_0));
    assert!(!accounts.contains(&pool_state.token_vault_1));
    assert!(!accounts.contains(&pyth_price_feed_address(&pool_state.token0_pyth_feed_id)));
    assert!(!accounts.contains(&pyth_price_feed_address(&pool_state.token1_pyth_feed_id)));
}

#[cfg(not(feature = "dynamic-pool"))]
#[test]
fn dynamic_pool_rejects_quote_and_swap_metas_when_sdk_flag_off() {
    let (mut amm, pool_state, _) = dynamic_test_amm();
    amm.update(&dynamic_update_account_map(amm.key(), &pool_state))
        .unwrap();

    let quote_err = amm
        .quote(&QuoteParams {
            amount: 1_000,
            input_mint: pool_state.token_mint_0,
            output_mint: pool_state.token_mint_1,
            swap_mode: SwapMode::ExactIn,
        })
        .err()
        .expect("dynamic pool disabled should reject swap metas");
    assert!(format!("{quote_err:#}").contains("dynamic pool disabled by compile-time feature"));

    let jupiter_program = Pubkey::new_unique();
    let swap_err = amm
        .get_swap_and_account_metas(&SwapParams {
            source_mint: pool_state.token_mint_0,
            destination_mint: pool_state.token_mint_1,
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
        .expect("dynamic pool route should fail closed");
    assert!(format!("{swap_err:#}").contains("dynamic pool disabled by compile-time feature"));
}

#[cfg(feature = "dynamic-pool")]
#[test]
fn dynamic_pool_feature_on_enters_real_quote_and_swap_paths() {
    let (amm, pool_state, _) = dynamic_test_amm();

    let quote_err = amm
        .quote(&QuoteParams {
            amount: 1_000,
            input_mint: pool_state.token_mint_0,
            output_mint: pool_state.token_mint_1,
            swap_mode: SwapMode::ExactIn,
        })
        .err()
        .expect("mock dynamic pool should still error without loaded pyth prices");
    assert!(format!("{quote_err:#}").contains("dynamic fee token0 pyth price missing"));

    let jupiter_program = Pubkey::new_unique();
    let swap_err = amm
        .get_swap_and_account_metas(&SwapParams {
            source_mint: pool_state.token_mint_0,
            destination_mint: pool_state.token_mint_1,
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
        .expect("mock dynamic pool should still error without loaded tick arrays");
    assert!(format!("{swap_err:#}")
        .contains("directional first tick array account missing for dynamic swap"));
}

#[cfg(feature = "dynamic-pool")]
#[test]
fn dynamic_pool_without_pyth_quotes_and_builds_in_both_swap_modes() {
    for (feed0, feed1) in [([0; 32], [0; 32]), ([0; 32], [2; 32]), ([1; 32], [0; 32])] {
        let (mut amm, mut pool, _) = dynamic_test_amm();
        pool.token0_pyth_feed_id = feed0;
        pool.token1_pyth_feed_id = feed1;
        amm.update(&dynamic_update_account_map(amm.key(), &pool))
            .unwrap();
        for swap_mode in [SwapMode::ExactIn, SwapMode::ExactOut] {
            let quote = amm
                .quote(&QuoteParams {
                    amount: 1_000,
                    input_mint: pool.token_mint_1,
                    output_mint: pool.token_mint_0,
                    swap_mode,
                })
                .unwrap();
            assert!(quote.in_amount > 0 && quote.out_amount > 0 && quote.fee_amount > 0);
            let jupiter_program = Pubkey::new_unique();
            let swap = amm
                .get_swap_and_account_metas(&SwapParams {
                    source_mint: pool.token_mint_1,
                    destination_mint: pool.token_mint_0,
                    source_token_account: Pubkey::new_unique(),
                    destination_token_account: Pubkey::new_unique(),
                    token_transfer_authority: Pubkey::new_unique(),
                    quote_mint_to_referrer: None,
                    jupiter_program_id: &jupiter_program,
                    in_amount: quote.in_amount,
                    out_amount: quote.out_amount,
                    missing_dynamic_accounts_as_default: false,
                    swap_mode,
                })
                .unwrap();
            assert_eq!(swap.account_metas.len(), 15);
        }
    }
}

#[cfg(feature = "dynamic-pool")]
#[test]
fn dynamic_pool_stale_prices_keep_quote_and_oracle_accounts() {
    let (mut amm, pool, clock) = dynamic_test_amm();
    amm.update(&dynamic_update_account_map(amm.key(), &pool))
        .unwrap();
    let params = QuoteParams {
        amount: 1_000,
        input_mint: pool.token_mint_1,
        output_mint: pool.token_mint_0,
        swap_mode: SwapMode::ExactIn,
    };
    let fresh = amm.quote(&params).unwrap();
    clock.unix_timestamp.store(3_701, Ordering::Relaxed);
    let stale = amm.quote(&params).unwrap();
    assert!(stale.out_amount > fresh.out_amount);
    assert!(stale.fee_amount > 0 && stale.fee_amount < fresh.fee_amount);
    let jupiter_program = Pubkey::new_unique();
    let swap = amm
        .get_swap_and_account_metas(&SwapParams {
            source_mint: pool.token_mint_1,
            destination_mint: pool.token_mint_0,
            source_token_account: Pubkey::new_unique(),
            destination_token_account: Pubkey::new_unique(),
            token_transfer_authority: Pubkey::new_unique(),
            quote_mint_to_referrer: None,
            jupiter_program_id: &jupiter_program,
            in_amount: stale.in_amount,
            out_amount: stale.out_amount,
            missing_dynamic_accounts_as_default: false,
            swap_mode: SwapMode::ExactIn,
        })
        .unwrap();
    assert_eq!(swap.account_metas.len(), 17);
    assert_eq!(
        swap.account_metas[15].pubkey,
        pyth_price_feed_address(&pool.token0_pyth_feed_id)
    );
    assert_eq!(
        swap.account_metas[16].pubkey,
        pyth_price_feed_address(&pool.token1_pyth_feed_id)
    );
}

fn dynamic_test_amm() -> (ByrealClmm, PoolState, ClockRef) {
    let pool_key = Pubkey::new_unique();
    let mut pool_state = PoolState::default();
    pool_state.amm_config = Pubkey::new_unique();
    pool_state.token_mint_0 = Pubkey::new_unique();
    pool_state.token_mint_1 = Pubkey::new_unique();
    pool_state.token_vault_0 = Pubkey::new_unique();
    pool_state.token_vault_1 = Pubkey::new_unique();
    pool_state.observation_key = Pubkey::new_unique();
    pool_state.tick_spacing = 1;
    pool_state.sqrt_price_x64 = byreal_clmm_common::tick_math::get_sqrt_price_at_tick(0).unwrap();
    pool_state.liquidity = 1_000_000_000;
    pool_state.mint_decimals_0 = 6;
    pool_state.mint_decimals_1 = 6;
    pool_state.set_quote_token_flag(true);
    pool_state.flip_tick_array_bit_internal(0).unwrap();
    pool_state.token0_pyth_feed_id = [1u8; 32];
    pool_state.token1_pyth_feed_id = [2u8; 32];
    pool_state.set_swap_dynamic_fee_enabled(true);

    let data = pool_state_account_data(&pool_state);
    let keyed_account = KeyedAccount {
        key: pool_key,
        account: Account {
            lamports: 1_000_000,
            data,
            owner: BYREAL_CLMM_PROGRAM,
            executable: false,
            rent_epoch: 0,
        },
        params: None,
    };
    let clock_ref = ClockRef::default();
    clock_ref.unix_timestamp.store(200, Ordering::Relaxed);
    let amm = ByrealClmm::from_keyed_account(
        &keyed_account,
        &AmmContext {
            clock_ref: clock_ref.clone(),
        },
    )
    .unwrap();

    (amm, pool_state, clock_ref)
}

fn pool_state_account_data(pool_state: &PoolState) -> Vec<u8> {
    let mut data = Vec::with_capacity(PoolState::LEN);
    data.extend_from_slice(&PoolState::DISCRIMINATOR);
    data.extend_from_slice(bytemuck::bytes_of(pool_state));
    let mut verify = data.as_slice();
    PoolState::try_deserialize(&mut verify).unwrap();
    data
}

fn amm_config_account_data(config: &byreal_clmm_common::AmmConfig) -> Vec<u8> {
    let mut data = Vec::new();
    anchor_lang::AccountSerialize::try_serialize(config, &mut data).unwrap();
    data
}

fn dynamic_update_account_map(
    pool_key: Pubkey,
    pool_state: &PoolState,
) -> jupiter_amm_interface::AccountMap {
    let mut account_map = jupiter_amm_interface::AccountMap::default();
    account_map.insert(
        pool_key,
        Account {
            lamports: 1_000_000,
            data: pool_state_account_data(pool_state),
            owner: BYREAL_CLMM_PROGRAM,
            executable: false,
            rent_epoch: 0,
        },
    );
    account_map.insert(
        pool_state.amm_config,
        Account {
            lamports: 1_000_000,
            data: amm_config_account_data(&byreal_clmm_common::AmmConfig {
                trade_fee_rate: 1_000,
                ..Default::default()
            }),
            owner: BYREAL_CLMM_PROGRAM,
            executable: false,
            rent_epoch: 0,
        },
    );
    account_map.insert(
        pool_state.token_mint_0,
        Account {
            lamports: 1_000_000,
            data: Vec::new(),
            owner: spl_token::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    account_map.insert(
        pool_state.token_mint_1,
        Account {
            lamports: 1_000_000,
            data: Vec::new(),
            owner: spl_token::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    #[cfg(feature = "dynamic-pool")]
    {
        use spl_token::solana_program::program_pack::Pack;
        for key in [pool_state.token_vault_0, pool_state.token_vault_1] {
            let vault = spl_token::state::Account {
                amount: 10_000_000_000,
                state: spl_token::state::AccountState::Initialized,
                ..Default::default()
            };
            let mut data = vec![0; spl_token::state::Account::LEN];
            spl_token::state::Account::pack(vault, &mut data).unwrap();
            account_map.insert(
                key,
                Account {
                    data,
                    owner: spl_token::ID,
                    ..Default::default()
                },
            );
        }
        let mut tick_array = byreal_clmm_common::TickArrayState {
            pool_id: pool_key,
            initialized_tick_count: 1,
            ..Default::default()
        };
        tick_array.ticks[1].tick = 1;
        tick_array.ticks[1].liquidity_gross = 1;
        let mut data = byreal_clmm_common::TickArrayState::DISCRIMINATOR.to_vec();
        data.extend_from_slice(bytemuck::bytes_of(&tick_array));
        let tick_key = Pubkey::find_program_address(
            &[b"tick_array", pool_key.as_ref(), &0i32.to_be_bytes()],
            &BYREAL_CLMM_PROGRAM,
        )
        .0;
        account_map.insert(
            tick_key,
            Account {
                data,
                owner: BYREAL_CLMM_PROGRAM,
                ..Default::default()
            },
        );
        if pool_state.token0_pyth_feed_id != [0; 32] && pool_state.token1_pyth_feed_id != [0; 32] {
            for (feed_id, price) in [
                (pool_state.token0_pyth_feed_id, 2_000_000),
                (pool_state.token1_pyth_feed_id, 1_000_000),
            ] {
                let update = pyth_solana_receiver_sdk::price_update::PriceUpdateV2 {
                    write_authority: Pubkey::new_unique(),
                    verification_level:
                        pyth_solana_receiver_sdk::price_update::VerificationLevel::Full,
                    price_message: pyth_solana_receiver_sdk::price_update::PriceFeedMessage {
                        feed_id,
                        price,
                        conf: 1,
                        exponent: -6,
                        publish_time: 100,
                        prev_publish_time: 99,
                        ema_price: price,
                        ema_conf: 1,
                    },
                    posted_slot: 1,
                };
                let mut data = Vec::new();
                anchor_lang::AccountSerialize::try_serialize(&update, &mut data).unwrap();
                account_map.insert(
                    pyth_price_feed_address(&feed_id),
                    Account {
                        data,
                        owner: pyth_solana_receiver_sdk::ID,
                        ..Default::default()
                    },
                );
            }
        }
    }
    account_map
}

fn pyth_price_feed_address(feed_id: &[u8; 32]) -> Pubkey {
    Pubkey::find_program_address(
        &[&0u16.to_le_bytes(), feed_id],
        &pyth_solana_receiver_sdk::PYTH_PUSH_ORACLE_ID,
    )
    .0
}

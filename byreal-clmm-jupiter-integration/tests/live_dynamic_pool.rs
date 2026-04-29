use anchor_lang::AccountDeserialize;
use byreal_clmm_common::{PoolState, TickArrayBitmapExtension};
use byreal_clmm_jupiter_integration::{ByrealClmm, BYREAL_CLMM_PROGRAM};
use jupiter_amm_interface::{Amm, AmmContext, ClockRef, KeyedAccount, QuoteParams, SwapMode};
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::{str::FromStr, sync::atomic::Ordering};

const DEFAULT_RPC_URL: &str = "https://api.mainnet-beta.solana.com";
const DEV_DYNAMIC_POOL: &str = "5Ccv9vfH134L6uiKLE3f3RS8n4ZnJfrfbqQR14X4Z5AL";
const WSOL_MINT: Pubkey = solana_sdk::pubkey!("So11111111111111111111111111111111111111112");
const USDC_MINT: Pubkey = solana_sdk::pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");

struct LoadedDynamicPool {
    amm: ByrealClmm,
}

#[cfg(not(feature = "dynamic-pool"))]
#[test]
#[ignore]
fn live_dev_dynamic_pool_flag_off_rejects_quote_after_loading_required_accounts() {
    let loaded = load_dynamic_pool();
    let err = loaded
        .amm
        .quote(&QuoteParams {
            amount: 1_000_000,
            input_mint: WSOL_MINT,
            output_mint: USDC_MINT,
            swap_mode: SwapMode::ExactIn,
        })
        .unwrap_err();

    println!("quote error: {err:#}");
    assert!(format!("{err:#}").contains("dynamic pool disabled by compile-time feature"));
}

#[cfg(feature = "dynamic-pool")]
#[test]
#[ignore]
fn live_dev_dynamic_pool_flag_on_still_fails_closed_until_swap_v3_dyn_route_exists() {
    let loaded = load_dynamic_pool();
    let err = loaded
        .amm
        .quote(&QuoteParams {
            amount: 1_000_000,
            input_mint: WSOL_MINT,
            output_mint: USDC_MINT,
            swap_mode: SwapMode::ExactIn,
        })
        .unwrap_err();

    println!("quote error: {err:#}");
    assert!(format!("{err:#}").contains("dynamic CLMM tx path is not supported yet"));
}

fn load_dynamic_pool() -> LoadedDynamicPool {
    let rpc_url = std::env::var("BYREAL_RPC_URL").unwrap_or_else(|_| DEFAULT_RPC_URL.to_string());
    let pool_address = std::env::var("BYREAL_DYNAMIC_POOL")
        .ok()
        .map(|value| Pubkey::from_str(&value).unwrap())
        .unwrap_or_else(|| Pubkey::from_str(DEV_DYNAMIC_POOL).unwrap());
    let rpc = RpcClient::new(rpc_url);

    let account = rpc.get_account(&pool_address).unwrap();
    assert_eq!(
        account.owner, BYREAL_CLMM_PROGRAM,
        "compile with the feature matching the pool owner"
    );

    let mut pool_data = account.data.as_slice();
    let pool_state = PoolState::try_deserialize(&mut pool_data).unwrap();
    assert!(pool_state.is_swap_dynamic_fee_enabled());
    assert!(
        (pool_state.token_mint_0 == WSOL_MINT && pool_state.token_mint_1 == USDC_MINT)
            || (pool_state.token_mint_0 == USDC_MINT && pool_state.token_mint_1 == WSOL_MINT),
        "expected WSOL/USDC pool, got {}/{}",
        pool_state.token_mint_0,
        pool_state.token_mint_1
    );

    let clock_ref = ClockRef::default();
    clock_ref
        .unix_timestamp
        .store(pool_state.open_time as i64 + 1, Ordering::Relaxed);
    let keyed_account = KeyedAccount {
        key: pool_address,
        account: account.into(),
        params: None,
    };
    let amm = ByrealClmm::from_keyed_account(&keyed_account, &AmmContext { clock_ref }).unwrap();

    let accounts_to_update = amm.get_accounts_to_update();
    assert!(accounts_to_update.contains(&pool_state.token_vault_0));
    assert!(accounts_to_update.contains(&pool_state.token_vault_1));
    assert!(accounts_to_update.contains(&TickArrayBitmapExtension::key(pool_address)));
    assert!(accounts_to_update.contains(&pyth_price_feed_address(&pool_state.token0_pyth_feed_id)));
    assert!(accounts_to_update.contains(&pyth_price_feed_address(&pool_state.token1_pyth_feed_id)));

    let accounts = rpc.get_multiple_accounts(&accounts_to_update).unwrap();
    let missing_accounts = accounts_to_update
        .iter()
        .zip(accounts.iter())
        .filter_map(|(key, account)| account.is_none().then_some(*key))
        .collect::<Vec<_>>();

    println!("pool: {pool_address}");
    println!("program: {}", BYREAL_CLMM_PROGRAM);
    println!("token_mint_0: {}", pool_state.token_mint_0);
    println!("token_mint_1: {}", pool_state.token_mint_1);
    println!("accounts_to_update: {}", accounts_to_update.len());
    if !missing_accounts.is_empty() {
        println!("missing update accounts:");
        for key in missing_accounts {
            println!("- {key}");
        }
    }

    LoadedDynamicPool { amm }
}

fn pyth_price_feed_address(feed_id: &[u8; 32]) -> Pubkey {
    Pubkey::find_program_address(&[&0u16.to_le_bytes(), feed_id], &pyth_solana_receiver_sdk::ID).0
}

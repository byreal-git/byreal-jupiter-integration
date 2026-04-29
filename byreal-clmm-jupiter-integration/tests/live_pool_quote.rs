use anchor_lang::AccountDeserialize;
use anyhow::{ensure, Result};
use byreal_clmm_common::{ByrealClmmAmm, PoolState, TickArrayBitmapExtension};
use byreal_clmm_jupiter_integration::{ByrealClmm, BYREAL_CLMM_PROGRAM};
use jupiter_amm_interface::{AccountMap, Amm, AmmContext, ClockRef, KeyedAccount, QuoteParams, SwapMode};
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::{collections::HashSet, str::FromStr, sync::atomic::Ordering};

const DEFAULT_RPC_URL: &str = "https://api.mainnet-beta.solana.com";
const DEFAULT_POOL: &str = "DCiq2T2tMxdRgoQ2jQ9JhCNaMU5TxsPdrCa7esSCvxiw";
const USDC_MINT: Pubkey = solana_sdk::pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
const DEFAULT_EXACT_IN: u64 = 1_000_000;
const DEFAULT_EXACT_OUT: u64 = 1_000_000;

#[cfg(not(feature = "dynamic-pool"))]
#[test]
#[ignore]
fn quote_live_usdc_pair_pool_dynamic_flag_off() {
    run_live_pool_quote_probe(Some("dynamic pool disabled by compile-time feature"));
}

#[cfg(feature = "dynamic-pool")]
#[test]
#[ignore]
fn quote_live_usdc_pair_pool_dynamic_flag_on() {
    run_live_pool_quote_probe(None);
}

fn run_live_pool_quote_probe(expected_error: Option<&str>) {
    if std::env::var("RUN_INTEGRATION_TESTS").is_err() {
        println!("Skipping integration test. Set RUN_INTEGRATION_TESTS=1 to run.");
        return;
    }

    let rpc = RpcClient::new(
        std::env::var("BYREAL_RPC_URL").unwrap_or_else(|_| DEFAULT_RPC_URL.to_string()),
    );
    let pool_address = std::env::var("BYREAL_LIVE_POOL")
        .ok()
        .map(|value| Pubkey::from_str(&value).unwrap())
        .unwrap_or_else(|| Pubkey::from_str(DEFAULT_POOL).unwrap());
    let loaded = load_pool(&rpc, pool_address).unwrap();
    let pool_state = &loaded.common_amm.pool_state;

    let output_mint = if pool_state.token_mint_0 == USDC_MINT {
        pool_state.token_mint_1
    } else if pool_state.token_mint_1 == USDC_MINT {
        pool_state.token_mint_0
    } else {
        panic!(
            "Pool {} is not a USDC pair (mints: {}, {})",
            pool_address, pool_state.token_mint_0, pool_state.token_mint_1
        );
    };

    let exact_in_amount = std::env::var("BYREAL_EXACT_IN_AMOUNT")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_EXACT_IN);
    let exact_out_amount = std::env::var("BYREAL_EXACT_OUT_AMOUNT")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_EXACT_OUT);

    println!("pool: {}", pool_address);
    println!("program: {}", BYREAL_CLMM_PROGRAM);
    println!("token_mint_0: {}", pool_state.token_mint_0);
    println!("token_mint_1: {}", pool_state.token_mint_1);
    println!("token0_pyth_feed_id: {}", hex_feed_id(&pool_state.token0_pyth_feed_id));
    println!("token1_pyth_feed_id: {}", hex_feed_id(&pool_state.token1_pyth_feed_id));
    println!(
        "token0_pyth_address: {}",
        pyth_price_feed_address(&pool_state.token0_pyth_feed_id)
    );
    println!(
        "token1_pyth_address: {}",
        pyth_price_feed_address(&pool_state.token1_pyth_feed_id)
    );
    println!(
        "dynamic_fee_enabled: {}",
        pool_state.is_swap_dynamic_fee_enabled()
    );
    println!("accounts_to_update: {}", loaded.accounts_to_update.len());
    if !loaded.missing_accounts.is_empty() {
        println!("missing update accounts:");
        for key in &loaded.missing_accounts {
            println!("- {key}");
        }
    }

    let exact_in_result = loaded
        .adapter
        .quote(&QuoteParams {
            amount: exact_in_amount,
            input_mint: USDC_MINT,
            output_mint,
            swap_mode: SwapMode::ExactIn,
        });
    match exact_in_result {
        Ok(quote) => {
            println!(
                "USDC -> pair exact-in: in={}, out={}, fee={}",
                quote.in_amount, quote.out_amount, quote.fee_amount
            );
            assert!(expected_error.is_none());
        }
        Err(err) => {
            println!("USDC -> pair exact-in error: {err:#}");
            assert!(format!("{err:#}").contains(expected_error.unwrap()));
        }
    }

    let exact_out_result = loaded
        .adapter
        .quote(&QuoteParams {
            amount: exact_out_amount,
            input_mint: USDC_MINT,
            output_mint,
            swap_mode: SwapMode::ExactOut,
        });
    match exact_out_result {
        Ok(quote) => {
            println!(
                "USDC -> pair exact-out: in={}, out={}, fee={}",
                quote.in_amount, quote.out_amount, quote.fee_amount
            );
            assert!(expected_error.is_none());
        }
        Err(err) => {
            println!("USDC -> pair exact-out error: {err:#}");
            assert!(format!("{err:#}").contains(expected_error.unwrap()));
        }
    }
}

struct LoadedPool {
    adapter: ByrealClmm,
    common_amm: ByrealClmmAmm,
    accounts_to_update: Vec<Pubkey>,
    missing_accounts: Vec<Pubkey>,
}

fn load_pool(rpc: &RpcClient, pool_address: Pubkey) -> Result<LoadedPool> {
    let pool_account = rpc.get_account(&pool_address)?;
    ensure!(
        pool_account.owner == BYREAL_CLMM_PROGRAM,
        "pool owner mismatch: expected {}, got {}",
        BYREAL_CLMM_PROGRAM,
        pool_account.owner
    );

    let mut pool_data = pool_account.data.as_slice();
    let pool_state = PoolState::try_deserialize(&mut pool_data)?;
    let swap_timestamp = (pool_state.open_time as i64).saturating_add(1);

    let clock_ref = ClockRef::default();
    clock_ref.unix_timestamp.store(swap_timestamp, Ordering::Relaxed);
    let amm_context = AmmContext { clock_ref };
    let keyed_account = KeyedAccount {
        key: pool_address,
        account: pool_account.into(),
        params: None,
    };
    let mut adapter = ByrealClmm::from_keyed_account(&keyed_account, &amm_context)?;

    let accounts_to_update = adapter.get_accounts_to_update();
    let accounts = rpc.get_multiple_accounts(&accounts_to_update)?;
    let missing_accounts = accounts_to_update
        .iter()
        .enumerate()
        .filter_map(|(i, key)| accounts[i].is_none().then_some(*key))
        .collect::<Vec<_>>();

    let bitmap_key = TickArrayBitmapExtension::key(pool_address);
    let mut account_map: AccountMap = accounts_to_update
        .iter()
        .enumerate()
        .filter_map(|(i, key)| accounts[i].as_ref().map(|account| (*key, account.clone().into())))
        .collect();
    let bitmap_extension = account_map
        .get(&bitmap_key)
        .and_then(|account| TickArrayBitmapExtension::try_deserialize(&mut account.data.as_ref()).ok());

    let common_amm = ByrealClmmAmm {
        key: pool_address,
        pool_state,
        amm_config: byreal_clmm_common::AmmConfig::default(),
        bitmap_extension,
        max_one_side_tick_arrays: 3,
        dynamic_tick_arrays: Default::default(),
        token0_vault_amount: 0,
        token1_vault_amount: 0,
        token0_pyth_price: None,
        token1_pyth_price: None,
        token0_transfer_fee_config: None,
        token1_transfer_fee_config: None,
    };

    let mut full_tick_addrs = HashSet::new();
    for &dir in &[true, false] {
        if let Ok((_, mut start)) = common_amm
            .pool_state
            .get_first_initialized_tick_array(&common_amm.bitmap_extension, dir)
        {
            loop {
                full_tick_addrs.insert(common_amm.get_tick_array_address(start));
                match common_amm.pool_state.next_initialized_tick_array_start_index(
                    &common_amm.bitmap_extension,
                    start,
                    dir,
                ) {
                    Ok(Some(next)) => start = next,
                    _ => break,
                }
            }
        }
    }
    for address in full_tick_addrs {
        if !account_map.contains_key(&address) {
            if let Ok(account) = rpc.get_account(&address) {
                account_map.insert(address, account.into());
            }
        }
    }

    adapter.update(&account_map)?;

    Ok(LoadedPool {
        adapter,
        common_amm,
        accounts_to_update,
        missing_accounts,
    })
}

fn pyth_price_feed_address(feed_id: &[u8; 32]) -> Pubkey {
    Pubkey::find_program_address(
        &[&0u16.to_le_bytes(), feed_id],
        &pyth_solana_receiver_sdk::PYTH_PUSH_ORACLE_ID,
    )
    .0
}

fn hex_feed_id(feed_id: &[u8; 32]) -> String {
    feed_id
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join("")
}

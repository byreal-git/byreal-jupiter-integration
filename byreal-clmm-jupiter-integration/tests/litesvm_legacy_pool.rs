#![cfg(feature = "with-litesvm")]

use anchor_lang::AccountDeserialize;
use anyhow::{anyhow, ensure, Result};
use byreal_clmm_common::{
    AmmConfig, ByrealClmmAmm, DynamicTickArrayState, PoolState, TickArrayBitmapExtension,
};
use byreal_clmm_jupiter_integration::{ByrealClmm, BYREAL_CLMM_PROGRAM};
use jupiter_amm_interface::{AccountMap, Amm, AmmContext, ClockRef, KeyedAccount, QuoteParams, SwapMode};
#[cfg(feature = "dynamic-pool")]
use jupiter_amm_interface::{Swap, SwapParams};
use litesvm::LiteSVM;
use solana_account::Account as RawAccount;
use solana_account::ReadableAccount;
use solana_client::rpc_client::RpcClient;
use solana_clock::Clock as RawClock;
use solana_instruction::{account_meta::AccountMeta as RawAccountMeta, Instruction as RawInstruction};
use solana_message::Message as RawMessage;
use solana_pubkey::Pubkey as RawPubkey;
use solana_sdk::account::Account as SdkAccount;
use solana_sdk::pubkey::Pubkey;
use solana_transaction::Transaction as RawTransaction;
use spl_token::solana_program::program_pack::Pack;
use std::{collections::{HashMap, HashSet}, sync::atomic::Ordering};
#[cfg(feature = "dynamic-pool")]
use std::sync::Mutex;

const LEGACY_PROGRAM: Pubkey = solana_sdk::pubkey!("45iBNkaENereLKMjLm2LHkF3hpDapf6mnvrM5HWFg9cY");
const LEGACY_POOL: Pubkey = solana_sdk::pubkey!("J4jiEPEu8c8nLdpkiMa7k1P8rL1HCJSNxCvzA5DsmYds");
const MAINNET_PROGRAM: Pubkey = solana_sdk::pubkey!("REALQqNEomY6cQGZJUGwywTBD2UmDT32rZcNnfxQ5N2");
const MAINNET_CBBTC_USDC_POOL: Pubkey =
    solana_sdk::pubkey!("A5vkCw1VXPNXq5VFbffPm6Bo4kVKAP1UUoRrEn3gyVey");
const MAINNET_USDC_MINT: Pubkey =
    solana_sdk::pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
const MAINNET_CBBTC_MINT: Pubkey =
    solana_sdk::pubkey!("cbbtcf3aa214zXHbiAZQwf4122FBYbraNdFqgw4iMij");
#[cfg(feature = "dynamic-pool")]
const DYNAMIC_JUP_USDC_POOL: Pubkey =
    solana_sdk::pubkey!("DCiq2T2tMxdRgoQ2jQ9JhCNaMU5TxsPdrCa7esSCvxiw");
#[cfg(feature = "dynamic-pool")]
const JUP_MINT: Pubkey =
    solana_sdk::pubkey!("JUPyiwrYJFskUPiHa7hkeR8VUtAeFoSYbKedZNsDvCN");
#[cfg(feature = "dynamic-pool")]
static LIVE_RPC_TEST_LOCK: Mutex<()> = Mutex::new(());

struct LoadedPool {
    pool_address: Pubkey,
    adapter: ByrealClmm,
    common_amm: ByrealClmmAmm,
    account_map: AccountMap,
    swap_timestamp: i64,
}

struct SwapIxArgs {
    amount: u64,
    other_amount_threshold: u64,
    sqrt_price_limit_x64: u128,
    is_base_input: bool,
}

#[test]
fn decode_upgradeable_programdata_offset_handles_immutable_program() {
    let state = solana_program::bpf_loader_upgradeable::UpgradeableLoaderState::ProgramData {
        slot: 1,
        upgrade_authority_address: None,
    };
    let payload = [1u8, 2, 3, 4];
    let mut data = bincode::serialize(&state).unwrap();
    let metadata_len = data.len();
    data.extend_from_slice(&payload);

    let (decoded, offset) = decode_upgradeable_loader_state_prefix(&data).unwrap();

    assert_eq!(decoded, state);
    assert_eq!(offset, metadata_len);
    assert_eq!(&data[offset..], payload);
}

#[test]
#[ignore]
fn litesvm_vs_sdk_legacy_sol_test_pool_exact_in() {
    if BYREAL_CLMM_PROGRAM != LEGACY_PROGRAM {
        println!("Skipping legacy pool LiteSVM test: compile with --features \"devnet with-litesvm\"");
        return;
    }

    let rpc = RpcClient::new("https://api.mainnet-beta.solana.com");
    let loaded = load_legacy_pool(&rpc).unwrap();
    let mints = loaded.adapter.get_reserve_mints();
    let input_mint = mints[0];
    let output_mint = mints[1];
    let amount_in = 100_000u64;

    let sdk_quote = loaded
        .adapter
        .quote(&QuoteParams {
            amount: amount_in,
            input_mint,
            output_mint,
            swap_mode: SwapMode::ExactIn,
        })
        .unwrap();
    println!(
        "Legacy SDK quote: in={}, out={}, fee={}",
        sdk_quote.in_amount, sdk_quote.out_amount, sdk_quote.fee_amount
    );

    let sim_result = simulate_legacy_swap(
        &rpc,
        &loaded,
        input_mint,
        output_mint,
        amount_in,
        0,
        true,
        amount_in.saturating_mul(2),
    )
    .unwrap();
    println!(
        "Legacy LiteSVM out={}, diff (sdk_math - litesvm)={}",
        sim_result.destination_amount,
        sdk_quote.out_amount.saturating_sub(sim_result.destination_amount)
    );

    assert_amounts_close(
        "legacy exact-in output amount",
        sdk_quote.out_amount,
        sim_result.destination_amount,
    );
}

#[test]
#[ignore]
fn litesvm_vs_sdk_legacy_sol_test_pool_exact_out() {
    if BYREAL_CLMM_PROGRAM != LEGACY_PROGRAM {
        println!("Skipping legacy pool LiteSVM test: compile with --features \"devnet with-litesvm\"");
        return;
    }

    let rpc = RpcClient::new("https://api.mainnet-beta.solana.com");
    let loaded = load_legacy_pool(&rpc).unwrap();
    let mints = loaded.adapter.get_reserve_mints();
    let input_mint = mints[0];
    let output_mint = mints[1];
    let desired_out = 50_000u64;

    let sdk_quote = loaded
        .adapter
        .quote(&QuoteParams {
            amount: desired_out,
            input_mint,
            output_mint,
            swap_mode: SwapMode::ExactOut,
        })
        .unwrap();
    println!(
        "Legacy SDK exact-out quote: in={}, out={}, fee={}",
        sdk_quote.in_amount, sdk_quote.out_amount, sdk_quote.fee_amount
    );

    let sim_result = simulate_legacy_swap(
        &rpc,
        &loaded,
        input_mint,
        output_mint,
        desired_out,
        sdk_quote.in_amount,
        false,
        sdk_quote.in_amount.saturating_mul(2),
    )
    .unwrap();
    println!(
        "Legacy LiteSVM exact-out in={}, out={}, diff (sdk_in - litesvm_in)={}",
        sim_result.source_debit,
        sim_result.destination_amount,
        sdk_quote.in_amount.saturating_sub(sim_result.source_debit)
    );

    assert_amounts_close(
        "legacy exact-out input amount",
        sdk_quote.in_amount,
        sim_result.source_debit,
    );
    assert_eq!(sim_result.destination_amount, desired_out);
}

#[test]
#[ignore]
fn litesvm_vs_sdk_mainnet_cbbtc_usdc_exact_in() {
    if BYREAL_CLMM_PROGRAM != MAINNET_PROGRAM {
        println!("Skipping mainnet pool LiteSVM test: compile with --features \"mainnet with-litesvm\"");
        return;
    }

    let rpc = RpcClient::new("https://api.mainnet-beta.solana.com");
    let loaded = load_pool(&rpc, MAINNET_CBBTC_USDC_POOL, MAINNET_PROGRAM).unwrap();
    let cb_btc_mint = cbbtc_mint_from_pool(&loaded);
    let amount_in = 1_250_000u64;

    let sdk_quote = loaded
        .adapter
        .quote(&QuoteParams {
            amount: amount_in,
            input_mint: cb_btc_mint,
            output_mint: MAINNET_USDC_MINT,
            swap_mode: SwapMode::ExactIn,
        })
        .unwrap();
    println!(
        "Mainnet SDK quote: in={}, out={}, fee={}",
        sdk_quote.in_amount, sdk_quote.out_amount, sdk_quote.fee_amount
    );

    let sim_result = simulate_legacy_swap(
        &rpc,
        &loaded,
        cb_btc_mint,
        MAINNET_USDC_MINT,
        amount_in,
        0,
        true,
        amount_in.saturating_mul(2),
    )
    .unwrap();
    println!(
        "Mainnet LiteSVM out={}, diff (sdk_math - litesvm)={}",
        sim_result.destination_amount,
        sdk_quote.out_amount.saturating_sub(sim_result.destination_amount)
    );

    assert_amounts_close(
        "mainnet exact-in output amount",
        sdk_quote.out_amount,
        sim_result.destination_amount,
    );
}

#[test]
#[ignore]
fn litesvm_vs_sdk_mainnet_cbbtc_usdc_exact_out() {
    if BYREAL_CLMM_PROGRAM != MAINNET_PROGRAM {
        println!("Skipping mainnet pool LiteSVM test: compile with --features \"mainnet with-litesvm\"");
        return;
    }

    let rpc = RpcClient::new("https://api.mainnet-beta.solana.com");
    let loaded = load_pool(&rpc, MAINNET_CBBTC_USDC_POOL, MAINNET_PROGRAM).unwrap();
    let cb_btc_mint = cbbtc_mint_from_pool(&loaded);
    let desired_out = 500_000_000u64;

    let sdk_quote = loaded
        .adapter
        .quote(&QuoteParams {
            amount: desired_out,
            input_mint: cb_btc_mint,
            output_mint: MAINNET_USDC_MINT,
            swap_mode: SwapMode::ExactOut,
        })
        .unwrap();
    println!(
        "Mainnet SDK exact-out quote: in={}, out={}, fee={}",
        sdk_quote.in_amount, sdk_quote.out_amount, sdk_quote.fee_amount
    );

    let sim_result = simulate_legacy_swap(
        &rpc,
        &loaded,
        cb_btc_mint,
        MAINNET_USDC_MINT,
        desired_out,
        sdk_quote.in_amount,
        false,
        sdk_quote.in_amount.saturating_mul(2),
    )
    .unwrap();
    println!(
        "Mainnet LiteSVM exact-out in={}, out={}, diff (sdk_in - litesvm_in)={}",
        sim_result.source_debit,
        sim_result.destination_amount,
        sdk_quote.in_amount.saturating_sub(sim_result.source_debit)
    );

    assert_amounts_close(
        "mainnet exact-out input amount",
        sdk_quote.in_amount,
        sim_result.source_debit,
    );
    assert_eq!(sim_result.destination_amount, desired_out);
}

#[cfg(feature = "dynamic-pool")]
#[test]
#[ignore]
fn litesvm_vs_sdk_dynamic_jup_usdc_exact_in() {
    let _guard = LIVE_RPC_TEST_LOCK.lock().unwrap();
    if BYREAL_CLMM_PROGRAM != LEGACY_PROGRAM {
        println!("Skipping dynamic pool LiteSVM test: compile with --features \"devnet dynamic-pool with-litesvm\"");
        return;
    }

    let rpc = RpcClient::new("https://api.mainnet-beta.solana.com");
    let loaded = load_pool(&rpc, DYNAMIC_JUP_USDC_POOL, LEGACY_PROGRAM).unwrap();
    assert!(loaded.common_amm.pool_state.is_swap_dynamic_fee_enabled());
    let amount_in = 1_000_000u64;

    let sdk_quote = loaded
        .adapter
        .quote(&QuoteParams {
            amount: amount_in,
            input_mint: MAINNET_USDC_MINT,
            output_mint: JUP_MINT,
            swap_mode: SwapMode::ExactIn,
        })
        .unwrap();
    println!(
        "Dynamic SDK quote: in={}, out={}, fee={}",
        sdk_quote.in_amount, sdk_quote.out_amount, sdk_quote.fee_amount
    );

    let sim_result = simulate_swap_v3_dyn(
        &rpc,
        &loaded,
        MAINNET_USDC_MINT,
        JUP_MINT,
        amount_in,
        0,
        true,
        amount_in.saturating_mul(2),
    )
    .unwrap();
    println!(
        "Dynamic LiteSVM out={}, diff (sdk_math - litesvm)={}",
        sim_result.destination_amount,
        sdk_quote.out_amount.saturating_sub(sim_result.destination_amount)
    );

    assert_amounts_close(
        "dynamic exact-in output amount",
        sdk_quote.out_amount,
        sim_result.destination_amount,
    );
}

#[cfg(feature = "dynamic-pool")]
#[test]
#[ignore]
fn litesvm_vs_sdk_dynamic_jup_usdc_exact_out() {
    let _guard = LIVE_RPC_TEST_LOCK.lock().unwrap();
    if BYREAL_CLMM_PROGRAM != LEGACY_PROGRAM {
        println!("Skipping dynamic pool LiteSVM test: compile with --features \"devnet dynamic-pool with-litesvm\"");
        return;
    }

    let rpc = RpcClient::new("https://api.mainnet-beta.solana.com");
    let loaded = load_pool(&rpc, DYNAMIC_JUP_USDC_POOL, LEGACY_PROGRAM).unwrap();
    assert!(loaded.common_amm.pool_state.is_swap_dynamic_fee_enabled());
    let desired_out = 1_000_000u64;

    let sdk_quote = loaded
        .adapter
        .quote(&QuoteParams {
            amount: desired_out,
            input_mint: MAINNET_USDC_MINT,
            output_mint: JUP_MINT,
            swap_mode: SwapMode::ExactOut,
        })
        .unwrap();
    println!(
        "Dynamic SDK exact-out quote: in={}, out={}, fee={}",
        sdk_quote.in_amount, sdk_quote.out_amount, sdk_quote.fee_amount
    );

    let sim_result = simulate_swap_v3_dyn(
        &rpc,
        &loaded,
        MAINNET_USDC_MINT,
        JUP_MINT,
        desired_out,
        sdk_quote.in_amount,
        false,
        sdk_quote.in_amount.saturating_mul(2),
    )
    .unwrap();
    println!(
        "Dynamic LiteSVM exact-out in={}, out={}, diff (sdk_in - litesvm_in)={}",
        sim_result.source_debit,
        sim_result.destination_amount,
        sdk_quote.in_amount.saturating_sub(sim_result.source_debit)
    );

    assert_amounts_close(
        "dynamic exact-out input amount",
        sdk_quote.in_amount,
        sim_result.source_debit,
    );
    assert_eq!(sim_result.destination_amount, desired_out);
}

#[cfg(feature = "dynamic-pool")]
#[test]
#[ignore]
fn litesvm_vs_sdk_dynamic_jup_usdc_reverse_exact_in() {
    let _guard = LIVE_RPC_TEST_LOCK.lock().unwrap();
    if BYREAL_CLMM_PROGRAM != LEGACY_PROGRAM {
        println!("Skipping dynamic pool LiteSVM test: compile with --features \"devnet dynamic-pool with-litesvm\"");
        return;
    }

    let rpc = RpcClient::new("https://api.mainnet-beta.solana.com");
    let loaded = load_pool(&rpc, DYNAMIC_JUP_USDC_POOL, LEGACY_PROGRAM).unwrap();
    assert!(loaded.common_amm.pool_state.is_swap_dynamic_fee_enabled());
    let amount_in = 1_000_000u64;

    let sdk_quote = loaded
        .adapter
        .quote(&QuoteParams {
            amount: amount_in,
            input_mint: JUP_MINT,
            output_mint: MAINNET_USDC_MINT,
            swap_mode: SwapMode::ExactIn,
        })
        .unwrap();
    println!(
        "Dynamic reverse SDK quote: in={}, out={}, fee={}",
        sdk_quote.in_amount, sdk_quote.out_amount, sdk_quote.fee_amount
    );

    let sim_result = simulate_swap_v3_dyn(
        &rpc,
        &loaded,
        JUP_MINT,
        MAINNET_USDC_MINT,
        amount_in,
        0,
        true,
        amount_in.saturating_mul(2),
    )
    .unwrap();
    println!(
        "Dynamic reverse LiteSVM out={}, diff (sdk_math - litesvm)={}",
        sim_result.destination_amount,
        sdk_quote.out_amount.saturating_sub(sim_result.destination_amount)
    );

    assert_amounts_close(
        "dynamic reverse exact-in output amount",
        sdk_quote.out_amount,
        sim_result.destination_amount,
    );
}

#[cfg(feature = "dynamic-pool")]
#[test]
#[ignore]
fn litesvm_vs_sdk_dynamic_jup_usdc_reverse_exact_out() {
    let _guard = LIVE_RPC_TEST_LOCK.lock().unwrap();
    if BYREAL_CLMM_PROGRAM != LEGACY_PROGRAM {
        println!("Skipping dynamic pool LiteSVM test: compile with --features \"devnet dynamic-pool with-litesvm\"");
        return;
    }

    let rpc = RpcClient::new("https://api.mainnet-beta.solana.com");
    let loaded = load_pool(&rpc, DYNAMIC_JUP_USDC_POOL, LEGACY_PROGRAM).unwrap();
    assert!(loaded.common_amm.pool_state.is_swap_dynamic_fee_enabled());
    let desired_out = 1_000_000u64;

    let sdk_quote = loaded
        .adapter
        .quote(&QuoteParams {
            amount: desired_out,
            input_mint: JUP_MINT,
            output_mint: MAINNET_USDC_MINT,
            swap_mode: SwapMode::ExactOut,
        })
        .unwrap();
    println!(
        "Dynamic reverse SDK exact-out quote: in={}, out={}, fee={}",
        sdk_quote.in_amount, sdk_quote.out_amount, sdk_quote.fee_amount
    );

    let sim_result = simulate_swap_v3_dyn(
        &rpc,
        &loaded,
        JUP_MINT,
        MAINNET_USDC_MINT,
        desired_out,
        sdk_quote.in_amount,
        false,
        sdk_quote.in_amount.saturating_mul(2),
    )
    .unwrap();
    println!(
        "Dynamic reverse LiteSVM exact-out in={}, out={}, diff (sdk_in - litesvm_in)={}",
        sim_result.source_debit,
        sim_result.destination_amount,
        sdk_quote.in_amount.saturating_sub(sim_result.source_debit)
    );

    assert_amounts_close(
        "dynamic reverse exact-out input amount",
        sdk_quote.in_amount,
        sim_result.source_debit,
    );
    assert_eq!(sim_result.destination_amount, desired_out);
}

fn load_legacy_pool(rpc: &RpcClient) -> Result<LoadedPool> {
    load_pool(rpc, LEGACY_POOL, LEGACY_PROGRAM)
}

fn load_pool(rpc: &RpcClient, pool_address: Pubkey, expected_program: Pubkey) -> Result<LoadedPool> {
    let pool_account = rpc.get_account(&pool_address)?;
    ensure!(
        pool_account.owner == expected_program,
        "pool owner mismatch: expected {}, got {}",
        expected_program,
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
    let mut account_map: AccountMap = accounts_to_update
        .iter()
        .enumerate()
        .filter_map(|(i, key)| accounts[i].as_ref().map(|account| (*key, account.clone().into())))
        .collect();
    let bitmap_key = TickArrayBitmapExtension::key(pool_address);
    let bitmap_extension = account_map
        .get(&bitmap_key)
        .and_then(|account| TickArrayBitmapExtension::try_deserialize(&mut account.data.as_ref()).ok());

    let temp_amm = ByrealClmmAmm {
        key: pool_address,
        pool_state,
        amm_config: AmmConfig::default(),
        bitmap_extension,
        max_one_side_tick_arrays: 3,
        dynamic_tick_arrays: HashMap::new(),
        token0_vault_amount: 0,
        token1_vault_amount: 0,
        token0_pyth_price: None,
        token1_pyth_price: None,
        token0_transfer_fee_config: None,
        token1_transfer_fee_config: None,
    };
    let mut full_tick_addrs = HashSet::new();
    for &dir in &[true, false] {
        if let Ok((_, mut start)) = temp_amm
            .pool_state
            .get_first_initialized_tick_array(&temp_amm.bitmap_extension, dir)
        {
            loop {
                full_tick_addrs.insert(temp_amm.get_tick_array_address(start));
                match temp_amm.pool_state.next_initialized_tick_array_start_index(
                    &temp_amm.bitmap_extension,
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

    for address in [
        pool_state.token_vault_0,
        pool_state.token_vault_1,
        pool_state.observation_key,
    ] {
        if !account_map.contains_key(&address) {
            account_map.insert(address, rpc.get_account(&address)?.into());
        }
    }
    adapter.update(&account_map)?;

    let amm_config = AmmConfig::try_deserialize(&mut account_map[&pool_state.amm_config].data.as_ref())?;
    let bitmap_extension = account_map
        .get(&bitmap_key)
        .and_then(|account| TickArrayBitmapExtension::try_deserialize(&mut account.data.as_ref()).ok());

    let dynamic_tick_arrays = account_map
        .iter()
        .filter_map(|(address, account)| {
            if account.owner != BYREAL_CLMM_PROGRAM {
                return None;
            }
            DynamicTickArrayState::from_bytes(&account.data).map(|tick_array| (*address, tick_array))
        })
        .collect::<HashMap<_, _>>();

    let common_amm = ByrealClmmAmm {
        key: pool_address,
        pool_state,
        amm_config,
        bitmap_extension,
        max_one_side_tick_arrays: 3,
        dynamic_tick_arrays,
        token0_vault_amount: 0,
        token1_vault_amount: 0,
        token0_pyth_price: None,
        token1_pyth_price: None,
        token0_transfer_fee_config: None,
        token1_transfer_fee_config: None,
    };

    Ok(LoadedPool {
        pool_address,
        adapter,
        common_amm,
        account_map,
        swap_timestamp,
    })
}

fn cbbtc_mint_from_pool(loaded: &LoadedPool) -> Pubkey {
    let pool_state = &loaded.common_amm.pool_state;
    let cb_btc_mint = if pool_state.token_mint_0 == MAINNET_USDC_MINT {
        pool_state.token_mint_1
    } else if pool_state.token_mint_1 == MAINNET_USDC_MINT {
        pool_state.token_mint_0
    } else {
        panic!(
            "Pool {} is not a USDC-cbBTC pool (mints: {}, {})",
            loaded.pool_address, pool_state.token_mint_0, pool_state.token_mint_1
        );
    };
    assert_eq!(cb_btc_mint, MAINNET_CBBTC_MINT);
    cb_btc_mint
}

struct SimResult {
    source_debit: u64,
    destination_amount: u64,
}

fn simulate_legacy_swap(
    rpc: &RpcClient,
    loaded: &LoadedPool,
    input_mint: Pubkey,
    output_mint: Pubkey,
    amount: u64,
    other_amount_threshold: u64,
    is_base_input: bool,
    source_balance: u64,
) -> Result<SimResult> {
    let mut svm = LiteSVM::new()
        .with_sysvars()
        .with_builtins()
        .with_default_programs()
        .with_sigverify(false)
        .with_blockhash_check(false);

    let program_bytes = deployed_program_bytes(rpc, &BYREAL_CLMM_PROGRAM)?;
    let clmm_program = RawPubkey::new_from_array(BYREAL_CLMM_PROGRAM.to_bytes());
    svm.add_program(clmm_program, &program_bytes).unwrap();

    for (addr, acc) in loaded.account_map.iter() {
        svm.set_account(RawPubkey::new_from_array(addr.to_bytes()), to_raw_account(acc))
            .unwrap();
    }

    let user = Pubkey::new_unique();
    let source_token_account = Pubkey::new_unique();
    let destination_token_account = Pubkey::new_unique();
    for (address, mint, token_amount) in [
        (source_token_account, input_mint, source_balance),
        (destination_token_account, output_mint, 0),
    ] {
        svm.set_account(
            RawPubkey::new_from_array(address.to_bytes()),
            RawAccount {
                lamports: 1_000_000_000,
                data: spl_token_account_data(mint, user, token_amount),
                owner: RawPubkey::new_from_array(anchor_spl::token::ID.to_bytes()),
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    }
    let user_raw = RawPubkey::new_from_array(user.to_bytes());
    svm.airdrop(&user_raw, 1_000_000_000).unwrap();

    let mut clock_sysvar: RawClock = svm.get_sysvar();
    clock_sysvar.unix_timestamp = loaded.swap_timestamp;
    svm.set_sysvar(&clock_sysvar);

    let zero_for_one = input_mint == loaded.common_amm.pool_state.token_mint_0;
    let (input_vault, output_vault) = if zero_for_one {
        (
            loaded.common_amm.pool_state.token_vault_0,
            loaded.common_amm.pool_state.token_vault_1,
        )
    } else {
        (
            loaded.common_amm.pool_state.token_vault_1,
            loaded.common_amm.pool_state.token_vault_0,
        )
    };

    let ordered_tick_arrays = loaded
        .common_amm
        .get_swap_tick_arrays(zero_for_one)
        .into_iter()
        .filter(|addr| loaded.account_map.contains_key(addr))
        .collect::<Vec<_>>();
    ensure!(
        !ordered_tick_arrays.is_empty(),
        "No tick array accounts available for legacy swap"
    );

    let mut data = vec![248u8, 198, 158, 145, 225, 117, 135, 200];
    data.extend(serialize_swap_ix_args(&SwapIxArgs {
        amount,
        other_amount_threshold,
        sqrt_price_limit_x64: 0,
        is_base_input,
    }));

    let mut accounts = vec![
        raw_meta(user, true, false),
        raw_meta(loaded.common_amm.pool_state.amm_config, false, false),
        raw_meta(loaded.pool_address, false, true),
        raw_meta(source_token_account, false, true),
        raw_meta(destination_token_account, false, true),
        raw_meta(input_vault, false, true),
        raw_meta(output_vault, false, true),
        raw_meta(loaded.common_amm.pool_state.observation_key, false, true),
        raw_meta(anchor_spl::token::ID, false, false),
        raw_meta(ordered_tick_arrays[0], false, true),
    ];

    let bitmap_key = TickArrayBitmapExtension::key(loaded.pool_address);
    if loaded.account_map.contains_key(&bitmap_key) {
        accounts.push(raw_meta(bitmap_key, false, false));
    }
    for addr in ordered_tick_arrays.iter().skip(1) {
        accounts.push(raw_meta(*addr, false, true));
    }

    let raw_ix = RawInstruction {
        program_id: clmm_program,
        accounts,
        data,
    };
    let tx = RawTransaction::new_unsigned(RawMessage::new(&[raw_ix], Some(&user_raw)));
    let sim = svm
        .simulate_transaction(tx)
        .map_err(|e| anyhow!("legacy LiteSVM simulate_transaction failed: {e:?}"))?;

    let source_post = post_token_amount(&sim.post_accounts, source_token_account)?;
    let destination_post = post_token_amount(&sim.post_accounts, destination_token_account)?;
    Ok(SimResult {
        source_debit: source_balance.saturating_sub(source_post),
        destination_amount: destination_post,
    })
}

#[cfg(feature = "dynamic-pool")]
fn simulate_swap_v3_dyn(
    rpc: &RpcClient,
    loaded: &LoadedPool,
    input_mint: Pubkey,
    output_mint: Pubkey,
    amount: u64,
    other_amount_threshold: u64,
    is_base_input: bool,
    source_balance: u64,
) -> Result<SimResult> {
    let mut svm = LiteSVM::new()
        .with_sysvars()
        .with_builtins()
        .with_default_programs()
        .with_sigverify(false)
        .with_blockhash_check(false);

    let program_bytes = deployed_program_bytes(rpc, &BYREAL_CLMM_PROGRAM)?;
    let clmm_program = RawPubkey::new_from_array(BYREAL_CLMM_PROGRAM.to_bytes());
    svm.add_program(clmm_program, &program_bytes).unwrap();

    for (addr, acc) in loaded.account_map.iter() {
        svm.set_account(RawPubkey::new_from_array(addr.to_bytes()), to_raw_account(acc))
            .unwrap();
    }
    for address in [input_mint, output_mint] {
        if !loaded.account_map.contains_key(&address) {
            let account = rpc.get_account(&address)?;
            svm.set_account(RawPubkey::new_from_array(address.to_bytes()), to_raw_account(&account))
                .unwrap();
        }
    }

    let user = Pubkey::new_unique();
    let source_token_account = Pubkey::new_unique();
    let destination_token_account = Pubkey::new_unique();
    for (address, mint, token_amount) in [
        (source_token_account, input_mint, source_balance),
        (destination_token_account, output_mint, 0),
    ] {
        svm.set_account(
            RawPubkey::new_from_array(address.to_bytes()),
            RawAccount {
                lamports: 1_000_000_000,
                data: spl_token_account_data(mint, user, token_amount),
                owner: RawPubkey::new_from_array(anchor_spl::token::ID.to_bytes()),
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    }
    let user_raw = RawPubkey::new_from_array(user.to_bytes());
    svm.airdrop(&user_raw, 1_000_000_000).unwrap();

    let mut clock_sysvar: RawClock = svm.get_sysvar();
    clock_sysvar.unix_timestamp = loaded.swap_timestamp;
    svm.set_sysvar(&clock_sysvar);

    let swap = loaded.adapter.get_swap_and_account_metas(&SwapParams {
        source_mint: input_mint,
        destination_mint: output_mint,
        source_token_account,
        destination_token_account,
        token_transfer_authority: user,
        quote_mint_to_referrer: None,
        jupiter_program_id: &Pubkey::new_unique(),
        in_amount: if is_base_input { amount } else { other_amount_threshold },
        out_amount: if is_base_input { other_amount_threshold } else { amount },
        missing_dynamic_accounts_as_default: false,
        swap_mode: if is_base_input {
            SwapMode::ExactIn
        } else {
            SwapMode::ExactOut
        },
    })?;
    assert_eq!(swap.swap, Swap::RaydiumClmmV2);

    let mut data = swap_v3_dyn_discriminator().to_vec();
    data.extend(serialize_swap_ix_args(&SwapIxArgs {
        amount,
        other_amount_threshold,
        sqrt_price_limit_x64: 0,
        is_base_input,
    }));

    let raw_ix = RawInstruction {
        program_id: clmm_program,
        accounts: swap
            .account_metas
            .iter()
            .map(raw_meta_from_sdk)
            .collect::<Vec<_>>(),
        data,
    };
    let tx = RawTransaction::new_unsigned(RawMessage::new(&[raw_ix], Some(&user_raw)));
    let sim = svm
        .simulate_transaction(tx)
        .map_err(|e| anyhow!("dynamic LiteSVM simulate_transaction failed: {e:?}"))?;

    let source_post = post_token_amount(&sim.post_accounts, source_token_account)?;
    let destination_post = post_token_amount(&sim.post_accounts, destination_token_account)?;
    Ok(SimResult {
        source_debit: source_balance.saturating_sub(source_post),
        destination_amount: destination_post,
    })
}

fn to_raw_account(account: &SdkAccount) -> RawAccount {
    RawAccount {
        lamports: account.lamports,
        data: account.data.clone(),
        owner: RawPubkey::new_from_array(account.owner.to_bytes()),
        executable: account.executable,
        rent_epoch: account.rent_epoch,
    }
}

fn serialize_swap_ix_args(args: &SwapIxArgs) -> Vec<u8> {
    let mut data = Vec::with_capacity(8 + 8 + 16 + 1);
    data.extend_from_slice(&args.amount.to_le_bytes());
    data.extend_from_slice(&args.other_amount_threshold.to_le_bytes());
    data.extend_from_slice(&args.sqrt_price_limit_x64.to_le_bytes());
    data.push(args.is_base_input as u8);
    data
}

fn raw_meta(pubkey: Pubkey, is_signer: bool, is_writable: bool) -> RawAccountMeta {
    RawAccountMeta {
        pubkey: RawPubkey::new_from_array(pubkey.to_bytes()),
        is_signer,
        is_writable,
    }
}

#[cfg(feature = "dynamic-pool")]
fn raw_meta_from_sdk(meta: &solana_sdk::instruction::AccountMeta) -> RawAccountMeta {
    RawAccountMeta {
        pubkey: RawPubkey::new_from_array(meta.pubkey.to_bytes()),
        is_signer: meta.is_signer,
        is_writable: meta.is_writable,
    }
}

#[cfg(feature = "dynamic-pool")]
fn swap_v3_dyn_discriminator() -> [u8; 8] {
    let hash = solana_program::hash::hash(b"global:swap_v3_dyn").to_bytes();
    hash[..8].try_into().unwrap()
}

fn post_token_amount(
    post_accounts: &[(RawPubkey, solana_account::AccountSharedData)],
    address: Pubkey,
) -> Result<u64> {
    let raw_address = RawPubkey::new_from_array(address.to_bytes());
    for (pubkey, account) in post_accounts {
        if *pubkey != raw_address {
            continue;
        }
        let token_account = spl_token::state::Account::unpack(account.data())?;
        return Ok(token_account.amount);
    }
    Err(anyhow!("post token account {address} not found"))
}

fn assert_amounts_close(label: &str, expected: u64, actual: u64) {
    let diff = expected.abs_diff(actual);
    let rel_ppm = if expected == 0 {
        0
    } else {
        (u128::from(diff) * 1_000_000u128) / u128::from(expected)
    };
    let within_abs = diff <= 10;
    let within_rel = rel_ppm <= 50;
    assert!(
        within_abs || within_rel,
        "{label} mismatch: expected={expected}, actual={actual}, diff={diff}, rel_ppm={rel_ppm}"
    );
}

fn spl_token_account_data(mint: Pubkey, owner: Pubkey, amount: u64) -> Vec<u8> {
    let mut token_account = spl_token::state::Account::default();
    token_account.mint = mint;
    token_account.owner = owner;
    token_account.amount = amount;
    token_account.state = spl_token::state::AccountState::Initialized;
    let mut data = vec![0u8; spl_token::state::Account::LEN];
    spl_token::state::Account::pack(token_account, &mut data).unwrap();
    data
}

fn decode_upgradeable_loader_state_prefix(
    data: &[u8],
) -> Result<(
    solana_program::bpf_loader_upgradeable::UpgradeableLoaderState,
    usize,
)> {
    let mut cursor = std::io::Cursor::new(data);
    let state: solana_program::bpf_loader_upgradeable::UpgradeableLoaderState =
        bincode::deserialize_from(&mut cursor)?;
    Ok((state, cursor.position() as usize))
}

fn deployed_program_bytes(rpc: &RpcClient, program_id: &Pubkey) -> Result<Vec<u8>> {
    let program_account = rpc.get_account(program_id)?;

    if program_account.owner == solana_program::bpf_loader::id() {
        return Ok(program_account.data);
    }

    ensure!(
        program_account.owner == solana_program::bpf_loader_upgradeable::id(),
        "program {} is not owned by a supported BPF loader: {}",
        program_id,
        program_account.owner
    );
    let (program_state, _) = decode_upgradeable_loader_state_prefix(&program_account.data)?;
    let programdata_address = match program_state {
        solana_program::bpf_loader_upgradeable::UpgradeableLoaderState::Program {
            programdata_address,
        } => programdata_address,
        other => {
            return Err(anyhow!(
                "account {} is not an upgradeable program account: {:?}",
                program_id,
                other
            ))
        }
    };

    let programdata_account = rpc.get_account(&programdata_address)?;
    ensure!(
        programdata_account.owner == solana_program::bpf_loader_upgradeable::id(),
        "programdata {} has unexpected owner {}",
        programdata_address,
        programdata_account.owner
    );
    let (programdata_state, metadata_len) =
        decode_upgradeable_loader_state_prefix(&programdata_account.data)?;
    ensure!(
        programdata_account.data.len() > metadata_len,
        "programdata {} has no program bytes",
        programdata_address
    );
    match programdata_state {
        solana_program::bpf_loader_upgradeable::UpgradeableLoaderState::ProgramData { .. } => {
            Ok(programdata_account.data[metadata_len..].to_vec())
        }
        other => Err(anyhow!(
            "account {} is not a programdata account: {:?}",
            programdata_address,
            other
        )),
    }
}

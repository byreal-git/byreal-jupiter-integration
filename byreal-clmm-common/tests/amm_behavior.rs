use anchor_lang::Discriminator;
use byreal_clmm_common::{
    tick_math, AmmConfig, ByrealClmmAmm, DynTickArrayState, DynamicTickArrayState, PoolState,
    TickArrayBitmapExtension, TickState,
};
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;

fn empty_amm(pool_key: Pubkey, pool_state: PoolState) -> ByrealClmmAmm {
    ByrealClmmAmm {
        key: pool_key,
        pool_state,
        amm_config: AmmConfig::default(),
        dynamic_tick_arrays: HashMap::new(),
        max_one_side_tick_arrays: 3,
        bitmap_extension: None,
        token0_vault_amount: 0,
        token1_vault_amount: 0,
        token0_pyth_price: None,
        token1_pyth_price: None,
        token0_transfer_fee_config: None,
        token1_transfer_fee_config: None,
    }
}

#[test]
fn tick_array_address_calculation() {
    let pool_key = Pubkey::new_unique();
    let amm = empty_amm(pool_key, PoolState::default());

    let tick_array_addr = amm.get_tick_array_address(0);
    assert_ne!(tick_array_addr, Pubkey::default());
    assert_ne!(amm.get_tick_array_address(100), amm.get_tick_array_address(200));
}

#[test]
fn swap_direction_tick_arrays() {
    let pool_key = Pubkey::new_unique();
    let mut pool_state = PoolState::default();
    pool_state.tick_current = 1000;
    pool_state.tick_spacing = 10;
    let amm = empty_amm(pool_key, pool_state);

    let arrays_down = amm.get_swap_tick_arrays(true);
    let arrays_up = amm.get_swap_tick_arrays(false);

    assert_eq!(arrays_down.len(), 3);
    assert_eq!(arrays_up.len(), 3);
    assert_ne!(arrays_down[1], arrays_up[1]);
}

#[test]
fn decay_fee_calculation() {
    let pool_key = Pubkey::new_unique();
    let mut pool_state = PoolState::default();
    pool_state.tick_current = 1000;
    pool_state.tick_spacing = 10;
    pool_state.open_time = 0;
    pool_state.decay_fee_flag = 0b111;
    pool_state.decay_fee_init_fee_rate = 80;
    pool_state.decay_fee_decrease_rate = 10;
    pool_state.decay_fee_decrease_interval = 10;
    let amm = empty_amm(pool_key, pool_state);

    assert!(amm.is_decay_fee_enabled());
    assert!(amm.is_decay_fee_on_sell_mint0());
    assert!(amm.is_decay_fee_on_sell_mint1());
    assert_eq!(amm.get_decay_fee_rate(0), Some(800_000));
    assert_eq!(amm.get_decay_fee_rate(9), Some(800_000));
    assert_eq!(amm.get_decay_fee_rate(10), Some(720_000));
    assert_eq!(amm.get_decay_fee_rate(19), Some(720_000));
    assert_eq!(amm.get_decay_fee_rate(20), Some(648_000));
    assert_eq!(amm.get_decay_fee_rate(30), Some(583_200));
    assert!(amm.get_decay_fee_rate(1000).is_some_and(|rate| rate < 100));
}

#[test]
fn decay_fee_disabled() {
    let mut pool_state = PoolState::default();
    pool_state.decay_fee_flag = 0;
    let amm = empty_amm(Pubkey::new_unique(), pool_state);

    assert!(!amm.is_decay_fee_enabled());
    assert_eq!(amm.get_decay_fee_rate(100), Some(0));
}

#[test]
fn decay_fee_before_open_time() {
    let mut pool_state = PoolState::default();
    pool_state.open_time = 1000;
    pool_state.decay_fee_flag = 0b111;
    pool_state.decay_fee_init_fee_rate = 50;
    pool_state.decay_fee_decrease_interval = 10;
    let amm = empty_amm(Pubkey::new_unique(), pool_state);

    assert_eq!(amm.get_decay_fee_rate(999), Some(0));
    assert_eq!(amm.get_decay_fee_rate(1000), Some(500_000));
}

#[test]
fn decode_dyn_tick_array_and_next_tick() {
    fn build_dyn_bytes(start: i32, spacing: u16, offsets: &[usize]) -> Vec<u8> {
        let mut header = DynTickArrayState::default();
        header.start_tick_index = start;
        header.alloc_tick_count = offsets.len() as u8;
        for (i, off) in offsets.iter().enumerate() {
            header.tick_offset_index[*off] = (i as u8) + 1;
        }
        let mut ticks = Vec::with_capacity(offsets.len());
        for off in offsets.iter() {
            let mut tick = TickState::default();
            tick.tick = start + (*off as i32) * i32::from(spacing);
            tick.liquidity_gross = 1;
            ticks.push(tick);
        }
        let mut data = Vec::new();
        data.extend_from_slice(&DynTickArrayState::DISCRIMINATOR);
        data.extend_from_slice(bytemuck::bytes_of(&header));
        data.extend_from_slice(bytemuck::cast_slice(&ticks));
        data
    }

    let spacing = 10;
    let start = 600;
    let bytes = build_dyn_bytes(start, spacing, &[0, 2, 5]);

    let (header, ticks) = match DynamicTickArrayState::from_bytes(&bytes).unwrap() {
        DynamicTickArrayState::Dynamic(inner) => inner,
        DynamicTickArrayState::Fixed(_) => panic!("expected dynamic tick array"),
    };

    let header_start = header.start_tick_index;
    assert_eq!(header_start, start);
    assert_eq!(header.alloc_tick_count, 3);
    assert_eq!(ticks.len(), 3);
    let tick0 = ticks[0].tick;
    let tick1 = ticks[1].tick;
    let tick2 = ticks[2].tick;
    assert_eq!(tick0, start);
    assert_eq!(tick1, start + 2 * i32::from(spacing));
    assert_eq!(tick2, start + 5 * i32::from(spacing));

    let current_tick_down = start + 5 * i32::from(spacing) + 1;
    let idx_down = header
        .next_initialized_tick_index(&ticks, current_tick_down, spacing, true)
        .unwrap()
        .unwrap();
    let tick_down = ticks[idx_down as usize].tick;
    assert_eq!(tick_down, start + 5 * i32::from(spacing));

    let idx_up = header.first_initialized_tick_index(&ticks, false).unwrap();
    let tick_up = ticks[idx_up as usize].tick;
    assert_eq!(tick_up, start);
}

#[test]
fn compute_swap_errors_when_not_enough_tick_arrays() {
    let mut pool_state = PoolState::default();
    pool_state.tick_spacing = 10;
    pool_state.tick_current = 0;
    pool_state.sqrt_price_x64 = tick_math::get_sqrt_price_at_tick(0).unwrap();
    pool_state.liquidity = 1_000_000u128;

    let mut amm = empty_amm(Pubkey::new_unique(), pool_state);
    amm.bitmap_extension = Some(TickArrayBitmapExtension::default());

    assert!(amm.compute_swap(true, 1_000u64, true, None, 0, 0).is_err());
}

pub fn tick_to_sqrt_price_x64(tick: i32) -> u128 { let base = 1u128 << 64; if tick >= 0 { base.saturating_add(tick as u128 * 1_000) } else { base.saturating_sub((-tick) as u128 * 1_000) } }

pub fn constant_product(x: u128, y: u128) -> u128 { x.saturating_mul(y) }
pub fn quote_out(x: u128, y: u128, amount_in: u128, fee_bps: u128) -> u128 { let net = amount_in.saturating_mul(10_000 - fee_bps) / 10_000; y.saturating_sub(constant_product(x, y) / x.saturating_add(net).max(1)) }

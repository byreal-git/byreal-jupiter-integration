# Jupiter Byreal Integration

This module implements the `Amm` trait defined [here](https://github.com/jup-ag/rust-amm-implementation).

Enable dynamic-fee pools with `--features dynamic-pool`. Both nonzero Pyth feed IDs
require their shard 0 oracle accounts. If either feed ID is zero, or an oracle
price is older than 3600 seconds or non-positive, or the computed price index is zero, only the
arbitrage fee is skipped. Base, trade-slippage, and imbalance fees still apply.
Missing or invalid oracle accounts remain errors when both feeds are configured.

To test, simply run:

```
cargo test -- --nocapture
```

To compare dynamic-fee quotes and account metas with local CLMM execution, rebuild
the CLMM optional-Pyth branch with `./build.sh local`, then run:

```sh
BYREAL_CLMM_PROGRAM_SO=/path/to/solana-dex-clmm/target/deploy/byreal_clmm.so \
  cargo test -p byreal-clmm-jupiter-integration \
  --features "devnet dynamic-pool with-litesvm" --test litesvm_legacy_pool \
  litesvm_local_pyth_ -- --ignored --nocapture
```

These tests use local fixtures, execute `swap_v3_dyn` in LiteSVM, and compare
token balances and actual fees in both directions and swap modes. The binary
must implement the optional-Pyth policy; an unset binary path fails the tests.
Other LiteSVM tests continue to load the deployed program from RPC unless
`BYREAL_CLMM_PROGRAM_SO` is set. This does not exercise Jupiter CPI.

Example quote for selling 1 SOL for Test SPL against the Byreal SOL/Test SPL market:

```
Pool: J4jiEPEu8c8nLdpkiMa7k1P8rL1HCJSNxCvzA5DsmYds
Label: Byreal
Program: 45iBNkaENereLKMjLm2LHkF3hpDapf6mnvrM5HWFg9cY
Accounts to update: 31
Reserve mints: [So11111111111111111111111111111111111111112, 5W84C59fbWLnzhM6ZQpBSPes6cm6dZnQ6czax89bRB2Y]
Quote (exact in):
  Input: 100000 So11111111111111111111111111111111111111112
  Output: 60007 5W84C59fbWLnzhM6ZQpBSPes6cm6dZnQ6czax89bRB2Y
  Fee: 12
  Fee %: 100
Quote (exact out):
  Input: 100000 So11111111111111111111111111111111111111112
  Output: 60007 5W84C59fbWLnzhM6ZQpBSPes6cm6dZnQ6czax89bRB2Y
  Fee: 12
Swap accounts: 20

```

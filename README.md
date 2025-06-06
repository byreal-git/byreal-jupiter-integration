# Jupiter Byreal Integration

This module implements the `Amm` trait defined [here](https://github.com/jup-ag/rust-amm-implementation).

To test, simply run:

```
cargo test -- --nocapture
```

This will print out a quote for selling 1 SOL for Test SPL against the Byreal SOL/Test SPL market. Sample output:
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

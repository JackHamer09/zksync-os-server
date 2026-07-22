# Atomic-swap demo driver (v32.0 multi_chain)

Self-contained **Rust** driver for an all-or-nothing cross-chain **atomic swap**
between the two L1-settling chains in the `../` preset (A = 6565 @ :3050,
B = 6566 @ :3051).

The contract shapes are **artifact-sourced**: every `sol!` binding reads a
vendored ABI under [`abis/`](abis/) extracted from the compiled era-contracts
artifacts (`./refresh-abis.sh`). There are no hand-written ABIs, so a contract
change is caught at compile time instead of drifting silently. This mirrors the
`atomic_swap_l1_settled` integration test — same flow, same server RPCs.

See [../ATOMIC_SWAP.md](../ATOMIC_SWAP.md) for what the swap does step by step.

## Prerequisites

Bring up the preset first (from the repo root):

```bash
./run_local.sh ./local-chains/v32.0/multi_chain
# wait until :3050 / :3051 answer eth_chainId (0x19a5 / 0x19a6)
```

Then register the two chains for interop once per anvil session (see
[../ATOMIC_SWAP.md](../ATOMIC_SWAP.md) step 2). The driver does not register
chains itself.

## Run

```bash
cd local-chains/v32.0/multi_chain/atomic-swap
PRIVATE_KEY=0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110 \
L2_RPC_URL=http://127.0.0.1:3050 \
L2_RPC_URL_SECOND=http://127.0.0.1:3051 \
L1_RPC_URL=http://127.0.0.1:8545 \
  cargo run --release
```

The private key above is the standard ZKsync rich L2 account
(`0x36615Cf349d7F6344891B1e7CA7C72883F5dc049`), funded on both chains by the deploy.

## Environment variables

| Var                 | Meaning                                  | Default                  |
|---------------------|------------------------------------------|--------------------------|
| `PRIVATE_KEY`       | funded L2 key (source + dest)            | — (required)             |
| `L2_RPC_URL`        | chain A (source) RPC                     | `http://127.0.0.1:3050`  |
| `L2_RPC_URL_SECOND` | chain B (destination) RPC                | `http://127.0.0.1:3051`  |
| `L1_RPC_URL`        | L1 (anvil) RPC, for interop-root waits   | `http://127.0.0.1:8545`  |
| `ATOMIC_DEADLINE`   | flow deadline (SL **timestamp**, unix s) | `10000000000`            |
| `ATOMIC_INTEROP_CENTER` / `ATOMIC_INTEROP_HANDLER` / `ATOMIC_COMMITMENT_TREE` / `ATOMIC_FLOW_MANAGER` | atomic-layout overrides | canonical atomic addrs |

Expected success line: `SUCCESS: atomic swap completed end-to-end (both legs executed atomically).`
(both bundles reach `bundleStatus = 2` / FullyExecuted, both wrapped tokens mint).

## Refreshing the ABIs

When the era-contracts interop contracts change, rebuild them and re-vendor:

```bash
(cd /path/to/era-contracts/l1-contracts && forge build)
PROTOCOL_CONTRACTS_ROOT=/path/to/era-contracts ./refresh-abis.sh
cargo build   # fails here if a shape the driver uses changed — that is the point
```

## Layout

```
atomic-swap/
├── Cargo.toml              # standalone crate (its own workspace)
├── refresh-abis.sh         # re-vendor abis/*.json from era-contracts out/
├── abis/                   # artifact-sourced ABIs the sol! bindings read
│   ├── InteropCenter.json
│   ├── L2InteropHandler.json
│   ├── AtomicFlowManager.json
│   ├── L2NativeTokenVault.json
│   ├── L2InteropRootStorage.json
│   └── IERC7786Attributes.json
└── src/
    ├── main.rs             # the driver
    └── token_bytecode.rs   # SimpleERC20 creation bytecode (self-contained test token)
```

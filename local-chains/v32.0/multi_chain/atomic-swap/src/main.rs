//! Atomic-interop demo: a cross-chain ATOMIC SWAP across the v32.0 multi-chain stack, driven
//! natively in Rust (ported from the former `atomic-swap-3chains.ts`).
//!
//! Chain A sends token X to a user on chain B, and chain B sends token Y to the same user on
//! chain A. Both legs are bound into ONE atomic flow (the IMT bundle model): either both execute
//! or neither does. Atomicity is proven per-leg via each chain's on-chain `L2InteropCommitmentTree`
//! (an Indexed Merkle Tree), coordinated by `AtomicFlowManager` — there is NO L1 coordination of
//! the swap itself. The per-leg proofs come from the atomic server's RPCs
//! (`zks_getL2ToL1LogProof` messageRoot target, `zks_getImtInclusionProof`,
//! `zks_getImtLowNullifierIndex`), exactly as the `atomic_swap_l1_settled` integration test does.
//!
//! ── Contract shapes are ARTIFACT-SOURCED ──────────────────────────────────────────────────────
//! Every protocol binding below (`sol!(Name, "abis/Name.json")`) reads a vendored ABI extracted
//! from the compiled era-contracts artifacts. When the contracts change, rebuild era-contracts and
//! run `./refresh-abis.sh`; this driver then fails to COMPILE if a shape it uses changed, instead
//! of silently drifting the way the old hand-written TS ABIs did.
//!
//! ── Requirement: an atomic-capable server ─────────────────────────────────────────────────────
//! Needs a zksync-os-server that predeploys the atomic built-ins (`L2InteropCommitmentTree`
//! @0x10012, `AtomicFlowManager` @0x10014) with the atomic protocol layout (`InteropCenter`
//! @0x1000d, `InteropHandler` @0x1000e), from the `kl/l1-settled-interop-proof` server branch +
//! `atomic-imt-interop` era-contracts genesis. The driver detects this at startup and prints a
//! precise BLOCKED message rather than failing obscurely. The two chains must already be registered
//! with each other for interop (the multi-chain preset does this at genesis).
//!
//! Usage (defaults A @ :3050, B @ :3051, L1 @ :8545):
//!   PRIVATE_KEY=0x... \
//!   L2_RPC_URL=http://127.0.0.1:3050 L2_RPC_URL_SECOND=http://127.0.0.1:3051 \
//!   cargo run --release
//!
//! Optional env: L1_RPC_URL, ATOMIC_DEADLINE (SL timestamp, default 10_000_000_000),
//!   ATOMIC_INTEROP_CENTER / ATOMIC_INTEROP_HANDLER / ATOMIC_COMMITMENT_TREE /
//!   ATOMIC_FLOW_MANAGER (layout overrides if a published image differs).

// Several `sol!`-generated contract methods (and our multi-param leg helpers) exceed clippy's
// arg-count threshold; the shapes come from the contracts, so silence it crate-wide.
#![allow(clippy::too_many_arguments)]

use std::time::{Duration, Instant};

use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{address, keccak256, Address, Bytes, FixedBytes, B256, U256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::{SolCall, SolEvent, SolValue};
use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use tokio::time::sleep;

mod token_bytecode;

// ── Fixed L2 built-in addresses that never move across atomic images ──
const NATIVE_TOKEN_VAULT: Address = address!("0000000000000000000000000000000000010004");
const ASSET_ROUTER: Address = address!("0000000000000000000000000000000000010003");
const INTEROP_ROOT_STORAGE: Address = address!("0000000000000000000000000000000000010008");

// ── Layout defaults (overridable via env; see `resolve_layout`) ──
const DEFAULT_INTEROP_CENTER: Address = address!("000000000000000000000000000000000001000d");
const DEFAULT_INTEROP_HANDLER: Address = address!("000000000000000000000000000000000001000e");
const DEFAULT_COMMITMENT_TREE: Address = address!("0000000000000000000000000000000000010012");
const DEFAULT_FLOW_MANAGER: Address = address!("0000000000000000000000000000000000010014");

const TOKEN_DECIMALS: u32 = 18;
const ATOMIC_SEND_GAS: u64 = 3_000_000;
const TX_GAS: u64 = 5_000_000;

/// `LegState.Committed` (IAtomicInterop.sol).
const LEG_COMMITTED: u8 = 1;
/// `BundleStatus.FullyExecuted` (common/Messaging.sol).
const BUNDLE_FULLY_EXECUTED: u8 = 2;

// ── Artifact-sourced protocol bindings (regenerate via ./refresh-abis.sh) ──
sol!(
    #[sol(rpc)]
    InteropCenter,
    "abis/InteropCenter.json"
);
sol!(
    #[sol(rpc)]
    L2InteropHandler,
    "abis/L2InteropHandler.json"
);
sol!(
    #[sol(rpc)]
    AtomicFlowManager,
    "abis/AtomicFlowManager.json"
);
sol!(
    #[sol(rpc)]
    L2NativeTokenVault,
    "abis/L2NativeTokenVault.json"
);
sol!(
    #[sol(rpc)]
    L2InteropRootStorage,
    "abis/L2InteropRootStorage.json"
);
sol!(
    #[sol(rpc)]
    IERC7786Attributes,
    "abis/IERC7786Attributes.json"
);

// ── Inline: the ERC-20 standard (not a drifting protocol shape) + the two commitment-tree views
//    used purely for the startup capability gate. ──
sol! {
    #[sol(rpc)]
    interface IERC20 {
        function approve(address spender, uint256 amount) external returns (bool);
        function balanceOf(address account) external view returns (uint256);
    }
    #[sol(rpc)]
    interface IL2InteropCommitmentTree {
        function root() external view returns (bytes32);
        function leafCount() external view returns (uint256);
    }
}

/// Per-chain context: a wallet-bound L2 provider, the chain id, and its freshly-deployed test token.
struct ChainCtx {
    name: &'static str,
    chain_id: u64,
    provider: DynProvider,
    token: Address,
}

/// Env-overridable addresses of the atomic contracts.
struct Layout {
    interop_center: Address,
    interop_handler: Address,
    commitment_tree: Address,
    flow_manager: Address,
}

// ── RPC response shapes (zks_* extensions) ──
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLogProof {
    batch_number: Option<u64>,
    id: u64,
    proof: Vec<B256>,
    gateway_block_number: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RpcImtLeaf {
    value: U256,
    next_index: U256,
    next_value: U256,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RpcImtProof {
    chain_imt_root: B256,
    leaf: RpcImtLeaf,
    imt_leaf_index: u64,
    imt_proof: Vec<B256>,
}

fn require_env(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("missing env var {name}"))
}

fn env_addr(name: &str, default: Address) -> Result<Address> {
    match std::env::var(name) {
        Ok(v) => v.parse().with_context(|| format!("parse {name} as address")),
        Err(_) => Ok(default),
    }
}

fn resolve_layout() -> Result<Layout> {
    Ok(Layout {
        interop_center: env_addr("ATOMIC_INTEROP_CENTER", DEFAULT_INTEROP_CENTER)?,
        interop_handler: env_addr("ATOMIC_INTEROP_HANDLER", DEFAULT_INTEROP_HANDLER)?,
        commitment_tree: env_addr("ATOMIC_COMMITMENT_TREE", DEFAULT_COMMITMENT_TREE)?,
        flow_manager: env_addr("ATOMIC_FLOW_MANAGER", DEFAULT_FLOW_MANAGER)?,
    })
}

/// `keccak256(abi.encode(uint256 chainId, address NTV, address token))` — mirrors `DataEncoding.encodeNTVAssetId`.
fn ntv_asset_id(chain_id: u64, token: Address) -> B256 {
    keccak256((U256::from(chain_id), NATIVE_TOKEN_VAULT, token).abi_encode_params())
}

/// ERC-7930 EVM chain reference without an address component.
fn encode_evm_chain(chain_id: u64) -> Bytes {
    let be = chain_id.to_be_bytes();
    let first = be.iter().position(|&b| b != 0).unwrap_or(be.len() - 1);
    let chain_ref = &be[first..];
    let mut out = vec![0x00, 0x01, 0x00, 0x00, chain_ref.len() as u8];
    out.extend_from_slice(chain_ref);
    out.push(0x00);
    Bytes::from(out)
}

/// ERC-7930 EVM address without a chain reference.
fn encode_evm_address(addr: Address) -> Bytes {
    let mut out = vec![0x00, 0x01, 0x00, 0x00, 0x00, 0x14];
    out.extend_from_slice(addr.as_slice());
    Bytes::from(out)
}

/// `secondBridgeData` for an ERC20 transfer via the L2 asset router: `0x01 ++ abi.encode(bytes32 assetId, bytes burnData)`
/// where `burnData = abi.encode(uint256 amount, address receiver, address(0))`.
fn token_transfer_data(asset_id: B256, amount: U256, recipient: Address) -> Bytes {
    let burn_data = (amount, recipient, Address::ZERO).abi_encode_params();
    let mut out = vec![0x01u8];
    out.extend_from_slice(&(asset_id, Bytes::from(burn_data)).abi_encode_params());
    Bytes::from(out)
}

/// Indirect-call ERC-7786 attribute with zero call value.
fn indirect_call_attr() -> Bytes {
    Bytes::from(
        IERC7786Attributes::indirectCallCall {
            _indirectCallMessageValue: U256::ZERO,
        }
        .abi_encode(),
    )
}

/// `atomicBundle` ERC-7786 attribute carrying the out-of-band atomic params.
fn atomic_bundle_attr(flow_id: B256, deadline: u64, low_nullifier_index: U256) -> Bytes {
    Bytes::from(
        IERC7786Attributes::atomicBundleCall {
            _flowId: flow_id,
            _deadline: deadline,
            _lowNullifierIndex: low_nullifier_index,
        }
        .abi_encode(),
    )
}

/// `interopBundleSalt` ERC-7786 attribute. The InteropCenter folds `keccak256(msg.sender, salt)` into
/// the bundle hash and rejects a reused `(sender, salt)` pair, so every leg needs a distinct salt.
fn interop_bundle_salt_attr(salt: B256) -> Bytes {
    Bytes::from(IERC7786Attributes::interopBundleSaltCall { _salt: salt }.abi_encode())
}

/// The bridge call-starter that burns `amount` of `source`'s token and mints it to `recipient` on the destination.
fn bridge_call_starter(source: &ChainCtx, amount: U256, recipient: Address) -> InteropCenter::InteropCallStarter {
    InteropCenter::InteropCallStarter {
        to: encode_evm_address(ASSET_ROUTER),
        data: token_transfer_data(ntv_asset_id(source.chain_id, source.token), amount, recipient),
        callAttributes: vec![indirect_call_attr()],
    }
}

/// `flowId = keccak256(abi.encode(bytes32[] legBundleHashes, uint256[] legSourceChainIds, uint64 deadline, uint256 settlementLayerChainId))` (both arrays ascending).
fn compute_flow_id(
    leg_hashes_asc: &[B256],
    chain_ids_asc: &[U256],
    deadline: u64,
    settlement_layer_chain_id: U256,
) -> B256 {
    keccak256(
        (
            leg_hashes_asc.to_vec(),
            chain_ids_asc.to_vec(),
            deadline,
            settlement_layer_chain_id,
        )
            .abi_encode_params(),
    )
}

/// `commitValue = keccak256(abi.encode(bytes4 ATOMIC_COMMIT_LEAF_TAG, bytes32 flowId, bytes32 bundleHash))` as uint256.
fn commit_value(flow_id: B256, bundle_hash: B256) -> U256 {
    let tag = FixedBytes::<4>::from_slice(&keccak256(b"AtomicInterop.commit.v1")[..4]);
    U256::from_be_bytes(keccak256((tag, flow_id, bundle_hash).abi_encode_params()).0)
}

/// Build a wallet-bound L2 provider for `signer`.
async fn l2_provider(rpc: &str, signer: PrivateKeySigner) -> Result<DynProvider> {
    Ok(ProviderBuilder::new()
        .wallet(EthereumWallet::from(signer))
        .connect(rpc)
        .await
        .with_context(|| format!("connect L2 {rpc}"))?
        .erased())
}

/// Deploy the self-contained SimpleERC20 (ctor mints `initial_supply` to the deployer), returning its address.
async fn deploy_token(provider: &DynProvider, initial_supply: U256) -> Result<Address> {
    let mut init = hex::decode(token_bytecode::TOKEN_BYTECODE.trim_start_matches("0x"))
        .context("decode token bytecode")?;
    init.extend_from_slice(&(initial_supply,).abi_encode_params());
    let tx = TransactionRequest::default()
        .with_deploy_code(init)
        .with_gas_limit(TX_GAS);
    let receipt = provider.send_transaction(tx).await?.get_receipt().await?;
    ensure!(receipt.status(), "token deploy reverted");
    receipt
        .contract_address
        .context("no contract address in deploy receipt")
}

/// Deploy + register-with-NTV + approve a test token for `signer` on the chain at `rpc`.
async fn setup_token(
    name: &'static str,
    rpc: &str,
    signer: PrivateKeySigner,
    supply: U256,
    approve_amount: U256,
) -> Result<ChainCtx> {
    let provider = l2_provider(rpc, signer).await?;
    let chain_id = provider.get_chain_id().await?;
    let token = deploy_token(&provider, supply).await?;

    let ntv = L2NativeTokenVault::new(NATIVE_TOKEN_VAULT, &provider);
    if ntv.assetId(token).call().await? == B256::ZERO {
        let r = ntv
            .registerToken(token)
            .gas(TX_GAS)
            .send()
            .await?
            .get_receipt()
            .await?;
        ensure!(r.status(), "registerToken reverted");
    }
    let r = IERC20::new(token, &provider)
        .approve(NATIVE_TOKEN_VAULT, approve_amount)
        .gas(TX_GAS)
        .send()
        .await?
        .get_receipt()
        .await?;
    ensure!(r.status(), "approve reverted");
    println!("[{name}] chain {chain_id}: token {token} deployed / registered / approved");
    Ok(ChainCtx {
        name,
        chain_id,
        provider,
        token,
    })
}

/// Capability gate: the commitment tree must be deployed and behave like the atomic built-in.
async fn assert_atomic_capable(name: &str, provider: &DynProvider, layout: &Layout) -> Result<()> {
    let code = provider.get_code_at(layout.commitment_tree).await?;
    if code.is_empty() {
        bail!(
            "BLOCKED: chain {name} has no L2InteropCommitmentTree at {} — needs an atomic-capable \
             zksync-os-server image + genesis (kl/l1-settled-interop-proof + atomic-imt-interop).",
            layout.commitment_tree
        );
    }
    let tree = IL2InteropCommitmentTree::new(layout.commitment_tree, provider);
    tree.root()
        .call()
        .await
        .context("commitment tree root() reverted — contract at that address is not the IMT")?;
    tree.leafCount().call().await.context("commitment tree leafCount() reverted")?;
    Ok(())
}

/// Predict a leg's bundleHash via an atomic `sendBundle` callStatic (bundleHash is independent of the atomic params).
async fn predict_bundle_hash(
    source: &ChainCtx,
    dest: &ChainCtx,
    amount: U256,
    recipient: Address,
    interop_center: Address,
    fee: U256,
    deadline: u64,
    salt: B256,
) -> Result<B256> {
    InteropCenter::new(interop_center, &source.provider)
        .sendBundle(
            encode_evm_chain(dest.chain_id),
            vec![bridge_call_starter(source, amount, recipient)],
            vec![
                atomic_bundle_attr(B256::ZERO, deadline, U256::ZERO),
                interop_bundle_salt_attr(salt),
            ],
        )
        .value(fee)
        .gas(ATOMIC_SEND_GAS)
        .call()
        .await
        .context("predict sendBundle callStatic")
}


/// Atomic-send one leg (burn + IMT insert). Returns `(bundleData, bundleHash, txHash, sendBlock)`.
async fn send_atomic_leg(
    source: &ChainCtx,
    dest: &ChainCtx,
    amount: U256,
    recipient: Address,
    flow_id: B256,
    predicted_hash: B256,
    interop_center: Address,
    fee: U256,
    deadline: u64,
    salt: B256,
) -> Result<(Bytes, B256, B256, u64)> {
    let value = commit_value(flow_id, predicted_hash);
    // Low-nullifier (predecessor) index from the server's Rust IMT engine against the pre-insert tree.
    let block = source.provider.get_block_number().await?;
    let low_null: Option<u64> = source
        .provider
        .raw_request("zks_getImtLowNullifierIndex".into(), (value, block))
        .await
        .context("zks_getImtLowNullifierIndex")?;
    let low_null = low_null.context("no low-nullifier leaf for commit value")?;

    let ic = InteropCenter::new(interop_center, &source.provider);
    let receipt = ic
        .sendBundle(
            encode_evm_chain(dest.chain_id),
            vec![bridge_call_starter(source, amount, recipient)],
            vec![
                atomic_bundle_attr(flow_id, deadline, U256::from(low_null)),
                interop_bundle_salt_attr(salt),
            ],
        )
        .value(fee)
        .gas(ATOMIC_SEND_GAS)
        .send()
        .await?
        .get_receipt()
        .await?;
    ensure!(receipt.status(), "atomic sendBundle reverted");
    let tx_hash = receipt.transaction_hash;

    // Extract the InteropBundleSent event: bundleHash + the bundle struct (re-encoded as bundleData).
    let mut found: Option<(B256, Bytes)> = None;
    for log in receipt.inner.logs() {
        if log.address() != interop_center {
            continue;
        }
        if let Ok(decoded) = InteropCenter::InteropBundleSent::decode_log(&log.inner) {
            found = Some((
                decoded.interopBundleHash,
                Bytes::from(decoded.interopBundle.abi_encode()),
            ));
            break;
        }
    }
    let (bundle_hash, bundle_data) = found.context("InteropBundleSent event not found")?;
    ensure!(
        bundle_hash == predicted_hash,
        "predicted bundleHash {predicted_hash} != emitted {bundle_hash}"
    );
    let send_block = receipt.block_number.context("send block number")?;
    println!(
        "[{}->{}] atomic leg sent, bundleHash={bundle_hash} lowNullifier={low_null}",
        source.name, dest.name
    );
    Ok((bundle_data, bundle_hash, tx_hash, send_block))
}

/// Index of the commitment-tree L2->L1 message within the tx (0 fallback).
async fn commitment_tree_message_index(
    provider: &DynProvider,
    tx_hash: B256,
    commitment_tree: Address,
) -> Result<u64> {
    let receipt: serde_json::Value = provider
        .raw_request("eth_getTransactionReceipt".into(), (tx_hash,))
        .await
        .context("eth_getTransactionReceipt (raw)")?;
    let tree = format!("{commitment_tree:#x}");
    if let Some(logs) = receipt["l2ToL1Logs"].as_array() {
        for (idx, l) in logs.iter().enumerate() {
            if l["sender"].as_str().unwrap_or("").to_lowercase() == tree {
                return Ok(idx as u64);
            }
        }
    }
    Ok(0)
}

/// Poll `zks_getL2ToL1LogProof` (messageRoot target) until the commitment-tree publish in `tx_hash` settles.
async fn wait_for_message_proof(provider: &DynProvider, tx_hash: B256, msg_index: u64) -> Result<RawLogProof> {
    let start = Instant::now();
    loop {
        let res: Option<RawLogProof> = provider
            .raw_request(
                "zks_getL2ToL1LogProof".into(),
                (tx_hash, msg_index, "messageRoot"),
            )
            .await
            .context("zks_getL2ToL1LogProof")?;
        if let Some(p) = res {
            return Ok(p);
        }
        ensure!(
            start.elapsed() < Duration::from_secs(300),
            "timed out waiting for message proof of {tx_hash}"
        );
        sleep(Duration::from_secs(2)).await;
    }
}

/// Build a leg's `ImtProof`: IMT half from the Rust engine RPC, message half from the real proof.
async fn build_inclusion_proof(
    source: &ChainCtx,
    value: U256,
    send_block: u64,
    raw: &RawLogProof,
) -> Result<L2InteropHandler::ImtProof> {
    let imt: Option<RpcImtProof> = source
        .provider
        .raw_request("zks_getImtInclusionProof".into(), (value, send_block))
        .await
        .context("zks_getImtInclusionProof")?;
    let imt = imt.context("commit value not present in IMT (server returned null)")?;
    Ok(L2InteropHandler::ImtProof {
        sourceChainId: U256::from(source.chain_id),
        batchNumber: U256::from(raw.batch_number.unwrap_or(0)),
        chainImtRoot: imt.chain_imt_root,
        messageTxNumberInBatch: 0,
        messageIndex: U256::from(raw.id),
        messageProof: raw.proof.clone(),
        leaf: L2InteropHandler::IMTLeaf {
            value: imt.leaf.value,
            nextIndex: imt.leaf.next_index,
            nextValue: imt.leaf.next_value,
        },
        imtLeafIndex: U256::from(imt.imt_leaf_index),
        imtProof: imt.imt_proof,
    })
}

/// Poll a chain's L2InteropRootStorage until it imports the interop root for `(l1_chain_id, sl_block)`.
async fn wait_for_interop_root(provider: &DynProvider, l1_chain_id: u64, sl_block: u64) -> Result<()> {
    let storage = L2InteropRootStorage::new(INTEROP_ROOT_STORAGE, provider);
    let start = Instant::now();
    loop {
        if storage
            .interopRoots(U256::from(l1_chain_id), U256::from(sl_block))
            .call()
            .await?
            != B256::ZERO
        {
            return Ok(());
        }
        ensure!(
            start.elapsed() < Duration::from_secs(180),
            "chain never imported interop root (L1 {l1_chain_id}, block {sl_block})"
        );
        sleep(Duration::from_secs(2)).await;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let private_key = require_env("PRIVATE_KEY")?;
    let rpc_a = std::env::var("L2_RPC_URL").unwrap_or_else(|_| "http://127.0.0.1:3050".into());
    let rpc_b = std::env::var("L2_RPC_URL_SECOND").unwrap_or_else(|_| "http://127.0.0.1:3051".into());
    let l1_rpc = std::env::var("L1_RPC_URL").unwrap_or_else(|_| "http://127.0.0.1:8545".into());
    let deadline: u64 = std::env::var("ATOMIC_DEADLINE")
        .ok()
        .map(|v| v.parse())
        .transpose()
        .context("parse ATOMIC_DEADLINE")?
        .unwrap_or(10_000_000_000);
    let layout = resolve_layout()?;
    let signer: PrivateKeySigner = private_key.parse().context("parse PRIVATE_KEY")?;
    let user = signer.address();

    let unit = U256::from(10u64).pow(U256::from(TOKEN_DECIMALS));
    let a_amount = unit * U256::from(100u64);
    let b_amount = unit * U256::from(100u64);
    let supply = unit * U256::from(1_000_000u64);

    println!("=== ATOMIC SWAP DEMO (IMT bundle model) ===");
    println!(
        "layout: interopCenter={} interopHandler={} commitmentTree={} flowManager={}",
        layout.interop_center, layout.interop_handler, layout.commitment_tree, layout.flow_manager
    );

    // ── Capability gate ──
    for (name, rpc) in [("A", &rpc_a), ("B", &rpc_b)] {
        let ro = ProviderBuilder::new().connect(rpc).await?.erased();
        assert_atomic_capable(name, &ro, &layout).await?;
    }
    println!("atomic built-ins detected on both chains; proceeding.");

    // ── Token setup ──
    let a = setup_token("A", &rpc_a, signer.clone(), supply, a_amount).await?;
    let b = setup_token("B", &rpc_b, signer.clone(), supply, b_amount).await?;

    // Bundle send value = interopProtocolFee * callCount; each leg has one call → value == fee.
    let fee = InteropCenter::new(layout.interop_center, &a.provider)
        .interopProtocolFee()
        .call()
        .await?;
    println!("interopProtocolFee (per call): {fee}");

    // ── Predict leg hashes → flowId ──
    // For L1-settling chains the flow's settlement layer is L1 itself; its chain id is part of the
    // flowId preimage, so resolve it before computing the flowId.
    let l1 = ProviderBuilder::new().connect(&l1_rpc).await?.erased();
    let l1_chain_id = l1.get_chain_id().await?;
    let salt_ab = keccak256(b"atomic-swap-leg-ab");
    let salt_ba = keccak256(b"atomic-swap-leg-ba");
    let h_ab = predict_bundle_hash(&a, &b, a_amount, user, layout.interop_center, fee, deadline, salt_ab).await?;
    let h_ba = predict_bundle_hash(&b, &a, b_amount, user, layout.interop_center, fee, deadline, salt_ba).await?;
    let mut leg_hashes_asc = [h_ab, h_ba];
    leg_hashes_asc.sort();
    let mut chain_ids_asc = [U256::from(a.chain_id), U256::from(b.chain_id)];
    chain_ids_asc.sort();
    let flow_id = compute_flow_id(&leg_hashes_asc, &chain_ids_asc, deadline, U256::from(l1_chain_id));
    println!("flowId={flow_id} deadline={deadline} settlementLayer(L1)={l1_chain_id}");

    // ── PHASE 1: atomic send both legs ──
    let a_token = IERC20::new(a.token, &a.provider);
    let b_token = IERC20::new(b.token, &b.provider);
    let a_before = a_token.balanceOf(user).call().await?;
    let b_before = b_token.balanceOf(user).call().await?;

    let (ab_data, _ab_hash, ab_tx, ab_block) =
        send_atomic_leg(&a, &b, a_amount, user, flow_id, h_ab, layout.interop_center, fee, deadline, salt_ab).await?;
    let (ba_data, _ba_hash, ba_tx, ba_block) =
        send_atomic_leg(&b, &a, b_amount, user, flow_id, h_ba, layout.interop_center, fee, deadline, salt_ba).await?;

    let mgr_a = AtomicFlowManager::new(layout.flow_manager, &a.provider);
    let mgr_b = AtomicFlowManager::new(layout.flow_manager, &b.provider);
    ensure!(mgr_a.legState(flow_id, h_ab).call().await? == LEG_COMMITTED, "AB committed on A");
    ensure!(mgr_b.legState(flow_id, h_ba).call().await? == LEG_COMMITTED, "BA committed on B");
    ensure!(a_token.balanceOf(user).call().await? == a_before - a_amount, "A burned aAmount");
    ensure!(b_token.balanceOf(user).call().await? == b_before - b_amount, "B burned bAmount");
    println!("PHASE 1 ok: both legs committed (burn + IMT insert)");

    // ── PHASE 2: wait for L1 settlement, fetch real proofs, build inclusion proofs ──
    let ab_msg_idx = commitment_tree_message_index(&a.provider, ab_tx, layout.commitment_tree).await?;
    let ba_msg_idx = commitment_tree_message_index(&b.provider, ba_tx, layout.commitment_tree).await?;
    println!("waiting for commitment-tree roots to settle on L1...");
    let ab_raw = wait_for_message_proof(&a.provider, ab_tx, ab_msg_idx).await?;
    let ba_raw = wait_for_message_proof(&b.provider, ba_tx, ba_msg_idx).await?;
    println!(
        "AB proof: batch={:?} slBlock={:?}; BA proof: batch={:?} slBlock={:?}",
        ab_raw.batch_number, ab_raw.gateway_block_number, ba_raw.batch_number, ba_raw.gateway_block_number
    );

    let ab_proof = build_inclusion_proof(&a, commit_value(flow_id, h_ab), ab_block, &ab_raw).await?;
    let ba_proof = build_inclusion_proof(&b, commit_value(flow_id, h_ba), ba_block, &ba_raw).await?;
    // Proofs ordered to match legBundleHashes ascending.
    let proofs_asc = if h_ab < h_ba {
        vec![ab_proof, ba_proof]
    } else {
        vec![ba_proof, ab_proof]
    };
    let finality = L2InteropHandler::AtomicFinalityProof {
        flow: L2InteropHandler::AtomicFlow {
            flowId: flow_id,
            deadline,
            settlementLayerChainId: U256::from(l1_chain_id),
            legBundleHashes: leg_hashes_asc.to_vec(),
            legSourceChainIds: chain_ids_asc.to_vec(),
        },
        proofs: proofs_asc,
    };

    // Both executeAtomicBundle calls verify every leg, so each executing chain must have imported the
    // L1 interop root at each leg's settlement block.
    let sl_blocks: Vec<u64> = [ab_raw.gateway_block_number, ba_raw.gateway_block_number]
        .into_iter()
        .flatten()
        .collect();
    println!("waiting for interop roots (L1 {l1_chain_id}) at blocks {sl_blocks:?} on both chains...");
    for ctx in [&a, &b] {
        for &sl in &sl_blocks {
            wait_for_interop_root(&ctx.provider, l1_chain_id, sl).await?;
        }
    }
    println!("interop roots imported on both chains");

    // ── PHASE 3: executeAtomicBundle on each destination ──
    println!("executing AB on B and BA on A...");
    let handler_b = L2InteropHandler::new(layout.interop_handler, &b.provider);
    let handler_a = L2InteropHandler::new(layout.interop_handler, &a.provider);
    let r = handler_b.executeAtomicBundle(ab_data, finality.clone()).gas(TX_GAS).send().await?.get_receipt().await?;
    ensure!(r.status(), "executeAtomicBundle AB on B succeeds");
    let r = handler_a.executeAtomicBundle(ba_data, finality).gas(TX_GAS).send().await?.get_receipt().await?;
    ensure!(r.status(), "executeAtomicBundle BA on A succeeds");
    ensure!(
        handler_b.bundleStatus(h_ab).call().await? == BUNDLE_FULLY_EXECUTED,
        "AB bundle FullyExecuted on B"
    );
    ensure!(
        handler_a.bundleStatus(h_ba).call().await? == BUNDLE_FULLY_EXECUTED,
        "BA bundle FullyExecuted on A"
    );

    // ── Destination mint assertions ──
    let ntv_b = L2NativeTokenVault::new(NATIVE_TOKEN_VAULT, &b.provider);
    let shim_a_on_b = ntv_b.tokenAddress(ntv_asset_id(a.chain_id, a.token)).call().await?;
    ensure!(shim_a_on_b != Address::ZERO, "shim for A's token on B");
    ensure!(
        IERC20::new(shim_a_on_b, &b.provider).balanceOf(user).call().await? >= a_amount,
        "recipient on B got aAmount"
    );
    let ntv_a = L2NativeTokenVault::new(NATIVE_TOKEN_VAULT, &a.provider);
    let shim_b_on_a = ntv_a.tokenAddress(ntv_asset_id(b.chain_id, b.token)).call().await?;
    ensure!(shim_b_on_a != Address::ZERO, "shim for B's token on A");
    ensure!(
        IERC20::new(shim_b_on_a, &a.provider).balanceOf(user).call().await? >= b_amount,
        "recipient on A got bAmount"
    );

    println!("\nSUCCESS: atomic swap completed end-to-end (both legs executed atomically).");
    Ok(())
}

// SPDX-License-Identifier: MIT
//! `taskfast cast` — general-purpose contract interaction (gh#101).
//!
//! Drop-in replacement for the foundry `cast` invocations that operator
//! runbooks and provisioning scripts still depend on:
//!
//!   * `cast call <addr> '<sig>' [args…]`  → read via `eth_call`, decoded.
//!   * `cast send <addr> '<sig>' [args…]`  → sign + broadcast + receipt.
//!   * `cast rpc  <method> [json-params]`  → raw JSON-RPC passthrough.
//!
//! Signatures are parsed at runtime (`alloy-json-abi` human-readable grammar,
//! which also accepts the cast-style `paused()(bool)` return-type shorthand),
//! args are coerced per declared Solidity type, and calldata is encoded with
//! the 4-byte selector via `alloy-dyn-abi` — no compile-time `sol!` bindings,
//! so any contract works.
//!
//! Deliberately out of scope (YAGNI, per gh#101): `abi-encode` / `abi-decode`
//! / `keccak` / `4byte` conveniences, `--block` on `call` (everything in-repo
//! reads `latest`; add an additive `eth_call_at` later if needed), `--value`
//! on `send` (every replaced invocation targets a non-payable function).
//!
//! Every chain touch goes through [`ChainConnection`]: an explicit `--rpc-url`
//! needs no TaskFast credentials, the deployment's proxy is same-host guarded,
//! and `send` verifies the node's `eth_chainId` before signing.

use std::path::PathBuf;
use std::time::Duration;

use alloy_dyn_abi::{DynSolValue, FunctionExt, JsonAbiExt, Specifier};
use alloy_json_abi::Function;
use alloy_primitives::{Address, Bytes};
use clap::Parser;
use serde_json::{json, Value};

use super::{parse_duration, validate_override_rpc_url, CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;
use crate::tempo_session::{load_wallet, ChainConnection, ChainTarget, WalletOpts};

#[derive(Debug, clap::Subcommand)]
pub enum Command {
    /// Read-only `eth_call`; result decoded per the signature's return types.
    Call(CallArgs),
    /// Sign + broadcast a transaction calling an arbitrary function.
    Send(SendArgs),
    /// Raw JSON-RPC passthrough: method + params in, untyped result out.
    Rpc(RpcArgs),
}

#[derive(Debug, Parser)]
pub struct CallArgs {
    /// Target contract address (`0x`-prefixed).
    pub to: String,

    /// Function signature, e.g. `balanceOf(address)(uint256)`. Return types
    /// (the second parens group) are optional — without them the raw hex
    /// response is returned undecoded.
    pub sig: String,

    /// Function arguments, one per declared input (use `--` before values
    /// that start with a dash, e.g. negative ints).
    pub args: Vec<String>,

    /// Tempo RPC override. Defaults to the deployment's authenticated proxy URL.
    #[arg(long, env = "TEMPO_RPC_URL")]
    pub rpc_url: Option<String>,
}

#[derive(Debug, Parser)]
pub struct SendArgs {
    /// Target contract address (`0x`-prefixed).
    pub to: String,

    /// Function signature, e.g. `setSlashRecipient(address,bool)`.
    pub sig: String,

    /// Function arguments, one per declared input (use `--` before values
    /// that start with a dash, e.g. negative ints).
    pub args: Vec<String>,

    /// Keystore reference (same form as `taskfast escrow sign`).
    #[arg(long, env = "TEMPO_KEY_SOURCE")]
    pub keystore: Option<String>,

    /// Path to keystore password file.
    #[arg(long, env = "TASKFAST_WALLET_PASSWORD_FILE")]
    pub wallet_password_file: Option<PathBuf>,

    /// Wallet address preflight (`0x`-prefixed). When set, fail before touching
    /// the chain if the keystore decrypts to a mismatch.
    #[arg(long, env = "TEMPO_WALLET_ADDRESS")]
    pub wallet_address: Option<String>,

    /// Tempo RPC override. Defaults to the deployment's authenticated proxy URL.
    #[arg(long, env = "TEMPO_RPC_URL")]
    pub rpc_url: Option<String>,

    /// Receipt-polling ceiling — human duration (`3min`, `90s`). Falls back to
    /// `receipt_timeout` in config, then a network-aware default.
    #[arg(long, env = "TASKFAST_RECEIPT_TIMEOUT", value_parser = parse_duration)]
    pub receipt_timeout: Option<Duration>,
}

#[derive(Debug, Parser)]
pub struct RpcArgs {
    /// JSON-RPC method name, e.g. `eth_chainId` or `tempo_fundAddress`.
    pub method: String,

    /// Params as a JSON value (usually an array), e.g. `'["0xabc…", true]'`.
    /// Defaults to `[]`.
    pub params: Option<String>,

    /// Tempo RPC override. Defaults to the deployment's authenticated proxy URL.
    #[arg(long, env = "TEMPO_RPC_URL")]
    pub rpc_url: Option<String>,
}

pub async fn run(ctx: &Ctx, cmd: Command) -> CmdResult {
    match cmd {
        Command::Call(a) => call(ctx, a).await,
        Command::Send(a) => send(ctx, a).await,
        Command::Rpc(a) => rpc(ctx, a).await,
    }
}

async fn call(ctx: &Ctx, args: CallArgs) -> CmdResult {
    let to = parse_to(&args.to)?;
    let func = parse_function(&args.sig)?;
    let values = coerce_args(&func, &args.args)?;
    let calldata = encode_calldata(&func, &values)?;

    let conn = connect(ctx, args.rpc_url.as_deref()).await?;
    let raw = conn
        .eth_call(to, &calldata)
        .await
        .map_err(|e| CmdError::Server(format!("eth_call: {e}")))?;

    // No declared return types → raw hex only (mirrors foundry cast, which
    // prints the undecoded response when the sig carries no output parens).
    let decoded = if func.outputs.is_empty() {
        Value::Null
    } else {
        let vals = func
            .abi_decode_output(&raw)
            .map_err(|e| CmdError::Decode(format!("decode output of {}: {e}", args.sig)))?;
        Value::Array(vals.iter().map(dyn_value_to_json).collect())
    };

    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({
            "to": format!("{to:#x}"),
            "sig": args.sig,
            "raw": format!("0x{}", hex::encode(&raw)),
            "decoded": decoded,
        }),
    ))
}

async fn send(ctx: &Ctx, args: SendArgs) -> CmdResult {
    let to = parse_to(&args.to)?;
    let func = parse_function(&args.sig)?;
    let values = coerce_args(&func, &args.args)?;
    let calldata = encode_calldata(&func, &values)?;

    // Preflight the wallet before any network I/O — same order as `bond post`,
    // so `--dry-run` still validates the keystore.
    let wallet = WalletOpts {
        keystore: args.keystore.as_deref(),
        password_file: args.wallet_password_file.as_deref(),
        wallet_address: args.wallet_address.as_deref(),
        rpc_url: args.rpc_url.as_deref(),
        receipt_timeout: args.receipt_timeout,
    };
    if ctx.dry_run {
        validate_dry_run_override(ctx, wallet.rpc_url)?;
        let (signer, _) = load_wallet(ctx, &wallet, "cast send transaction")?;
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({
                "action": "would_send",
                "to": format!("{to:#x}"),
                "sig": args.sig,
                "from": format!("{:#x}", signer.address()),
                "calldata": format!("0x{}", hex::encode(&calldata)),
            }),
        ));
    }

    let session = connect(ctx, wallet.rpc_url)
        .await?
        .into_session(ctx, &wallet, "cast send transaction")
        .await?;

    let tx_hash = session
        .send_transaction_and_wait(to, calldata, "", None)
        .await?;

    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({
            "action": "sent",
            "to": format!("{to:#x}"),
            "sig": args.sig,
            "from": format!("{:#x}", session.address()),
            "tx_hash": format!("{tx_hash:#x}"),
        }),
    ))
}

async fn rpc(ctx: &Ctx, args: RpcArgs) -> CmdResult {
    let params: Value = match args.params.as_deref() {
        Some(raw) => serde_json::from_str(raw)
            .map_err(|e| CmdError::Usage(format!("params is not valid JSON: {e}")))?,
        None => json!([]),
    };

    // A raw passthrough can't tell reads from mutations (`eth_sendRawTransaction`
    // is one method call away), so `--dry-run` short-circuits the whole verb
    // rather than honoring the reads-pass-through convention. Fail safe.
    if ctx.dry_run {
        validate_dry_run_override(ctx, args.rpc_url.as_deref())?;
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({
                "action": "would_rpc",
                "method": args.method,
                "params": params,
            }),
        ));
    }

    let conn = connect(ctx, args.rpc_url.as_deref()).await?;
    let result = conn
        .raw_call(&args.method, params)
        .await
        .map_err(|e| CmdError::Server(format!("{}: {e}", args.method)))?;

    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({
            "method": args.method,
            "result": result,
        }),
    ))
}

/// The chain-session seam's endpoint choice: an explicit `--rpc-url` goes
/// through the endpoint guard and needs no TaskFast credentials; otherwise the
/// deployment's `GET /config/network` supplies the same-host proxy for the
/// environment's network. Makes no node request.
async fn connect(ctx: &Ctx, override_url: Option<&str>) -> Result<ChainConnection, CmdError> {
    match override_url {
        Some(url) => ChainConnection::open_override(ctx, url),
        None => {
            ChainConnection::resolve(
                ctx,
                &ctx.client()?,
                None,
                ChainTarget::Network(ctx.environment.network()),
            )
            .await
        }
    }
}

/// Endpoint-guard check for dry-run short-circuits: `validate_override_rpc_url`
/// is pure (no I/O), so dry-run can reject the same override URLs live mode
/// would while still guaranteeing zero network traffic.
fn validate_dry_run_override(ctx: &Ctx, override_url: Option<&str>) -> Result<(), CmdError> {
    if let Some(url) = override_url {
        validate_override_rpc_url(url, ctx.environment.network(), ctx.allow_custom_endpoints)?;
    }
    Ok(())
}

fn parse_to(raw: &str) -> Result<Address, CmdError> {
    raw.parse()
        .map_err(|e| CmdError::Usage(format!("`{raw}` is not a valid EVM address: {e}")))
}

fn parse_function(sig: &str) -> Result<Function, CmdError> {
    Function::parse(sig).map_err(|e| {
        CmdError::Usage(format!(
            "invalid function signature {sig:?}: {e} \
             (expected e.g. `transfer(address,uint256)` or `paused()(bool)`)"
        ))
    })
}

/// Coerce each CLI string to the [`DynSolValue`] its declared input demands.
fn coerce_args(func: &Function, args: &[String]) -> Result<Vec<DynSolValue>, CmdError> {
    if func.inputs.len() != args.len() {
        return Err(CmdError::Usage(format!(
            "`{}` takes {} argument(s), got {}",
            func.signature(),
            func.inputs.len(),
            args.len()
        )));
    }
    func.inputs
        .iter()
        .zip(args)
        .enumerate()
        .map(|(i, (param, raw))| {
            let ty = param.resolve().map_err(|e| {
                CmdError::Usage(format!("cannot resolve type of input #{i} `{param}`: {e}"))
            })?;
            ty.coerce_str(raw).map_err(|e| {
                CmdError::Usage(format!("argument #{i} `{raw}` is not a valid {ty}: {e}"))
            })
        })
        .collect()
}

/// Selector-prefixed calldata for `func(values…)`.
fn encode_calldata(func: &Function, values: &[DynSolValue]) -> Result<Bytes, CmdError> {
    func.abi_encode_input(values)
        .map(Bytes::from)
        .map_err(|e| CmdError::Usage(format!("encode {}: {e}", func.signature())))
}

/// Render a decoded [`DynSolValue`] as JSON for the envelope. `alloy-dyn-abi`
/// ships no `Serialize` impl. Numbers become decimal strings (U256/I256
/// overflow every JSON number type); byte-ish values become `0x`-hex.
fn dyn_value_to_json(v: &DynSolValue) -> Value {
    match v {
        DynSolValue::Bool(b) => json!(b),
        DynSolValue::Int(i, _) => json!(i.to_string()),
        DynSolValue::Uint(u, _) => json!(u.to_string()),
        DynSolValue::FixedBytes(word, size) => {
            json!(format!("0x{}", hex::encode(&word.as_slice()[..*size])))
        }
        DynSolValue::Address(a) => json!(format!("{a:#x}")),
        DynSolValue::Function(f) => json!(format!("0x{}", hex::encode(f.as_slice()))),
        DynSolValue::Bytes(b) => json!(format!("0x{}", hex::encode(b))),
        DynSolValue::String(s) => json!(s),
        DynSolValue::Array(vals) | DynSolValue::FixedArray(vals) | DynSolValue::Tuple(vals) => {
            Value::Array(vals.iter().map(dyn_value_to_json).collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, U256};
    use alloy_sol_types::SolCall;
    use taskfast_agent::chain::IERC20;

    const ADDR: &str = "0x00000000000000000000000000000000000000aa";

    #[test]
    fn parses_plain_signature() {
        let f = parse_function("setSlashRecipient(address,bool)").unwrap();
        assert_eq!(f.name, "setSlashRecipient");
        assert_eq!(f.inputs.len(), 2);
        assert!(
            f.outputs.is_empty(),
            "expected no outputs, got {:?}",
            f.outputs
        );
    }

    #[test]
    fn parses_cast_style_return_shorthand() {
        let f = parse_function("paused()(bool)").unwrap();
        assert!(
            f.inputs.is_empty(),
            "expected no inputs, got {:?}",
            f.inputs
        );
        assert_eq!(f.outputs.len(), 1);
        assert_eq!(f.outputs[0].ty, "bool");
    }

    #[test]
    fn rejects_garbage_signature() {
        let err = parse_function("not a signature").unwrap_err();
        assert!(matches!(err, CmdError::Usage(_)), "got {err:?}");
    }

    #[test]
    fn rejects_arity_mismatch() {
        let f = parse_function("transfer(address,uint256)").unwrap();
        let err = coerce_args(&f, &[ADDR.to_string()]).unwrap_err();
        match err {
            CmdError::Usage(m) => assert!(m.contains("takes 2"), "msg: {m}"),
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    #[test]
    fn rejects_uncoercible_argument() {
        let f = parse_function("transfer(address,uint256)").unwrap();
        let err = coerce_args(&f, &[ADDR.to_string(), "not-a-number".to_string()]).unwrap_err();
        assert!(matches!(err, CmdError::Usage(_)), "got {err:?}");
    }

    #[test]
    fn dynamic_encoding_matches_static_sol_bindings() {
        // Cross-check the whole parse→coerce→encode pipeline against the
        // compile-time `sol!` encoding the rest of the CLI uses.
        let f = parse_function("approve(address,uint256)").unwrap();
        let values = coerce_args(&f, &[ADDR.to_string(), "5000000".to_string()]).unwrap();
        let dynamic = encode_calldata(&f, &values).unwrap();
        let expected = IERC20::approveCall {
            spender: address!("00000000000000000000000000000000000000aa"),
            amount: U256::from(5_000_000u64),
        }
        .abi_encode();
        assert_eq!(dynamic.as_ref(), expected.as_slice());
    }

    #[test]
    fn decodes_bool_output() {
        let f = parse_function("paused()(bool)").unwrap();
        let mut raw = [0u8; 32];
        raw[31] = 1;
        let vals = f.abi_decode_output(&raw).unwrap();
        assert_eq!(vals.len(), 1);
        assert_eq!(dyn_value_to_json(&vals[0]), json!(true));
    }

    #[test]
    fn coerces_array_argument() {
        let f = parse_function("setAll(uint256[])").unwrap();
        let values = coerce_args(&f, &["[1, 2, 3]".to_string()]).unwrap();
        let encoded = encode_calldata(&f, &values).unwrap();
        // selector + offset + length + 3 words
        assert_eq!(encoded.len(), 4 + 32 * 5);
    }

    #[test]
    fn dyn_value_to_json_covers_scalar_and_nested_shapes() {
        let addr = address!("00000000000000000000000000000000000000aa");
        assert_eq!(
            dyn_value_to_json(&DynSolValue::Address(addr)),
            json!("0x00000000000000000000000000000000000000aa")
        );
        assert_eq!(
            dyn_value_to_json(&DynSolValue::Uint(U256::MAX, 256)),
            json!(U256::MAX.to_string())
        );
        assert_eq!(
            dyn_value_to_json(&DynSolValue::Bytes(vec![0xde, 0xad])),
            json!("0xdead")
        );
        assert_eq!(
            dyn_value_to_json(&DynSolValue::String("hi".into())),
            json!("hi")
        );
        let nested = DynSolValue::Tuple(vec![
            DynSolValue::Bool(true),
            DynSolValue::Array(vec![DynSolValue::Uint(U256::from(7u64), 256)]),
        ]);
        assert_eq!(dyn_value_to_json(&nested), json!([true, ["7"]]));
    }
}

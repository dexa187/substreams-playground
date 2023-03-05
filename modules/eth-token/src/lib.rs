mod pb;
mod eth;
mod rpc;

use substreams::errors::Error;
use substreams::{log, proto, store, Hex, hex};
use substreams_ethereum::pb::eth as ethpb;
use crate::rpc::create_rpc_calls;
use serde_json::json;

use pb::sinkfiles::Lines;


const INITIALIZE_METHOD_HASH: [u8; 4] = hex!("1459457a");

#[substreams::handlers::map]
fn map_tokens(blk: ethpb::v1::Block) -> Result<pb::tokens::Tokens, Error> {
    let mut tokens = vec![];

    for trx in blk.transaction_traces {
        for call in trx.calls {
            if call.state_reverted {
                continue;
            }
            if call.call_type == ethpb::v1::CallType::Create as i32
                || call.call_type == ethpb::v1::CallType::Call as i32
            // proxy contract creation
            {
                let call_input_len = call.input.len();
                if call.call_type == ethpb::v1::CallType::Call as i32
                    && (call_input_len < 4 || call.input[0..4] != INITIALIZE_METHOD_HASH)
                {
                    // this will check if a proxy contract has been called to create a ERC20/721/1155 contract.
                    // if that is the case the Proxy contract will call the initialize function on the ERC20/721/1155 contract
                    // this is part of the OpenZeppelin Proxy contract standard
                    continue;
                }

                if call.call_type == ethpb::v1::CallType::Create as i32 {
                    let mut code_change_len = 0;
                    for code_change in &call.code_changes {
                        code_change_len += code_change.new_code.len()
                    }

                    log::debug!(
                        "found contract creation: {}, caller {}, code change {}, input {}",
                        Hex(&call.address),
                        Hex(&call.caller),
                        code_change_len,
                        call_input_len,
                    );

                    if code_change_len <= 150 {
                        // optimization to skip none viable SC
                        log::info!(
                            "skipping too small code to be a token contract: {}",
                            Hex(&call.address)
                        );
                        continue;
                    }
                } else {
                    log::debug!(
                        "found proxy initialization: contract {}, caller {}",
                        Hex(&call.address),
                        Hex(&call.caller)
                    );
                }

                if call.caller == hex!("0000000000004946c0e9f43f4dee607b0ef1fa1c")
                    || call.caller == hex!("00000000687f5b66638856396bee28c1db0178d1")
                {
                    log::debug!("skipping known caller address");
                    continue;
                }

                let rpc_call_decimal = create_rpc_calls(&call.address, vec![rpc::DECIMALS]);
                let rpc_responses_unmarshalled_decimal: ethpb::rpc::RpcResponses =
                    substreams_ethereum::rpc::eth_call(&rpc_call_decimal);
                let response_decimal = rpc_responses_unmarshalled_decimal.responses;
                let decimals: u64;
                if response_decimal[0].failed {
                    let decimals_error = String::from_utf8_lossy(response_decimal[0].raw.as_ref());
                    log::debug!(
                        "{} is not an ERC20 token contract because of 'eth_call' failures [decimals: {}]",
                        Hex(&call.address),
                        decimals_error,
                    );
                    decimals = 0;
                }
                else {
                    let decoded_decimals = eth::read_uint32(response_decimal[0].raw.as_ref());
                    if decoded_decimals.is_err() {
                        log::debug!(
                            "{} is not an ERC20 token contract decimal `eth_call` failed: {}",
                            Hex(&call.address),
                            decoded_decimals.err().unwrap(),
                        );
                        decimals = 0;
                    }
                    else {
                        decimals = decoded_decimals.unwrap() as u64;
                    }
                }

                let rpc_call_name_symbol = create_rpc_calls(&call.address, vec![rpc::NAME, rpc::SYMBOL]);
                let rpc_responses_unmarshalled: ethpb::rpc::RpcResponses =
                    substreams_ethereum::rpc::eth_call(&rpc_call_name_symbol);
                let responses = rpc_responses_unmarshalled.responses;
                if responses[0].failed || responses[1].failed {
                    let name_error = String::from_utf8_lossy(responses[0].raw.as_ref());
                    let symbol_error = String::from_utf8_lossy(responses[1].raw.as_ref());
                    log::debug!(
                        "{} is not an ERC20/721/1155 token contract because of 'eth_call' failures [name: {}, symbol: {}]",
                        Hex(&call.address),
                        name_error,
                        symbol_error,
                    );
                    continue;
                };

                let decoded_name = eth::read_string(responses[0].raw.as_ref());
                if decoded_name.is_err() {
                    log::debug!(
                        "{} is not an ERC20/721/1155 token contract name `eth_call` failed: {}",
                        Hex(&call.address),
                        decoded_name.err().unwrap(),
                    );
                    continue;
                }

                let symbol: String ;
                let decoded_symbol = eth::read_string(responses[1].raw.as_ref());
                if decoded_symbol.is_err() {
                    log::debug!(
                        "{} is not an ERC20/721/1155 token contract symbol `eth_call` failed: {}",
                        Hex(&call.address),
                        decoded_symbol.err().unwrap(),
                    );
                    symbol = String::from("");
                }
                else {
                    symbol = decoded_symbol.unwrap();
                }


                let name = decoded_name.unwrap();
                log::debug!(
                    "{} is an ERC20/721/1155 token contract with name {}",
                    Hex(&call.address),
                    name,
                );
                let token = pb::tokens::Token {
                    address: Hex(&call.address).to_string(),
                    name,
                    symbol,
                    decimals,
                };

                tokens.push(token);
            }
        }
    }

    Ok(pb::tokens::Tokens { tokens })
}

#[substreams::handlers::store]
fn store_tokens(tokens: pb::tokens::Tokens, store: store::StoreSet) {
    for token in tokens.tokens {
        let key = format!("token:{}", token.address);
        store.set(1, key, &proto::encode(&token).unwrap());
    }
}

#[substreams::handlers::map]
fn jsonout(tokens: pb::tokens::Tokens) -> Result<Lines, substreams::errors::Error> {
    Ok(pb::sinkfiles::Lines {
        lines: tokens.tokens
            .iter()
            .flat_map(|token| {
                [
                    json!({
                        "address": token.address,
                        "name": token.name,
                        "symbol": token.symbol,
                        "decimals": token.decimals,
                    })
                    .to_string(),
                ]
            })
            .collect(),
    })
}


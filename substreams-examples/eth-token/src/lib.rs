mod eth;
mod pb;
mod rpc;

use eth::{address_pretty, decode_string, decode_uint32};
use substreams::{log, proto, state};

const INITIALIZE_METHOD_HASH: &str = "0x1459457a";

#[no_mangle]
pub extern "C" fn block_to_tokens(block_ptr: *mut u8, block_len: usize) {
    substreams::register_panic_hook();

    let mut tokens = pb::tokens::Tokens { tokens: vec![] };
    let blk: pb::eth::Block = proto::decode_ptr(block_ptr, block_len).unwrap();

    for trx in blk.transaction_traces {
        for call in trx.calls {
            if call.call_type == pb::eth::CallType::Create as i32
                || call.call_type == pb::eth::CallType::Call as i32 // proxy contract creation
                && !call.state_reverted
            {
                let call_input_len = call.input.len();

                if call.call_type == pb::eth::CallType::Call as i32
                    && (call_input_len < 4
                        || !address_pretty(&call.input).starts_with(INITIALIZE_METHOD_HASH))
                {
                    continue;
                }

                let contract_address = address_pretty(&call.address);
                let caller_address = address_pretty(&call.caller);

                //pancake v1 and v2
                if caller_address == "0xca143ce32fe78f1f7019d7d551a6402fc5350c73"
                    || caller_address == "0xbcfccbde45ce874adcb698cc183debcf17952812"
                {
                    continue;
                }

                if call.call_type == pb::eth::CallType::Create as i32 {
                    let mut code_change_len = 0;
                    for code_change in &call.code_changes {
                        code_change_len += code_change.new_code.len()
                    }

                    log::println(format!(
                        "found contract creation: {}, caller {}, code change {}, input {}",
                        contract_address, caller_address, code_change_len, call_input_len,
                    ));

                    if code_change_len <= 150 {
                        // optimization to skip none viable SC
                        log::println(format!(
                            "skipping too small code to be a token contract: {}",
                            address_pretty(&call.address)
                        ));
                        continue;
                    }
                } else if call.call_type == pb::eth::CallType::Call as i32 {
                    log::println(format!(
                        "found contract that may be a proxy contract: {}",
                        caller_address
                    ))
                }

                if caller_address == "0x0000000000004946c0e9f43f4dee607b0ef1fa1c"
                    || caller_address == "0x00000000687f5b66638856396bee28c1db0178d1"
                {
                    continue;
                }

                let rpc_calls = rpc::create_rpc_calls(&call.address);

                let rpc_responses_marshalled: Vec<u8> =
                    substreams::rpc::eth_call(substreams::proto::encode(&rpc_calls).unwrap());
                let rpc_responses_unmarshalled: substreams::pb::eth::RpcResponses =
                    substreams::proto::decode(&rpc_responses_marshalled).unwrap();

                let responses = rpc_responses_unmarshalled.responses;

                if responses[0].failed || responses[1].failed || responses[2].failed {
                    let decimals_error = String::from_utf8_lossy(responses[0].raw.as_ref());
                    let name_error = String::from_utf8_lossy(responses[1].raw.as_ref());
                    let symbol_error = String::from_utf8_lossy(responses[2].raw.as_ref());

                    log::println(format!(
                        "{} is not a an ERC20 token contract because of 'eth_call' failures [decimals: {}, name: {}, symbol: {}]",
                        contract_address,
                        decimals_error,
                        name_error,
                        symbol_error,
                    ));
                    continue;
                };

                if !(responses[1].raw.len() >= 96)
                    || responses[0].raw.len() != 32
                    || !(responses[2].raw.len() >= 96)
                {
                    log::println(format!(
                        "{} is not a an ERC20 token contract because of 'eth_call' failures [decimals length: {}, name length: {}, symbo length: {}]",
                        contract_address,
                        responses[0].raw.len(),
                        responses[1].raw.len(),
                        responses[2].raw.len(),
                    ));
                    continue;
                };

                let decoded_address = address_pretty(&call.address);
                let decoded_decimals = decode_uint32(responses[0].raw.as_ref());
                let decoded_name = decode_string(responses[1].raw.as_ref());
                let decoded_symbol = decode_string(responses[2].raw.as_ref());

                log::println(format!(
                    "{} is an ERC20 token contract with name {}",
                    decoded_address, decoded_name,
                ));

                let token = pb::tokens::Token {
                    address: decoded_address,
                    name: decoded_name,
                    symbol: decoded_symbol,
                    decimals: decoded_decimals as u64,
                };

                tokens.tokens.push(token);
            }
        }
    }

    substreams::output(tokens);
}

#[no_mangle]
pub extern "C" fn build_tokens_state(tokens_ptr: *mut u8, tokens_len: usize) {
    substreams::register_panic_hook();

    let tokens: pb::tokens::Tokens = proto::decode_ptr(tokens_ptr, tokens_len).unwrap();

    for token in tokens.tokens {
        let key = format!("token:{}", token.address);
        state::set(1, key, &proto::encode(&token).unwrap());
    }
}

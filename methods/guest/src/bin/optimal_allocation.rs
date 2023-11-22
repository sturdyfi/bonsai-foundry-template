#![no_main]

use std::io::Read;

use ethabi::{ethereum_types::U256, ParamType, Token, Address};
use ethers_core::types::I256;

use risc0_zkvm::guest::env;

risc0_zkvm::guest::entry!(main);

#[derive(Clone, Copy)]
struct SturdyDataParams {
    cur_timestamp: U256,
    last_timestamp: U256,
    rate_per_sec: U256,
    full_utilization_rate: U256,
    total_asset: U256,
    total_borrow: U256,
    util_prec: U256,
    min_target_util: U256,
    max_target_util: U256,
    vertex_utilization: U256,
    min_full_util_rate: U256,
    max_full_util_rate: U256,
    zero_util_rate: U256,
    rate_half_life: U256,
    vertex_rate_percent: U256,
    rate_prec: U256,
    is_interest_paused: bool
}

#[derive(Clone)]
struct Position {
    strategy: Address,
    debt: U256,
}

struct StrategyParams {
    activation: U256,
    last_report: U256,
    current_debt: U256,
    max_debt: U256
}

const SECONDS_PER_YEAR: u128 = 31556952 as u128;

fn get_full_utilization_interest(delta_time: U256, utilization: U256, sturdy_data: SturdyDataParams) -> u64 {
    let mut new_full_utilization_interest: u64;

    if utilization < sturdy_data.min_target_util {
        let delta_utilization = ((sturdy_data.min_target_util - utilization) * U256::from(1e18 as u128)) / sturdy_data.min_target_util;
        let decay_growth = (sturdy_data.rate_half_life * U256::from(1e36 as u128)) + (delta_utilization * delta_utilization * delta_time);
        new_full_utilization_interest =
            ((sturdy_data.full_utilization_rate * (sturdy_data.rate_half_life * U256::from(1e36 as u128))) / decay_growth).as_u64();
    } else if utilization > sturdy_data.max_target_util {
        let delta_utilization = ((utilization - sturdy_data.max_target_util) * U256::from(1e18 as u128)) / (sturdy_data.util_prec - sturdy_data.max_target_util);
        let decay_growth = (sturdy_data.rate_half_life * U256::from(1e36 as u128)) + (delta_utilization * delta_utilization * delta_time);
        new_full_utilization_interest =
            ((sturdy_data.full_utilization_rate * decay_growth) / (sturdy_data.rate_half_life * U256::from(1e36 as u128))).as_u64();
    } else {
        new_full_utilization_interest = sturdy_data.full_utilization_rate.as_u64();
    }

    if new_full_utilization_interest > sturdy_data.max_full_util_rate.as_u64() {
        new_full_utilization_interest = sturdy_data.max_full_util_rate.as_u64();
    } else if new_full_utilization_interest < sturdy_data.min_full_util_rate.as_u64() {
        new_full_utilization_interest = sturdy_data.min_full_util_rate.as_u64();
    }

    new_full_utilization_interest
}

fn get_new_rate(delta_time: U256, utilization: U256, sturdy_data: SturdyDataParams) -> (u64, u64) {
    let new_full_utilization_interest = get_full_utilization_interest(delta_time, utilization, sturdy_data);

    let vertex_interest =
        (((U256::from(new_full_utilization_interest) - sturdy_data.zero_util_rate) * sturdy_data.vertex_rate_percent) / sturdy_data.rate_prec) + sturdy_data.zero_util_rate;

    let new_rate_per_sec = if utilization < sturdy_data.vertex_utilization {
        (sturdy_data.zero_util_rate + (utilization * (vertex_interest - sturdy_data.zero_util_rate)) / sturdy_data.vertex_utilization).as_u64()
    } else {
        (vertex_interest + ((utilization - sturdy_data.vertex_utilization) * (U256::from(new_full_utilization_interest) - vertex_interest)) / (sturdy_data.util_prec - sturdy_data.vertex_utilization)).as_u64()
    };

    (new_rate_per_sec, new_full_utilization_interest)
}

fn apr_after_debt_change(
    sturdy_data: SturdyDataParams,
    delta: I256
) -> U256 {
    if delta == I256::from(0 as i128) {
        return sturdy_data.rate_per_sec * U256::from(SECONDS_PER_YEAR);
    }

    let asset_amount = U256::from((I256::from(sturdy_data.total_asset.as_u128() as i128) + delta).as_i128() as u128);

    if sturdy_data.is_interest_paused {
        return sturdy_data.rate_per_sec * U256::from(SECONDS_PER_YEAR);
    }

    let delta_time = sturdy_data.cur_timestamp - sturdy_data.last_timestamp;
    let utilization_rate;
    if asset_amount == U256::from(0 as u128) {
        utilization_rate = U256::from(0 as u128);
    } else {
        utilization_rate = (sturdy_data.util_prec * sturdy_data.total_borrow) / asset_amount
    };

    let (rate_per_sec, _) = get_new_rate(
        delta_time,
        utilization_rate,
        sturdy_data,
    );

    U256::from(rate_per_sec) * U256::from(SECONDS_PER_YEAR)
}

fn get_optimal_allocation(
    c: u64,
    total_initial_amount: U256,
    total_available_amount: U256,
    initial_datas: &Vec<Position>,
    sturdy_datas: &Vec<SturdyDataParams>,
    strategy_datas: &Vec<StrategyParams>
) -> Vec<Position> {
    let mut b = initial_datas.clone();
    let deposit_unit = (total_available_amount - total_initial_amount) / c;
    let strategy_count = initial_datas.len();
    if deposit_unit == U256::from(0 as u128) {
        return vec![];
    }

    // Iterate chunk count
    for i in 0..c {
        // Calculate the correct last remained amount
        if i == c - 1 {
            b[i as usize].debt += total_available_amount - total_initial_amount - deposit_unit * (c - 1);
        }

        // Find max apr silo when deposit unit amount
        let mut max_apr = 0;
        let mut max_index = 0;

        for j in 0..strategy_count {
            // Check silo's max debt
            if b[j].debt + deposit_unit > strategy_datas[j].max_debt {
                continue;
            }

            let apr = apr_after_debt_change(sturdy_datas[j], I256::from((b[j].debt + deposit_unit - strategy_datas[j].current_debt).as_u128() as i128)).as_u64();

            if max_apr >= apr {
                continue;
            }

            max_apr = apr;
            max_index = j;
        }

        if max_apr == 0 {
            println!("There is no max apr");
        }

        b[max_index].debt += deposit_unit;
    }

    // Make position array - first withdraw positions and next deposit positions.
    let mut deposits = Vec::new();
    let mut withdraws = Vec::new();

    for i in 0..strategy_count {
        let position = Position {
            strategy: initial_datas[i].strategy,
            debt: b[i].debt,
        };

        if strategy_datas[i].current_debt > b[i].debt {
            withdraws.push(position);
        } else {
            deposits.push(position);
        }
    }

    deposits.reverse();
    withdraws.extend(deposits);

    withdraws
}

fn get_current_and_new_apr(
    initial_datas: &Vec<Position>,
    sturdy_datas: &Vec<SturdyDataParams>,
    strategy_datas: &Vec<StrategyParams>,
    optimal_datas: &Vec<Position>
) -> (u64, u64) {
    let strategy_count = initial_datas.len();
    let mut total_amount = U256::from(0 as u128);
    let mut total_apr = U256::from(0 as u128);
    if optimal_datas.len() == 0 {
        return (0, 0);
    }

    // get current apr
    for i in 0..strategy_count {
        let apr = apr_after_debt_change(sturdy_datas[i], I256::from(0 as i128));
        total_apr += apr * strategy_datas[i].current_debt;
        total_amount += strategy_datas[i].current_debt;
    }
    let current_apr = if total_apr == U256::from(0 as u128) || total_amount == U256::from(0 as u128) {
        0
    } else {
        (total_apr / total_amount).as_u64()
    };

    total_amount = U256::from(0 as u128);
    total_apr = U256::from(0 as u128);
    // get new apr
    for i in 0..strategy_count {
        let mut index = strategy_count;
        for j in 0..strategy_count {
            if initial_datas[j].strategy == optimal_datas[i].strategy {
                index = j;
                break;
            }
        }
        
        if index == strategy_count {
            break;
        }

        let apr = apr_after_debt_change(sturdy_datas[index], I256::from(optimal_datas[i].debt.as_u128() as i128) - I256::from(strategy_datas[index].current_debt.as_u128() as i128));
        total_apr += apr * optimal_datas[i].debt;
        total_amount += optimal_datas[i].debt;
    }
    let new_apr = if total_apr == U256::from(0 as u128) || total_amount == U256::from(0 as u128) {
        0
    } else {
        (total_apr / total_amount).as_u64()
    };

    (current_apr, new_apr)
}

fn main() {
    // Read data sent from the application contract.
    let mut input_bytes = Vec::<u8>::new();
    env::stdin().read_to_end(&mut input_bytes).unwrap();
    // Type array passed to `ethabi::decode_whole` should match the types encoded in
    // the application contract.
    let input = ethabi::decode_whole(
        &[
            ParamType::Uint(256),      // chunk count
            ParamType::Uint(256),      // total initial amount
            ParamType::Uint(256),      // total available amount
            ParamType::Array(Box::new(ParamType::Tuple(vec![
                ParamType::Address,
                ParamType::Uint(256),
            ]))),                   // initial datas
            ParamType::Array(Box::new(ParamType::Tuple(vec![
                ParamType::Uint(256),
                ParamType::Uint(256),
                ParamType::Uint(256),
                ParamType::Uint(256),
            ]))),                   // strategy datas
            ParamType::Array(Box::new(ParamType::Tuple(vec![
                ParamType::Uint(256),
                ParamType::Uint(256),
                ParamType::Uint(256),
                ParamType::Uint(256),
                ParamType::Uint(256),
                ParamType::Uint(256),
                ParamType::Uint(256),
                ParamType::Uint(256),
                ParamType::Uint(256),
                ParamType::Uint(256),
                ParamType::Uint(256),
                ParamType::Uint(256),
                ParamType::Uint(256),
                ParamType::Uint(256),
                ParamType::Uint(256),
                ParamType::Uint(256),
                ParamType::Bool,
            ]))),                   // sturdy datas
        ],
        &input_bytes,
    )
    .unwrap();

    let chunk_count: U256 = input[0].clone().into_uint().unwrap();
    let total_initial_amount: U256 = input[1].clone().into_uint().unwrap();
    let total_available_amount: U256 = input[2].clone().into_uint().unwrap();
    let initial_datas: Vec<Position> = input[3].clone().into_array().unwrap().into_iter().map(|item| {
        let fields = item.into_tuple().unwrap();
        Position {
            strategy: fields[0].clone().into_address().unwrap(),
            debt: fields[1].clone().into_uint().unwrap(),
        }
    }).collect();
    let strategy_datas: Vec<StrategyParams> = input[4].clone().into_array().unwrap().into_iter().map(|item| {
        let fields = item.into_tuple().unwrap();
        StrategyParams {
            activation: fields[0].clone().into_uint().unwrap(),
            last_report: fields[1].clone().into_uint().unwrap(),
            current_debt: fields[2].clone().into_uint().unwrap(),
            max_debt: fields[3].clone().into_uint().unwrap()
        }
    }).collect();
    let sturdy_datas: Vec<SturdyDataParams> = input[5].clone().into_array().unwrap().into_iter().map(|item| {
        let fields = item.into_tuple().unwrap();
        SturdyDataParams {
            cur_timestamp: fields[0].clone().into_uint().unwrap(),
            last_timestamp: fields[1].clone().into_uint().unwrap(),
            rate_per_sec: fields[2].clone().into_uint().unwrap(),
            full_utilization_rate: fields[3].clone().into_uint().unwrap(),
            total_asset: fields[4].clone().into_uint().unwrap(),
            total_borrow: fields[5].clone().into_uint().unwrap(),
            util_prec: fields[6].clone().into_uint().unwrap(),
            min_target_util: fields[7].clone().into_uint().unwrap(),
            max_target_util: fields[8].clone().into_uint().unwrap(),
            vertex_utilization: fields[9].clone().into_uint().unwrap(),
            min_full_util_rate: fields[10].clone().into_uint().unwrap(),
            max_full_util_rate: fields[11].clone().into_uint().unwrap(),
            zero_util_rate: fields[12].clone().into_uint().unwrap(),
            rate_half_life: fields[13].clone().into_uint().unwrap(),
            vertex_rate_percent: fields[14].clone().into_uint().unwrap(),
            rate_prec: fields[15].clone().into_uint().unwrap(),
            is_interest_paused: fields[16].clone().into_bool().unwrap()
        }
    }).collect();

    let optimal_allocations: Vec<Position> = get_optimal_allocation(
        chunk_count.as_u64(), 
        total_initial_amount, 
        total_available_amount, 
        &initial_datas,
        &sturdy_datas, 
        &strategy_datas
    );

    let (current_apr, new_apr) = get_current_and_new_apr(
        &initial_datas, 
        &sturdy_datas,
        &strategy_datas, 
        &optimal_allocations
    );

    // Commit the journal that will be received by the application contract.
    // Encoded types should match the args expected by the application callback.
    if new_apr > current_apr {
        let result: Vec<Token> = optimal_allocations.iter().map(|allocation| {
            vec![
                Token::Address(allocation.strategy),
                Token::Uint(allocation.debt),
            ]
        }).flatten().collect();
        env::commit_slice(&ethabi::encode(&[
            Token::Array(result),
            Token::Uint(U256::from(new_apr)),
            Token::Uint(U256::from(current_apr)),
            Token::Bool(true)
        ]));
    } else {
        env::commit_slice(&ethabi::encode(&[
            Token::Array(vec![]),
            Token::Uint(U256::from(new_apr)),
            Token::Uint(U256::from(current_apr)),
            Token::Bool(false)
        ]));
    }
}
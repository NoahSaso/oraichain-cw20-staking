use crate::msg::LockInfo;
use crate::rewards::before_share_change;
use crate::state::{
    insert_lock_info, read_pool_info, read_unbonding_period, remove_and_accumulate_lock_info,
    rewards_read, rewards_store, stakers_store, store_pool_info, PoolInfo, RewardInfo,
    STAKED_BALANCES, STAKED_TOTAL,
};
use cosmwasm_std::{
    attr, to_binary, Addr, Api, CanonicalAddr, CosmosMsg, Decimal, DepsMut, Env, Response,
    StdError, StdResult, Storage, Uint128, WasmMsg,
};
use cw20::Cw20ExecuteMsg;
use oraiswap::asset::{self, Asset};

pub fn bond(
    deps: DepsMut,
    env: Env,
    staker_addr: Addr,
    staking_token: Addr,
    amount: Uint128,
) -> StdResult<Response> {
    let staker_addr_raw: CanonicalAddr = deps.api.addr_canonicalize(staker_addr.as_str())?;
    _increase_bond_amount(
        deps.storage,
        deps.api,
        env.block.height,
        &staker_addr_raw,
        staking_token.clone(),
        amount,
    )?;

    Ok(Response::new().add_attributes([
        ("action", "bond"),
        ("staker_addr", staker_addr.as_str()),
        ("staking_token", staking_token.as_str()),
        ("amount", &amount.to_string()),
    ]))
}

pub fn unbond(
    deps: DepsMut,
    env: Env,
    staker_addr: Addr,
    staking_token: Addr,
    amount: Uint128,
) -> StdResult<Response> {
    let staker_addr_raw: CanonicalAddr = deps.api.addr_canonicalize(staker_addr.as_str())?;
    let mut messages = vec![];
    let mut response = Response::new();
    let asset_key = deps.api.addr_canonicalize(staking_token.as_str())?;

    // withdraw_avaiable_lock
    let withdraw_response = _withdraw_lock(deps.storage, &env, &staker_addr, &staking_token)?;

    messages.extend(
        withdraw_response
            .clone()
            .messages
            .into_iter()
            .map(|msg| msg.msg)
            .collect::<Vec<CosmosMsg>>(),
    );

    let withdraw_attrs = withdraw_response.attributes;
    if !amount.is_zero() {
        let (_, reward_assets) = _decrease_bond_amount(
            deps.storage,
            deps.api,
            env.block.height,
            &staker_addr_raw,
            &staking_token,
            amount,
        )?;
        // withdraw pending_withdraw assets (accumulated when changing reward_per_sec)
        messages.extend(
            reward_assets
                .into_iter()
                .map(|ra| ra.into_msg(None, &deps.querier, staker_addr.clone()))
                .collect::<StdResult<Vec<CosmosMsg>>>()?,
        );
        // checking bonding period
        if let Ok(period) = read_unbonding_period(deps.storage, &asset_key) {
            let unlock_time = env.block.time.plus_seconds(period);
            insert_lock_info(
                deps.storage,
                staking_token.as_bytes(),
                staker_addr.as_bytes(),
                LockInfo {
                    amount,
                    unlock_time,
                },
            )?;

            response = response.add_attributes([
                attr("action", "unbonding"),
                attr("staker_addr", staker_addr.as_str()),
                attr("amount", amount.to_string()),
                attr("staking_token", staking_token.as_str()),
                attr("unlock_time", unlock_time.seconds().to_string()),
            ])
        } else {
            let unbond_response = _unbond(&staker_addr, &staking_token, amount)?;
            messages.extend(
                unbond_response
                    .messages
                    .into_iter()
                    .map(|msg| msg.msg)
                    .collect::<Vec<CosmosMsg>>(),
            );
            response = response.add_attributes(unbond_response.attributes);
        }
    }
    Ok(response
        .add_messages(messages)
        .add_attributes(withdraw_attrs))
}

pub fn _withdraw_lock(
    storage: &mut dyn Storage,
    env: &Env,
    staker_addr: &Addr,
    staking_token: &Addr,
) -> StdResult<Response> {
    // execute 10 lock a time
    let unlock_amount = remove_and_accumulate_lock_info(
        storage,
        staking_token.as_bytes(),
        staker_addr.as_bytes(),
        env.block.time,
    )?;

    if unlock_amount.is_zero() {
        return Ok(Response::new());
    }

    let unbond_response = _unbond(staker_addr, staking_token, unlock_amount)?;

    Ok(unbond_response)
}

fn _increase_bond_amount(
    storage: &mut dyn Storage,
    api: &dyn Api,
    height: u64,
    staker_addr: &CanonicalAddr,
    staking_token: Addr,
    amount: Uint128,
) -> StdResult<()> {
    let asset_key = api.addr_canonicalize(staking_token.as_str())?.to_vec();
    let mut pool_info = read_pool_info(storage, &asset_key)?;
    let mut reward_info: RewardInfo = rewards_read(storage, staker_addr)
        .load(&asset_key)
        .unwrap_or_else(|_| RewardInfo {
            native_token: false,
            index: Decimal::zero(),
            bond_amount: Uint128::zero(),
            pending_reward: Uint128::zero(),
            pending_withdraw: vec![],
        });

    // Withdraw reward to pending reward; before changing share
    before_share_change(pool_info.reward_index, &mut reward_info)?;

    // Increase total bond amount
    pool_info.total_bond_amount += amount;

    reward_info.bond_amount += amount;

    STAKED_BALANCES.update(
        storage,
        (&asset_key, &api.addr_humanize(staker_addr)?),
        height,
        |bal| -> StdResult<Uint128> { Ok(bal.unwrap_or_default().checked_add(amount)?) },
    )?;

    STAKED_TOTAL.update(storage, &asset_key, height, |total| -> StdResult<Uint128> {
        // Initialized during instantiate - OK to unwrap.
        Ok(total.unwrap_or_default().checked_add(amount)?)
    })?;

    rewards_store(storage, staker_addr).save(&asset_key, &reward_info)?;

    store_pool_info(storage, &asset_key, &pool_info)?;

    // mark this staker belong to the pool the first time
    let mut stakers_bucket = stakers_store(storage, &asset_key);
    if stakers_bucket.may_load(staker_addr)?.is_none() {
        stakers_bucket.save(staker_addr, &true)?;
    }

    Ok(())
}

fn _decrease_bond_amount(
    storage: &mut dyn Storage,
    api: &dyn Api,
    height: u64,
    staker_addr: &CanonicalAddr,
    staking_token: &Addr,
    amount: Uint128,
) -> StdResult<(CanonicalAddr, Vec<Asset>)> {
    let asset_key = api.addr_canonicalize(staking_token.as_str())?.to_vec();
    let mut pool_info: PoolInfo = read_pool_info(storage, &asset_key)?;
    let mut reward_info: RewardInfo = rewards_read(storage, staker_addr).load(&asset_key)?;
    let mut reward_assets = vec![];
    if reward_info.bond_amount < amount {
        return Err(StdError::generic_err("Cannot unbond more than bond amount"));
    }

    // if the lp token was migrated, and the user did not close their position yet, cap the reward at the snapshot
    let (pool_index, staking_token) = (pool_info.reward_index, pool_info.staking_token.clone());

    // Distribute reward to pending reward; before changing share
    before_share_change(pool_index, &mut reward_info)?;

    // Update rewards info
    reward_info.bond_amount = reward_info.bond_amount.checked_sub(amount)?;

    // Update pool_info
    pool_info.total_bond_amount = pool_info.total_bond_amount.checked_sub(amount)?;

    // update snapshot
    STAKED_BALANCES.update(
        storage,
        (&asset_key, &api.addr_humanize(staker_addr)?),
        height,
        |bal| -> StdResult<Uint128> { Ok(bal.unwrap_or_default().checked_sub(amount)?) },
    )?;
    STAKED_TOTAL.update(storage, &asset_key, height, |total| -> StdResult<Uint128> {
        // Initialized during instantiate - OK to unwrap.
        Ok(total.unwrap_or_default().checked_sub(amount)?)
    })?;

    if reward_info.pending_reward.is_zero() && reward_info.bond_amount.is_zero() {
        // if pending_withdraw is not empty, then return reward_assets to withdraw money
        reward_assets = reward_info
            .pending_withdraw
            .iter()
            .map(|ra| ra.to_normal(api))
            .collect::<StdResult<Vec<Asset>>>()?;
        reward_info.pending_withdraw = vec![];
    }
    rewards_store(storage, staker_addr).save(&asset_key, &reward_info)?;

    // Update pool info
    store_pool_info(storage, &asset_key, &pool_info)?;

    Ok((staking_token, reward_assets))
}

fn _unbond(staker_addr: &Addr, staking_token_addr: &Addr, amount: Uint128) -> StdResult<Response> {
    let messages: Vec<CosmosMsg> = vec![WasmMsg::Execute {
        contract_addr: staking_token_addr.to_string(),
        msg: to_binary(&Cw20ExecuteMsg::Transfer {
            recipient: staker_addr.to_string(),
            amount,
        })?,
        funds: vec![],
    }
    .into()];

    Ok(Response::new().add_messages(messages).add_attributes([
        attr("action", "unbond"),
        attr("staker_addr", staker_addr.as_str()),
        attr("amount", amount.to_string()),
        attr("staking_token", staking_token_addr.as_str()),
    ]))
}

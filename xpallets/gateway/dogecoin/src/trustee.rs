// Copyright 2019-2020 ChainX Project Authors. Licensed under GPL-3.0.
#![allow(clippy::type_complexity)]
extern crate alloc;

use alloc::string::ToString;
use frame_support::{
    dispatch::{DispatchError, DispatchResult},
    ensure,
};
use sp_runtime::SaturatedConversion;
use sp_std::{cmp::max, convert::TryFrom, prelude::*};

use light_bitcoin::{
    chain::Transaction,
    keys::{Address, Public},
    mast::{
        compute_min_threshold,
        p2sh::{generate_p2sh_address, generate_redeem_script},
    },
    primitives::Bytes,
    script::Script,
};

use xp_assets_registrar::Chain;
use xp_gateway_dogecoin::extract_output_addr;
use xpallet_gateway_common::{
    traits::{TrusteeForChain, TrusteeSession},
    trustees::dogecoin::{DogeTrusteeAddrInfo, DogeTrusteeType},
    types::{ScriptInfo, TrusteeInfoConfig, TrusteeIntentionProps, TrusteeSessionInfo},
    utils::{two_thirds_unsafe, MAX_TAPROOT_NODES},
};

use crate::{
    log,
    types::{DogeWithdrawalProposal, VoteResult},
    Config, Error, Event, Pallet, WithdrawalProposal,
};

pub fn current_trustee_session<T: Config>(
) -> Result<TrusteeSessionInfo<T::AccountId, T::BlockNumber, DogeTrusteeAddrInfo>, DispatchError> {
    T::TrusteeSessionProvider::current_trustee_session()
}

pub fn current_proxy_account<T: Config>() -> Result<Vec<T::AccountId>, DispatchError> {
    T::TrusteeSessionProvider::current_proxy_account()
}

#[inline]
fn current_trustee_addr_pair<T: Config>(
) -> Result<(DogeTrusteeAddrInfo, DogeTrusteeAddrInfo), DispatchError> {
    T::TrusteeSessionProvider::current_trustee_session()
        .map(|session_info| (session_info.hot_address, session_info.cold_address))
}

pub fn get_hot_trustee_address<T: Config>() -> Result<Address, DispatchError> {
    current_trustee_addr_pair::<T>()
        .and_then(|(addr_info, _)| Pallet::<T>::verify_doge_address(&addr_info.addr))
}

pub fn get_hot_trustee_redeem_script<T: Config>() -> Result<Script, DispatchError> {
    current_trustee_addr_pair::<T>().map(|(addr_info, _)| addr_info.redeem_script.into())
}

#[inline]
pub fn get_current_trustee_address_pair<T: Config>() -> Result<(Address, Address), DispatchError> {
    current_trustee_addr_pair::<T>().map(|(hot_info, cold_info)| {
        (
            Pallet::<T>::verify_doge_address(&hot_info.addr)
                .expect("should not parse error from storage data; qed"),
            Pallet::<T>::verify_doge_address(&cold_info.addr)
                .expect("should not parse error from storage data; qed"),
        )
    })
}

#[inline]
pub fn get_last_trustee_address_pair<T: Config>() -> Result<(Address, Address), DispatchError> {
    T::TrusteeSessionProvider::last_trustee_session().map(|session_info| {
        (
            Pallet::<T>::verify_doge_address(&session_info.hot_address.addr)
                .expect("should not parse error from storage data; qed"),
            Pallet::<T>::verify_doge_address(&session_info.cold_address.addr)
                .expect("should not parse error from storage data; qed"),
        )
    })
}

pub fn check_keys<T: Config>(keys: &[Public]) -> DispatchResult {
    let has_duplicate = (1..keys.len()).any(|i| keys[i..].contains(&keys[i - 1]));
    if has_duplicate {
        log!(
            error,
            "[generate_new_trustees] Keys contains duplicate pubkey"
        );
        return Err(Error::<T>::DuplicatedKeys.into());
    }
    let has_compressed_pubkey = keys
        .iter()
        .any(|public: &Public| matches!(public, Public::Compressed(_)));
    if has_compressed_pubkey {
        return Err("Unexpect! All keys(dogecoin Public) should be Normal".into());
    }
    Ok(())
}

//const EC_P = Buffer.from('fffffffffffffffffffffffffffffffffffffffffffffffffffffffefffffc2f', 'hex')
const EC_P: [u8; 32] = [
    255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
    255, 255, 255, 255, 255, 255, 255, 255, 254, 255, 255, 252, 47,
];

const ZERO_P: [u8; 32] = [0; 32];

impl<T: Config> TrusteeForChain<T::AccountId, T::BlockNumber, DogeTrusteeType, DogeTrusteeAddrInfo>
    for Pallet<T>
{
    fn check_trustee_entity(raw_addr: &[u8]) -> Result<DogeTrusteeType, DispatchError> {
        let trustee_type = DogeTrusteeType::try_from(raw_addr.to_vec())
            .map_err(|_| Error::<T>::InvalidPublicKey)?;
        let public = trustee_type.0;

        if public.len() != 65 {
            return Err(Error::<T>::InvalidPublicKey.into());
        }

        if 4 != raw_addr[0] {
            log!(error, "Not Full Public(prefix not 4)");
            return Err(Error::<T>::InvalidPublicKey.into());
        }

        if ZERO_P == raw_addr[1..33] {
            log!(error, "Not Public X(Zero32)");
            return Err(Error::<T>::InvalidPublicKey.into());
        }

        if raw_addr[1..33].to_vec() >= EC_P.to_vec() {
            log!(error, "Not Public X(EC_P)");
            return Err(Error::<T>::InvalidPublicKey.into());
        }

        Ok(DogeTrusteeType(public))
    }

    // generate dogecoin multi-sig address
    fn generate_trustee_session_info(
        props: Vec<(
            T::AccountId,
            TrusteeIntentionProps<T::AccountId, DogeTrusteeType>,
        )>,
        config: TrusteeInfoConfig,
    ) -> Result<
        (
            TrusteeSessionInfo<T::AccountId, T::BlockNumber, DogeTrusteeAddrInfo>,
            ScriptInfo<T::AccountId>,
        ),
        DispatchError,
    > {
        let (trustees, props_info): (
            Vec<T::AccountId>,
            Vec<TrusteeIntentionProps<T::AccountId, DogeTrusteeType>>,
        ) = props.into_iter().unzip();

        let (hot_keys, cold_keys): (Vec<Public>, Vec<Public>) = props_info
            .into_iter()
            .map(|props| (props.hot_entity.0, props.cold_entity.0))
            .unzip();

        // judge all props has different pubkey
        check_keys::<T>(&hot_keys)?;
        check_keys::<T>(&cold_keys)?;

        // [min, max] e.g. bitcoin min is 4, max is 15
        if (trustees.len() as u32) < config.min_trustee_count
            || (trustees.len() as u32) > config.max_trustee_count
        {
            log!(
                error,
                "[generate_trustee_session_info] Trustees {:?} is less/more than {{min:{}, max:{}}} people, \
                can't generate trustee addr",
                trustees, config.min_trustee_count, config.max_trustee_count
            );
            return Err(Error::<T>::InvalidTrusteeCount.into());
        }

        #[cfg(feature = "std")]
        let pretty_print_keys = |keys: &[Public]| {
            keys.iter()
                .map(|k| k.to_string().replace("\n", ""))
                .collect::<Vec<_>>()
                .join(", ")
        };
        #[cfg(feature = "std")]
        log!(
            info,
            "[generate_trustee_session_info] hot_keys:[{}], cold_keys:[{}]",
            pretty_print_keys(&hot_keys),
            pretty_print_keys(&cold_keys)
        );

        #[cfg(not(feature = "std"))]
        log!(
            info,
            "[generate_trustee_session_info] hot_keys:{:?}, cold_keys:{:?}",
            hot_keys,
            cold_keys
        );

        let sig_num = max(
            two_thirds_unsafe(trustees.len() as u32),
            compute_min_threshold(trustees.len() as u32, MAX_TAPROOT_NODES) as u32,
        );

        let hot_trustee_addr_info: DogeTrusteeAddrInfo =
            create_multi_address::<T>(&hot_keys, sig_num).ok_or_else(|| {
                log!(
                    error,
                    "[generate_trustee_session_info] Create cold_addr error, cold_keys:{:?}",
                    hot_keys
                );
                Error::<T>::GenerateMultisigFailed
            })?;

        let cold_trustee_addr_info: DogeTrusteeAddrInfo =
            create_multi_address::<T>(&cold_keys, sig_num).ok_or_else(|| {
                log!(
                    error,
                    "[generate_trustee_session_info] Create cold_addr error, cold_keys:{:?}",
                    cold_keys
                );
                Error::<T>::GenerateMultisigFailed
            })?;

        // Aggregate public key script and corresponding personal public key index
        let agg_pubkeys: Vec<Vec<u8>> = vec![];
        let personal_accounts: Vec<Vec<T::AccountId>> = vec![];

        log!(
            info,
            "[generate_trustee_session_info] hot_addr:{:?}, cold_addr:{:?}, trustee_list:{:?}",
            hot_trustee_addr_info,
            cold_trustee_addr_info,
            trustees
        );
        let start_height = frame_system::Pallet::<T>::block_number();
        let trustee_num = trustees.len();
        Ok((
            TrusteeSessionInfo {
                trustee_list: trustees
                    .into_iter()
                    .zip(vec![0u64; trustee_num])
                    .collect::<Vec<_>>(),
                multi_account: None,
                start_height: Some(start_height),
                threshold: sig_num as u16,
                hot_address: hot_trustee_addr_info,
                cold_address: cold_trustee_addr_info,
                end_height: None,
            },
            ScriptInfo {
                agg_pubkeys,
                personal_accounts,
            },
        ))
    }
}

impl<T: Config> Pallet<T> {
    pub fn ensure_trustee_or_bot(who: &T::AccountId) -> DispatchResult {
        match Self::coming_bot() {
            Some(n) if &n == who => return Ok(()),
            _ => (),
        }

        if current_proxy_account::<T>()?.iter().any(|n| n == who) {
            return Ok(());
        }

        let trustee_session_info = current_trustee_session::<T>()?;
        if trustee_session_info
            .trustee_list
            .iter()
            .any(|n| &n.0 == who)
        {
            Ok(())
        } else {
            log!(
                error,
                "[ensure_trustee_or_bot] Committer {:?} not in the trustee list:{:?}",
                who,
                trustee_session_info.trustee_list
            );
            Err(Error::<T>::NotTrustee.into())
        }
    }

    pub fn apply_create_dogecoin_withdraw(
        who: T::AccountId,
        tx: Transaction,
        withdrawal_id_list: Vec<u32>,
    ) -> DispatchResult {
        let withdraw_amount = Self::max_withdrawal_count();
        if withdrawal_id_list.len() > withdraw_amount as usize {
            log!(
                error,
                "[apply_create_withdraw] Current list (len:{}) exceeding the max withdrawal amount {}",
                withdrawal_id_list.len(), withdraw_amount
            );
            return Err(Error::<T>::WroungWithdrawalCount.into());
        }
        // remove duplicate
        let mut withdrawal_id_list = withdrawal_id_list;
        withdrawal_id_list.sort_unstable();
        withdrawal_id_list.dedup();

        check_withdraw_tx::<T>(&tx, &withdrawal_id_list)?;
        log!(
            info,
            "[apply_create_withdraw] Create new withdraw, id_list:{:?}",
            withdrawal_id_list
        );

        // check sig
        // if parse_check_taproot_tx::<T>(&tx, &spent_outputs).is_err() {
        //     return Err(Error::<T>::VerifySignFailed.into());
        // };

        xpallet_gateway_records::Pallet::<T>::process_withdrawals(
            &withdrawal_id_list,
            Chain::Dogecoin,
        )?;

        let proposal = DogeWithdrawalProposal::new(
            VoteResult::Finish,
            withdrawal_id_list.clone(),
            tx,
            Vec::new(),
        );

        log!(
            info,
            "[apply_create_withdraw] Pass the legality check of withdrawal"
        );

        Self::deposit_event(Event::<T>::WithdrawalProposalCreated(
            who,
            withdrawal_id_list,
        ));

        WithdrawalProposal::<T>::put(proposal);

        Ok(())
    }

    pub fn force_replace_withdraw_tx(tx: Transaction) -> DispatchResult {
        let mut proposal: DogeWithdrawalProposal<T::AccountId> =
            Self::withdrawal_proposal().ok_or(Error::<T>::NoProposal)?;

        ensure!(
            proposal.sig_state == VoteResult::Finish,
            "Only allow force change finished vote"
        );

        // make sure withdrawal list is same as current proposal
        let current_withdrawal_list = &proposal.withdrawal_id_list;
        check_withdraw_tx_impl::<T>(&tx, current_withdrawal_list)?;

        // check sig
        // if parse_check_taproot_tx::<T>(&tx, &spent_outputs).is_err() {
        //     return Err(Error::<T>::VerifySignFailed.into());
        // };

        // replace old transaction
        proposal.tx = tx;

        WithdrawalProposal::<T>::put(proposal);
        Ok(())
    }
}

/// Get the required number of signatures
/// sig_num: Number of signatures required
/// trustee_num: Total number of multiple signatures
/// NOTE: Signature ratio greater than 2/3
pub fn get_sig_num<T: Config>() -> (u32, u32) {
    let trustee_list = T::TrusteeSessionProvider::current_trustee_session()
        .map(|session_info| session_info.trustee_list)
        .expect("the trustee_list must exist; qed");
    let trustee_num = trustee_list.len() as u32;
    (two_thirds_unsafe(trustee_num), trustee_num)
}

pub(crate) fn create_multi_address<T: Config>(
    pubkeys: &[Public],
    sig_num: u32,
) -> Option<DogeTrusteeAddrInfo> {
    if let Ok(redeem_script) = generate_redeem_script(pubkeys.to_vec(), sig_num) {
        let addr = generate_p2sh_address(&redeem_script, Pallet::<T>::network_id());

        let script_bytes: Bytes = redeem_script.into();
        Some(DogeTrusteeAddrInfo {
            addr: addr.into_bytes(),
            redeem_script: script_bytes.into(),
        })
    } else {
        None
    }
}

/// Check that the cash withdrawal transaction is correct
pub fn check_withdraw_tx<T: Config>(
    tx: &Transaction,
    withdrawal_id_list: &[u32],
) -> DispatchResult {
    match Pallet::<T>::withdrawal_proposal() {
        Some(_) => Err(Error::<T>::NotFinishProposal.into()),
        None => check_withdraw_tx_impl::<T>(tx, withdrawal_id_list),
    }
}

fn check_withdraw_tx_impl<T: Config>(
    tx: &Transaction,
    withdrawal_id_list: &[u32],
) -> DispatchResult {
    // withdrawal addr list for account withdrawal application
    let mut appl_withdrawal_list: Vec<(Address, u64)> = Vec::new();
    for withdraw_index in withdrawal_id_list.iter() {
        let record = xpallet_gateway_records::Pallet::<T>::pending_withdrawals(withdraw_index)
            .ok_or(Error::<T>::NoWithdrawalRecord)?;
        // record.addr() is base58
        // verify Doge address would conveRelayedTx a base58 addr to Address
        let addr: Address = Pallet::<T>::verify_doge_address(record.addr())?;

        appl_withdrawal_list.push((addr, record.balance().saturated_into::<u64>()));
    }
    // not allow deposit directly to cold address, only hot address allow
    let hot_trustee_address: Address = get_hot_trustee_address::<T>()?;
    // withdrawal addr list for tx outputs
    let doge_withdrawal_fee = Pallet::<T>::doge_withdrawal_fee();
    let doge_network = Pallet::<T>::network_id();
    let mut tx_withdraw_list = Vec::new();
    for output in &tx.outputs {
        let addr = extract_output_addr(output, doge_network).ok_or("not found addr in this out")?;
        if addr.hash != hot_trustee_address.hash {
            // expect change to trustee_addr output
            tx_withdraw_list.push((addr, output.value + doge_withdrawal_fee));
        }
    }

    tx_withdraw_list.sort();
    appl_withdrawal_list.sort();

    // appl_withdrawal_list must match to tx_withdraw_list
    if appl_withdrawal_list.len() != tx_withdraw_list.len() {
        log!(
            error,
            "Withdrawal tx's outputs (len:{}) != withdrawal application list (len:{}), \
            withdrawal tx's outputs:{:?}, withdrawal application list:{:?}",
            tx_withdraw_list.len(),
            appl_withdrawal_list.len(),
            tx_withdraw_list,
            withdrawal_id_list
                .iter()
                .zip(appl_withdrawal_list)
                .collect::<Vec<_>>()
        );
        return Err(Error::<T>::TxOutputsNotMatch.into());
    }

    let count = appl_withdrawal_list
        .iter()
        .zip(tx_withdraw_list)
        .filter(|(a, b)| {
            if a.0.hash == b.0.hash && a.1 == b.1 {
                true
            } else {
                log!(
                    error,
                    "Withdrawal tx's output not match to withdrawal application. \
                    withdrawal application:{:?}, tx withdrawal output:{:?}",
                    a,
                    b
                );
                false
            }
        })
        .count();

    if count != appl_withdrawal_list.len() {
        return Err(Error::<T>::TxOutputsNotMatch.into());
    }

    Ok(())
}

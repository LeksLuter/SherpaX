// Copyright 2019-2021 ChainX Project Authors. Licensed under GPL-3.0.

//! this module is for dogecoin-bridge

#![cfg_attr(not(feature = "std"), no_std)]

mod header;
pub mod migrations;
pub mod trustee;
mod tx;
pub mod types;
pub mod weights;

#[cfg(any(feature = "runtime-benchmarks", test))]
mod benchmarking;
#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

use sp_runtime::SaturatedConversion;
use sp_std::{marker::PhantomData, prelude::*};

#[cfg(feature = "std")]
pub use light_bitcoin::primitives::h256_rev;
pub use light_bitcoin::{
    chain::{BlockHeader, Transaction, TransactionOutputArray},
    keys::{Address, DisplayLayout, Network as DogecoinNetwork},
    primitives::{hash_rev, Compact, H256, H264},
    serialization::{deserialize, Reader},
};

use sherpax_primitives::ReferralId;
use xp_assets_registrar::Chain;
use xp_gateway_common::AccountExtractor;

use xpallet_gateway_common::{
    traits::{
        AddressBinding, ProposalProvider, ReferralBinding, TotalSupply, TrusteeInfoUpdate,
        TrusteeSession,
    },
    trustees::dogecoin::DogeTrusteeAddrInfo,
};
use xpallet_gateway_records::{ChainT, WithdrawalLimit};
use xpallet_support::try_addr;

use self::{
    trustee::{get_current_trustee_address_pair, get_last_trustee_address_pair},
    tx::remove_pending_deposit,
    types::{
        DogeDepositCache, DogeHeaderIndex, DogeRelayedTx, DogeRelayedTxInfo, DogeTxResult,
        DogeTxState,
    },
};

pub use self::{
    types::{DogeAddress, DogeHeaderInfo, DogeParams, DogeWithdrawalProposal},
    weights::WeightInfo,
};

pub use pallet::*;

// syntactic sugar for native log.
#[macro_export]
macro_rules! log {
    ($level:tt, $patter:expr $(, $values:expr)* $(,)?) => {
        frame_support::log::$level!(
            target: "runtime::bitcoin",
            $patter $(, $values)*
        )
    };
}

#[frame_support::pallet]
pub mod pallet {
    use super::*;
    use frame_support::{
        dispatch::DispatchResult, pallet_prelude::*, traits::UnixTime, transactional,
    };
    use frame_system::pallet_prelude::*;

    #[pallet::pallet]
    #[pallet::generate_store(pub(crate) trait Store)]
    #[pallet::without_storage_info]
    pub struct Pallet<T>(PhantomData<T>);

    #[pallet::config]
    pub trait Config:
        frame_system::Config + pallet_assets::Config + xpallet_gateway_records::Config
    {
        /// The overarching event type.
        type Event: From<Event<Self>> + IsType<<Self as frame_system::Config>::Event>;

        /// The unix time type.
        type UnixTime: UnixTime;

        /// A majority of the council can excute some transactions.
        type CouncilOrigin: EnsureOrigin<Self::Origin>;

        /// Extract the account and possible extra from the data.
        type AccountExtractor: AccountExtractor<Self::AccountId, ReferralId>;

        /// Get information about the trustee.
        type TrusteeSessionProvider: TrusteeSession<
            Self::AccountId,
            Self::BlockNumber,
            DogeTrusteeAddrInfo,
        >;

        /// Update information about the trustee.
        type TrusteeInfoUpdate: TrusteeInfoUpdate;

        /// Handle referral of assets across chains.
        type ReferralBinding: ReferralBinding<Self::AccountId, Self::AssetId>;

        /// Handle address binding about pending deposit.
        type AddressBinding: AddressBinding<Self::AccountId, DogeAddress>;

        /// Weight information for extrinsics in this pallet.
        type WeightInfo: WeightInfo;
    }

    #[pallet::call]
    impl<T: Config> Pallet<T> {
        /// if use `DogeHeader` struct would export in metadata, cause complex in front-end
        #[pallet::weight(<T as Config>::WeightInfo::push_header())]
        pub fn push_header(origin: OriginFor<T>, header: Vec<u8>) -> DispatchResultWithPostInfo {
            let from = ensure_signed(origin)?;
            let header: BlockHeader =
                deserialize(header.as_slice()).map_err(|_| Error::<T>::DeserializeErr)?;
            log!(debug, "[push_header] from:{:?}, header:{:?}", from, header);

            Self::apply_push_header(header)?;

            // Relayer does not pay a fee.
            Ok(Pays::No.into())
        }

        /// if use `RelayTx` struct would export in metadata, cause complex in front-end
        #[pallet::weight(<T as Config>::WeightInfo::push_transaction())]
        pub fn push_transaction(
            origin: OriginFor<T>,
            raw_tx: Vec<u8>,
            relayed_info: Vec<u8>,
            prev_tx: Option<Vec<u8>>,
        ) -> DispatchResultWithPostInfo {
            let _from = ensure_signed(origin)?;
            let raw_tx = Self::deserialize_tx(raw_tx.as_slice())?;
            let relayed_info: DogeRelayedTxInfo =
                Decode::decode(&mut &relayed_info[..]).map_err(|_| Error::<T>::DeserializeErr)?;
            let prev_tx = if let Some(prev_tx) = prev_tx {
                Some(Self::deserialize_tx(prev_tx.as_slice())?)
            } else {
                None
            };
            let relay_tx = relayed_info.into_relayed_tx(raw_tx);
            log!(
                debug,
                "[push_transaction] from:{:?}, relay_tx:{:?}, prev_tx:{:?}",
                _from,
                relay_tx,
                prev_tx
            );

            Self::apply_push_transaction(relay_tx, prev_tx)?;

            Ok(Pays::No.into())
        }

        /// Trustee create a proposal for a withdrawal list. `tx` is the proposal withdrawal transaction.
        #[pallet::weight(<T as Config>::WeightInfo::create_taproot_withdraw_tx())]
        pub fn create_dogecoin_withdraw_tx(
            origin: OriginFor<T>,
            withdrawal_id_list: Vec<u32>,
            tx: Vec<u8>,
        ) -> DispatchResultWithPostInfo {
            let from = ensure_signed(origin)?;

            ensure!(
                !T::TrusteeSessionProvider::trustee_transition_state(Chain::Dogecoin),
                Error::<T>::TrusteeTransitionPeriod
            );

            // committer must be in the trustee list or coming bot
            Self::ensure_trustee_or_bot(&from)?;

            let tx = Self::deserialize_tx(tx.as_slice())?;
            log!(
                debug,
                "[create_dogecoin_withdraw_tx] from:{:?}, withdrawal list:{:?}, tx:{:?}",
                from,
                withdrawal_id_list,
                tx
            );

            Self::apply_create_dogecoin_withdraw(from, tx, withdrawal_id_list)?;
            Ok(Pays::No.into())
        }

        /// Dangerous! Be careful to set BestIndex
        #[pallet::weight(<T as Config>::WeightInfo::set_best_index())]
        pub fn set_best_index(origin: OriginFor<T>, index: DogeHeaderIndex) -> DispatchResult {
            ensure_root(origin)?;
            BestIndex::<T>::put(index);
            Ok(())
        }

        /// Dangerous! Be careful to set ConfirmedIndex
        #[pallet::weight(<T as Config>::WeightInfo::set_confirmed_index())]
        pub fn set_confirmed_index(origin: OriginFor<T>, index: DogeHeaderIndex) -> DispatchResult {
            ensure_root(origin)?;
            ConfirmedIndex::<T>::put(index);
            Ok(())
        }

        /// Dangerous! Be careful to set ConfirmedIndex
        #[pallet::weight(1_000_000u64)]
        pub fn set_confirmed_number(origin: OriginFor<T>, number: u32) -> DispatchResult {
            ensure_root(origin)?;
            ConfirmationNumber::<T>::put(number);
            Ok(())
        }

        /// Allow root or trustees could remove pending deposits for an address and decide whether
        /// deposit to an account id. if pass `None` to `who`, would just remove pendings, if pass
        /// Some, would deposit to this account id.
        #[pallet::weight(<T as Config>::WeightInfo::remove_pending())]
        pub fn remove_pending(
            origin: OriginFor<T>,
            addr: DogeAddress,
            who: Option<T::AccountId>,
        ) -> DispatchResult {
            T::CouncilOrigin::try_origin(origin)
                .map(|_| ())
                .or_else(ensure_root)?;

            if let Some(w) = who {
                remove_pending_deposit::<T>(&addr, &w);
            } else {
                log!(info, "[remove_pending] Release pending deposit directly, not deposit to someone, addr:{:?}", try_addr(&addr));
                PendingDeposits::<T>::remove(&addr);
            }
            Ok(())
        }

        /// Dangerous! remove current withdrawal proposal directly. Please check business logic before
        /// do this operation.
        #[pallet::weight(<T as Config>::WeightInfo::remove_proposal())]
        #[transactional]
        pub fn remove_proposal(origin: OriginFor<T>) -> DispatchResult {
            T::CouncilOrigin::try_origin(origin)
                .map(|_| ())
                .or_else(ensure_root)?;
            Self::apply_remove_proposal()
        }

        /// Set dogecoin withdrawal fee
        #[pallet::weight(<T as Config>::WeightInfo::set_doge_withdrawal_fee())]
        pub fn set_doge_withdrawal_fee(
            origin: OriginFor<T>,
            #[pallet::compact] fee: u64,
        ) -> DispatchResult {
            T::CouncilOrigin::try_origin(origin)
                .map(|_| ())
                .or_else(ensure_root)?;
            DogeWithdrawalFee::<T>::put(fee);
            Ok(())
        }

        /// Set dogecoin deposit limit
        #[pallet::weight(<T as Config>::WeightInfo::set_doge_deposit_limit())]
        pub fn set_doge_deposit_limit(
            origin: OriginFor<T>,
            #[pallet::compact] value: u64,
        ) -> DispatchResult {
            T::CouncilOrigin::try_origin(origin)
                .map(|_| ())
                .or_else(ensure_root)?;
            DogeMinDeposit::<T>::put(value);
            Ok(())
        }

        /// Set coming bot
        #[pallet::weight(<T as Config>::WeightInfo::set_coming_bot())]
        pub fn set_coming_bot(origin: OriginFor<T>, bot: Option<T::AccountId>) -> DispatchResult {
            T::CouncilOrigin::try_origin(origin)
                .map(|_| ())
                .or_else(ensure_root)?;
            match bot {
                None => ComingBot::<T>::kill(),
                Some(n) => ComingBot::<T>::put(n),
            }
            Ok(())
        }
    }

    /// Error for the XBridge Bitcoin module
    #[pallet::error]
    pub enum Error<T> {
        /// Parse base58 addr error
        InvalidBase58,
        /// Load addr from bytes error
        InvalidAddr,
        /// Can't find the best header in chain or it's invalid
        InvalidBestIndex,
        /// Invalid proof-of-work (Block hash does not satisfy nBits)
        InvalidPoW,
        /// Fork is too long to proceed
        AncientFork,
        /// Previous tx id not equal input point hash
        InvalidPrevTx,
        /// Futuristic timestamp
        HeaderFuturisticTimestamp,
        /// nBits do not match difficulty rules
        HeaderNBitsNotMatch,
        /// Unknown parent
        HeaderUnknownParent,
        /// Not Found
        HeaderNotFound,
        /// Ancient fork
        HeaderAncientFork,
        /// Header already exists
        ExistingHeader,
        /// Can't find previous header
        PrevHeaderNotExisted,
        /// Cannot deserialize the header or tx vec
        DeserializeErr,
        /// Merkle proof is invalid
        BadMerkleProof,
        /// The tx is not yet confirmed, i.e, the block of which is not confirmed.
        UnconfirmedTx,
        /// Reject replay proccessed tx
        ReplayedTx,
        /// Process tx failed
        ProcessTxFailed,
        /// Withdraw tx not match expected tx
        MismatchedTx,
        /// Invalid bitcoin address
        InvalidAddress,
        /// Verify tx signature failed
        VerifySignFailed,
        /// Invalid sign count in trustee withdrawal tx proposal
        InvalidSignCount,
        /// Invalid bitcoin public key
        InvalidPublicKey,
        /// Construct bad signature
        ConstructBadSign,
        /// Invalid signature
        BadSignature,
        /// Parse redeem script failed
        BadRedeemScript,
        /// Not set trustee yet
        NotTrustee,
        /// Duplicated pubkey for trustees
        DuplicatedKeys,
        /// Can't generate multisig address
        GenerateMultisigFailed,
        /// Invalid trustee count
        InvalidTrusteeCount,
        /// Unexpected withdraw records count
        WroungWithdrawalCount,
        /// Reject sig for current proposal
        RejectSig,
        /// No proposal for current withdrawal
        NoProposal,
        /// Tx's outputs not match withdrawal id list
        TxOutputsNotMatch,
        /// Last proposal not finished yet
        NotFinishProposal,
        /// No withdrawal record for this id
        NoWithdrawalRecord,
        /// Already vote for this withdrawal proposal
        DuplicateVote,
        /// Trustee transition period
        TrusteeTransitionPeriod,
        /// The output address must be a cold address during the trust transition process
        TxOutputNotColdAddr,
        /// The total amount of the trust must be transferred out in full
        TxNotFullAmount,
    }

    #[pallet::event]
    #[pallet::generate_deposit(pub(crate) fn deposit_event)]
    pub enum Event<T: Config> {
        /// A Bitcoin header was validated and inserted. [Doge_header_hash]
        HeaderInserted(H256),
        /// A Bitcoin transaction was processed. [tx_hash, block_hash, tx_state]
        TxProcessed(H256, H256, DogeTxState),
        /// An account deposited some token. [tx_hash, who, amount]
        Deposited(H256, T::AccountId, T::Balance),
        /// A list of withdrawal applications were processed successfully. [tx_hash, withdrawal_ids, total_withdrawn]
        Withdrawn(H256, Vec<u32>, T::Balance),
        /// A new record of unclaimed deposit. [tx_hash, Doge_address]
        UnclaimedDeposit(H256, DogeAddress),
        /// A unclaimed deposit record was removed. [depositor, deposit_amount, tx_hash, Doge_address]
        PendingDepositRemoved(T::AccountId, T::Balance, H256, DogeAddress),
        /// A new withdrawal proposal was created. [proposer, withdrawal_ids]
        WithdrawalProposalCreated(T::AccountId, Vec<u32>),
        /// A trustee voted/vetoed a withdrawal proposal. [trustee, vote_status]
        WithdrawalProposalVoted(T::AccountId, bool),
        /// A withdrawal proposal was dropped. [reject_count, total_count, withdrawal_ids]
        WithdrawalProposalDropped(u32, u32, Vec<u32>),
        /// The proposal has been processed successfully and is waiting for broadcasting. [tx_hash]
        WithdrawalProposalCompleted(H256),
        /// A fatal error happened during the withdrwal process. [tx_hash, proposal_hash]
        WithdrawalFatalErr(H256, H256),
    }

    /// best header info
    #[pallet::storage]
    #[pallet::getter(fn best_index)]
    pub(crate) type BestIndex<T: Config> = StorageValue<_, DogeHeaderIndex, ValueQuery>;

    /// confirmed header info
    #[pallet::storage]
    #[pallet::getter(fn confirmed_index)]
    pub(crate) type ConfirmedIndex<T: Config> = StorageValue<_, DogeHeaderIndex>;

    /// block hash list for a height, include forked header hash
    #[pallet::storage]
    #[pallet::getter(fn block_hash_for)]
    pub(crate) type BlockHashFor<T: Config> =
        StorageMap<_, Twox64Concat, u32, Vec<H256>, ValueQuery>;

    /// mark this blockhash is in mainchain
    #[pallet::storage]
    #[pallet::getter(fn main_chain)]
    pub(crate) type MainChain<T: Config> = StorageMap<_, Identity, H256, bool, ValueQuery>;

    /// all valid blockheader (include forked blockheader)
    #[pallet::storage]
    #[pallet::getter(fn headers)]
    pub(crate) type Headers<T: Config> = StorageMap<_, Identity, H256, DogeHeaderInfo>;

    /// mark tx has been handled, in case re-handle this tx, and log handle result
    #[pallet::storage]
    #[pallet::getter(fn tx_state)]
    pub(crate) type TxState<T: Config> = StorageMap<_, Identity, H256, DogeTxState>;

    /// unclaimed deposit info, addr => tx_hash, Doge value,
    #[pallet::storage]
    #[pallet::getter(fn pending_deposits)]
    pub(crate) type PendingDeposits<T: Config> =
        StorageMap<_, Blake2_128Concat, DogeAddress, Vec<DogeDepositCache>, ValueQuery>;

    /// withdrawal tx outs for account, tx_hash => outs ( out index => withdrawal account )
    #[pallet::storage]
    #[pallet::getter(fn withdrawal_proposal)]
    pub(crate) type WithdrawalProposal<T: Config> =
        StorageValue<_, DogeWithdrawalProposal<T::AccountId>>;

    /// get GenesisInfo (header, height)
    #[pallet::storage]
    #[pallet::getter(fn genesis_info)]
    pub(crate) type GenesisInfo<T: Config> = StorageValue<_, (BlockHeader, u32), ValueQuery>;

    /// get ParamsInfo from genesis_config
    #[pallet::storage]
    #[pallet::getter(fn params_info)]
    pub(crate) type ParamsInfo<T: Config> = StorageValue<_, DogeParams, ValueQuery>;

    ///  NetworkId for testnet or mainnet
    #[pallet::storage]
    #[pallet::getter(fn network_id)]
    pub(crate) type NetworkId<T: Config> = StorageValue<_, DogecoinNetwork, ValueQuery>;

    /// get ConfirmationNumber from genesis_config
    #[pallet::storage]
    #[pallet::getter(fn confirmation_number)]
    pub(crate) type ConfirmationNumber<T: Config> = StorageValue<_, u32, ValueQuery>;

    /// get DogeWithdrawalFee from genesis_config
    #[pallet::storage]
    #[pallet::getter(fn doge_withdrawal_fee)]
    pub(crate) type DogeWithdrawalFee<T: Config> = StorageValue<_, u64, ValueQuery>;

    #[pallet::type_value]
    pub fn DefaultForMinDeposit<T: Config>() -> u64 {
        100000
    }

    /// min deposit value limit, default is 10w sotashi(0.001 Doge)
    #[pallet::storage]
    #[pallet::getter(fn doge_min_deposit)]
    pub(crate) type DogeMinDeposit<T: Config> =
        StorageValue<_, u64, ValueQuery, DefaultForMinDeposit<T>>;

    /// max withdraw account count in dogeocin withdrawal transaction
    #[pallet::storage]
    #[pallet::getter(fn max_withdrawal_count)]
    pub(crate) type MaxWithdrawalCount<T: Config> = StorageValue<_, u32, ValueQuery>;

    /// Coming bot helps update dogecoin withdrawal transaction status
    #[pallet::storage]
    #[pallet::getter(fn coming_bot)]
    pub(crate) type ComingBot<T: Config> = StorageValue<_, T::AccountId, OptionQuery>;

    #[pallet::genesis_config]
    pub struct GenesisConfig<T: Config> {
        pub genesis_hash: H256,
        pub genesis_info: (BlockHeader, u32),
        pub genesis_trustees: Vec<T::AccountId>,
        pub params_info: DogeParams,
        pub network_id: DogecoinNetwork,
        pub confirmation_number: u32,
        pub doge_withdrawal_fee: u64,
        pub max_withdrawal_count: u32,
    }

    #[cfg(feature = "std")]
    impl<T: Config> Default for GenesisConfig<T> {
        fn default() -> Self {
            Self {
                genesis_hash: Default::default(),
                genesis_info: Default::default(),
                genesis_trustees: Default::default(),
                params_info: Default::default(),
                network_id: Default::default(),
                confirmation_number: Default::default(),
                doge_withdrawal_fee: Default::default(),
                max_withdrawal_count: Default::default(),
            }
        }
    }

    #[pallet::genesis_build]
    #[cfg(feature = "std")]
    impl<T: Config> GenesisBuild<T> for GenesisConfig<T> {
        fn build(&self) {
            let genesis_hash = &self.genesis_hash.clone();
            let (genesis_header, genesis_height) = &self.genesis_info.clone();
            let genesis_index = DogeHeaderIndex {
                hash: *genesis_hash,
                height: *genesis_height,
            };
            let header_info = DogeHeaderInfo {
                header: *genesis_header,
                height: *genesis_height,
            };

            Headers::<T>::insert(&genesis_hash, header_info);
            BlockHashFor::<T>::insert(&genesis_index.height, vec![genesis_hash]);
            MainChain::<T>::insert(&genesis_hash, true);
            BestIndex::<T>::put(genesis_index);
            GenesisInfo::<T>::put(self.genesis_info);
            ParamsInfo::<T>::put(self.params_info);
            NetworkId::<T>::put(self.network_id);
            ConfirmationNumber::<T>::put(self.confirmation_number);
            DogeWithdrawalFee::<T>::put(self.doge_withdrawal_fee);
            MaxWithdrawalCount::<T>::put(self.max_withdrawal_count);

            // init trustee (not this action should ha)
            if !self.genesis_trustees.is_empty() {
                T::TrusteeSessionProvider::genesis_trustee(
                    Pallet::<T>::chain(),
                    &self.genesis_trustees,
                );
            }
        }
    }

    impl<T: Config> ChainT<T::AssetId, T::Balance> for Pallet<T> {
        fn chain() -> Chain {
            Chain::Dogecoin
        }

        fn check_addr(addr: &[u8], _: &[u8]) -> DispatchResult {
            // this addr is base58 addr
            let address = Self::verify_doge_address(addr).map_err(|err| {
                log!(
                    error,
                    "[verify_doge_address] Verify failed, error:{:?}, source addr:{:?}",
                    err,
                    xpallet_support::try_addr(addr)
                );
                err
            })?;

            match get_current_trustee_address_pair::<T>() {
                Ok((hot_addr, cold_addr)) => {
                    // do not allow withdraw from trustee address
                    if address == hot_addr || address == cold_addr {
                        return Err(Error::<T>::InvalidAddress.into());
                    }
                }
                Err(err) => {
                    log!(error, "[check_addr] Can not get trustee addr:{:?}", err);
                }
            }

            Ok(())
        }

        fn withdrawal_limit(
            asset_id: &T::AssetId,
        ) -> Result<WithdrawalLimit<T::Balance>, DispatchError> {
            if *asset_id != T::DogeAssetId::get() {
                return Err(pallet_assets::Error::<T>::Unknown.into());
            }
            let fee = Self::doge_withdrawal_fee().saturated_into();
            let limit = WithdrawalLimit::<T::Balance> {
                minimal_withdrawal: fee * 3u32.saturated_into() / 2u32.saturated_into(),
                fee,
            };
            Ok(limit)
        }
    }

    impl<T: Config> TotalSupply<T::Balance> for Pallet<T> {
        fn total_supply() -> T::Balance {
            let pending_deposits: T::Balance = PendingDeposits::<T>::iter_values()
                .map(|deposits| {
                    deposits
                        .into_iter()
                        .map(|deposit| deposit.balance)
                        .sum::<u64>()
                })
                .sum::<u64>()
                .saturated_into();

            let asset_id = T::DogeAssetId::get();
            let asset_supply = pallet_assets::Pallet::<T>::total_supply(asset_id);
            asset_supply + pending_deposits
        }
    }

    impl<T: Config> ProposalProvider for Pallet<T> {
        type WithdrawalProposal = DogeWithdrawalProposal<T::AccountId>;
        fn get_withdrawal_proposal() -> Option<Self::WithdrawalProposal> {
            Self::withdrawal_proposal()
        }
    }

    /// Storage Query RPCs
    impl<T: Config> Pallet<T> {
        /// Get withdrawal proposal
        pub fn get_withdrawal_proposal() -> Option<DogeWithdrawalProposal<T::AccountId>> {
            Self::withdrawal_proposal()
        }

        /// Get genesis info
        pub fn get_genesis_info() -> (BlockHeader, u32) {
            Self::genesis_info()
        }

        /// Ger Doge block headers
        pub fn get_doge_block_header(txid: H256) -> Option<DogeHeaderInfo> {
            Self::headers(txid)
        }
    }

    impl<T: Config> Pallet<T> {
        pub fn verify_doge_address(data: &[u8]) -> Result<Address, DispatchError> {
            let r = bs58::decode(data)
                .into_vec()
                .map_err(|_| Error::<T>::InvalidBase58)?;
            let addr = Address::from_layout(&r).map_err(|_| Error::<T>::InvalidAddr)?;
            Ok(addr)
        }

        pub fn verify_tx_valid(
            raw_tx: Vec<u8>,
            withdrawal_id_list: Vec<u32>,
            full_amount: bool,
        ) -> Result<bool, DispatchError> {
            let tx = Self::deserialize_tx(raw_tx.as_slice())?;

            let current_trustee_pair = get_current_trustee_address_pair::<T>()?;
            let all_outputs_is_trustee = tx
                .outputs
                .iter()
                .map(|output| {
                    xp_gateway_dogecoin::extract_output_addr(output, NetworkId::<T>::get())
                        .unwrap_or_default()
                })
                .all(|addr| xp_gateway_dogecoin::is_trustee_addr(addr, current_trustee_pair));

            // check trustee transition status
            if T::TrusteeSessionProvider::trustee_transition_state(Chain::Bitcoin) {
                // check trustee transition tx
                // tx output address = new hot address
                let prev_trustee_pair = get_last_trustee_address_pair::<T>()?;
                let all_outputs_is_current_cold_address = tx
                    .outputs
                    .iter()
                    .map(|output| {
                        xp_gateway_dogecoin::extract_output_addr(output, NetworkId::<T>::get())
                            .unwrap_or_default()
                    })
                    .all(|addr| addr.hash == current_trustee_pair.1.hash);

                let all_outputs_is_prev_cold_address = tx
                    .outputs
                    .iter()
                    .map(|output| {
                        xp_gateway_dogecoin::extract_output_addr(output, NetworkId::<T>::get())
                            .unwrap_or_default()
                    })
                    .all(|addr| addr.hash == prev_trustee_pair.1.hash);

                // ensure that all outputs are cold addresses
                ensure!(
                    all_outputs_is_current_cold_address || all_outputs_is_prev_cold_address,
                    Error::<T>::TxOutputNotColdAddr
                );
                // ensure that all amounts are sent
                ensure!(full_amount, Error::<T>::TxNotFullAmount);

                Ok(true)
            } else if all_outputs_is_trustee {
                Ok(true)
            } else {
                // check normal withdrawal tx
                trustee::check_withdraw_tx::<T>(&tx, &withdrawal_id_list)?;
                Ok(true)
            }
        }

        /// Helper function for deserializing the slice of raw tx.
        #[inline]
        pub(crate) fn deserialize_tx(input: &[u8]) -> Result<Transaction, Error<T>> {
            deserialize(Reader::new(input)).map_err(|_| Error::<T>::DeserializeErr)
        }

        pub(crate) fn apply_remove_proposal() -> DispatchResult {
            if let Some(proposal) = WithdrawalProposal::<T>::take() {
                for id in proposal.withdrawal_id_list.iter() {
                    xpallet_gateway_records::Pallet::<T>::set_withdrawal_state_by_root(
                        *id,
                        xpallet_gateway_records::WithdrawalState::Applying,
                    )?;
                }
            }
            Ok(())
        }

        #[transactional]
        pub(crate) fn apply_push_header(header: BlockHeader) -> DispatchResult {
            // current should not exist
            if Self::headers(&header.hash()).is_some() {
                log!(
                    error,
                    "[apply_push_header] The doge header already exists, hash:{:?}",
                    header.hash()
                );
                return Err(Error::<T>::ExistingHeader.into());
            }
            // prev header should exist, thus we reject orphan block
            let prev_info = Self::headers(header.previous_header_hash).ok_or_else(|| {
                log!(
                    error,
                    "[check_prev_and_convert] Can not find prev header, current header:{:?}",
                    header
                );
                Error::<T>::PrevHeaderNotExisted
            })?;

            // convert dogecoin header to self header info
            let header_info = DogeHeaderInfo {
                header,
                height: prev_info.height + 1,
            };
            // verify header
            let header_verifier = header::HeaderVerifier::new::<T>(&header_info);
            header_verifier.check::<T>()?;

            // insert into storage
            let hash = header_info.header.hash();
            // insert valid header into storage
            Headers::<T>::insert(&hash, header_info.clone());
            // storage height => block list (contains forked header hash)
            BlockHashFor::<T>::mutate(header_info.height, |v| {
                if !v.contains(&hash) {
                    v.push(hash);
                }
            });

            log!(debug,
                "[apply_push_header] Verify successfully, insert header to storage [height:{}, hash:{:?}, all hashes of the height:{:?}]",
                header_info.height,
                hash,
                Self::block_hash_for(header_info.height)
            );

            let best_index = Self::best_index();

            if header_info.height > best_index.height {
                // note update_confirmed_header would mutate other storage depend on BlockHashFor
                let confirmed_index = header::update_confirmed_header::<T>(&header_info);
                log!(
                    info,
                    "[apply_push_header] Update new height:{}, hash:{:?}, confirm:{:?}",
                    header_info.height,
                    hash,
                    confirmed_index
                );

                // new best index
                let new_best_index = DogeHeaderIndex {
                    hash,
                    height: header_info.height,
                };
                BestIndex::<T>::put(new_best_index);
            } else {
                // forked chain
                log!(
                    info,
                    "[apply_push_header] Best index {} larger than this height {}",
                    best_index.height,
                    header_info.height
                );
                header::check_confirmed_header::<T>(&header_info)?;
            };
            Self::deposit_event(Event::<T>::HeaderInserted(hash));
            Ok(())
        }

        pub(crate) fn apply_push_transaction(
            tx: DogeRelayedTx,
            prev_tx: Option<Transaction>,
        ) -> DispatchResult {
            let tx_hash = tx.raw.hash();
            let block_hash = tx.block_hash;
            let header_info = Pallet::<T>::headers(&tx.block_hash).ok_or_else(|| {
                log!(
                    error,
                    "[apply_push_transaction] Tx's block header ({:?}) must exist before",
                    block_hash
                );
                "Tx's block header must already exist"
            })?;
            let merkle_root = header_info.header.merkle_root_hash;
            // verify, check merkle proof
            tx::validate_transaction::<T>(&tx, merkle_root, prev_tx.as_ref())?;

            // ensure the tx should belong to the main chain, means should submit main chain tx,
            // e.g. a tx may be packed in main chain block, and forked chain block, only submit main chain tx
            // could pass the verify.
            ensure!(Self::main_chain(&tx.block_hash), Error::<T>::UnconfirmedTx);
            // if ConfirmedIndex not set, due to confirm height not beyond genesis height
            let confirmed = Self::confirmed_index().ok_or(Error::<T>::UnconfirmedTx)?;
            let height = header_info.height;
            if height > confirmed.height {
                log!(error,
                "[apply_push_transaction] Receive an unconfirmed tx (height:{}, hash:{:?}), confirmed index (height:{}, hash:{:?})",
                height, tx_hash, confirmed.height, confirmed.hash
            );
                return Err(Error::<T>::UnconfirmedTx.into());
            }
            // check whether replayed tx has been processed, just process failed and not processed tx;
            match Self::tx_state(&tx_hash) {
                None => { /* do nothing */ }
                Some(state) => {
                    if state.result == DogeTxResult::Success {
                        log!(error,
                        "[apply_push_transaction] Reject processed tx (hash:{:?}, type:{:?}, result:{:?})",
                        tx_hash, state.tx_type, state.result
                    );
                        return Err(Error::<T>::ReplayedTx.into());
                    }
                }
            }

            let network = Pallet::<T>::network_id();
            let min_deposit = Pallet::<T>::doge_min_deposit();
            let current_trustee_pair = get_current_trustee_address_pair::<T>()?;
            let last_trustee_pair = get_last_trustee_address_pair::<T>().ok();
            let state = tx::process_tx::<T>(
                tx.raw,
                prev_tx,
                network,
                min_deposit,
                current_trustee_pair,
                last_trustee_pair,
            );
            TxState::<T>::insert(&tx_hash, state);
            Self::deposit_event(Event::<T>::TxProcessed(tx_hash, block_hash, state));
            match state.result {
                DogeTxResult::Success => Ok(()),
                DogeTxResult::Failure => Err(Error::<T>::ProcessTxFailed.into()),
            }
        }
    }
}

// Copyright 2019-2020 ChainX Project Authors. Licensed under GPL-3.0.

use std::cmp::max;
use std::convert::TryInto;
use std::{cell::RefCell, convert::TryFrom, time::Duration};

use codec::{Decode, Encode};
use frame_support::{
    parameter_types, sp_io,
    traits::{ChangeMembers, GenesisBuild, LockIdentifier, UnixTime},
};
use frame_system::{EnsureRoot, EnsureSigned};
use light_bitcoin::keys::{Address, Public};
use light_bitcoin::mast::{compute_min_threshold, Mast};
use light_bitcoin::script::{Builder, Bytes, Opcode};
use sp_core::H256;
use sp_io::hashing::blake2_256;
use sp_keyring::sr25519;
use sp_runtime::{
    testing::Header,
    traits::{BlakeTwo256, IdentityLookup, Saturating},
    AccountId32, DispatchError, DispatchResult,
};

use sherpax_primitives::AssetId;
use xp_assets_registrar::Chain;
pub use xp_protocol::{X_BTC, X_ETH};
use xpallet_gateway_bitcoin::trustee::check_keys;
use xpallet_gateway_records::{ChainT, WithdrawalLimit};
use xpallet_support::traits::{MultisigAddressFor, Validator};

use crate::traits::TotalSupply;
use crate::utils::{two_thirds_unsafe, MAX_TAPROOT_NODES};
use crate::{
    self as xpallet_gateway_common,
    traits::TrusteeForChain,
    trustees::{
        self,
        bitcoin::{BtcTrusteeAddrInfo, BtcTrusteeType},
    },
    types::*,
    SaturatedConversion,
};

pub(crate) type AccountId = AccountId32;
pub(crate) type BlockNumber = u64;
pub(crate) type Balance = u128;

type UncheckedExtrinsic = frame_system::mocking::MockUncheckedExtrinsic<Test>;
type Block = frame_system::mocking::MockBlock<Test>;

frame_support::construct_runtime!(
    pub enum Test where
        Block = Block,
        NodeBlock = Block,
        UncheckedExtrinsic = UncheckedExtrinsic,
    {
        System: frame_system::{Pallet, Call, Config, Storage, Event<T>},
        Balances: pallet_balances::{Pallet, Call, Storage, Config<T>, Event<T>},
        Elections: pallet_elections_phragmen::{Pallet, Call, Storage, Event<T>, Config<T>},
        Assets: pallet_assets::{Pallet, Call, Storage, Event<T>},
        XGatewayRecords: xpallet_gateway_records::{Pallet, Call, Storage, Event<T>, Config<T>},
        XGatewayCommon: xpallet_gateway_common::{Pallet, Call, Storage, Event<T>, Config<T>},
        XGatewayBitcoin: xpallet_gateway_bitcoin::{Pallet, Call, Storage, Event<T>, Config<T>},
    }
);

parameter_types! {
    pub const BlockHashCount: u64 = 250;
    pub const SS58Prefix: u8 = 42;
}
impl frame_system::Config for Test {
    type BaseCallFilter = frame_support::traits::Everything;
    type BlockWeights = ();
    type BlockLength = ();
    type Origin = Origin;
    type Call = Call;
    type Index = u64;
    type BlockNumber = BlockNumber;
    type Hash = H256;
    type Hashing = BlakeTwo256;
    type AccountId = AccountId;
    type Lookup = IdentityLookup<Self::AccountId>;
    type Header = Header;
    type Event = ();
    type BlockHashCount = BlockHashCount;
    type DbWeight = ();
    type Version = ();
    type PalletInfo = PalletInfo;
    type AccountData = pallet_balances::AccountData<Balance>;
    type OnNewAccount = ();
    type OnKilledAccount = ();
    type SystemWeightInfo = ();
    type SS58Prefix = SS58Prefix;
    type OnSetCode = ();
    type MaxConsumers = frame_support::traits::ConstU32<16>;
}

parameter_types! {
    pub const ExistentialDeposit: u64 = 0;
    pub const MaxReserves: u32 = 50;
}
impl pallet_balances::Config for Test {
    type MaxLocks = ();
    type Balance = Balance;
    type DustRemoval = ();
    type Event = ();
    type ExistentialDeposit = ExistentialDeposit;
    type AccountStore = System;
    type WeightInfo = ();
    type ReserveIdentifier = [u8; 8];
    type MaxReserves = MaxReserves;
}

parameter_types! {
    pub const ElectionsPhragmenPalletId: LockIdentifier = *b"phrelect";
}

frame_support::parameter_types! {
    pub static VotingBondBase: u64 = 2;
    pub static VotingBondFactor: u64 = 0;
    pub static CandidacyBond: u64 = 3;
    pub static DesiredMembers: u32 = 4;
    pub static DesiredRunnersUp: u32 = 2;
    pub static TermDuration: u64 = 5;
    pub static Members: Vec<u64> = vec![];
    pub static Prime: Option<u64> = None;
}

pub struct TestChangeMembers;
impl ChangeMembers<u64> for TestChangeMembers {
    fn change_members_sorted(incoming: &[u64], outgoing: &[u64], new: &[u64]) {
        // new, incoming, outgoing must be sorted.
        let mut new_sorted = new.to_vec();
        new_sorted.sort_unstable();
        assert_eq!(new, &new_sorted[..]);

        let mut incoming_sorted = incoming.to_vec();
        incoming_sorted.sort_unstable();
        assert_eq!(incoming, &incoming_sorted[..]);

        let mut outgoing_sorted = outgoing.to_vec();
        outgoing_sorted.sort_unstable();
        assert_eq!(outgoing, &outgoing_sorted[..]);

        // incoming and outgoing must be disjoint
        for x in incoming.iter() {
            assert!(outgoing.binary_search(x).is_err());
        }

        let mut old_plus_incoming = MEMBERS.with(|m| m.borrow().to_vec());
        old_plus_incoming.extend_from_slice(incoming);
        old_plus_incoming.sort_unstable();

        let mut new_plus_outgoing = new.to_vec();
        new_plus_outgoing.extend_from_slice(outgoing);
        new_plus_outgoing.sort_unstable();

        assert_eq!(
            old_plus_incoming, new_plus_outgoing,
            "change members call is incorrect!"
        );

        MEMBERS.with(|m| *m.borrow_mut() = new.to_vec());
        PRIME.with(|p| *p.borrow_mut() = None);
    }

    fn set_prime(who: Option<u64>) {
        PRIME.with(|p| *p.borrow_mut() = who);
    }

    fn get_prime() -> Option<u64> {
        PRIME.with(|p| *p.borrow())
    }
}

impl pallet_elections_phragmen::Config for Test {
    type PalletId = ElectionsPhragmenPalletId;
    type Event = ();
    type Currency = Balances;
    type CurrencyToVote = frame_support::traits::SaturatingCurrencyToVote;
    type ChangeMembers = ();
    type InitializeMembers = ();
    type CandidacyBond = CandidacyBond;
    type VotingBondBase = VotingBondBase;
    type VotingBondFactor = VotingBondFactor;
    type TermDuration = TermDuration;
    type DesiredMembers = DesiredMembers;
    type DesiredRunnersUp = DesiredRunnersUp;
    type LoserCandidate = ();
    type KickedMember = ();
    type WeightInfo = ();
}

parameter_types! {
    pub const AssetDeposit: Balance = 1;
    pub const ApprovalDeposit: Balance = 1;
    pub const StringLimit: u32 = 50;
    pub const MetadataDepositBase: Balance = 1;
    pub const MetadataDepositPerByte: Balance = 1;
}

impl pallet_assets::Config for Test {
    type Event = ();
    type Balance = Balance;
    type AssetId = AssetId;
    type Currency = Balances;
    type ForceOrigin = EnsureRoot<AccountId>;
    type AssetDeposit = AssetDeposit;
    type AssetAccountDeposit = ();
    type MetadataDepositBase = MetadataDepositBase;
    type MetadataDepositPerByte = MetadataDepositPerByte;
    type ApprovalDeposit = ApprovalDeposit;
    type StringLimit = StringLimit;
    type Freezer = XGatewayRecords;
    type Extra = ();
    type WeightInfo = pallet_assets::weights::SubstrateWeight<Test>;
}

// assets
parameter_types! {
    pub const BtcAssetId: AssetId = 1;
    pub const DogeAssetId: AssetId = 9;
}

impl xpallet_gateway_records::Config for Test {
    type Event = ();
    type Currency = Balances;
    type WeightInfo = ();
    type BtcAssetId = BtcAssetId;
    type DogeAssetId = DogeAssetId;
}

thread_local! {
    pub static NOW: RefCell<Option<Duration>> = RefCell::new(None);
}
pub struct Timestamp;
impl UnixTime for Timestamp {
    fn now() -> Duration {
        NOW.with(|m| {
            m.borrow().unwrap_or_else(|| {
                use std::time::{SystemTime, UNIX_EPOCH};
                let start = SystemTime::now();
                start
                    .duration_since(UNIX_EPOCH)
                    .expect("Time went backwards")
            })
        })
    }
}

impl xpallet_gateway_bitcoin::Config for Test {
    type Event = ();
    type UnixTime = Timestamp;
    type CouncilOrigin = EnsureSigned<AccountId>;
    type AccountExtractor = xp_gateway_bitcoin::OpReturnExtractor;
    type TrusteeSessionProvider = ();
    type TrusteeInfoUpdate = ();
    type ReferralBinding = ();
    type AddressBinding = ();
    type WeightInfo = ();
}

pub struct MultisigAddr;
impl MultisigAddressFor<AccountId> for MultisigAddr {
    fn calc_multisig(who: &[AccountId], threshold: u16) -> AccountId {
        let entropy = (b"modlpy/utilisuba", who, threshold).using_encoded(blake2_256);
        AccountId::decode(&mut &entropy[..]).unwrap()
    }
}
pub struct AlwaysValidator;
impl Validator<AccountId> for AlwaysValidator {
    fn is_validator(_who: &AccountId) -> bool {
        true
    }

    fn validator_for(_: &[u8]) -> Option<AccountId> {
        None
    }
}
pub struct MockBitcoin<T: xpallet_gateway_bitcoin::Config>(sp_std::marker::PhantomData<T>);
impl<T: xpallet_gateway_bitcoin::Config> ChainT<T::AssetId, T::Balance> for MockBitcoin<T> {
    fn chain() -> Chain {
        Chain::Bitcoin
    }

    fn check_addr(_: &[u8], _: &[u8]) -> DispatchResult {
        Ok(())
    }

    fn withdrawal_limit(
        asset_id: &T::AssetId,
    ) -> Result<WithdrawalLimit<T::Balance>, DispatchError> {
        xpallet_gateway_bitcoin::Pallet::<T>::withdrawal_limit(asset_id)
    }
}

impl<T: xpallet_gateway_bitcoin::Config> TotalSupply<T::Balance> for MockBitcoin<T> {
    fn total_supply() -> T::Balance {
        Default::default()
    }
}

const EC_P: [u8; 32] = [
    255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
    255, 255, 255, 255, 255, 255, 255, 255, 254, 255, 255, 252, 47,
];

const ZERO_P: [u8; 32] = [0; 32];

impl<T: xpallet_gateway_bitcoin::Config>
    TrusteeForChain<T::AccountId, T::BlockNumber, BtcTrusteeType, BtcTrusteeAddrInfo>
    for MockBitcoin<T>
{
    fn check_trustee_entity(raw_addr: &[u8]) -> Result<BtcTrusteeType, DispatchError> {
        let trustee_type = BtcTrusteeType::try_from(raw_addr.to_vec())
            .map_err(|_| xpallet_gateway_bitcoin::Error::<T>::InvalidPublicKey)?;
        let public = trustee_type.0;

        if public.len() != 65 {
            return Err(xpallet_gateway_bitcoin::Error::<T>::InvalidPublicKey.into());
        }

        if 4 != raw_addr[0] {
            return Err(xpallet_gateway_bitcoin::Error::<T>::InvalidPublicKey.into());
        }

        if ZERO_P == raw_addr[1..33] {
            return Err(xpallet_gateway_bitcoin::Error::<T>::InvalidPublicKey.into());
        }

        if raw_addr[1..33].to_vec() >= EC_P.to_vec() {
            return Err(xpallet_gateway_bitcoin::Error::<T>::InvalidPublicKey.into());
        }

        Ok(BtcTrusteeType(public))
    }

    fn generate_trustee_session_info(
        props: Vec<(
            T::AccountId,
            TrusteeIntentionProps<T::AccountId, BtcTrusteeType>,
        )>,
        config: TrusteeInfoConfig,
    ) -> Result<
        (
            TrusteeSessionInfo<T::AccountId, T::BlockNumber, BtcTrusteeAddrInfo>,
            ScriptInfo<T::AccountId>,
        ),
        DispatchError,
    > {
        let (trustees, props_info): (
            Vec<T::AccountId>,
            Vec<TrusteeIntentionProps<T::AccountId, BtcTrusteeType>>,
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
            return Err(xpallet_gateway_bitcoin::Error::<T>::InvalidTrusteeCount.into());
        }

        let sig_num = max(
            two_thirds_unsafe(trustees.len() as u32),
            compute_min_threshold(trustees.len() as u32, MAX_TAPROOT_NODES) as u32,
        );

        // Set hot address for taproot threshold address
        let hot_pks = hot_keys
            .into_iter()
            .map(|k| {
                k.try_into()
                    .map_err(|_| xpallet_gateway_bitcoin::Error::<T>::InvalidPublicKey)
            })
            .collect::<Result<Vec<_>, xpallet_gateway_bitcoin::Error<T>>>()?;

        let hot_mast = Mast::new(hot_pks, sig_num)
            .map_err(|_| xpallet_gateway_bitcoin::Error::<T>::InvalidAddress)?;

        let hot_threshold_addr: Address = hot_mast
            .generate_address(&xpallet_gateway_bitcoin::Pallet::<T>::network_id().to_string())
            .map_err(|_| xpallet_gateway_bitcoin::Error::<T>::InvalidAddress)?
            .parse()
            .map_err(|_| xpallet_gateway_bitcoin::Error::<T>::InvalidAddress)?;

        // Set cold address for taproot threshold address
        let cold_pks = cold_keys
            .into_iter()
            .map(|k| {
                k.try_into()
                    .map_err(|_| xpallet_gateway_bitcoin::Error::<T>::InvalidAddress)
            })
            .collect::<Result<Vec<_>, xpallet_gateway_bitcoin::Error<T>>>()?;

        let cold_mast = Mast::new(cold_pks, sig_num)
            .map_err(|_| xpallet_gateway_bitcoin::Error::<T>::InvalidAddress)?;

        let cold_threshold_addr: Address = cold_mast
            .generate_address(&xpallet_gateway_bitcoin::Pallet::<T>::network_id().to_string())
            .map_err(|_| "InvalidAddress")?
            .parse()
            .map_err(|_| "InvalidAddress")?;

        // Aggregate public key script and corresponding personal public key index
        let mut agg_pubkeys: Vec<Vec<u8>> = vec![];
        let mut personal_accounts: Vec<Vec<T::AccountId>> = vec![];
        for (i, p) in hot_mast.person_pubkeys.iter().enumerate() {
            let script: Bytes = Builder::default()
                .push_bytes(&p.x_coor().to_vec())
                .push_opcode(Opcode::OP_CHECKSIG)
                .into_script()
                .into();
            let mut accounts = vec![];
            for index in hot_mast.indexs[i].iter() {
                accounts.push(trustees[(index - 1) as usize].clone())
            }
            agg_pubkeys.push(script.into());
            personal_accounts.push(accounts);
        }

        let hot_trustee_addr_info: BtcTrusteeAddrInfo = BtcTrusteeAddrInfo {
            addr: hot_threshold_addr.to_string().into_bytes(),
            redeem_script: vec![],
        };

        let cold_trustee_addr_info: BtcTrusteeAddrInfo = BtcTrusteeAddrInfo {
            addr: cold_threshold_addr.to_string().into_bytes(),
            redeem_script: vec![],
        };

        let start_height = frame_system::Pallet::<T>::block_number();
        let trustee_num = trustees.len();
        Ok((
            TrusteeSessionInfo {
                trustee_list: trustees
                    .into_iter()
                    .zip(vec![1u64; trustee_num])
                    .collect::<Vec<_>>(),
                multi_account: None,
                start_height: Some(start_height),
                threshold: sig_num as u16,
                hot_address: hot_trustee_addr_info,
                cold_address: cold_trustee_addr_info,
                end_height: Some(T::BlockNumber::default().saturating_add(10u32.saturated_into())),
            },
            ScriptInfo {
                agg_pubkeys,
                personal_accounts,
            },
        ))
    }
}

impl crate::Config for Test {
    type Event = ();
    type Validator = AlwaysValidator;
    type DetermineMultisigAddress = MultisigAddr;
    type CouncilOrigin = EnsureSigned<AccountId>;
    type Bitcoin = MockBitcoin<Test>;
    type BitcoinTrustee = MockBitcoin<Test>;
    type BitcoinTrusteeSessionProvider = trustees::bitcoin::BtcTrusteeSessionManager<Test>;
    type BitcoinTotalSupply = MockBitcoin<Test>;
    type BitcoinWithdrawalProposal = ();
    type Dogecoin = ();
    type DogecoinTrustee = ();
    type DogecoinTrusteeSessionProvider = ();
    type DogecoinTotalSupply = ();
    type DogecoinWithdrawalProposal = ();
    type WeightInfo = ();
}

pub fn alice() -> AccountId32 {
    sr25519::Keyring::Alice.to_account_id()
}
pub fn bob() -> AccountId32 {
    sr25519::Keyring::Bob.to_account_id()
}
pub fn charlie() -> AccountId32 {
    sr25519::Keyring::Charlie.to_account_id()
}
pub fn dave() -> AccountId32 {
    sr25519::Keyring::Dave.to_account_id()
}

pub struct ExtBuilder;
impl Default for ExtBuilder {
    fn default() -> Self {
        Self
    }
}
impl ExtBuilder {
    pub fn build(self) -> sp_io::TestExternalities {
        let mut storage = frame_system::GenesisConfig::default()
            .build_storage::<Test>()
            .unwrap();

        let _ = crate::GenesisConfig::<Test> {
            trustees: trustees(),
            ..Default::default()
        }
        .assimilate_storage(&mut storage);

        let _ = xpallet_gateway_records::GenesisConfig::<Test> {
            initial_asset_chain: vec![(X_BTC, Chain::Bitcoin)],
        }
        .assimilate_storage(&mut storage);

        let _ = pallet_assets::GenesisConfig::<Test> {
            assets: vec![(X_BTC, alice(), true, 1)],
            metadata: vec![(
                X_BTC,
                "XBTC".to_string().into_bytes(),
                "XBTC".to_string().into_bytes(),
                8,
            )],
            accounts: vec![],
        }
        .assimilate_storage(&mut storage);

        let members = vec![(alice(), 0), (bob(), 0), (charlie(), 0), (dave(), 0)];
        let _ = pallet_elections_phragmen::GenesisConfig::<Test> { members }
            .assimilate_storage(&mut storage);

        sp_io::TestExternalities::new(storage)
    }
}

fn trustees() -> Vec<(
    Chain,
    TrusteeInfoConfig,
    Vec<(AccountId, Vec<u8>, Vec<u8>, Vec<u8>)>,
)> {
    let btc_trustees = vec![
        (
            alice(),
            b"".to_vec(),
            hex::decode("0483f579dd2380bd31355d066086e1b4d46b518987c1f8a64d4c0101560280eae2b16f3068e94333e11ee63770936eca9692a25f76012511d38ac30ece20f07dca")
                .expect("hex decode failed"),
            hex::decode("0400849497d4f88ebc3e1bc2583677c5abdbd3b63640b3c5c50cd4628a33a2a2cab6b69094b5a213da80f9ef730fab39de770ca124f2d9a9cb161856be54b9adc5")
                .expect("hex decode failed"),
        ),
        (
            bob(),
            b"".to_vec(),
            hex::decode("047a0868a14bd18e2e45ff3ad960f892df8d0edd1a5685f0a1dc63c7986d4ad55d47c09531e4f2ca2ae7f9ed80c1f9df2edd8afa19188692724d2bc18c18d98c10")
                .expect("hex decode failed"),
            hex::decode("042122032ae9656f9a133405ffe02101469a8d62002270a33ceccf0e40dda54d08c989b55f1b6b46a8dee284cf6737de0a377e410bcfd361a015528ae80a349529")
                .expect("hex decode failed"),
        ),
        (
            charlie(),
            b"".to_vec(),
            hex::decode("04c9929543dfa1e0bb84891acd47bfa6546b05e26b7a04af8eb6765fcc969d565faced14acb5172ee19aee5417488fecdda33f4cfea9ff04f250e763e6f7458d5e")
                .expect("hex decode failed"),
            hex::decode("04b3cc747f572d33f12870fa6866aebbfd2b992ba606b8dc89b676b3697590ad63d5ca398bdb6f8ee619f2e16997f21e5e8f0e0b00e2f275c7cb1253f381058d56")
                .expect("hex decode failed"),
        ),
        (
            dave(),
            b"".to_vec(),
            hex::decode("042122032ae9656f9a133405ffe02101469a8d62002270a33ceccf0e40dda54d08c989b55f1b6b46a8dee284cf6737de0a377e410bcfd361a015528ae80a349529")
                .expect("hex decode failed"),
            hex::decode("047a0868a14bd18e2e45ff3ad960f892df8d0edd1a5685f0a1dc63c7986d4ad55d47c09531e4f2ca2ae7f9ed80c1f9df2edd8afa19188692724d2bc18c18d98c10")
                .expect("hex decode failed"),
        ),
    ];
    let btc_config = TrusteeInfoConfig {
        min_trustee_count: 3,
        max_trustee_count: 15,
    };
    vec![(Chain::Bitcoin, btc_config, btc_trustees)]
}

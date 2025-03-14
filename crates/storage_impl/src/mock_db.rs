use std::sync::Arc;

use diesel_models::{self as store};
use error_stack::ResultExt;
use futures::lock::Mutex;
use hyperswitch_domain_models::{
    errors::StorageError,
    payments::{payment_attempt::PaymentAttempt, PaymentIntent},
};
use redis_interface::RedisSettings;

use crate::redis::RedisStore;

pub mod payment_attempt;
pub mod payment_intent;
#[cfg(feature = "payouts")]
pub mod payout_attempt;
#[cfg(feature = "payouts")]
pub mod payouts;
pub mod redis_conn;
#[cfg(not(feature = "payouts"))]
use hyperswitch_domain_models::{PayoutAttemptInterface, PayoutsInterface};

#[derive(Clone)]
pub struct MockDb {
    pub addresses: Arc<Mutex<Vec<store::Address>>>,
    pub configs: Arc<Mutex<Vec<store::Config>>>,
    pub merchant_accounts: Arc<Mutex<Vec<store::MerchantAccount>>>,
    pub merchant_connector_accounts: Arc<Mutex<Vec<store::MerchantConnectorAccount>>>,
    pub payment_attempts: Arc<Mutex<Vec<PaymentAttempt>>>,
    pub payment_intents: Arc<Mutex<Vec<PaymentIntent>>>,
    pub payment_methods: Arc<Mutex<Vec<store::PaymentMethod>>>,
    pub customers: Arc<Mutex<Vec<store::Customer>>>,
    pub refunds: Arc<Mutex<Vec<store::Refund>>>,
    pub processes: Arc<Mutex<Vec<store::ProcessTracker>>>,
    pub redis: Arc<RedisStore>,
    pub api_keys: Arc<Mutex<Vec<store::ApiKey>>>,
    pub ephemeral_keys: Arc<Mutex<Vec<store::EphemeralKey>>>,
    pub cards_info: Arc<Mutex<Vec<store::CardInfo>>>,
    pub events: Arc<Mutex<Vec<store::Event>>>,
    pub disputes: Arc<Mutex<Vec<store::Dispute>>>,
    pub lockers: Arc<Mutex<Vec<store::LockerMockUp>>>,
    pub mandates: Arc<Mutex<Vec<store::Mandate>>>,
    pub captures: Arc<Mutex<Vec<store::capture::Capture>>>,
    pub merchant_key_store: Arc<Mutex<Vec<store::merchant_key_store::MerchantKeyStore>>>,
    pub business_profiles: Arc<Mutex<Vec<store::business_profile::BusinessProfile>>>,
    pub reverse_lookups: Arc<Mutex<Vec<store::ReverseLookup>>>,
    pub payment_link: Arc<Mutex<Vec<store::payment_link::PaymentLink>>>,
    pub organizations: Arc<Mutex<Vec<store::organization::Organization>>>,
    pub users: Arc<Mutex<Vec<store::user::User>>>,
    pub user_roles: Arc<Mutex<Vec<store::user_role::UserRole>>>,
    pub authorizations: Arc<Mutex<Vec<store::authorization::Authorization>>>,
    pub dashboard_metadata: Arc<Mutex<Vec<store::user::dashboard_metadata::DashboardMetadata>>>,
    #[cfg(feature = "payouts")]
    pub payout_attempt: Arc<Mutex<Vec<store::payout_attempt::PayoutAttempt>>>,
    #[cfg(feature = "payouts")]
    pub payouts: Arc<Mutex<Vec<store::payouts::Payouts>>>,
    pub authentications: Arc<Mutex<Vec<store::authentication::Authentication>>>,
    pub roles: Arc<Mutex<Vec<store::role::Role>>>,
}

impl MockDb {
    pub async fn new(redis: &RedisSettings) -> error_stack::Result<Self, StorageError> {
        Ok(Self {
            addresses: Default::default(),
            configs: Default::default(),
            merchant_accounts: Default::default(),
            merchant_connector_accounts: Default::default(),
            payment_attempts: Default::default(),
            payment_intents: Default::default(),
            payment_methods: Default::default(),
            customers: Default::default(),
            refunds: Default::default(),
            processes: Default::default(),
            redis: Arc::new(
                RedisStore::new(redis)
                    .await
                    .change_context(StorageError::InitializationError)?,
            ),
            api_keys: Default::default(),
            ephemeral_keys: Default::default(),
            cards_info: Default::default(),
            events: Default::default(),
            disputes: Default::default(),
            lockers: Default::default(),
            mandates: Default::default(),
            captures: Default::default(),
            merchant_key_store: Default::default(),
            business_profiles: Default::default(),
            reverse_lookups: Default::default(),
            payment_link: Default::default(),
            organizations: Default::default(),
            users: Default::default(),
            user_roles: Default::default(),
            authorizations: Default::default(),
            dashboard_metadata: Default::default(),
            #[cfg(feature = "payouts")]
            payout_attempt: Default::default(),
            #[cfg(feature = "payouts")]
            payouts: Default::default(),
            authentications: Default::default(),
            roles: Default::default(),
        })
    }
}

#[cfg(not(feature = "payouts"))]
impl PayoutsInterface for MockDb {}

#[cfg(not(feature = "payouts"))]
impl PayoutAttemptInterface for MockDb {}

use std::collections::HashMap;

use api_models::payment_methods::PaymentMethodsData;
use common_enums::PaymentMethod;
use common_utils::{
    ext_traits::{Encode, ValueExt},
    pii,
};
use error_stack::{report, ResultExt};
use masking::ExposeInterface;
use router_env::{instrument, tracing};

use super::helpers;
use crate::{
    consts,
    core::{
        errors::{self, ConnectorErrorExt, RouterResult, StorageErrorExt},
        mandate, payment_methods, payments,
    },
    logger,
    routes::{metrics, AppState},
    services,
    types::{
        self,
        api::{self, CardDetailFromLocker, CardDetailsPaymentMethod, PaymentMethodCreateExt},
        domain,
        storage::{self, enums as storage_enums},
    },
    utils::{generate_id, OptionExt},
};

pub struct SavePaymentMethodData<Req> {
    request: Req,
    response: Result<types::PaymentsResponseData, types::ErrorResponse>,
    payment_method_token: Option<types::PaymentMethodToken>,
    payment_method: PaymentMethod,
    attempt_status: common_enums::AttemptStatus,
}

impl<F, Req: Clone> From<&types::RouterData<F, Req, types::PaymentsResponseData>>
    for SavePaymentMethodData<Req>
{
    fn from(router_data: &types::RouterData<F, Req, types::PaymentsResponseData>) -> Self {
        Self {
            request: router_data.request.clone(),
            response: router_data.response.clone(),
            payment_method_token: router_data.payment_method_token.clone(),
            payment_method: router_data.payment_method,
            attempt_status: router_data.status,
        }
    }
}

#[instrument(skip_all)]
#[allow(clippy::too_many_arguments)]
pub async fn save_payment_method<FData>(
    state: &AppState,
    connector_name: String,
    merchant_connector_id: Option<String>,
    save_payment_method_data: SavePaymentMethodData<FData>,
    customer_id: Option<String>,
    merchant_account: &domain::MerchantAccount,
    payment_method_type: Option<storage_enums::PaymentMethodType>,
    key_store: &domain::MerchantKeyStore,
    amount: Option<i64>,
    currency: Option<storage_enums::Currency>,
    billing_name: Option<masking::Secret<String>>,
    business_profile: &storage::business_profile::BusinessProfile,
) -> RouterResult<(Option<String>, Option<common_enums::PaymentMethodStatus>)>
where
    FData: mandate::MandateBehaviour + Clone,
{
    let mut pm_status = None;
    match save_payment_method_data.response {
        Ok(responses) => {
            let db = &*state.store;
            let token_store = state
                .conf
                .tokenization
                .0
                .get(&connector_name.to_string())
                .map(|token_filter| token_filter.long_lived_token)
                .unwrap_or(false);

            let network_transaction_id = match &responses {
                types::PaymentsResponseData::TransactionResponse { network_txn_id, .. } => {
                    network_txn_id.clone()
                }
                _ => None,
            };

            let network_transaction_id =
                if let Some(network_transaction_id) = network_transaction_id {
                    if business_profile.is_connector_agnostic_mit_enabled == Some(true)
                        && save_payment_method_data.request.get_setup_future_usage()
                            == Some(storage_enums::FutureUsage::OffSession)
                    {
                        Some(network_transaction_id)
                    } else {
                        logger::info!("Skip storing network transaction id");
                        None
                    }
                } else {
                    None
                };

            let connector_token = if token_store {
                let tokens = save_payment_method_data
                    .payment_method_token
                    .to_owned()
                    .get_required_value("payment_token")?;
                let token = match tokens {
                    types::PaymentMethodToken::Token(connector_token) => connector_token,
                    types::PaymentMethodToken::ApplePayDecrypt(_) => {
                        Err(errors::ApiErrorResponse::NotSupported {
                            message: "Apple Pay Decrypt token is not supported".to_string(),
                        })?
                    }
                };
                Some((connector_name, token))
            } else {
                None
            };

            let mandate_data_customer_acceptance = save_payment_method_data
                .request
                .get_setup_mandate_details()
                .and_then(|mandate_data| mandate_data.customer_acceptance.clone());

            let customer_acceptance = save_payment_method_data
                .request
                .get_customer_acceptance()
                .or(mandate_data_customer_acceptance.clone().map(From::from))
                .map(|ca| ca.encode_to_value())
                .transpose()
                .change_context(errors::ApiErrorResponse::InternalServerError)
                .attach_printable("Unable to serialize customer acceptance to value")?;

            let connector_mandate_id = match responses {
                types::PaymentsResponseData::TransactionResponse {
                    ref mandate_reference,
                    ..
                } => {
                    if let Some(mandate_ref) = mandate_reference {
                        mandate_ref.connector_mandate_id.clone()
                    } else {
                        None
                    }
                }
                _ => None,
            };
            let check_for_mit_mandates = save_payment_method_data
                .request
                .get_setup_mandate_details()
                .is_none()
                && save_payment_method_data
                    .request
                    .get_setup_future_usage()
                    .map(|future_usage| future_usage == storage_enums::FutureUsage::OffSession)
                    .unwrap_or(false);
            // insert in PaymentMethods if its a off-session mit payment
            let connector_mandate_details = if check_for_mit_mandates {
                add_connector_mandate_details_in_payment_method(
                    payment_method_type,
                    amount,
                    currency,
                    merchant_connector_id.clone(),
                    connector_mandate_id.clone(),
                )
            } else {
                None
            }
            .map(|connector_mandate_data| connector_mandate_data.encode_to_value())
            .transpose()
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("Unable to serialize customer acceptance to value")?;

            let pm_id = if customer_acceptance.is_some() {
                let payment_method_create_request = helpers::get_payment_method_create_request(
                    Some(&save_payment_method_data.request.get_payment_method_data()),
                    Some(save_payment_method_data.payment_method),
                    payment_method_type,
                    &customer_id.clone(),
                    billing_name,
                )
                .await?;
                let customer_id = customer_id.to_owned().get_required_value("customer_id")?;
                let merchant_id = &merchant_account.merchant_id;
                let (mut resp, duplication_check) = if !state.conf.locker.locker_enabled {
                    skip_saving_card_in_locker(
                        merchant_account,
                        payment_method_create_request.to_owned(),
                    )
                    .await?
                } else {
                    pm_status = Some(common_enums::PaymentMethodStatus::from(
                        save_payment_method_data.attempt_status,
                    ));
                    Box::pin(save_in_locker(
                        state,
                        merchant_account,
                        payment_method_create_request.to_owned(),
                    ))
                    .await?
                };

                let pm_card_details = resp.card.as_ref().map(|card| {
                    PaymentMethodsData::Card(CardDetailsPaymentMethod::from(card.clone()))
                });

                let pm_data_encrypted =
                    payment_methods::cards::create_encrypted_payment_method_data(
                        key_store,
                        pm_card_details,
                    )
                    .await;

                let mut payment_method_id = resp.payment_method_id.clone();
                let mut locker_id = None;

                match duplication_check {
                    Some(duplication_check) => match duplication_check {
                        payment_methods::transformers::DataDuplicationCheck::Duplicated => {
                            let payment_method = {
                                let existing_pm_by_pmid = db
                                    .find_payment_method(
                                        &payment_method_id,
                                        merchant_account.storage_scheme,
                                    )
                                    .await;

                                if let Err(err) = existing_pm_by_pmid {
                                    if err.current_context().is_db_not_found() {
                                        locker_id = Some(payment_method_id.clone());
                                        let existing_pm_by_locker_id = db
                                            .find_payment_method_by_locker_id(
                                                &payment_method_id,
                                                merchant_account.storage_scheme,
                                            )
                                            .await;

                                        match &existing_pm_by_locker_id {
                                            Ok(pm) => {
                                                payment_method_id.clone_from(&pm.payment_method_id);
                                            }
                                            Err(_) => {
                                                payment_method_id =
                                                    generate_id(consts::ID_LENGTH, "pm")
                                            }
                                        };
                                        existing_pm_by_locker_id
                                    } else {
                                        Err(err)
                                    }
                                } else {
                                    existing_pm_by_pmid
                                }
                            };

                            resp.payment_method_id = payment_method_id;

                            match payment_method {
                                Ok(pm) => {
                                    let pm_metadata = create_payment_method_metadata(
                                        pm.metadata.as_ref(),
                                        connector_token,
                                    )?;
                                    if let Some(metadata) = pm_metadata {
                                        payment_methods::cards::update_payment_method(
                                            db,
                                            pm.clone(),
                                            metadata,
                                            merchant_account.storage_scheme,
                                        )
                                        .await
                                        .change_context(
                                            errors::ApiErrorResponse::InternalServerError,
                                        )
                                        .attach_printable("Failed to add payment method in db")?;
                                    };
                                    // update if its a off-session mit payment
                                    if check_for_mit_mandates {
                                        let connector_mandate_details =
                                            update_connector_mandate_details_in_payment_method(
                                                pm.clone(),
                                                payment_method_type,
                                                amount,
                                                currency,
                                                merchant_connector_id.clone(),
                                                connector_mandate_id.clone(),
                                            )?;

                                        payment_methods::cards::update_payment_method_connector_mandate_details(db, pm, connector_mandate_details, merchant_account.storage_scheme).await.change_context(
                                        errors::ApiErrorResponse::InternalServerError,
                                    )
                                    .attach_printable("Failed to update payment method in db")?;
                                    }
                                }
                                Err(err) => {
                                    if err.current_context().is_db_not_found() {
                                        let pm_metadata =
                                            create_payment_method_metadata(None, connector_token)?;
                                        payment_methods::cards::create_payment_method(
                                            db,
                                            &payment_method_create_request,
                                            customer_id.as_str(),
                                            &resp.payment_method_id,
                                            locker_id,
                                            merchant_id,
                                            pm_metadata,
                                            customer_acceptance,
                                            pm_data_encrypted,
                                            key_store,
                                            connector_mandate_details,
                                            None,
                                            network_transaction_id,
                                            merchant_account.storage_scheme,
                                        )
                                        .await
                                    } else {
                                        Err(err)
                                            .change_context(
                                                errors::ApiErrorResponse::InternalServerError,
                                            )
                                            .attach_printable("Error while finding payment method")
                                    }?;
                                }
                            };
                        }
                        payment_methods::transformers::DataDuplicationCheck::MetaDataChanged => {
                            if let Some(card) = payment_method_create_request.card.clone() {
                                let payment_method = {
                                    let existing_pm_by_pmid = db
                                        .find_payment_method(
                                            &payment_method_id,
                                            merchant_account.storage_scheme,
                                        )
                                        .await;

                                    if let Err(err) = existing_pm_by_pmid {
                                        if err.current_context().is_db_not_found() {
                                            locker_id = Some(payment_method_id.clone());
                                            let existing_pm_by_locker_id = db
                                                .find_payment_method_by_locker_id(
                                                    &payment_method_id,
                                                    merchant_account.storage_scheme,
                                                )
                                                .await;

                                            match &existing_pm_by_locker_id {
                                                Ok(pm) => {
                                                    payment_method_id
                                                        .clone_from(&pm.payment_method_id);
                                                }
                                                Err(_) => {
                                                    payment_method_id =
                                                        generate_id(consts::ID_LENGTH, "pm")
                                                }
                                            };
                                            existing_pm_by_locker_id
                                        } else {
                                            Err(err)
                                        }
                                    } else {
                                        existing_pm_by_pmid
                                    }
                                };

                                resp.payment_method_id = payment_method_id;

                                let existing_pm = match payment_method {
                                    Ok(pm) => {
                                        // update if its a off-session mit payment
                                        if check_for_mit_mandates {
                                            let connector_mandate_details =
                                                update_connector_mandate_details_in_payment_method(
                                                    pm.clone(),
                                                    payment_method_type,
                                                    amount,
                                                    currency,
                                                    merchant_connector_id.clone(),
                                                    connector_mandate_id.clone(),
                                                )?;

                                            payment_methods::cards::update_payment_method_connector_mandate_details(db, pm.clone(), connector_mandate_details, merchant_account.storage_scheme).await.change_context(
                                            errors::ApiErrorResponse::InternalServerError,
                                        )
                                        .attach_printable("Failed to update payment method in db")?;
                                        }
                                        Ok(pm)
                                    }
                                    Err(err) => {
                                        if err.current_context().is_db_not_found() {
                                            payment_methods::cards::insert_payment_method(
                                                db,
                                                &resp,
                                                payment_method_create_request.clone(),
                                                key_store,
                                                &merchant_account.merchant_id,
                                                customer_id.as_str(),
                                                resp.metadata.clone().map(|val| val.expose()),
                                                customer_acceptance,
                                                locker_id,
                                                connector_mandate_details,
                                                network_transaction_id,
                                                merchant_account.storage_scheme,
                                            )
                                            .await
                                        } else {
                                            Err(err)
                                                .change_context(
                                                    errors::ApiErrorResponse::InternalServerError,
                                                )
                                                .attach_printable(
                                                    "Error while finding payment method",
                                                )
                                        }
                                    }
                                }?;

                                payment_methods::cards::delete_card_from_locker(
                                    state,
                                    customer_id.as_str(),
                                    merchant_id,
                                    existing_pm
                                        .locker_id
                                        .as_ref()
                                        .unwrap_or(&existing_pm.payment_method_id),
                                )
                                .await?;

                                let add_card_resp = payment_methods::cards::add_card_hs(
                                    state,
                                    payment_method_create_request,
                                    &card,
                                    customer_id.clone(),
                                    merchant_account,
                                    api::enums::LockerChoice::HyperswitchCardVault,
                                    Some(
                                        existing_pm
                                            .locker_id
                                            .as_ref()
                                            .unwrap_or(&existing_pm.payment_method_id),
                                    ),
                                )
                                .await;

                                if let Err(err) = add_card_resp {
                                    logger::error!(vault_err=?err);
                                    db.delete_payment_method_by_merchant_id_payment_method_id(
                                        merchant_id,
                                        &resp.payment_method_id,
                                    )
                                    .await
                                    .to_not_found_response(
                                        errors::ApiErrorResponse::PaymentMethodNotFound,
                                    )?;

                                    Err(report!(errors::ApiErrorResponse::InternalServerError)
                                        .attach_printable(
                                            "Failed while updating card metadata changes",
                                        ))?
                                };

                                let updated_card = Some(CardDetailFromLocker {
                                    scheme: None,
                                    last4_digits: Some(card.card_number.clone().get_last4()),
                                    issuer_country: None,
                                    card_number: Some(card.card_number),
                                    expiry_month: Some(card.card_exp_month),
                                    expiry_year: Some(card.card_exp_year),
                                    card_token: None,
                                    card_fingerprint: None,
                                    card_holder_name: card.card_holder_name,
                                    nick_name: card.nick_name,
                                    card_network: None,
                                    card_isin: None,
                                    card_issuer: None,
                                    card_type: None,
                                    saved_to_locker: true,
                                });

                                let updated_pmd = updated_card.as_ref().map(|card| {
                                    PaymentMethodsData::Card(CardDetailsPaymentMethod::from(
                                        card.clone(),
                                    ))
                                });
                                let pm_data_encrypted =
                                    payment_methods::cards::create_encrypted_payment_method_data(
                                        key_store,
                                        updated_pmd,
                                    )
                                    .await;

                                let pm_update =
                                    storage::PaymentMethodUpdate::PaymentMethodDataUpdate {
                                        payment_method_data: pm_data_encrypted,
                                    };

                                db.update_payment_method(
                                    existing_pm,
                                    pm_update,
                                    merchant_account.storage_scheme,
                                )
                                .await
                                .change_context(errors::ApiErrorResponse::InternalServerError)
                                .attach_printable("Failed to add payment method in db")?;
                            }
                        }
                    },
                    None => {
                        let pm_metadata = create_payment_method_metadata(None, connector_token)?;

                        locker_id = resp.payment_method.and_then(|pm| {
                            if pm == PaymentMethod::Card {
                                Some(resp.payment_method_id)
                            } else {
                                None
                            }
                        });

                        resp.payment_method_id = generate_id(consts::ID_LENGTH, "pm");
                        payment_methods::cards::create_payment_method(
                            db,
                            &payment_method_create_request,
                            customer_id.as_str(),
                            &resp.payment_method_id,
                            locker_id,
                            merchant_id,
                            pm_metadata,
                            customer_acceptance,
                            pm_data_encrypted,
                            key_store,
                            connector_mandate_details,
                            None,
                            network_transaction_id,
                            merchant_account.storage_scheme,
                        )
                        .await?;
                    }
                }

                Some(resp.payment_method_id)
            } else {
                None
            };
            Ok((pm_id, pm_status))
        }
        Err(_) => Ok((None, None)),
    }
}

async fn skip_saving_card_in_locker(
    merchant_account: &domain::MerchantAccount,
    payment_method_request: api::PaymentMethodCreate,
) -> RouterResult<(
    api_models::payment_methods::PaymentMethodResponse,
    Option<payment_methods::transformers::DataDuplicationCheck>,
)> {
    let merchant_id = &merchant_account.merchant_id;
    let customer_id = payment_method_request
        .clone()
        .customer_id
        .clone()
        .get_required_value("customer_id")?;
    let payment_method_id = common_utils::generate_id(consts::ID_LENGTH, "pm");

    let last4_digits = payment_method_request
        .card
        .clone()
        .map(|c| c.card_number.get_last4());

    let card_isin = payment_method_request
        .card
        .clone()
        .map(|c| c.card_number.get_card_isin());

    match payment_method_request.card.clone() {
        Some(card) => {
            let card_detail = CardDetailFromLocker {
                scheme: None,
                issuer_country: card.card_issuing_country.clone(),
                last4_digits: last4_digits.clone(),
                card_number: None,
                expiry_month: Some(card.card_exp_month.clone()),
                expiry_year: Some(card.card_exp_year),
                card_token: None,
                card_holder_name: card.card_holder_name.clone(),
                card_fingerprint: None,
                nick_name: None,
                card_isin: card_isin.clone(),
                card_issuer: card.card_issuer.clone(),
                card_network: card.card_network.clone(),
                card_type: card.card_type.clone(),
                saved_to_locker: false,
            };
            let pm_resp = api::PaymentMethodResponse {
                merchant_id: merchant_id.to_string(),
                customer_id: Some(customer_id),
                payment_method_id,
                payment_method: payment_method_request.payment_method,
                payment_method_type: payment_method_request.payment_method_type,
                card: Some(card_detail),
                recurring_enabled: false,
                installment_payment_enabled: false,
                payment_experience: Some(vec![api_models::enums::PaymentExperience::RedirectToUrl]),
                metadata: None,
                created: Some(common_utils::date_time::now()),
                #[cfg(feature = "payouts")]
                bank_transfer: None,
                last_used_at: Some(common_utils::date_time::now()),
                client_secret: None,
            };

            Ok((pm_resp, None))
        }
        None => {
            let pm_id = common_utils::generate_id(consts::ID_LENGTH, "pm");
            let payment_method_response = api::PaymentMethodResponse {
                merchant_id: merchant_id.to_string(),
                customer_id: Some(customer_id),
                payment_method_id: pm_id,
                payment_method: payment_method_request.payment_method,
                payment_method_type: payment_method_request.payment_method_type,
                card: None,
                metadata: None,
                created: Some(common_utils::date_time::now()),
                recurring_enabled: false,
                installment_payment_enabled: false,
                payment_experience: Some(vec![api_models::enums::PaymentExperience::RedirectToUrl]),
                #[cfg(feature = "payouts")]
                bank_transfer: None,
                last_used_at: Some(common_utils::date_time::now()),
                client_secret: None,
            };
            Ok((payment_method_response, None))
        }
    }
}

pub async fn save_in_locker(
    state: &AppState,
    merchant_account: &domain::MerchantAccount,
    payment_method_request: api::PaymentMethodCreate,
) -> RouterResult<(
    api_models::payment_methods::PaymentMethodResponse,
    Option<payment_methods::transformers::DataDuplicationCheck>,
)> {
    payment_method_request.validate()?;
    let merchant_id = &merchant_account.merchant_id;
    let customer_id = payment_method_request
        .customer_id
        .clone()
        .get_required_value("customer_id")?;
    match payment_method_request.card.clone() {
        Some(card) => payment_methods::cards::add_card_to_locker(
            state,
            payment_method_request,
            &card,
            &customer_id,
            merchant_account,
            None,
        )
        .await
        .change_context(errors::ApiErrorResponse::InternalServerError)
        .attach_printable("Add Card Failed"),
        None => {
            let pm_id = common_utils::generate_id(consts::ID_LENGTH, "pm");
            let payment_method_response = api::PaymentMethodResponse {
                merchant_id: merchant_id.to_string(),
                customer_id: Some(customer_id),
                payment_method_id: pm_id,
                payment_method: payment_method_request.payment_method,
                payment_method_type: payment_method_request.payment_method_type,
                #[cfg(feature = "payouts")]
                bank_transfer: None,
                card: None,
                metadata: None,
                created: Some(common_utils::date_time::now()),
                recurring_enabled: false,           //[#219]
                installment_payment_enabled: false, //[#219]
                payment_experience: Some(vec![api_models::enums::PaymentExperience::RedirectToUrl]), //[#219]
                last_used_at: Some(common_utils::date_time::now()),
                client_secret: None,
            };
            Ok((payment_method_response, None))
        }
    }
}

pub fn create_payment_method_metadata(
    metadata: Option<&pii::SecretSerdeValue>,
    connector_token: Option<(String, String)>,
) -> RouterResult<Option<serde_json::Value>> {
    let mut meta = match metadata {
        None => serde_json::Map::new(),
        Some(meta) => {
            let metadata = meta.clone().expose();
            let existing_metadata: serde_json::Map<String, serde_json::Value> = metadata
                .parse_value("Map<String, Value>")
                .change_context(errors::ApiErrorResponse::InternalServerError)
                .attach_printable("Failed to parse the metadata")?;
            existing_metadata
        }
    };
    Ok(connector_token.and_then(|connector_and_token| {
        meta.insert(
            connector_and_token.0,
            serde_json::Value::String(connector_and_token.1),
        )
    }))
}

pub async fn add_payment_method_token<F: Clone, T: types::Tokenizable + Clone>(
    state: &AppState,
    connector: &api::ConnectorData,
    tokenization_action: &payments::TokenizationAction,
    router_data: &mut types::RouterData<F, T, types::PaymentsResponseData>,
    pm_token_request_data: types::PaymentMethodTokenizationData,
) -> RouterResult<Option<String>> {
    match tokenization_action {
        payments::TokenizationAction::TokenizeInConnector
        | payments::TokenizationAction::TokenizeInConnectorAndApplepayPreDecrypt => {
            let connector_integration: services::BoxedConnectorIntegration<
                '_,
                api::PaymentMethodToken,
                types::PaymentMethodTokenizationData,
                types::PaymentsResponseData,
            > = connector.connector.get_connector_integration();

            let pm_token_response_data: Result<types::PaymentsResponseData, types::ErrorResponse> =
                Err(types::ErrorResponse::default());

            let mut pm_token_router_data =
                helpers::router_data_type_conversion::<_, api::PaymentMethodToken, _, _, _, _>(
                    router_data.clone(),
                    pm_token_request_data,
                    pm_token_response_data,
                );

            connector_integration
                .execute_pretasks(&mut pm_token_router_data, state)
                .await
                .to_payment_failed_response()?;

            router_data
                .request
                .set_session_token(pm_token_router_data.session_token.clone());

            let resp = services::execute_connector_processing_step(
                state,
                connector_integration,
                &pm_token_router_data,
                payments::CallConnectorAction::Trigger,
                None,
            )
            .await
            .to_payment_failed_response()?;

            metrics::CONNECTOR_PAYMENT_METHOD_TOKENIZATION.add(
                &metrics::CONTEXT,
                1,
                &[
                    metrics::request::add_attributes(
                        "connector",
                        connector.connector_name.to_string(),
                    ),
                    metrics::request::add_attributes(
                        "payment_method",
                        router_data.payment_method.to_string(),
                    ),
                ],
            );

            let pm_token = match resp.response {
                Ok(response) => match response {
                    types::PaymentsResponseData::TokenizationResponse { token } => Some(token),
                    _ => None,
                },
                Err(err) => {
                    logger::debug!(payment_method_tokenization_error=?err);
                    None
                }
            };
            Ok(pm_token)
        }
        _ => Ok(None),
    }
}

pub fn add_connector_mandate_details_in_payment_method(
    payment_method_type: Option<storage_enums::PaymentMethodType>,
    authorized_amount: Option<i64>,
    authorized_currency: Option<storage_enums::Currency>,
    merchant_connector_id: Option<String>,
    connector_mandate_id: Option<String>,
) -> Option<storage::PaymentsMandateReference> {
    let mut mandate_details = HashMap::new();

    if let Some((mca_id, connector_mandate_id)) =
        merchant_connector_id.clone().zip(connector_mandate_id)
    {
        mandate_details.insert(
            mca_id,
            storage::PaymentsMandateReferenceRecord {
                connector_mandate_id,
                payment_method_type,
                original_payment_authorized_amount: authorized_amount,
                original_payment_authorized_currency: authorized_currency,
            },
        );
        Some(storage::PaymentsMandateReference(mandate_details))
    } else {
        None
    }
}

pub fn update_connector_mandate_details_in_payment_method(
    payment_method: diesel_models::PaymentMethod,
    payment_method_type: Option<storage_enums::PaymentMethodType>,
    authorized_amount: Option<i64>,
    authorized_currency: Option<storage_enums::Currency>,
    merchant_connector_id: Option<String>,
    connector_mandate_id: Option<String>,
) -> RouterResult<Option<serde_json::Value>> {
    let mandate_reference = match payment_method.connector_mandate_details {
        Some(_) => {
            let mandate_details = payment_method
                .connector_mandate_details
                .map(|val| {
                    val.parse_value::<storage::PaymentsMandateReference>("PaymentsMandateReference")
                })
                .transpose()
                .change_context(errors::ApiErrorResponse::InternalServerError)
                .attach_printable("Failed to deserialize to Payment Mandate Reference ")?;

            if let Some((mca_id, connector_mandate_id)) =
                merchant_connector_id.clone().zip(connector_mandate_id)
            {
                let updated_record = storage::PaymentsMandateReferenceRecord {
                    connector_mandate_id: connector_mandate_id.clone(),
                    payment_method_type,
                    original_payment_authorized_amount: authorized_amount,
                    original_payment_authorized_currency: authorized_currency,
                };
                mandate_details.map(|mut payment_mandate_reference| {
                    payment_mandate_reference
                        .entry(mca_id)
                        .and_modify(|pm| *pm = updated_record)
                        .or_insert(storage::PaymentsMandateReferenceRecord {
                            connector_mandate_id,
                            payment_method_type,
                            original_payment_authorized_amount: authorized_amount,
                            original_payment_authorized_currency: authorized_currency,
                        });
                    payment_mandate_reference
                })
            } else {
                None
            }
        }
        None => add_connector_mandate_details_in_payment_method(
            payment_method_type,
            authorized_amount,
            authorized_currency,
            merchant_connector_id,
            connector_mandate_id,
        ),
    };
    let connector_mandate_details = mandate_reference
        .map(|mand| mand.encode_to_value())
        .transpose()
        .change_context(errors::ApiErrorResponse::InternalServerError)
        .attach_printable("Unable to serialize customer acceptance to value")?;

    Ok(connector_mandate_details)
}

use std::marker::PhantomData;

use api_models::{
    enums::FrmSuggestion, mandates::RecurringDetails, payments::RequestSurchargeDetails,
};
use async_trait::async_trait;
use common_utils::ext_traits::{AsyncExt, Encode, ValueExt};
use error_stack::{report, ResultExt};
use router_derive::PaymentOperation;
use router_env::{instrument, tracing};

use super::{BoxedOperation, Domain, GetTracker, Operation, UpdateTracker, ValidateRequest};
use crate::{
    core::{
        errors::{self, CustomResult, RouterResult, StorageErrorExt},
        mandate::helpers as m_helpers,
        payment_methods::PaymentMethodRetrieve,
        payments::{self, helpers, operations, CustomerDetails, PaymentAddress, PaymentData},
        utils as core_utils,
    },
    db::StorageInterface,
    routes::{app::ReqState, AppState},
    services,
    types::{
        api::{self, PaymentIdTypeExt},
        domain,
        storage::{self, enums as storage_enums, payment_attempt::PaymentAttemptExt},
    },
    utils::OptionExt,
};

#[derive(Debug, Clone, Copy, PaymentOperation)]
#[operation(operations = "all", flow = "authorize")]
pub struct PaymentUpdate;

#[async_trait]
impl<F: Send + Clone, Ctx: PaymentMethodRetrieve>
    GetTracker<F, PaymentData<F>, api::PaymentsRequest, Ctx> for PaymentUpdate
{
    #[instrument(skip_all)]
    async fn get_trackers<'a>(
        &'a self,
        state: &'a AppState,
        payment_id: &api::PaymentIdType,
        request: &api::PaymentsRequest,
        merchant_account: &domain::MerchantAccount,
        key_store: &domain::MerchantKeyStore,
        auth_flow: services::AuthFlow,
        _payment_confirm_source: Option<common_enums::PaymentSource>,
    ) -> RouterResult<operations::GetTrackerResponse<'a, F, api::PaymentsRequest, Ctx>> {
        let (mut payment_intent, mut payment_attempt, currency): (_, _, storage_enums::Currency);

        let payment_id = payment_id
            .get_payment_intent_id()
            .change_context(errors::ApiErrorResponse::PaymentNotFound)?;
        let merchant_id = &merchant_account.merchant_id;
        let storage_scheme = merchant_account.storage_scheme;

        let db = &*state.store;

        payment_intent = db
            .find_payment_intent_by_payment_id_merchant_id(&payment_id, merchant_id, storage_scheme)
            .await
            .to_not_found_response(errors::ApiErrorResponse::PaymentNotFound)?;

        if let Some(order_details) = &request.order_details {
            helpers::validate_order_details_amount(
                order_details.to_owned(),
                payment_intent.amount,
                false,
            )?;
        }

        payment_intent.setup_future_usage = request
            .setup_future_usage
            .or(payment_intent.setup_future_usage);

        helpers::validate_customer_access(&payment_intent, auth_flow, request)?;

        helpers::validate_card_data(
            request
                .payment_method_data
                .as_ref()
                .and_then(|pmd| pmd.payment_method_data.clone()),
        )?;

        helpers::validate_payment_status_against_not_allowed_statuses(
            &payment_intent.status,
            &[
                storage_enums::IntentStatus::Failed,
                storage_enums::IntentStatus::Succeeded,
                storage_enums::IntentStatus::PartiallyCaptured,
                storage_enums::IntentStatus::RequiresCapture,
            ],
            "update",
        )?;

        helpers::authenticate_client_secret(request.client_secret.as_ref(), &payment_intent)?;

        payment_intent = db
            .find_payment_intent_by_payment_id_merchant_id(&payment_id, merchant_id, storage_scheme)
            .await
            .to_not_found_response(errors::ApiErrorResponse::PaymentNotFound)?;

        payment_intent.order_details = request
            .get_order_details_as_value()
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("Failed to convert order details to value")?
            .or(payment_intent.order_details);

        payment_attempt = db
            .find_payment_attempt_by_payment_id_merchant_id_attempt_id(
                payment_intent.payment_id.as_str(),
                merchant_id,
                payment_intent.active_attempt.get_id().as_str(),
                storage_scheme,
            )
            .await
            .to_not_found_response(errors::ApiErrorResponse::PaymentNotFound)?;

        let customer_acceptance = request.customer_acceptance.clone().map(From::from);
        let recurring_details = request.recurring_details.clone();

        let mandate_type = m_helpers::get_mandate_type(
            request.mandate_data.clone(),
            request.off_session,
            payment_intent.setup_future_usage,
            request.customer_acceptance.clone(),
            request.payment_token.clone(),
        )
        .change_context(errors::ApiErrorResponse::MandateValidationFailed {
            reason: "Expected one out of recurring_details and mandate_data but got both".into(),
        })?;

        let m_helpers::MandateGenericData {
            token,
            payment_method,
            payment_method_type,
            mandate_data,
            recurring_mandate_payment_data,
            mandate_connector,
            payment_method_info,
        } = helpers::get_token_pm_type_mandate_details(
            state,
            request,
            mandate_type.to_owned(),
            merchant_account,
            key_store,
            None,
        )
        .await?;
        helpers::validate_amount_to_capture_and_capture_method(Some(&payment_attempt), request)?;

        helpers::validate_request_amount_and_amount_to_capture(
            request.amount,
            request.amount_to_capture,
            request
                .surcharge_details
                .or(payment_attempt.get_surcharge_details()),
        )
        .change_context(errors::ApiErrorResponse::InvalidDataFormat {
            field_name: "amount_to_capture".to_string(),
            expected_format: "amount_to_capture lesser than or equal to amount".to_string(),
        })?;

        currency = request
            .currency
            .or(payment_attempt.currency)
            .get_required_value("currency")?;

        payment_attempt.payment_method = payment_method.or(payment_attempt.payment_method);
        payment_attempt.payment_method_type =
            payment_method_type.or(payment_attempt.payment_method_type);
        let customer_details = helpers::get_customer_details_from_request(request);

        let amount = request
            .amount
            .unwrap_or_else(|| payment_attempt.amount.into());

        if request.confirm.unwrap_or(false) {
            helpers::validate_customer_id_mandatory_cases(
                request.setup_future_usage.is_some(),
                &payment_intent
                    .customer_id
                    .clone()
                    .or_else(|| customer_details.customer_id.clone()),
            )?;
        }

        let shipping_address = helpers::create_or_update_address_for_payment_by_request(
            db,
            request.shipping.as_ref(),
            payment_intent.shipping_address_id.as_deref(),
            merchant_id,
            payment_intent
                .customer_id
                .as_ref()
                .or(customer_details.customer_id.as_ref()),
            key_store,
            &payment_intent.payment_id,
            merchant_account.storage_scheme,
        )
        .await?;
        let billing_address = helpers::create_or_update_address_for_payment_by_request(
            db,
            request.billing.as_ref(),
            payment_intent.billing_address_id.as_deref(),
            merchant_id,
            payment_intent
                .customer_id
                .as_ref()
                .or(customer_details.customer_id.as_ref()),
            key_store,
            &payment_intent.payment_id,
            merchant_account.storage_scheme,
        )
        .await?;

        let payment_method_billing = helpers::create_or_update_address_for_payment_by_request(
            db,
            request
                .payment_method_data
                .as_ref()
                .and_then(|pmd| pmd.billing.as_ref()),
            payment_attempt.payment_method_billing_address_id.as_deref(),
            merchant_id,
            payment_intent
                .customer_id
                .as_ref()
                .or(customer_details.customer_id.as_ref()),
            key_store,
            &payment_intent.payment_id,
            merchant_account.storage_scheme,
        )
        .await?;

        payment_intent.shipping_address_id = shipping_address.clone().map(|x| x.address_id);
        payment_intent.billing_address_id = billing_address.clone().map(|x| x.address_id);

        payment_intent.allowed_payment_method_types = request
            .get_allowed_payment_method_types_as_value()
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("Error converting allowed_payment_types to Value")?
            .or(payment_intent.allowed_payment_method_types);

        payment_intent.connector_metadata = request
            .get_connector_metadata_as_value()
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("Error converting connector_metadata to Value")?
            .or(payment_intent.connector_metadata);

        payment_intent.feature_metadata = request
            .get_feature_metadata_as_value()
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("Error converting feature_metadata to Value")?
            .or(payment_intent.feature_metadata);
        payment_intent.metadata = request.metadata.clone().or(payment_intent.metadata);
        Self::populate_payment_intent_with_request(&mut payment_intent, request);

        let token = token.or_else(|| payment_attempt.payment_token.clone());

        if request.confirm.unwrap_or(false) {
            helpers::validate_pm_or_token_given(
                &request.payment_method,
                &request
                    .payment_method_data
                    .as_ref()
                    .and_then(|pmd| pmd.payment_method_data.clone()),
                &request.payment_method_type,
                &mandate_type,
                &token,
            )?;
        }

        let token_data = if let Some(token) = token.clone() {
            Some(helpers::retrieve_payment_token_data(state, token, payment_method).await?)
        } else {
            None
        };

        let mandate_id = request
            .mandate_id
            .as_ref()
            .or_else(|| {
            request.recurring_details
                .as_ref()
                .and_then(|recurring_details| match recurring_details {
                    RecurringDetails::MandateId(id) => Some(id),
                    _ => None,
                })
        })
            .async_and_then(|mandate_id| async {
                let mandate = db
                    .find_mandate_by_merchant_id_mandate_id(merchant_id, mandate_id, merchant_account.storage_scheme)
                    .await
                    .change_context(errors::ApiErrorResponse::MandateNotFound);
                Some(mandate.and_then(|mandate_obj| {
                    match (
                        mandate_obj.network_transaction_id,
                        mandate_obj.connector_mandate_ids,
                    ) {
                        (Some(network_tx_id), _) => Ok(api_models::payments::MandateIds {
                            mandate_id: Some(mandate_obj.mandate_id),
                            mandate_reference_id: Some(
                                api_models::payments::MandateReferenceId::NetworkMandateId(
                                    network_tx_id,
                                ),
                            ),
                        }),
                        (_, Some(connector_mandate_id)) => connector_mandate_id
                        .parse_value("ConnectorMandateId")
                        .change_context(errors::ApiErrorResponse::MandateNotFound)
                        .map(|connector_id: api_models::payments::ConnectorMandateReferenceId| {
                            api_models::payments::MandateIds {
                                mandate_id: Some(mandate_obj.mandate_id),
                                mandate_reference_id: Some(api_models::payments::MandateReferenceId::ConnectorMandateId(
                                    api_models::payments::ConnectorMandateReferenceId {connector_mandate_id:connector_id.connector_mandate_id,payment_method_id:connector_id.payment_method_id, update_history: None },
                                ))
                            }
                         }),
                        (_, _) => Ok(api_models::payments::MandateIds {
                            mandate_id: Some(mandate_obj.mandate_id),
                            mandate_reference_id: None,
                        }),
                    }
                }))
            })
            .await
            .transpose()?;
        let (next_operation, amount): (BoxedOperation<'a, F, api::PaymentsRequest, Ctx>, _) =
            if request.confirm.unwrap_or(false) {
                let amount = {
                    let amount = request
                        .amount
                        .map(Into::into)
                        .unwrap_or(payment_attempt.amount);
                    payment_attempt.amount = amount;
                    payment_intent.amount = amount;
                    let surcharge_amount = request
                        .surcharge_details
                        .as_ref()
                        .map(RequestSurchargeDetails::get_total_surcharge_amount)
                        .or(payment_attempt.get_total_surcharge_amount());
                    (amount + surcharge_amount.unwrap_or(0)).into()
                };
                (Box::new(operations::PaymentConfirm), amount)
            } else {
                (Box::new(self), amount)
            };

        payment_intent.status = match request.payment_method_data.as_ref() {
            Some(_) => {
                if request.confirm.unwrap_or(false) {
                    payment_intent.status
                } else {
                    storage_enums::IntentStatus::RequiresConfirmation
                }
            }
            None => storage_enums::IntentStatus::RequiresPaymentMethod,
        };
        payment_intent.request_external_three_ds_authentication = request
            .request_external_three_ds_authentication
            .or(payment_intent.request_external_three_ds_authentication);

        Self::populate_payment_attempt_with_request(&mut payment_attempt, request);

        let creds_identifier = request
            .merchant_connector_details
            .as_ref()
            .map(|mcd| mcd.creds_identifier.to_owned());
        request
            .merchant_connector_details
            .to_owned()
            .async_map(|mcd| async {
                helpers::insert_merchant_connector_creds_to_config(
                    db,
                    merchant_account.merchant_id.as_str(),
                    mcd,
                )
                .await
            })
            .await
            .transpose()?;

        // The operation merges mandate data from both request and payment_attempt
        let setup_mandate = mandate_data.map(Into::into);
        let mandate_details_present =
            payment_attempt.mandate_details.is_some() || request.mandate_data.is_some();
        helpers::validate_mandate_data_and_future_usage(
            payment_intent.setup_future_usage,
            mandate_details_present,
        )?;
        let profile_id = payment_intent
            .profile_id
            .as_ref()
            .get_required_value("profile_id")
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("'profile_id' not set in payment intent")?;

        let business_profile = db
            .find_business_profile_by_profile_id(profile_id)
            .await
            .to_not_found_response(errors::ApiErrorResponse::BusinessProfileNotFound {
                id: profile_id.to_string(),
            })?;

        let surcharge_details = request.surcharge_details.map(|request_surcharge_details| {
            payments::types::SurchargeDetails::from((&request_surcharge_details, &payment_attempt))
        });

        let payment_data = PaymentData {
            flow: PhantomData,
            payment_intent,
            payment_attempt,
            currency,
            amount,
            email: request.email.clone(),
            mandate_id,
            mandate_connector,
            token,
            token_data,
            setup_mandate,
            customer_acceptance,
            address: PaymentAddress::new(
                shipping_address.as_ref().map(From::from),
                billing_address.as_ref().map(From::from),
                payment_method_billing.as_ref().map(From::from),
            ),
            confirm: request.confirm,
            payment_method_data: request
                .payment_method_data
                .as_ref()
                .and_then(|pmd| pmd.payment_method_data.clone()),
            payment_method_info,
            force_sync: None,
            refunds: vec![],
            disputes: vec![],
            attempts: None,
            sessions_token: vec![],
            card_cvc: request.card_cvc.clone(),
            creds_identifier,
            pm_token: None,
            connector_customer_id: None,
            recurring_mandate_payment_data,
            ephemeral_key: None,
            multiple_capture_data: None,
            redirect_response: None,
            surcharge_details,
            frm_message: None,
            payment_link_data: None,
            incremental_authorization_details: None,
            authorizations: vec![],
            authentication: None,
            frm_metadata: request.frm_metadata.clone(),
            recurring_details,
            poll_config: None,
        };

        let get_trackers_response = operations::GetTrackerResponse {
            operation: next_operation,
            customer_details: Some(customer_details),
            payment_data,
            business_profile,
            mandate_type,
        };

        Ok(get_trackers_response)
    }
}

#[async_trait]
impl<F: Clone + Send, Ctx: PaymentMethodRetrieve> Domain<F, api::PaymentsRequest, Ctx>
    for PaymentUpdate
{
    #[instrument(skip_all)]
    async fn get_or_create_customer_details<'a>(
        &'a self,
        db: &dyn StorageInterface,
        payment_data: &mut PaymentData<F>,
        request: Option<CustomerDetails>,
        key_store: &domain::MerchantKeyStore,
        storage_scheme: common_enums::enums::MerchantStorageScheme,
    ) -> CustomResult<
        (
            BoxedOperation<'a, F, api::PaymentsRequest, Ctx>,
            Option<domain::Customer>,
        ),
        errors::StorageError,
    > {
        helpers::create_customer_if_not_exist(
            Box::new(self),
            db,
            payment_data,
            request,
            &key_store.merchant_id,
            key_store,
            storage_scheme,
        )
        .await
    }

    #[instrument(skip_all)]
    async fn make_pm_data<'a>(
        &'a self,
        state: &'a AppState,
        payment_data: &mut PaymentData<F>,
        storage_scheme: storage_enums::MerchantStorageScheme,
        merchant_key_store: &domain::MerchantKeyStore,
        customer: &Option<domain::Customer>,
    ) -> RouterResult<(
        BoxedOperation<'a, F, api::PaymentsRequest, Ctx>,
        Option<api::PaymentMethodData>,
        Option<String>,
    )> {
        helpers::make_pm_data(
            Box::new(self),
            state,
            payment_data,
            merchant_key_store,
            customer,
            storage_scheme,
        )
        .await
    }

    #[instrument(skip_all)]
    async fn add_task_to_process_tracker<'a>(
        &'a self,
        _state: &'a AppState,
        _payment_attempt: &storage::PaymentAttempt,
        _requeue: bool,
        _schedule_time: Option<time::PrimitiveDateTime>,
    ) -> CustomResult<(), errors::ApiErrorResponse> {
        Ok(())
    }

    async fn get_connector<'a>(
        &'a self,
        _merchant_account: &domain::MerchantAccount,
        state: &AppState,
        request: &api::PaymentsRequest,
        _payment_intent: &storage::PaymentIntent,
        _key_store: &domain::MerchantKeyStore,
    ) -> CustomResult<api::ConnectorChoice, errors::ApiErrorResponse> {
        helpers::get_connector_default(state, request.routing.clone()).await
    }

    #[instrument(skip_all)]
    async fn guard_payment_against_blocklist<'a>(
        &'a self,
        _state: &AppState,
        _merchant_account: &domain::MerchantAccount,
        _payment_data: &mut PaymentData<F>,
    ) -> CustomResult<bool, errors::ApiErrorResponse> {
        Ok(false)
    }
}

#[async_trait]
impl<F: Clone, Ctx: PaymentMethodRetrieve>
    UpdateTracker<F, PaymentData<F>, api::PaymentsRequest, Ctx> for PaymentUpdate
{
    #[instrument(skip_all)]
    async fn update_trackers<'b>(
        &'b self,
        state: &'b AppState,
        _req_state: ReqState,
        mut payment_data: PaymentData<F>,
        customer: Option<domain::Customer>,
        storage_scheme: storage_enums::MerchantStorageScheme,
        _updated_customer: Option<storage::CustomerUpdate>,
        _key_store: &domain::MerchantKeyStore,
        _frm_suggestion: Option<FrmSuggestion>,
        _header_payload: api::HeaderPayload,
    ) -> RouterResult<(
        BoxedOperation<'b, F, api::PaymentsRequest, Ctx>,
        PaymentData<F>,
    )>
    where
        F: 'b + Send,
    {
        let is_payment_method_unavailable =
            payment_data.payment_attempt.payment_method_id.is_none()
                && payment_data.payment_intent.status
                    == storage_enums::IntentStatus::RequiresPaymentMethod;

        let payment_method = payment_data.payment_attempt.payment_method;

        let get_attempt_status = || {
            if is_payment_method_unavailable {
                storage_enums::AttemptStatus::PaymentMethodAwaited
            } else {
                storage_enums::AttemptStatus::ConfirmationAwaited
            }
        };
        let profile_id = payment_data
            .payment_intent
            .profile_id
            .as_ref()
            .get_required_value("profile_id")
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("'profile_id' not set in payment intent")?;

        let additional_pm_data = payment_data
            .payment_method_data
            .as_ref()
            .async_map(|payment_method_data| async {
                helpers::get_additional_payment_data(payment_method_data, &*state.store, profile_id)
                    .await
            })
            .await
            .as_ref()
            .map(Encode::encode_to_value)
            .transpose()
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("Failed to encode additional pm data")?;

        let business_sub_label = payment_data.payment_attempt.business_sub_label.clone();

        let payment_method_type = payment_data.payment_attempt.payment_method_type;
        let payment_experience = payment_data.payment_attempt.payment_experience;
        let amount_to_capture = payment_data.payment_attempt.amount_to_capture;
        let capture_method = payment_data.payment_attempt.capture_method;

        let surcharge_amount = payment_data
            .surcharge_details
            .as_ref()
            .map(|surcharge_details| surcharge_details.surcharge_amount);
        let tax_amount = payment_data
            .surcharge_details
            .as_ref()
            .map(|surcharge_details| surcharge_details.tax_on_surcharge_amount);
        payment_data.payment_attempt = state
            .store
            .update_payment_attempt_with_attempt_id(
                payment_data.payment_attempt,
                storage::PaymentAttemptUpdate::Update {
                    amount: payment_data.amount.into(),
                    currency: payment_data.currency,
                    status: get_attempt_status(),
                    authentication_type: None,
                    payment_method,
                    payment_token: payment_data.token.clone(),
                    payment_method_data: additional_pm_data,
                    payment_experience,
                    payment_method_type,
                    business_sub_label,
                    amount_to_capture,
                    capture_method,
                    surcharge_amount,
                    tax_amount,
                    fingerprint_id: None,
                    updated_by: storage_scheme.to_string(),
                },
                storage_scheme,
            )
            .await
            .to_not_found_response(errors::ApiErrorResponse::PaymentNotFound)?;

        let customer_id = customer.map(|c| c.customer_id);

        let intent_status = {
            let current_intent_status = payment_data.payment_intent.status;
            if is_payment_method_unavailable {
                storage_enums::IntentStatus::RequiresPaymentMethod
            } else if !payment_data.confirm.unwrap_or(true)
                || current_intent_status == storage_enums::IntentStatus::RequiresCustomerAction
            {
                storage_enums::IntentStatus::RequiresConfirmation
            } else {
                payment_data.payment_intent.status
            }
        };

        let (shipping_address, billing_address) = (
            payment_data.payment_intent.shipping_address_id.clone(),
            payment_data.payment_intent.billing_address_id.clone(),
        );

        let return_url = payment_data.payment_intent.return_url.clone();
        let setup_future_usage = payment_data.payment_intent.setup_future_usage;
        let business_label = payment_data.payment_intent.business_label.clone();
        let business_country = payment_data.payment_intent.business_country;
        let description = payment_data.payment_intent.description.clone();
        let statement_descriptor_name = payment_data
            .payment_intent
            .statement_descriptor_name
            .clone();
        let statement_descriptor_suffix = payment_data
            .payment_intent
            .statement_descriptor_suffix
            .clone();
        let order_details = payment_data.payment_intent.order_details.clone();
        let metadata = payment_data.payment_intent.metadata.clone();
        let session_expiry = payment_data.payment_intent.session_expiry;

        payment_data.payment_intent = state
            .store
            .update_payment_intent(
                payment_data.payment_intent.clone(),
                storage::PaymentIntentUpdate::Update {
                    amount: payment_data.amount.into(),
                    currency: payment_data.currency,
                    setup_future_usage,
                    status: intent_status,
                    customer_id: customer_id.clone(),
                    shipping_address_id: shipping_address,
                    billing_address_id: billing_address,
                    return_url,
                    business_country,
                    business_label,
                    description,
                    statement_descriptor_name,
                    statement_descriptor_suffix,
                    order_details,
                    metadata,
                    payment_confirm_source: None,
                    updated_by: storage_scheme.to_string(),
                    fingerprint_id: None,
                    session_expiry,
                    request_external_three_ds_authentication: payment_data
                        .payment_intent
                        .request_external_three_ds_authentication,
                },
                storage_scheme,
            )
            .await
            .to_not_found_response(errors::ApiErrorResponse::PaymentNotFound)?;

        Ok((
            payments::is_confirm(self, payment_data.confirm),
            payment_data,
        ))
    }
}

impl<F: Send + Clone, Ctx: PaymentMethodRetrieve> ValidateRequest<F, api::PaymentsRequest, Ctx>
    for PaymentUpdate
{
    #[instrument(skip_all)]
    fn validate_request<'a, 'b>(
        &'b self,
        request: &api::PaymentsRequest,
        merchant_account: &'a domain::MerchantAccount,
    ) -> RouterResult<(
        BoxedOperation<'b, F, api::PaymentsRequest, Ctx>,
        operations::ValidateResult<'a>,
    )> {
        helpers::validate_customer_details_in_request(request)?;
        if let Some(session_expiry) = &request.session_expiry {
            helpers::validate_session_expiry(session_expiry.to_owned())?;
        }
        let payment_id = request
            .payment_id
            .clone()
            .ok_or(report!(errors::ApiErrorResponse::PaymentNotFound))?;

        let request_merchant_id = request.merchant_id.as_deref();
        helpers::validate_merchant_id(&merchant_account.merchant_id, request_merchant_id)
            .change_context(errors::ApiErrorResponse::InvalidDataFormat {
                field_name: "merchant_id".to_string(),
                expected_format: "merchant_id from merchant account".to_string(),
            })?;

        helpers::validate_request_amount_and_amount_to_capture(
            request.amount,
            request.amount_to_capture,
            request.surcharge_details,
        )
        .change_context(errors::ApiErrorResponse::InvalidDataFormat {
            field_name: "amount_to_capture".to_string(),
            expected_format: "amount_to_capture lesser than or equal to amount".to_string(),
        })?;

        helpers::validate_payment_method_fields_present(request)?;

        let _mandate_type = helpers::validate_mandate(request, false)?;

        helpers::validate_recurring_details_and_token(
            &request.recurring_details,
            &request.payment_token,
            &request.mandate_id,
        )?;

        Ok((
            Box::new(self),
            operations::ValidateResult {
                merchant_id: &merchant_account.merchant_id,
                payment_id: payment_id.and_then(|id| core_utils::validate_id(id, "payment_id"))?,
                storage_scheme: merchant_account.storage_scheme,
                requeue: matches!(
                    request.retry_action,
                    Some(api_models::enums::RetryAction::Requeue)
                ),
            },
        ))
    }
}

impl PaymentUpdate {
    fn populate_payment_attempt_with_request(
        payment_attempt: &mut storage::PaymentAttempt,
        request: &api::PaymentsRequest,
    ) {
        request
            .business_sub_label
            .clone()
            .map(|bsl| payment_attempt.business_sub_label.replace(bsl));
        request
            .payment_method_type
            .map(|pmt| payment_attempt.payment_method_type.replace(pmt));
        request
            .payment_experience
            .map(|experience| payment_attempt.payment_experience.replace(experience));
        payment_attempt.amount_to_capture = request
            .amount_to_capture
            .or(payment_attempt.amount_to_capture);
        request
            .capture_method
            .map(|i| payment_attempt.capture_method.replace(i));
    }
    fn populate_payment_intent_with_request(
        payment_intent: &mut storage::PaymentIntent,
        request: &api::PaymentsRequest,
    ) {
        request
            .return_url
            .clone()
            .map(|i| payment_intent.return_url.replace(i.to_string()));

        payment_intent.business_country = request.business_country;

        payment_intent
            .business_label
            .clone_from(&request.business_label);

        request
            .description
            .clone()
            .map(|i| payment_intent.description.replace(i));

        request
            .statement_descriptor_name
            .clone()
            .map(|i| payment_intent.statement_descriptor_name.replace(i));

        request
            .statement_descriptor_suffix
            .clone()
            .map(|i| payment_intent.statement_descriptor_suffix.replace(i));

        request
            .client_secret
            .clone()
            .map(|i| payment_intent.client_secret.replace(i));
    }
}

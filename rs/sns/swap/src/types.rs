use crate::logs::{ERROR, INFO};
use crate::pb::v1::{
    error_refund_icp_response, sns_neuron_recipe::Investor, BuyerState, CfInvestment, CfNeuron,
    CfParticipant, DirectInvestment, ErrorRefundIcpResponse, FinalizeSwapResponse, Init, Lifecycle,
    OpenRequest, Params, SetDappControllersCallResult, SetModeCallResult,
    SettleCommunityFundParticipationResult, SnsNeuronRecipe, SweepResult, TransferableAmount,
};
use crate::swap::is_valid_principal;
use ic_base_types::{CanisterId, PrincipalId};
use ic_canister_log::log;
use ic_icrc1::{Account, Subaccount};
use ic_ledger_core::Tokens;
use ic_nervous_system_common::ledger::ICRC1Ledger;
use ic_nervous_system_common::SECONDS_PER_DAY;
use std::str::FromStr;

pub fn validate_principal(p: &str) -> Result<(), String> {
    let _ = PrincipalId::from_str(p).map_err(|x| {
        format!(
            "Couldn't validate PrincipalId. String \"{}\" could not be converted to PrincipalId: {}",
            p, x
        )
    })?;
    Ok(())
}

pub fn validate_canister_id(p: &str) -> Result<(), String> {
    let pp = PrincipalId::from_str(p).map_err(|x| {
        format!(
            "Couldn't validate CanisterId. String \"{}\" could not be converted to PrincipalId: {}",
            p, x
        )
    })?;
    let _cid = CanisterId::new(pp).map_err(|x| {
        format!(
            "Couldn't validate CanisterId. PrincipalId \"{}\" could not be converted to CanisterId: {}",
            pp,
            x
        )
    })?;
    Ok(())
}

impl ErrorRefundIcpResponse {
    pub(crate) fn new_ok(block_height: u64) -> Self {
        use error_refund_icp_response::{Ok, Result};

        Self {
            result: Some(Result::Ok(Ok {
                block_height: Some(block_height),
            })),
        }
    }

    pub(crate) fn new_precondition_error(description: impl ToString) -> Self {
        Self::new_error(
            error_refund_icp_response::err::Type::Precondition,
            description,
        )
    }

    pub(crate) fn new_invalid_request_error(description: impl ToString) -> Self {
        Self::new_error(
            error_refund_icp_response::err::Type::InvalidRequest,
            description,
        )
    }

    pub(crate) fn new_external_error(description: impl ToString) -> Self {
        Self::new_error(error_refund_icp_response::err::Type::External, description)
    }

    fn new_error(
        error_type: error_refund_icp_response::err::Type,
        description: impl ToString,
    ) -> Self {
        use error_refund_icp_response::{Err, Result};

        Self {
            result: Some(Result::Err(Err {
                error_type: Some(error_type as i32),
                description: Some(description.to_string()),
            })),
        }
    }
}

impl Init {
    pub fn nns_governance_or_panic(&self) -> CanisterId {
        CanisterId::new(PrincipalId::from_str(&self.nns_governance_canister_id).unwrap()).unwrap()
    }

    pub fn nns_governance(&self) -> Result<CanisterId, String> {
        let principal_id = PrincipalId::from_str(&self.nns_governance_canister_id)
            .map_err(|err| err.to_string())?;

        CanisterId::new(principal_id).map_err(|err| err.to_string())
    }

    pub fn sns_root_or_panic(&self) -> CanisterId {
        CanisterId::new(PrincipalId::from_str(&self.sns_root_canister_id).unwrap()).unwrap()
    }

    pub fn sns_governance_or_panic(&self) -> CanisterId {
        CanisterId::new(PrincipalId::from_str(&self.sns_governance_canister_id).unwrap()).unwrap()
    }

    pub fn sns_governance(&self) -> Result<CanisterId, String> {
        let principal_id = PrincipalId::from_str(&self.sns_governance_canister_id)
            .map_err(|err| err.to_string())?;

        CanisterId::new(principal_id).map_err(|err| err.to_string())
    }

    pub fn sns_ledger_or_panic(&self) -> CanisterId {
        CanisterId::new(PrincipalId::from_str(&self.sns_ledger_canister_id).unwrap()).unwrap()
    }

    pub fn icp_ledger_or_panic(&self) -> CanisterId {
        CanisterId::new(PrincipalId::from_str(&self.icp_ledger_canister_id).unwrap()).unwrap()
    }

    pub fn validate(&self) -> Result<(), String> {
        validate_canister_id(&self.nns_governance_canister_id)?;
        validate_canister_id(&self.sns_governance_canister_id)?;
        validate_canister_id(&self.sns_ledger_canister_id)?;
        validate_canister_id(&self.icp_ledger_canister_id)?;
        validate_canister_id(&self.sns_root_canister_id)?;

        if self.fallback_controller_principal_ids.is_empty() {
            return Err("at least one fallback controller required".to_string());
        }
        for fc in &self.fallback_controller_principal_ids {
            validate_principal(fc)?;
        }

        if self.transaction_fee_e8s.is_none() {
            return Err("transaction_fee_e8s is required.".to_string());
        }
        // The value itself is not checked; only that it is supplied. Needs to
        // match the value in SNS ledger though.

        if self.neuron_minimum_stake_e8s.is_none() {
            return Err("neuron_minimum_stake_e8s is required.".to_string());
        }
        // As with transaction_fee_e8s, the value itself is not checked; only
        // that it is supplied. Needs to match the value in SNS governance
        // though.

        Ok(())
    }
}

impl Params {
    pub fn validate(&self, init: &Init) -> Result<(), String> {
        if self.min_icp_e8s == 0 {
            return Err("min_icp_e8s must be > 0".to_string());
        }

        if self.min_participants == 0 {
            return Err("min_participants must be > 0".to_string());
        }

        let transaction_fee_e8s = init
            .transaction_fee_e8s
            .expect("transaction_fee_e8s was not supplied.");

        let neuron_minimum_stake_e8s = init
            .neuron_minimum_stake_e8s
            .expect("neuron_minimum_stake_e8s was not supplied");

        let neuron_basket_count = self
            .neuron_basket_construction_parameters
            .as_ref()
            .expect("participant_neuron_basket not populated.")
            .count as u128;

        let min_participant_sns_e8s = self.min_participant_icp_e8s as u128
            * self.sns_token_e8s as u128
            / self.max_icp_e8s as u128;

        let min_participant_icp_e8s_big_enough = min_participant_sns_e8s
            >= neuron_basket_count * (neuron_minimum_stake_e8s + transaction_fee_e8s) as u128;

        if !min_participant_icp_e8s_big_enough {
            return Err(format!(
                "min_participant_icp_e8s={} is too small. It needs to be \
                 large enough to ensure that participants will end up with \
                 enough SNS tokens to form {} SNS neurons, each of which \
                 require at least {} SNS e8s, plus {} e8s in transaction \
                 fees. More precisely, the following inequality must hold: \
                 min_participant_icp_e8s >= neuron_basket_count * (neuron_minimum_stake_e8s + transaction_fee_e8s) * max_icp_e8s / sns_token_e8s \
                 (where / denotes floor division).",
                self.min_participant_icp_e8s,
                neuron_basket_count,
                neuron_minimum_stake_e8s,
                transaction_fee_e8s,
            ));
        }

        if self.sns_token_e8s == 0 {
            return Err("sns_token_e8s must be > 0".to_string());
        }

        if self.max_participant_icp_e8s < self.min_participant_icp_e8s {
            return Err(format!(
                "max_participant_icp_e8s ({}) must be >= min_participant_icp_e8s ({})",
                self.max_participant_icp_e8s, self.min_participant_icp_e8s
            ));
        }

        if self.min_icp_e8s > self.max_icp_e8s {
            return Err(format!(
                "min_icp_e8s ({}) must be <= max_icp_e8s ({})",
                self.min_icp_e8s, self.max_icp_e8s
            ));
        }

        if self.max_participant_icp_e8s > self.max_icp_e8s {
            return Err(format!(
                "max_participant_icp_e8s ({}) must be <= max_icp_e8s ({})",
                self.max_participant_icp_e8s, self.max_icp_e8s
            ));
        }

        // Cap `max_icp_e8s` at 1 billion ICP
        if self.max_icp_e8s > /* 1B */ 1_000_000_000 * /* e8s per ICP */ 100_000_000 {
            return Err(format!(
                "max_icp_e8s ({}) can be at most 1B ICP",
                self.max_icp_e8s
            ));
        }

        // Cap `min_participant_icp_e8s` at 100.
        if self.min_participants > 100 {
            return Err(format!(
                "min_participants ({}) can be at most 100",
                self.min_participants
            ));
        }

        // 100 * 1B * E8S should fit in a u64.
        assert!(self
            .max_icp_e8s
            .checked_mul(self.min_participants as u64)
            .is_some());

        if self.max_icp_e8s
            < (self.min_participants as u64).saturating_mul(self.min_participant_icp_e8s)
        {
            return Err(format!(
                "max_icp_e8s ({}) must be >= min_participants ({}) * min_participant_icp_e8s ({})",
                self.max_icp_e8s, self.min_participants, self.min_participant_icp_e8s
            ));
        }

        if self.neuron_basket_construction_parameters.is_none() {
            return Err("neuron_basket_construction_parameters must be provided".to_string());
        }

        let neuron_basket = self
            .neuron_basket_construction_parameters
            .as_ref()
            .expect("Expected neuron_basket_construction_parameters to be set");

        if neuron_basket.count == 0 {
            return Err(format!(
                "neuron_basket_construction_parameters.count ({}) must be > 0",
                neuron_basket.count,
            ));
        }

        if neuron_basket.dissolve_delay_interval_seconds == 0 {
            return Err(format!(
                "neuron_basket_construction_parameters.dissolve_delay_interval_seconds ({}) must be > 0",
                neuron_basket.dissolve_delay_interval_seconds,
            ));
        }

        // The maximum dissolve delay is one dissolve_delay_interval_seconds longer than count as
        // the algorithm adds a random jitter in addition to the count * dissolve_delay_interval_seconds.
        let maximum_dissolve_delay = neuron_basket
            .count
            .saturating_add(1)
            .saturating_mul(neuron_basket.dissolve_delay_interval_seconds);

        if maximum_dissolve_delay == u64::MAX {
            return Err(
                "Chosen neuron_basket_construction_parameters will result in u64 overflow"
                    .to_string(),
            );
        }

        Ok(())
    }

    pub fn is_valid_at(&self, now_seconds: u64) -> bool {
        now_seconds.saturating_add(SECONDS_PER_DAY) <= self.swap_due_timestamp_seconds
            && self.swap_due_timestamp_seconds <= now_seconds.saturating_add(90 * SECONDS_PER_DAY)
    }
}

impl BuyerState {
    pub fn new(amount_icp_e8s: u64) -> Self {
        Self {
            icp: Some(TransferableAmount {
                amount_e8s: amount_icp_e8s,
                transfer_start_timestamp_seconds: 0,
                transfer_success_timestamp_seconds: 0,
            }),
        }
    }
    pub fn validate(&self) -> Result<(), String> {
        if let Some(icp) = &self.icp {
            icp.validate()
        } else {
            Err("Field 'icp' is missing but required".to_string())
        }
    }

    pub fn amount_icp_e8s(&self) -> u64 {
        if let Some(icp) = &self.icp {
            return icp.amount_e8s;
        }
        0
    }

    pub fn set_amount_icp_e8s(&mut self, val: u64) {
        if let Some(ref mut icp) = &mut self.icp {
            icp.amount_e8s = val;
        } else {
            self.icp = Some(TransferableAmount {
                amount_e8s: val,
                transfer_start_timestamp_seconds: 0,
                transfer_success_timestamp_seconds: 0,
            });
        }
    }
}

impl TransferableAmount {
    pub fn validate(&self) -> Result<(), String> {
        if self.transfer_start_timestamp_seconds == 0 && self.transfer_success_timestamp_seconds > 0
        {
            // Successful transfer without start time.
            return Err(format!("Invariant violation: transfer_start_timestamp_seconds is zero but transfer_success_timestamp_seconds ({}) is non-zero", self.transfer_success_timestamp_seconds));
        }
        if self.transfer_start_timestamp_seconds > self.transfer_success_timestamp_seconds
            && self.transfer_success_timestamp_seconds > 0
        {
            // Successful transfer before the transfer started.
            return Err(format!("Invariant violation: transfer_start_timestamp_seconds ({}) > transfer_success_timestamp_seconds ({}) > 0", self.transfer_start_timestamp_seconds, self.transfer_success_timestamp_seconds));
        }
        Ok(())
    }

    pub(crate) async fn transfer_helper(
        &mut self,
        now_fn: fn(bool) -> u64,
        fee: Tokens,
        subaccount: Option<Subaccount>,
        dst: &Account,
        ledger: &dyn ICRC1Ledger,
    ) -> TransferResult {
        let amount = Tokens::from_e8s(self.amount_e8s);
        if amount <= fee {
            // Skip: amount too small...
            return TransferResult::AmountTooSmall;
        }
        if self.transfer_start_timestamp_seconds > 0 {
            // Operation in progress...
            return TransferResult::AlreadyStarted;
        }
        self.transfer_start_timestamp_seconds = now_fn(false);

        // The ICRC1Ledger Trait converts any errors to Err(NervousSystemError).
        // No panics should occur when issuing this transfer.
        let result = ledger
            .transfer_funds(
                amount.get_e8s().saturating_sub(fee.get_e8s()),
                fee.get_e8s(),
                subaccount,
                dst.clone(),
                0,
            )
            .await;
        if self.transfer_start_timestamp_seconds == 0 {
            log!(
                ERROR,
                "Token disburse logic error: expected transfer start time",
            );
        }
        match result {
            Ok(h) => {
                self.transfer_success_timestamp_seconds = now_fn(true);
                log!(
                    INFO,
                    "Transferred {} from subaccount {:?} to {} at height {} in Ledger Canister {}",
                    amount,
                    subaccount,
                    dst,
                    h,
                    ledger.canister_id()
                );
                TransferResult::Success(h)
            }
            Err(e) => {
                self.transfer_start_timestamp_seconds = 0;
                self.transfer_success_timestamp_seconds = 0;
                log!(
                    ERROR,
                    "Failed to transfer {} from subaccount {:#?}: {}",
                    amount,
                    subaccount,
                    e
                );
                TransferResult::Failure(e.to_string())
            }
        }
    }
}

impl OpenRequest {
    pub fn validate(&self, current_timestamp_seconds: u64, init: &Init) -> Result<(), String> {
        let mut defects = vec![];

        // Inspect params.
        match self.params.as_ref() {
            None => {
                defects.push("The parameters of the swap are missing.".to_string());
            }
            Some(params) => {
                if !params.is_valid_at(current_timestamp_seconds) {
                    defects.push("The parameters of the swap are invalid.".to_string());
                } else if let Err(err) = params.validate(init) {
                    defects.push(err);
                }
            }
        }

        // Inspect open_sns_token_swap_proposal_id.
        if self.open_sns_token_swap_proposal_id.is_none() {
            defects.push("The open_sns_token_swap_proposal_id field has no value.".to_string());
        }

        // Return result.
        if defects.is_empty() {
            Ok(())
        } else {
            Err(defects.join("\n"))
        }
    }
}

impl DirectInvestment {
    pub fn validate(&self) -> Result<(), String> {
        if !is_valid_principal(&self.buyer_principal) {
            return Err(format!("Invalid principal {}", self.buyer_principal));
        }
        Ok(())
    }
}

impl CfInvestment {
    pub fn validate(&self) -> Result<(), String> {
        if !is_valid_principal(&self.hotkey_principal) {
            return Err(format!(
                "Invalid hotkey principal {}",
                self.hotkey_principal
            ));
        }
        if self.nns_neuron_id == 0 {
            return Err("Missing nns_neuron_id".to_string());
        }
        Ok(())
    }
}

impl SnsNeuronRecipe {
    pub fn amount_e8s(&self) -> u64 {
        if let Some(sns) = &self.sns {
            return sns.amount_e8s;
        }
        0
    }

    pub fn validate(&self) -> Result<(), String> {
        if let Some(sns) = &self.sns {
            sns.validate()?;
        } else {
            return Err("Missing required field 'sns'".to_string());
        }
        match &self.investor {
            Some(Investor::Direct(di)) => di.validate()?,
            Some(Investor::CommunityFund(cf)) => cf.validate()?,
            None => return Err("Missing required field 'investor'".to_string()),
        }
        Ok(())
    }
}

impl CfParticipant {
    pub fn validate(&self) -> Result<(), String> {
        if !is_valid_principal(&self.hotkey_principal) {
            return Err(format!(
                "Invalid hotkey principal {}",
                self.hotkey_principal
            ));
        }
        if self.cf_neurons.is_empty() {
            return Err(format!(
                "A CF participant ({}) must have at least one neuron",
                self.hotkey_principal
            ));
        }
        for n in &self.cf_neurons {
            n.validate()?;
        }
        Ok(())
    }
    pub fn participant_total_icp_e8s(&self) -> u64 {
        self.cf_neurons
            .iter()
            .map(|x| x.amount_icp_e8s)
            .fold(0, |sum, v| sum.saturating_add(v))
    }
}

impl CfNeuron {
    pub fn validate(&self) -> Result<(), String> {
        if self.nns_neuron_id == 0 {
            return Err("nns_neuron_id must be specified".to_string());
        }
        if self.amount_icp_e8s == 0 {
            return Err("amount_icp_e8s must be specified".to_string());
        }
        Ok(())
    }
}

impl Lifecycle {
    pub fn is_terminal(&self) -> bool {
        match self {
            Self::Committed | Self::Aborted => true,

            Self::Pending | Self::Open => false,
            Self::Unspecified => {
                log!(ERROR, "A wild Lifecycle::Unspecified appeared.",);
                false
            }
        }
    }
}

impl FinalizeSwapResponse {
    pub fn with_error(error_message: String) -> Self {
        FinalizeSwapResponse {
            error_message: Some(error_message),
            ..Default::default()
        }
    }

    pub fn set_error_message(&mut self, error_message: String) {
        self.error_message = Some(error_message)
    }

    pub fn set_sweep_icp(&mut self, sweep_icp: SweepResult) {
        self.sweep_icp = Some(sweep_icp)
    }

    pub fn set_settle_community_fund_participation_result(
        &mut self,
        result: SettleCommunityFundParticipationResult,
    ) {
        self.settle_community_fund_participation_result = Some(result)
    }

    pub fn set_set_dapp_controllers_result(&mut self, result: SetDappControllersCallResult) {
        self.set_dapp_controllers_result = Some(result)
    }

    pub fn set_sweep_sns(&mut self, sweep_sns: SweepResult) {
        self.sweep_sns = Some(sweep_sns)
    }

    pub fn set_create_neuron(&mut self, create_neuron: SweepResult) {
        self.create_neuron = Some(create_neuron)
    }

    pub fn set_sns_governance_normal_mode_enabled(
        &mut self,
        set_mode_call_result: SetModeCallResult,
    ) {
        self.sns_governance_normal_mode_enabled = Some(set_mode_call_result)
    }
}

/// Result of a token transfer (commit or abort) on a ledger (ICP or
/// SNS) for a single buyer.
pub enum TransferResult {
    /// Transfer was skipped as the amount was less than the requested fee.
    AmountTooSmall,
    /// Transferred was skipped as an operation is already in progress or completed.
    AlreadyStarted,
    /// The operation was successful at the specified block height.
    Success(u64),
    /// The operation failed with the specified error message.
    Failure(String),
}

/// Intermediate struct used when generating the basket of neurons for investors.
pub(crate) struct ScheduledVestingEvent {
    /// The dissolve_delay of the neuron
    pub(crate) dissolve_delay_seconds: u64,
    /// The amount of tokens in e8s
    pub(crate) amount_e8s: u64,
}

#[cfg(test)]
mod tests {
    use crate::pb::v1::{
        params::NeuronBasketConstructionParameters, CfNeuron, CfParticipant, Init, OpenRequest,
        Params,
    };
    use ic_base_types::PrincipalId;
    use ic_nervous_system_common::{
        assert_is_err, assert_is_ok, E8, SECONDS_PER_DAY, START_OF_2022_TIMESTAMP_SECONDS,
    };
    use lazy_static::lazy_static;
    use prost::Message;

    const OPEN_SNS_TOKEN_SWAP_PROPOSAL_ID: u64 = 489102;

    const PARAMS: Params = Params {
        max_icp_e8s: 1_000 * E8,
        max_participant_icp_e8s: 1_000 * E8,
        min_icp_e8s: 10 * E8,
        min_participant_icp_e8s: 5 * E8,
        sns_token_e8s: 5_000 * E8,
        min_participants: 10,
        swap_due_timestamp_seconds: START_OF_2022_TIMESTAMP_SECONDS + 14 * SECONDS_PER_DAY,
        neuron_basket_construction_parameters: Some(NeuronBasketConstructionParameters {
            count: 3,
            dissolve_delay_interval_seconds: 7890000, // 3 months
        }),
    };

    lazy_static! {
        static ref OPEN_REQUEST: OpenRequest = OpenRequest {
            params: Some(PARAMS),
            cf_participants: vec![CfParticipant {
                hotkey_principal: PrincipalId::new_user_test_id(423939).to_string(),
                cf_neurons: vec![CfNeuron {
                    nns_neuron_id: 42,
                    amount_icp_e8s: 99,
                }],
            },],
            open_sns_token_swap_proposal_id: Some(OPEN_SNS_TOKEN_SWAP_PROPOSAL_ID),
        };

        // Fill out Init just enough to test Params validation. These values are
        // similar to, but not the same analogous values in NNS.
        static ref INIT: Init = Init {
            transaction_fee_e8s: Some(12_345),
            neuron_minimum_stake_e8s: Some(123_456_789),
            ..Default::default()
        };
    }

    #[test]
    fn accept_iff_can_form_sns_neuron_in_the_worst_case() {
        let mut init = INIT.clone();

        let sns_token_e8s = PARAMS.min_participant_icp_e8s as u128 * PARAMS.sns_token_e8s as u128
            / PARAMS.max_icp_e8s as u128;
        let neuron_basket_count = PARAMS
            .neuron_basket_construction_parameters
            .as_ref()
            .expect("participant_neuron_basket not populated.")
            .count as u128;
        let available_sns_token_e8s_per_neuron =
            sns_token_e8s / neuron_basket_count as u128 - init.transaction_fee_e8s.unwrap() as u128;
        assert!(available_sns_token_e8s_per_neuron < u64::MAX as u128);
        let available_sns_token_e8s_per_neuron = available_sns_token_e8s_per_neuron as u64;
        assert!(init.neuron_minimum_stake_e8s.unwrap() <= available_sns_token_e8s_per_neuron);

        // Set the bar as high as min_participant_icp_e8s can "jump".
        init.neuron_minimum_stake_e8s = Some(available_sns_token_e8s_per_neuron);
        assert_is_ok!(PARAMS.validate(&init));

        // The bar can still be cleared if lowered.
        init.neuron_minimum_stake_e8s = Some(available_sns_token_e8s_per_neuron - 1);
        assert_is_ok!(PARAMS.validate(&init));

        // Raise the bar so that it can no longer be cleared.
        init.neuron_minimum_stake_e8s = Some(available_sns_token_e8s_per_neuron + 1);
        assert_is_err!(PARAMS.validate(&init));
    }

    #[test]
    fn open_request_validate_ok() {
        assert_is_ok!(OPEN_REQUEST.validate(START_OF_2022_TIMESTAMP_SECONDS, &INIT));
    }

    #[test]
    fn open_request_validate_invalid_params() {
        let request = OpenRequest {
            params: Some(Params {
                swap_due_timestamp_seconds: 42,
                ..PARAMS.clone()
            }),
            ..OPEN_REQUEST.clone()
        };

        assert_is_err!(request.validate(START_OF_2022_TIMESTAMP_SECONDS, &INIT));
    }

    #[test]
    fn open_request_validate_no_proposal_id() {
        let request = OpenRequest {
            open_sns_token_swap_proposal_id: None,
            ..OPEN_REQUEST.clone()
        };

        assert_is_err!(request.validate(START_OF_2022_TIMESTAMP_SECONDS, &INIT));
    }

    #[test]
    fn participant_total_icp_e8s_no_overflow() {
        let participant = CfParticipant {
            hotkey_principal: "".to_string(),
            cf_neurons: vec![
                CfNeuron {
                    nns_neuron_id: 0,
                    amount_icp_e8s: u64::MAX,
                },
                CfNeuron {
                    nns_neuron_id: 0,
                    amount_icp_e8s: u64::MAX,
                },
            ],
        };
        let total = participant.participant_total_icp_e8s();
        assert_eq!(total, u64::MAX);
    }

    #[test]
    fn large_community_fund_does_not_result_in_over_sized_open_request() {
        const MAX_SIZE_BYTES: usize = 1 << 21; // 2 Mi

        let neurons_per_principal = 3;

        let cf_participant = CfParticipant {
            hotkey_principal: PrincipalId::new_user_test_id(789362).to_string(),
            cf_neurons: (0..neurons_per_principal)
                .map(|_| CfNeuron {
                    nns_neuron_id: 592523,
                    amount_icp_e8s: 1_000 * E8,
                })
                .collect(),
        };

        let mut open_request = OpenRequest {
            cf_participants: vec![cf_participant],
            ..Default::default()
        };

        // Crescendo
        loop {
            let mut buffer: Vec<u8> = vec![];
            open_request.encode(&mut buffer).unwrap();
            if buffer.len() > MAX_SIZE_BYTES {
                break;
            }

            // Double size of cf_participants.
            open_request
                .cf_participants
                .append(&mut open_request.cf_participants.clone());
        }

        // TODO: Get more precise using our favorite algo: binary search!
        let safe_len = open_request.cf_participants.len() / 2;
        assert!(safe_len > 10_000);
        println!(
            "Looks like we can support at least {} Community Fund neurons (among {} principals).",
            safe_len * neurons_per_principal,
            safe_len,
        );
    }
}
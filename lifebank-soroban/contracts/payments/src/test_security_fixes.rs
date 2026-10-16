#[cfg(test)]
mod security_tests {
    use crate::{DisputeReason, Error, PaymentContract, PaymentContractClient, REQ_CONTRACT};
    use request_contract::{
        BloodComponent, BloodType, RequestContract, RequestContractClient, Urgency,
    };
    use soroban_sdk::{testutils::Address as _, Address, Env, String};

    struct Setup<'a> {
        env: Env,
        client: PaymentContractClient<'a>,
        payer: Address,
        payee: Address,
        payment_id: u64,
    }

    /// Registers the payment and request contracts and creates one Pending
    /// payment between a fresh payer and payee.
    fn setup<'a>() -> Setup<'a> {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let payer = Address::generate(&env);
        let payee = Address::generate(&env);

        let payment_contract_id = env.register(PaymentContract, ());
        let requests_contract_id = env.register(RequestContract, ());
        let requests_client = RequestContractClient::new(&env, &requests_contract_id);
        requests_client.initialize(&admin, &Address::generate(&env));
        let hospital = Address::generate(&env);
        requests_client.authorize_hospital(&hospital);

        let client = PaymentContractClient::new(&env, &payment_contract_id);
        client.initialize(&admin, &None);
        env.as_contract(&payment_contract_id, || {
            env.storage()
                .instance()
                .set(&REQ_CONTRACT, &requests_contract_id);
        });

        let request_id = requests_client.create_request(
            &hospital,
            &BloodType::OPositive,
            &BloodComponent::WholeBlood,
            &250u32,
            &Urgency::Routine,
            &(env.ledger().timestamp() + 3_600),
        );
        let payment_id = client.create_payment(&request_id, &payer, &payee, &100i128);

        Setup {
            env,
            client,
            payer,
            payee,
            payment_id,
        }
    }

    /// #1152: Verify record_dispute requires caller to be payer or payee.
    /// A third party cannot open disputes on payments they're not part of.
    #[test]
    fn test_record_dispute_requires_party_involvement() {
        let s = setup();
        let third_party = Address::generate(&s.env);

        let dispute_result = s.client.try_record_dispute(
            &s.payment_id,
            &DisputeReason::PaymentContested,
            &String::from_str(&s.env, "case123"),
            &third_party,
        );

        assert_eq!(dispute_result, Err(Ok(Error::Unauthorized)));
    }

    /// #1152: Verify record_dispute succeeds when called by payer.
    #[test]
    fn test_record_dispute_by_payer_succeeds() {
        let s = setup();

        let result = s.client.try_record_dispute(
            &s.payment_id,
            &DisputeReason::PaymentContested,
            &String::from_str(&s.env, "case123"),
            &s.payer,
        );

        assert_eq!(result, Ok(Ok(())));
    }

    /// #1152: Verify record_dispute succeeds when called by payee.
    #[test]
    fn test_record_dispute_by_payee_succeeds() {
        let s = setup();

        let result = s.client.try_record_dispute(
            &s.payment_id,
            &DisputeReason::PaymentContested,
            &String::from_str(&s.env, "case123"),
            &s.payee,
        );

        assert_eq!(result, Ok(Ok(())));
    }
}

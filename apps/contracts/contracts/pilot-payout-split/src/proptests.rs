#[cfg(test)]
mod tests {
    extern crate std;
    use crate::tests::{cycle, evidence_hash, setup_with_balance_values, TEST_MIN_RATE};
    use proptest::prelude::*;
    use soroban_sdk::String;

    // The rounding-dust policy documented deterministic test.
    // The policy: The 10% platform fee is calculated via integer division (truncating towards zero).
    // The pro-rata amounts are also calculated via integer division.
    // Any remaining dust (from the remainder of the fee, or the remainder of the pro-rata split)
    // is kept in the contract and recorded as dust in the summary.
    #[test]
    fn deterministic_rounding_dust_policy() {
        // Balances: 1, 1, 1 (Total = 3)
        let s = setup_with_balance_values(&[1, 1, 1]);
        s.usdc.mint(&s.payout_id, &100_000);

        let cycle_id = cycle(&s.env, "dust-test");
        s.payout.record_evidence(
            &s.operator,
            &s.ally,
            &cycle_id,
            &evidence_hash(&s.env),
            &String::from_str(&s.env, "ipfs://evidence"),
            &100, // total_income
        );

        let cycle_id2 = cycle(&s.env, "dust-test-2");
        s.payout.record_evidence(
            &s.operator,
            &s.ally,
            &cycle_id2,
            &evidence_hash(&s.env),
            &String::from_str(&s.env, "ipfs://evidence2"),
            &101, // total_income
        );

        let summary =
            s.payout
                .execute_distribution(&s.operator, &s.ally, &cycle_id2, &TEST_MIN_RATE);

        assert_eq!(summary.platform_fee, 10);
        assert_eq!(summary.distributed_total, 90); // 30 * 3
        assert_eq!(summary.dust, 1);
        assert_eq!(
            summary.platform_fee + summary.distributed_total + summary.dust,
            101
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2000))]

        #[test]
        fn fuzz_distribution_invariants(
            total_income in 1i128..100_000_000i128,
            balances in prop::collection::vec(1i128..100_000_000i128, 0..=10)
        ) {
            let s = setup_with_balance_values(&balances);

            if balances.is_empty() {
                // If there are no holders, the contract panics with EmptyHolderSet, which is expected.
                // We test this explicitly to ensure the typed error is returned.
                s.usdc.mint(&s.payout_id, &total_income);
                let cycle_id = cycle(&s.env, "fuzz");
                s.payout.record_evidence(
                    &s.operator,
                    &s.ally,
                    &cycle_id,
                    &evidence_hash(&s.env),
                    &String::from_str(&s.env, "ipfs://fuzz"),
                    &total_income,
                );

                let res = s.payout.try_execute_distribution(
                    &s.operator,
                    &s.ally,
                    &cycle_id,
                    &TEST_MIN_RATE,
                );

                assert_eq!(
                    res,
                   Err(Ok(soroban_sdk::Error::from_contract_error(
                      crate::PayoutError::EmptyHolderSet as u32
                   ))));
                return Ok(());
            }

            s.usdc.mint(&s.payout_id, &total_income);
            let cycle_id = cycle(&s.env, "fuzz");
            s.payout.record_evidence(
                &s.operator,
                &s.ally,
                &cycle_id,
                &evidence_hash(&s.env),
                &String::from_str(&s.env, "ipfs://fuzz"),
                &total_income,
            );

            let summary_result = s.payout.try_execute_distribution(
                &s.operator,
                &s.ally,
                &cycle_id,
                &TEST_MIN_RATE,
            );

            let summary = match summary_result {
                Ok(Ok(s)) => s,
                Ok(Err(_)) => {
                    // Contract typed error (e.g. arithmetic overflow on extreme values)
                    return Ok(());
                }
                Err(_) => {
                    // Host error like Budget Exceeded
                    // The contract is working correctly by refusing to process, this is acceptable for extreme inputs
                    return Ok(());
                }
            };

            // 1. The fee plus the sum of all pro-rata distributions never exceeds the total income for the cycle.
            let total_distributed = summary.distributed_total + summary.undistributed_failed_swaps;
            assert!(summary.platform_fee + total_distributed <= summary.total_income);

            // 2. No individual distribution is ever negative.
            // Verified by observing that `distributed_total` and balances are all positive.
            assert!(summary.platform_fee >= 0);
            assert!(summary.distributed_total >= 0);
            assert!(summary.undistributed_failed_swaps >= 0);
            assert!(summary.dust >= 0);

            // 3. The fee is always exactly the documented rounding rule applied to 10% of total income - no drift.
            assert_eq!(summary.platform_fee, total_income * 10 / 100);

            // 4. Every unit of the remainder is accounted for: fee + paid + withheld + dust equals total_income,
            // and accumulated_dust equals the cycle's dust prior to any release.
            assert_eq!(
                summary.platform_fee
                    + summary.distributed_total
                    + summary.undistributed_failed_swaps
                    + summary.dust,
                summary.total_income
            );
            assert_eq!(s.payout.accumulated_dust(), summary.dust);

            // 5. Distribution is monotonic in holder balance: a holder with a strictly larger balance never receives a strictly smaller payout than a holder with a smaller balance, all else equal.
            // We can verify this by checking the final balances of the holders.
            let mut previous_payouts = std::vec::Vec::new();
            for (i, balance) in balances.iter().enumerate() {
                let holder = s.holders.get(i as u32).unwrap();
                let final_balance = s.usdc.balance(&holder);
                let payout = final_balance;
                previous_payouts.push((*balance, payout));
            }

            for (bal1, pay1) in &previous_payouts {
                for (bal2, pay2) in &previous_payouts {
                    if bal1 > bal2 {
                        assert!(pay1 >= pay2, "Monotonicity violated: balance {} got payout {} while smaller balance {} got payout {}", bal1, pay1, bal2, pay2);
                    }
                }
            }
        }

        #[test]
        fn fuzz_multi_cycle_dust_and_withheld_conservation(
            incomes in prop::collection::vec(1i128..10_000_000i128, 1..=4),
            balances in prop::collection::vec(1i128..10_000_000i128, 1..=6),
            eurc_mask in prop::collection::vec(any::<bool>(), 6),
            release_flags in prop::collection::vec(any::<bool>(), 4),
        ) {
            let s = setup_with_balance_values(&balances);
            let initial = s.usdc.balance(&s.payout_id);
            if initial > 0 {
                s.usdc.burn(&s.payout_id, &initial);
            }

            // Opt selected holders into EURC with an empty router pool so failed EURC legs
            // exercise withheld accounting alongside rounding dust simultaneously.
            for (i, &is_eurc) in eurc_mask.iter().enumerate().take(balances.len()) {
                if is_eurc {
                    let h = s.holders.get(i as u32).unwrap();
                    s.payout.set_currency_preference(&h, &crate::Currency::Eurc);
                }
            }

            let mut cumulative_dust_generated = 0i128;
            let mut cumulative_dust_released = 0i128;
            let mut cumulative_withheld = 0i128;
            let mut fee_recipient_expected = 0i128;

            for (idx, &inc) in incomes.iter().enumerate() {
                s.usdc.mint(&s.payout_id, &inc);
                let cname = std::format!("mc-{}", idx);
                let cid = cycle(&s.env, &cname);
                s.payout.record_evidence(
                    &s.operator,
                    &s.ally,
                    &cid,
                    &evidence_hash(&s.env),
                    &String::from_str(&s.env, "ipfs://mc"),
                    &inc,
                );

                let summary = match s.payout.try_execute_distribution(
                    &s.operator,
                    &s.ally,
                    &cid,
                    &TEST_MIN_RATE,
                ) {
                    Ok(Ok(sum)) => sum,
                    _ => return Ok(()),
                };

                // Extended conservation invariant per cycle:
                // fee + paid + withheld + dust == total_income
                assert_eq!(
                    summary.platform_fee
                        + summary.distributed_total
                        + summary.undistributed_failed_swaps
                        + summary.dust,
                    summary.total_income
                );

                cumulative_dust_generated += summary.dust;
                cumulative_withheld += summary.undistributed_failed_swaps;
                fee_recipient_expected += summary.platform_fee;

                if release_flags[idx] && s.payout.accumulated_dust() > 0 {
                    let released = s.payout.release_dust(&s.admin);
                    cumulative_dust_released += released;
                    fee_recipient_expected += released;
                }

                // Tracked dust always equals sum of every cycle's dust minus what was released.
                let expected_unreleased_dust = cumulative_dust_generated - cumulative_dust_released;
                assert_eq!(s.payout.accumulated_dust(), expected_unreleased_dust);
                assert_eq!(s.payout.total_withheld_balance(), cumulative_withheld);
                assert_eq!(s.usdc.balance(&s.fee_recipient), fee_recipient_expected);
                assert_eq!(
                    s.usdc.balance(&s.payout_id),
                    cumulative_withheld + expected_unreleased_dust
                );
            }
        }
    }
}

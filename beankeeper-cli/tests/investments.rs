//! Exact lot selection, historical valuation, and atomic audited trade storage.

use beankeeper::core::{LotMethod, PositiveQuantity, Quantity};
use beankeeper::types::{Currency, Money, NonNegativeMoney, PositiveMoney};
use beankeeper_cli::{
    db::{
        Actor, Db, accounts, audit, companies,
        investments::{
            self, BuyParams, LotId, LotSelection, PriceParams, SecurityParams, SellParams,
        },
        verification,
    },
    error::CliError,
};
use chrono::NaiveDate;

type TestResult = Result<(), CliError>;

fn date(value: &str) -> Result<NaiveDate, CliError> {
    value
        .parse()
        .map_err(|error| CliError::Validation(format!("{error}")))
}

fn quantity(value: &str) -> Result<PositiveQuantity, CliError> {
    value
        .parse()
        .map_err(|error| CliError::Validation(format!("{error}")))
}

fn usd(amount: i64) -> Result<NonNegativeMoney, CliError> {
    NonNegativeMoney::new(Money::usd(i128::from(amount)))
        .map_err(|error| CliError::Bean(error.into()))
}

fn minor(value: impl Into<Money>) -> i128 {
    value.into().amount().minor_units()
}

fn fixture() -> Result<Db, CliError> {
    let db = Db::open_in_memory()?;
    for company in ["acme", "other"] {
        companies::create_company(db.conn(), company, company, None)?;
        for (code, kind) in [
            ("1000", "asset"),
            ("1200", "asset"),
            ("1300", "asset"),
            ("4000", "revenue"),
            ("5000", "expense"),
        ] {
            accounts::create_account(db.conn(), company, code, code, kind, None)?;
        }
        for symbol in ["ABC", "XYZ"] {
            investments::create_security(
                db.conn(),
                &Actor::new("investment-test"),
                company,
                &SecurityParams {
                    symbol: symbol.into(),
                    name: format!("{symbol} shares"),
                    currency: Currency::USD,
                },
            )?;
        }
    }
    Ok(db)
}

fn purchase(shares: &str, cost: i64, day: &str) -> Result<BuyParams, CliError> {
    Ok(BuyParams {
        symbol: "ABC".into(),
        quantity: quantity(shares)?,
        cost: PositiveMoney::new(Money::usd(i128::from(cost)))
            .map_err(|error| CliError::Bean(error.into()))?,
        date: date(day)?,
        asset: "1200".into(),
        cash: "1000".into(),
        reference: None,
    })
}

fn sale(shares: &str, proceeds: i64, day: &str) -> Result<SellParams, CliError> {
    Ok(SellParams {
        symbol: "ABC".into(),
        quantity: quantity(shares)?,
        proceeds: usd(proceeds)?,
        date: date(day)?,
        asset: "1200".into(),
        cash: "1000".into(),
        gain: "4000".into(),
        selection: LotSelection::Fifo,
        reference: None,
    })
}

fn state(db: &Db) -> Result<(i64, i64, i64, i64), CliError> {
    Ok(db.conn().query_row(
        "SELECT (SELECT COUNT(*) FROM transactions),
                (SELECT COUNT(*) FROM investment_lots),
                (SELECT COUNT(*) FROM lot_reliefs),
                (SELECT COUNT(*) FROM audit_log)",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?)
}

fn verify(db: &Db) -> TestResult {
    verification::verify_accounting(db.conn())?;
    audit::verify_hash_chain(db.conn())?;
    audit::verify_creations(db.conn())
}

#[test]
fn fifo_and_lifo_relieve_different_acquisition_costs() -> TestResult {
    for (method, expected_basis) in [(LotSelection::Fifo, 100), (LotSelection::Lifo, 200)] {
        let db = fixture()?;
        let actor = Actor::new("investment-test");
        let older = investments::buy(
            db.conn(),
            &actor,
            "acme",
            &purchase("1", 100, "2026-01-01")?,
        )?;
        let newer = investments::buy(
            db.conn(),
            &actor,
            "acme",
            &purchase("1", 200, "2026-01-02")?,
        )?;
        let mut params = sale("1", 150, "2026-01-03")?;
        params.selection = method.clone();
        let sold = investments::sell(db.conn(), &actor, "acme", &params)?;
        assert_eq!(minor(sold.cost_basis), expected_basis);
        assert_eq!(minor(sold.realized_gain), 150 - expected_basis);
        let expected_lot = if method == LotSelection::Fifo {
            older.lot_id
        } else {
            newer.lot_id
        };
        assert_eq!(sold.relief.len(), 1);
        assert_eq!(Some(&sold.relief[0].lot_id), expected_lot.as_ref());
        let (_, entries) =
            beankeeper_cli::db::get_transaction(db.conn(), "acme", sold.transaction_id)?;
        let gain = entries
            .iter()
            .find(|entry| entry.account_code == "4000")
            .unwrap();
        assert_eq!(gain.amount, 50);
        assert_eq!(
            gain.direction,
            if method == LotSelection::Fifo {
                "credit"
            } else {
                "debit"
            }
        );
        verify(&db)?;
    }
    Ok(())
}

#[test]
fn a_sale_can_exhaust_one_lot_and_partially_relieve_another() -> TestResult {
    let db = fixture()?;
    let actor = Actor::new("investment-test");
    investments::buy(
        db.conn(),
        &actor,
        "acme",
        &purchase("1", 100, "2026-01-01")?,
    )?;
    investments::buy(
        db.conn(),
        &actor,
        "acme",
        &purchase("1", 200, "2026-01-02")?,
    )?;
    let sold = investments::sell(db.conn(), &actor, "acme", &sale("1.5", 300, "2026-01-03")?)?;
    assert_eq!(minor(sold.cost_basis), 200);
    assert_eq!(minor(sold.realized_gain), 100);
    assert_eq!(sold.relief.len(), 2);
    assert_eq!(sold.relief[0].quantity.to_string(), "1");
    assert_eq!(sold.relief[1].quantity.to_string(), "0.5");
    let lots = investments::list_lots(db.conn(), "acme", "ABC")?;
    assert_eq!(lots[0].remaining_quantity.to_string(), "0");
    assert_eq!(lots[1].remaining_quantity.to_string(), "0.5");
    assert_eq!(minor(lots[1].remaining_cost_basis), 100);
    verify(&db)
}

#[test]
fn same_date_lots_have_a_stable_id_tiebreaker() -> TestResult {
    for method in [LotSelection::Fifo, LotSelection::Lifo] {
        let db = fixture()?;
        let actor = Actor::new("investment-test");
        investments::buy(
            db.conn(),
            &actor,
            "acme",
            &purchase("1", 100, "2026-01-01")?,
        )?;
        investments::buy(
            db.conn(),
            &actor,
            "acme",
            &purchase("1", 200, "2026-01-01")?,
        )?;
        let lots = investments::list_lots(db.conn(), "acme", "ABC")?;
        assert!(lots[0].id < lots[1].id);
        let expected = &lots[usize::from(method == LotSelection::Lifo)];
        let mut params = sale("1", 150, "2026-01-01")?;
        params.selection = method.clone();
        let sold = investments::sell(db.conn(), &actor, "acme", &params)?;
        assert_eq!(sold.relief[0].lot_id, expected.id);
        assert_eq!(sold.cost_basis, expected.cost_basis.nonnegative());
        verify(&db)?;
    }
    Ok(())
}

#[test]
fn specific_lots_are_scoped_to_company_symbol_and_account() -> TestResult {
    let db = fixture()?;
    let actor = Actor::new("investment-test");
    investments::buy(
        db.conn(),
        &actor,
        "acme",
        &purchase("1", 100, "2026-01-01")?,
    )?;
    let newer = investments::buy(
        db.conn(),
        &actor,
        "acme",
        &purchase("1", 200, "2026-01-02")?,
    )?;
    let mut params = sale("1", 250, "2026-01-03")?;
    let lot = newer
        .lot_id
        .ok_or_else(|| CliError::General("expected purchased lot".into()))?;
    params.selection = LotSelection::Specific(lot.clone());
    let before = state(&db)?;
    assert!(investments::sell(db.conn(), &actor, "other", &params).is_err());
    params.symbol = "XYZ".into();
    assert!(investments::sell(db.conn(), &actor, "acme", &params).is_err());
    params.symbol = "ABC".into();
    params.asset = "1300".into();
    assert!(investments::sell(db.conn(), &actor, "acme", &params).is_err());
    params.asset = "1200".into();
    assert!(LotSelection::from_flags(LotMethod::Fifo, Some(lot.clone())).is_err());
    assert!(LotSelection::from_flags(LotMethod::Specific, None).is_err());
    assert_eq!(state(&db)?, before);
    let sold = investments::sell(db.conn(), &actor, "acme", &params)?;
    assert_eq!(minor(sold.cost_basis), 200);
    assert_eq!(sold.relief[0].lot_id, lot);
    let lots = investments::list_lots(db.conn(), "acme", "ABC")?;
    assert_eq!(minor(lots[0].remaining_cost_basis), 100);
    assert_eq!(lots[1].remaining_quantity.to_string(), "0");
    verify(&db)
}

#[test]
fn fractional_partial_sales_conserve_the_original_basis() -> TestResult {
    let db = fixture()?;
    let actor = Actor::new("investment-test");
    investments::buy(
        db.conn(),
        &actor,
        "acme",
        &purchase("0.75", 100, "2026-01-01")?,
    )?;
    let mut costs = Vec::new();
    for day in ["2026-01-02", "2026-01-03", "2026-01-04"] {
        let sold = investments::sell(db.conn(), &actor, "acme", &sale("0.25", 50, day)?)?;
        assert_eq!(sold.quantity.to_string(), "0.25");
        costs.push(minor(sold.cost_basis));
    }
    assert_eq!(costs, [33, 34, 33]);
    assert_eq!(costs.iter().sum::<i128>(), 100);
    let lots = investments::list_lots(db.conn(), "acme", "ABC")?;
    assert_eq!(lots[0].remaining_quantity.to_string(), "0");
    assert_eq!(minor(lots[0].remaining_cost_basis), 0);
    assert!(investments::holdings(db.conn(), "acme", date("2026-01-04")?)?.is_empty());
    verify(&db)
}

#[test]
fn historical_holdings_apply_only_trades_and_prices_known_at_the_cutoff() -> TestResult {
    let db = fixture()?;
    let actor = Actor::new("investment-test");
    investments::buy(
        db.conn(),
        &actor,
        "acme",
        &purchase("3", 300, "2026-01-01")?,
    )?;
    investments::sell(db.conn(), &actor, "acme", &sale("1", 150, "2026-01-03")?)?;
    for (day, price) in [("2026-01-02", 120), ("2026-01-04", 250)] {
        investments::record_price(
            db.conn(),
            &actor,
            "acme",
            &PriceParams {
                symbol: "ABC".into(),
                date: date(day)?,
                price: usd(price)?,
            },
        )?;
    }
    let before = state(&db)?;
    let unpriced = investments::holdings(db.conn(), "acme", date("2026-01-01")?)?;
    assert_eq!(unpriced[0].quantity.to_string(), "3");
    assert_eq!(minor(unpriced[0].cost_basis), 300);
    assert_eq!(unpriced[0].latest_price, None);
    assert_eq!(unpriced[0].market_value, None);
    assert_eq!(unpriced[0].unrealized_gain, None);
    let prior = investments::holdings(db.conn(), "acme", date("2026-01-02")?)?;
    assert_eq!(prior[0].quantity.to_string(), "3");
    assert_eq!(prior[0].market_value.map(minor), Some(360));
    assert_eq!(prior[0].unrealized_gain.map(minor), Some(60));
    let current = investments::holdings(db.conn(), "acme", date("2026-01-03")?)?;
    assert_eq!(current[0].quantity.to_string(), "2");
    assert_eq!(minor(current[0].cost_basis), 200);
    assert_eq!(current[0].latest_price.map(minor), Some(120));
    assert_eq!(current[0].price_date, Some(date("2026-01-02")?));
    assert_eq!(current[0].market_value.map(minor), Some(240));
    assert_eq!(current[0].unrealized_gain.map(minor), Some(40));
    let future = investments::holdings(db.conn(), "acme", date("2026-01-04")?)?;
    assert_eq!(future[0].market_value.map(minor), Some(500));
    assert!(investments::holdings(db.conn(), "other", date("2026-01-04")?)?.is_empty());
    assert!(investments::holdings(db.conn(), "acme", date("2025-12-31")?)?.is_empty());
    assert_eq!(state(&db)?, before);
    verify(&db)
}

#[test]
fn dates_across_the_four_digit_year_boundary_keep_chronological_order() -> TestResult {
    let db = fixture()?;
    let actor = Actor::new("investment-test");
    for (day, price) in [("9999-12-31", 120), ("+10000-01-01", 250)] {
        investments::buy(db.conn(), &actor, "acme", &purchase("1", 100, day)?)?;
        investments::record_price(
            db.conn(),
            &actor,
            "acme",
            &PriceParams {
                symbol: "ABC".into(),
                date: date(day)?,
                price: usd(price)?,
            },
        )?;
    }
    let before = state(&db)?;
    assert!(investments::sell(db.conn(), &actor, "acme", &sale("1", 150, "9999-12-31")?).is_err());
    let prior = investments::holdings(db.conn(), "acme", date("9999-12-31")?)?;
    let current = investments::holdings(db.conn(), "acme", date("+10000-01-01")?)?;
    assert_eq!(prior[0].market_value.map(minor), Some(120));
    assert_eq!(current[0].market_value.map(minor), Some(500));
    assert_eq!(state(&db)?, before);
    verify(&db)
}

#[test]
fn investment_money_stays_bound_to_security_currency() -> TestResult {
    let db = fixture()?;
    let actor = Actor::new("investment-test");
    let mut purchase = purchase("1", 100, "2026-01-01")?;
    let before = state(&db)?;
    purchase.cost =
        PositiveMoney::new(Money::eur(100)).map_err(|error| CliError::Bean(error.into()))?;
    assert!(investments::buy(db.conn(), &actor, "acme", &purchase).is_err());
    let wrong_price =
        NonNegativeMoney::new(Money::eur(100)).map_err(|error| CliError::Bean(error.into()))?;
    let mut price = PriceParams {
        symbol: "ABC".into(),
        date: date("2026-01-01")?,
        price: wrong_price,
    };
    assert!(investments::record_price(db.conn(), &actor, "acme", &price).is_err());
    assert_eq!(state(&db)?, before);
    price.price = usd(100)?;
    let recorded = investments::record_price(db.conn(), &actor, "acme", &price)?;
    assert!(recorded.id.as_str().parse::<LotId>().is_err());
    verify(&db)
}

#[test]
fn oversells_and_duplicate_references_leave_no_partial_state() -> TestResult {
    let db = fixture()?;
    let actor = Actor::new("investment-test");
    let mut bought = purchase("1", 100, "2026-01-01")?;
    bought.reference = Some("buy-1".into());
    investments::buy(db.conn(), &actor, "acme", &bought)?;
    let before = state(&db)?;
    assert!(investments::buy(db.conn(), &actor, "acme", &bought).is_err());
    assert!(investments::sell(db.conn(), &actor, "acme", &sale("2", 300, "2026-01-02")?).is_err());
    let mut duplicate = sale("1", 150, "2026-01-02")?;
    duplicate.reference = Some("buy-1".into());
    assert!(investments::sell(db.conn(), &actor, "acme", &duplicate).is_err());
    assert_eq!(state(&db)?, before);
    let lots = investments::list_lots(db.conn(), "acme", "ABC")?;
    assert_eq!(lots[0].remaining_quantity.to_string(), "1");
    assert_eq!(minor(lots[0].remaining_cost_basis), 100);
    verify(&db)
}

#[test]
fn relief_or_audit_failure_rolls_back_the_entire_sale() -> TestResult {
    for trigger in [
        "CREATE TRIGGER fail_relief BEFORE INSERT ON lot_reliefs
         WHEN NEW.cost_basis = 200
         BEGIN SELECT RAISE(ABORT, 'relief unavailable'); END;",
        "CREATE TRIGGER fail_lot_audit BEFORE INSERT ON audit_log
         WHEN NEW.entity = 'lot'
         BEGIN SELECT RAISE(ABORT, 'audit unavailable'); END;",
    ] {
        let db = fixture()?;
        let actor = Actor::new("investment-test");
        investments::buy(
            db.conn(),
            &actor,
            "acme",
            &purchase("1", 100, "2026-01-01")?,
        )?;
        investments::buy(
            db.conn(),
            &actor,
            "acme",
            &purchase("1", 200, "2026-01-02")?,
        )?;
        let before = state(&db)?;
        db.conn().execute_batch(trigger)?;
        assert!(
            investments::sell(db.conn(), &actor, "acme", &sale("2", 450, "2026-01-03")?).is_err()
        );
        assert_eq!(state(&db)?, before);
        let lots = investments::list_lots(db.conn(), "acme", "ABC")?;
        assert_eq!(lots[0].remaining_quantity.to_string(), "1");
        assert_eq!(minor(lots[0].remaining_cost_basis), 100);
        assert_eq!(lots[1].remaining_quantity.to_string(), "1");
        assert_eq!(minor(lots[1].remaining_cost_basis), 200);
        verify(&db)?;
    }
    Ok(())
}

#[test]
fn lot_creation_failure_rolls_back_the_purchase_posting() -> TestResult {
    let db = fixture()?;
    let actor = Actor::new("investment-test");
    let before = state(&db)?;
    db.conn().execute_batch(
        "CREATE TRIGGER fail_lot BEFORE INSERT ON investment_lots
         BEGIN SELECT RAISE(ABORT, 'lot unavailable'); END;",
    )?;
    assert!(
        investments::buy(
            db.conn(),
            &actor,
            "acme",
            &purchase("1", 100, "2026-01-01")?
        )
        .is_err()
    );
    assert_eq!(state(&db)?, before);
    verify(&db)
}

#[test]
fn aggregate_basis_overflow_rejects_reports_and_sales_without_changes() -> TestResult {
    let db = fixture()?;
    let actor = Actor::new("investment-test");
    investments::buy(
        db.conn(),
        &actor,
        "acme",
        &purchase("1", i64::MAX, "2026-01-01")?,
    )?;
    investments::buy(db.conn(), &actor, "acme", &purchase("1", 1, "2026-01-02")?)?;
    let before = state(&db)?;
    let report_error = investments::holdings(db.conn(), "acme", date("2026-01-03")?).unwrap_err();
    assert!(report_error.to_string().contains("overflow"));
    let sell_error =
        investments::sell(db.conn(), &actor, "acme", &sale("2", 0, "2026-01-03")?).unwrap_err();
    assert!(sell_error.to_string().contains("overflow"));
    assert_eq!(state(&db)?, before);
    verify(&db)
}

#[test]
fn valuation_and_quantity_overflow_are_reported_explicitly() -> TestResult {
    let db = fixture()?;
    let actor = Actor::new("investment-test");
    investments::buy(db.conn(), &actor, "acme", &purchase("2", 1, "2026-01-01")?)?;
    investments::record_price(
        db.conn(),
        &actor,
        "acme",
        &PriceParams {
            symbol: "ABC".into(),
            date: date("2026-01-01")?,
            price: usd(i64::MAX)?,
        },
    )?;
    let before = state(&db)?;
    assert!(
        investments::holdings(db.conn(), "acme", date("2026-01-01")?)
            .unwrap_err()
            .to_string()
            .contains("overflow")
    );
    assert_eq!(state(&db)?, before);
    let maximum =
        PositiveQuantity::from_atoms(i128::MAX).map_err(|e| CliError::Validation(e.to_string()))?;
    for day in ["2026-01-01", "2026-01-02"] {
        let mut params = purchase("1", 1, day)?;
        params.symbol = "XYZ".into();
        params.quantity = maximum;
        investments::buy(db.conn(), &actor, "other", &params)?;
    }
    assert!(
        investments::holdings(db.conn(), "other", date("2026-01-02")?)
            .unwrap_err()
            .to_string()
            .contains("overflow")
    );
    verify(&db)
}

#[test]
fn trades_reject_backdating_within_the_same_security_and_account() -> TestResult {
    let db = fixture()?;
    let actor = Actor::new("investment-test");
    investments::buy(
        db.conn(),
        &actor,
        "acme",
        &purchase("2", 200, "2026-01-02")?,
    )?;
    let before = state(&db)?;
    assert!(
        investments::buy(
            db.conn(),
            &actor,
            "acme",
            &purchase("1", 100, "2026-01-01")?
        )
        .is_err()
    );
    assert!(investments::sell(db.conn(), &actor, "acme", &sale("1", 100, "2026-01-01")?).is_err());
    assert_eq!(state(&db)?, before);
    investments::sell(db.conn(), &actor, "acme", &sale("1", 100, "2026-01-04")?)?;
    let after_sale = state(&db)?;
    assert!(
        investments::buy(
            db.conn(),
            &actor,
            "acme",
            &purchase("1", 100, "2026-01-03")?
        )
        .is_err()
    );
    assert_eq!(state(&db)?, after_sale);
    let mut other_position = purchase("1", 100, "2026-01-01")?;
    other_position.asset = "1300".into();
    investments::buy(db.conn(), &actor, "acme", &other_position)?;
    verify(&db)
}

#[test]
fn invalid_inputs_and_duplicate_prices_do_not_add_audit_records() -> TestResult {
    let db = fixture()?;
    let actor = Actor::new("investment-test");
    let before = state(&db)?;
    assert!(Currency::from_code("BAD").is_err());
    for (symbol, name) in [("bad symbol", "bad"), ("NEW", " ")] {
        assert!(
            investments::create_security(
                db.conn(),
                &actor,
                "acme",
                &SecurityParams {
                    symbol: symbol.into(),
                    name: name.into(),
                    currency: Currency::USD,
                }
            )
            .is_err()
        );
    }
    let mut params = purchase("1", 100, "2026-01-01")?;
    params.asset = "5000".into();
    assert!(investments::buy(db.conn(), &actor, "acme", &params).is_err());
    params.asset = "1000".into();
    assert!(investments::buy(db.conn(), &actor, "acme", &params).is_err());
    assert!(PositiveQuantity::try_from(Quantity::ZERO).is_err());
    assert_eq!(state(&db)?, before);
    let price = PriceParams {
        symbol: "ABC".into(),
        date: date("2026-01-01")?,
        price: usd(0)?,
    };
    investments::record_price(db.conn(), &actor, "acme", &price)?;
    let after_price = state(&db)?;
    assert!(investments::record_price(db.conn(), &actor, "acme", &price).is_err());
    assert_eq!(state(&db)?, after_price);
    verify(&db)
}

#[test]
fn investment_records_cannot_be_updated_deleted_or_replaced() -> TestResult {
    let db = fixture()?;
    let actor = Actor::new("investment-test");
    investments::buy(
        db.conn(),
        &actor,
        "acme",
        &purchase("2", 200, "2026-01-01")?,
    )?;
    investments::sell(db.conn(), &actor, "acme", &sale("1", 150, "2026-01-02")?)?;
    investments::record_price(
        db.conn(),
        &actor,
        "acme",
        &PriceParams {
            symbol: "ABC".into(),
            date: date("2026-01-02")?,
            price: usd(150)?,
        },
    )?;
    let before = state(&db)?;
    for table in [
        "securities",
        "security_prices",
        "investment_lots",
        "lot_reliefs",
    ] {
        for sql in [
            format!("UPDATE {table} SET company_slug = company_slug"),
            format!("DELETE FROM {table}"),
            format!("INSERT OR REPLACE INTO {table} SELECT * FROM {table}"),
        ] {
            assert!(db.conn().execute_batch(&sql).is_err(), "{sql}");
        }
    }
    assert!(
        db.conn()
            .execute_batch(
                "INSERT OR REPLACE INTO security_prices (id,company_slug,symbol,date,price)
         SELECT 'replacement-price',company_slug,symbol,date,price FROM security_prices"
            )
            .is_err()
    );
    assert!(db.conn().execute_batch(
        "INSERT OR REPLACE INTO investment_lots (id,company_slug,symbol,asset,quantity,cost_basis,acquired_date,transaction_id)
         SELECT 'replacement-lot',company_slug,symbol,asset,quantity,cost_basis,acquired_date,transaction_id FROM investment_lots"
    ).is_err());
    assert!(db.conn().execute_batch(
        "INSERT OR REPLACE INTO lot_reliefs (id,company_slug,lot_id,quantity,cost_basis,date,transaction_id)
         SELECT 'replacement-relief',company_slug,lot_id,quantity,cost_basis,date,transaction_id FROM lot_reliefs"
    ).is_err());
    assert_eq!(state(&db)?, before);
    verify(&db)
}

#[test]
fn generic_reversals_reject_linked_trades_after_metadata_annotations() -> TestResult {
    use beankeeper_cli::db::metadata::MetadataEdit;

    let db = fixture()?;
    let actor = Actor::new("investment-test");
    let bought = investments::buy(
        db.conn(),
        &actor,
        "acme",
        &purchase("2", 200, "2026-01-01")?,
    )?;
    let sold = investments::sell(db.conn(), &actor, "acme", &sale("1", 150, "2026-01-02")?)?;
    for id in [bought.transaction_id, sold.transaction_id] {
        audit::edit_metadata(
            db.conn(),
            &actor,
            "acme",
            audit::MetadataTarget::Id(id),
            &MetadataEdit::Unset(vec!["investment".into()]),
        )?;
        let before = state(&db)?;
        let error = audit::reverse_transaction(
            db.conn(),
            &audit::ReverseTransactionParams {
                company_slug: "acme",
                id,
                date: "2026-01-03",
                description: None,
                reference: None,
                actor: &actor,
            },
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("investment trades cannot use txn reverse")
        );
        assert_eq!(state(&db)?, before);
    }
    let lots = investments::list_lots(db.conn(), "acme", "ABC")?;
    assert_eq!(lots[0].remaining_quantity.to_string(), "1");
    assert_eq!(minor(lots[0].remaining_cost_basis), 100);
    verify(&db)
}

#[test]
fn closed_period_blocks_purchases_and_sales_without_lot_side_effects() -> TestResult {
    let db = fixture()?;
    let actor = Actor::new("close-test");
    investments::buy(
        db.conn(),
        &actor,
        "acme",
        &purchase("2", 200, "2026-01-01")?,
    )?;
    audit::change_period(db.conn(), &actor, "acme", Some(date("2026-01-31")?), None)?;
    let before = state(&db)?;
    assert!(matches!(
        investments::buy(
            db.conn(),
            &actor,
            "acme",
            &purchase("1", 100, "2026-01-31")?
        ),
        Err(CliError::Validation(_))
    ));
    assert_eq!(state(&db)?, before);
    assert!(matches!(
        investments::sell(db.conn(), &actor, "acme", &sale("1", 150, "2026-01-31")?),
        Err(CliError::Validation(_))
    ));
    assert_eq!(state(&db)?, before);
    beankeeper_cli::commands::verify::execute_json(&db)?;
    Ok(())
}

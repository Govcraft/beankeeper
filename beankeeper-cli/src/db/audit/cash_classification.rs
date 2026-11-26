//! Audited account liquidity classification and immediate receipt undo.

use std::fmt;

use beankeeper::types::{AccountClassification, AccountCode, CashClass};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::{
    Actor, AuditAction, AuditChange, AuditEntity, AuditLinks, record_with_links,
    savepoint::AuditedTx, snapshot,
};
use crate::db::{
    Db, accounts,
    companies::{Company, CompanySlug},
    ids::ReceiptId,
};
use crate::error::CliError;
use crate::identity::{Admin, Granted};

/// A persisted classification decoded at the audit boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassificationSnapshot {
    /// Whether this asset is cash, a cash equivalent, or neither.
    pub cash_class: CashClass,
}

/// A completed classification change, including a one-step undo receipt.
#[derive(Debug, Serialize)]
pub struct ClassificationSubmission {
    pub code: AccountCode,
    pub previous: CashClass,
    pub cash_class: CashClass,
    pub receipt_id: ReceiptId,
    pub desktop_path: String,
}

impl fmt::Display for ClassificationSubmission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Classified account {} as {}. Undo receipt #{}. Desktop {}",
            self.code, self.cash_class, self.receipt_id, self.desktop_path
        )
    }
}

pub(crate) fn submit(
    db: &Db,
    actor: &Actor,
    company: &CompanySlug,
    code: AccountCode,
    class: CashClass,
) -> Result<ClassificationSubmission, CliError> {
    Granted::<Admin>::with_db(db, actor, |grant| {
        grant.mutate("sp_account_classify", |tx, actor| {
            let scope = Company::bind(tx.read(), company.clone())?;
            let account = accounts::get_account(&scope, code.as_str())?;
            let classification = AccountClassification::try_from((account.account_type, class))
                .map_err(|error| CliError::Validation(error.to_string()))?;
            let before = ClassificationSnapshot {
                cash_class: account.cash_class,
            };
            let after = ClassificationSnapshot {
                cash_class: classification.cash_class(),
            };
            let receipt_id = store(
                tx,
                actor,
                &scope,
                &code,
                (&before, classification),
                AuditLinks::None,
            )?;
            Ok(ClassificationSubmission {
                code,
                previous: before.cash_class,
                cash_class: after.cash_class,
                receipt_id,
                desktop_path: super::receipts::desktop_receipt_path(company, receipt_id),
            })
        })
    })
}

pub(in crate::db) fn store_initial_classification(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &CompanySlug,
    code: &AccountCode,
    class: CashClass,
) -> Result<(), CliError> {
    let scope = Company::bind(tx.read(), company.clone())?;
    let account = accounts::get_account(&scope, code.as_str())?;
    let classification = AccountClassification::try_from((account.account_type, class))
        .map_err(|error| CliError::Validation(error.to_string()))?;
    let before = ClassificationSnapshot {
        cash_class: account.cash_class,
    };
    store(
        tx,
        actor,
        &scope,
        code,
        (&before, classification),
        AuditLinks::None,
    )?;
    Ok(())
}

fn store(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    scope: &Company<'_>,
    code: &AccountCode,
    (before, classification): (&ClassificationSnapshot, AccountClassification),
    links: AuditLinks,
) -> Result<ReceiptId, CliError> {
    let after = ClassificationSnapshot {
        cash_class: classification.cash_class(),
    };
    let conn = tx.connection();
    record_with_links(
        conn,
        actor,
        AuditChange {
            company_slug: Some(scope.slug()),
            entity: AuditEntity::Account,
            entity_id: code.as_str(),
            action: AuditAction::AccountClassify,
            snapshots: (Some(&snapshot(before)), Some(&snapshot(&after))),
        },
        links,
    )?;
    let receipt = conn.last_insert_rowid().try_into()?;
    conn.execute(
        "UPDATE accounts SET cash_class = ?3 WHERE company_slug = ?1 AND code = ?2",
        params![scope.slug(), code.as_str(), after.cash_class],
    )?;
    Ok(receipt)
}

pub(super) fn restore(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    scope: &Company<'_>,
    code: &AccountCode,
    receipt: ReceiptId,
    (before, after): (&ClassificationSnapshot, &ClassificationSnapshot),
) -> Result<(), CliError> {
    let latest: Option<ReceiptId> = scope
        .read()
        .query_row(
            &format!(
                "SELECT id FROM {} WHERE company_slug = ?1 AND entity = 'account'
            AND entity_id = ?2 AND action IN ('create', 'account_delete', 'account_classify')
            ORDER BY id DESC LIMIT 1",
                scope.read().bookkeeping_audit()
            ),
            params![scope.slug(), code.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    let account = accounts::get_account(scope, code.as_str())?;
    if latest != Some(receipt) || account.cash_class != after.cash_class {
        return Err(CliError::Validation(format!(
            "account {code} has newer classification changes; undo its latest change first"
        )));
    }
    let classification = AccountClassification::try_from((account.account_type, before.cash_class))
        .map_err(|error| CliError::Database(error.to_string()))?;
    store(
        tx,
        actor,
        scope,
        code,
        (after, classification),
        AuditLinks::Undo(receipt),
    )?;
    Ok(())
}

/// Verify that the latest classification receipt still describes the account.
pub(in crate::db) fn verify(reader: crate::db::connection::ReadConn<'_>) -> Result<(), CliError> {
    let mismatch: Option<ReceiptId> = reader.query_row(
        &format!("SELECT event.id FROM {audit} event
          LEFT JOIN accounts account ON account.company_slug = event.company_slug AND account.code = event.entity_id
          WHERE event.action = 'account_classify' AND NOT EXISTS (
              SELECT 1 FROM {audit} newer WHERE newer.company_slug = event.company_slug
                AND newer.entity = 'account' AND newer.entity_id = event.entity_id
                AND newer.action IN ('create', 'account_delete', 'account_classify') AND newer.id > event.id
          ) AND (account.code IS NULL OR account.cash_class IS NOT json_extract(event.after, '$.cash_class')) LIMIT 1",
          audit = reader.bookkeeping_audit()), [], |row| row.get(0),
    ).optional()?;
    if let Some(receipt) = mismatch {
        return Err(CliError::Database(format!(
            "account classification does not match receipt #{receipt}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::accounts::{CreateAccountParams, ListAccountParams};
    use crate::db::test_support::fixture_actor;
    use beankeeper::types::AccountType;

    fn fixture() -> Result<Db, CliError> {
        let db = Db::open_in_memory()?;
        let actor = fixture_actor("fixture");
        Granted::<Admin>::with_db(&db, &actor, |grant| {
            crate::db::create_company(grant, &"acme".parse()?, &"Acme".parse()?, None)
        })?;
        for (code, name, account_type) in [
            ("1000", "Travel Wallet", AccountType::Asset),
            ("1100", "Treasury Reserve", AccountType::Asset),
            ("1200", "Cash register equipment", AccountType::Asset),
            ("2000", "Bank Loan", AccountType::Liability),
        ] {
            Granted::<Admin>::with_db(&db, &actor, |grant| {
                accounts::create_account(
                    grant,
                    &CreateAccountParams {
                        company_slug: &"acme".parse()?,
                        code,
                        name,
                        account_type,
                        default_tax_category: None,
                    },
                )
            })?;
        }
        Ok(db)
    }

    #[test]
    fn cash_membership_is_typed_and_independent_of_names() -> Result<(), CliError> {
        let db = fixture()?;
        let actor = fixture_actor("fixture");
        let company = "acme".parse()?;
        assert_eq!(
            accounts::cash_accounts(db.read(), "acme")?,
            Vec::<(String, String)>::new()
        );
        submit(
            &db,
            &actor,
            &company,
            "1000".parse().map_err(beankeeper::error::BeanError::from)?,
            CashClass::Cash,
        )?;
        submit(
            &db,
            &actor,
            &company,
            "1100".parse().map_err(beankeeper::error::BeanError::from)?,
            CashClass::CashEquivalent,
        )?;
        assert_eq!(accounts::cash_accounts(db.read(), "acme")?.len(), 2);
        let scope = Company::bind(db.read(), company)?;
        let wallet = accounts::get_account(&scope, "1000")?;
        assert_eq!(
            accounts::row_to_account(&wallet)?.cash_class(),
            CashClass::Cash
        );
        assert_eq!(
            accounts::list_accounts(
                &scope,
                &ListAccountParams {
                    type_filter: None,
                    name_filter: None
                }
            )?
            .len(),
            4
        );
        super::verify(db.read())?;
        Ok(())
    }

    #[test]
    fn rejects_non_asset_without_leaving_an_audit_event() -> Result<(), CliError> {
        let db = fixture()?;
        let before: i64 = db
            .read()
            .query_row("SELECT COUNT(*) FROM audit_log", [], |row| row.get(0))?;
        assert!(
            submit(
                &db,
                &fixture_actor("fixture"),
                &"acme".parse()?,
                "2000".parse().map_err(beankeeper::error::BeanError::from)?,
                CashClass::Cash
            )
            .is_err()
        );
        let after: i64 = db
            .read()
            .query_row("SELECT COUNT(*) FROM audit_log", [], |row| row.get(0))?;
        assert_eq!(before, after);
        Ok(())
    }

    #[test]
    fn verification_detects_classification_that_disagrees_with_receipt() -> Result<(), CliError> {
        let db = fixture()?;
        submit(
            &db,
            &fixture_actor("fixture"),
            &"acme".parse()?,
            "1000".parse().map_err(beankeeper::error::BeanError::from)?,
            CashClass::Cash,
        )?;
        db.conn().execute_batch(
            "DROP TRIGGER protect_cash_class_update;
            UPDATE accounts SET cash_class = 'non_cash' WHERE code = '1000';",
        )?;
        assert!(super::verify(db.read()).is_err());
        Ok(())
    }

    #[test]
    fn storage_rejects_unaudited_classification_changes() -> Result<(), CliError> {
        let db = fixture()?;
        assert!(
            db.conn()
                .execute(
                    "UPDATE accounts SET cash_class = 'cash' WHERE code = '1000'",
                    []
                )
                .is_err()
        );
        assert!(
            db.conn()
                .execute(
                    "UPDATE accounts SET cash_class = 'cash' WHERE code = '2000'",
                    []
                )
                .is_err()
        );
        Ok(())
    }
}

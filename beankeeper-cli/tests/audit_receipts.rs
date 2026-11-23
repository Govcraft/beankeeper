//! The public receipt API, exercised with independent ledger connections.

use std::{error::Error, path::Path, process::Command, time::Duration};

use assert_cmd::cargo::CommandCargoExt as _;
use beankeeper::types::{AccountType, Currency, DebitOrCredit};
use beankeeper_cli::{
    capability::Capability,
    db::{
        self, Actor, CreateAccountParams, Db, PostEntryParams, PostResult, PostTransactionParams,
        accounts::{AccountStatus, account_status},
        audit::{
            AuditAction, AuditEntity, ReverseTransactionParams,
            receipts::{
                AuditCursor, ReceiptId, ReceiptPage, ReceiptPageLimit, UndoOutcome,
                continue_receipts, list_receipts, undo_receipt,
            },
            reverse_transaction,
        },
        companies::{Company, CompanySlug},
        ids::TransactionId,
        rules,
    },
    error::{CliError, ErrorCode},
    identity::{self, Admin, Granted, IssueRequest, OwnerUndo, Post, Reverse},
};
use secrecy::ExposeSecret as _;

type TestResult = Result<(), Box<dyn Error>>;

struct Books {
    db: Db,
    company: CompanySlug,
    local: Actor,
}

impl Books {
    fn open(path: &Path) -> Result<Self, CliError> {
        let books = Self {
            db: Db::open(path, None)?,
            company: "acme".parse()?,
            local: Actor::new("bookkeeper".parse()?),
        };
        for (slug, name) in [("acme", "Acme"), ("other", "Other")] {
            Granted::<Admin>::with_db(&books.db, &books.local, |grant| {
                db::create_company(grant, &slug.parse()?, &name.parse()?, None)
            })?;
        }
        books.account("1000", AccountType::Asset)?;
        books.account("5000", AccountType::Expense)?;
        Ok(books)
    }

    fn account(&self, code: &str, account_type: AccountType) -> Result<(), CliError> {
        Granted::<Admin>::with_db(&self.db, &self.local, |grant| {
            db::create_account(
                grant,
                &CreateAccountParams {
                    company_slug: &self.company,
                    code,
                    name: code,
                    account_type,
                    default_tax_category: None,
                },
            )
        })?;
        Ok(())
    }

    fn post(&self, db: &Db, reference: &str) -> Result<TransactionId, CliError> {
        let entries = [
            PostEntryParams {
                account_code: "5000".into(),
                direction: DebitOrCredit::Debit,
                amount: 1200_i64.try_into()?,
                memo: None,
                tax_category: None,
            },
            PostEntryParams {
                account_code: "1000".into(),
                direction: DebitOrCredit::Credit,
                amount: 1200_i64.try_into()?,
                memo: None,
                tax_category: None,
            },
        ];
        let params = PostTransactionParams {
            company_slug: &self.company,
            description: "Supplies",
            metadata: None,
            currency: Currency::USD,
            date: "2026-01-01".parse().map_err(date_error)?,
            entries: &entries,
            correlate: None,
            reference: Some(reference),
            on_conflict: db::ConflictStrategy::Error,
            actor: &self.local,
        };
        match Granted::<Post>::with_db(db, &self.local, |grant| {
            db::post_transaction(grant, &params)
        })? {
            PostResult::Created(id) => Ok(id),
            PostResult::Skipped(_) => Err(CliError::General("expected a new posting".into())),
        }
    }

    fn credential(&self, name: &str, cap: Capability) -> Result<Actor, CliError> {
        let credential = Granted::<Admin>::with_db(&self.db, &self.local, |grant| {
            identity::issue_credential(
                grant,
                &IssueRequest {
                    name,
                    capabilities: &[cap.path().into()],
                    ttl: Duration::from_hours(1),
                    public_key: None,
                    generate_key: true,
                },
            )
        })?;
        let verified = identity::verify(self.db.read(), credential.token().expose_secret())?;
        Ok(Actor::new("desktop".parse()?).with_identity(verified))
    }

    fn receipts(&self) -> Result<ReceiptPage, CliError> {
        list_receipts(
            &self.db,
            &self.company,
            AuditCursor::default(),
            ReceiptPageLimit::default(),
        )
    }

    fn undo(
        &self,
        actor: &Actor,
        company: &CompanySlug,
        id: ReceiptId,
    ) -> Result<UndoOutcome, CliError> {
        Granted::<OwnerUndo>::with_db(&self.db, actor, |grant| undo_receipt(grant, company, id))
    }
}

fn date_error(error: chrono::ParseError) -> CliError {
    CliError::Validation(error.to_string())
}

#[test]
fn paging_keeps_a_reversal_whole_and_picks_up_later_writes_in_the_next_range() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("books.db");
    let books = Books::open(&path)?;
    let writer = Db::open_existing(&path, None)?;
    let previous = books.receipts()?.through();
    let original = books.post(&writer, "ORIGINAL")?;
    let PostResult::Created(mirror) =
        Granted::<Reverse>::with_db(&writer, &books.local, |grant| {
            reverse_transaction(
                grant,
                &ReverseTransactionParams {
                    company_slug: &books.company,
                    id: original,
                    date: "2026-01-02".parse().map_err(date_error)?,
                    description: None,
                    reference: Some("MIRROR"),
                },
            )
        })?
    else {
        return Err("expected mirror posting".into());
    };

    let limit = ReceiptPageLimit::try_from(1)?;
    let first = list_receipts(&books.db, &books.company, previous, limit)?;
    assert_eq!(first.receipts().len(), 1);
    let posted = &first.receipts()[0];
    assert_eq!(posted.change().entity_id, original.to_string());
    assert_eq!(posted.change().actor, "bookkeeper");
    assert!(posted.change().agent_uri.is_none());
    assert!(posted.change().before.is_none());
    assert_eq!(posted.source_reference(), Some("ORIGINAL"));

    let later = books.post(&writer, "LATER")?;
    let next = first.continuation().ok_or("missing continuation")?;
    let last = continue_receipts(&books.db, next, limit)?;
    assert!(last.continuation().is_none());
    assert_eq!(first.through(), last.through());
    assert_eq!(last.receipts().len(), 1);
    let reversed = &last.receipts()[0];
    assert!(reversed.id() > posted.id());
    assert_eq!(reversed.change().action, AuditAction::Reverse);
    assert_eq!(reversed.change().related_posting, Some(mirror));
    assert_eq!(reversed.source_reference(), Some("MIRROR"));
    let creation = reversed.posting().ok_or("missing mirror creation")?;
    assert_eq!(creation.entity_id, mirror.to_string());
    assert!(creation.id < reversed.change().id);
    assert_eq!(creation.action, AuditAction::Create);
    assert!(reversed.change().before.is_some());

    let following = list_receipts(&books.db, &books.company, last.through(), limit)?;
    assert!(following.continuation().is_none());
    assert_eq!(following.receipts().len(), 1);
    assert_eq!(
        following.receipts()[0].change().entity_id,
        later.to_string()
    );
    let empty = list_receipts(&books.db, &books.company, following.through(), limit)?;
    assert!(empty.receipts().is_empty());
    assert_eq!(empty.through(), following.through());
    assert!(AuditCursor::try_from(-1).is_err());
    assert!(ReceiptId::try_from(0).is_err());
    assert!(ReceiptPageLimit::try_from(0).is_err());
    Ok(())
}

fn check_code(result: Result<UndoOutcome, CliError>, expected: ErrorCode) -> TestResult {
    assert_eq!(result.err().ok_or("undo should fail")?.code(), expected);
    Ok(())
}

#[test]
fn owner_undo_is_scoped_revocable_and_idempotent_with_audited_outcomes() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("books.db");
    let books = Books::open(&path)?;
    Granted::<Admin>::with_db(&books.db, &books.local, identity::init_root)?;
    let owner = books.credential("owner", Capability::Admin)?;
    let agent = books.credential("reverse-agent", Capability::Reverse)?;
    let rule = Granted::<Post>::with_db(&books.db, &books.local, |grant| {
        rules::create_rule(grant, &books.company, "supplies".parse()?, "5000", 10)
    })?;
    let transaction = books.post(&books.db, "ORDER-1")?;
    let page = books.receipts()?;
    let id_of = |entity: AuditEntity, entity_id: &str| {
        page.receipts()
            .iter()
            .find(|receipt| {
                receipt.change().entity == entity && receipt.change().entity_id == entity_id
            })
            .map(beankeeper_cli::db::audit::receipts::AuditReceipt::id)
            .ok_or("missing receipt")
    };
    let account_receipt = id_of(AuditEntity::Account, "5000")?;
    let posting_receipt = id_of(AuditEntity::Transaction, &transaction.to_string())?;
    assert_owner_required(&books, &owner, &agent, account_receipt)?;
    check_code(
        books.undo(
            &owner,
            &books.company,
            id_of(AuditEntity::Rule, &rule.id.to_string())?,
        ),
        ErrorCode::UndoUnsupported,
    )?;

    let archived = books.undo(&owner, &books.company, account_receipt)?;
    let UndoOutcome::Archived {
        ref account,
        undo_receipt: archive_change,
    } = archived
    else {
        return Err("expected account archive".into());
    };
    assert_eq!(account.as_str(), "5000");
    assert_archived_account(&books, transaction)?;

    let reversed = books.undo(&owner, &books.company, posting_receipt)?;
    let UndoOutcome::Reversed {
        transaction: original,
        reversal,
        undo_receipt: reverse_change,
    } = reversed
    else {
        return Err("expected a reversal".into());
    };
    assert_eq!(original, transaction);
    let scope = Company::bind(books.db.read(), books.company.clone())?;
    assert_eq!(db::get_transaction(&scope, reversal)?.1.len(), 2);
    let through = books.receipts()?.through();
    assert_eq!(
        books.undo(&owner, &books.company, account_receipt)?,
        archived
    );
    assert_eq!(
        books.undo(&owner, &books.company, posting_receipt)?,
        reversed
    );
    assert_eq!(books.receipts()?.through(), through);

    let outcomes = list_receipts(
        &books.db,
        &books.company,
        page.through(),
        ReceiptPageLimit::default(),
    )?;
    assert_eq!(outcomes.receipts().len(), 2);
    let archive = &outcomes.receipts()[0];
    assert_eq!(archive.id(), archive_change);
    assert_eq!(archive.change().undo_of, Some(account_receipt));
    assert_eq!(archive.change().action, AuditAction::AccountArchive);
    let reverse = &outcomes.receipts()[1];
    assert_eq!(reverse.id(), reverse_change);
    assert_eq!(reverse.change().undo_of, Some(posting_receipt));
    assert_eq!(reverse.source_reference(), Some("ORDER-1"));
    assert_eq!(
        reverse.posting().ok_or("missing mirror event")?.entity_id,
        reversal.to_string()
    );
    for outcome in outcomes.receipts() {
        assert_eq!(outcome.change().actor, "desktop");
        assert_eq!(outcome.change().agent_uri.as_deref(), owner.agent_uri());
        assert_eq!(outcome.change().token_jti.as_deref(), owner.token_jti());
    }
    verify_ledger(&path)?;

    let jti = owner.token_jti().ok_or("owner token id missing")?;
    Granted::<Admin>::with_db(&books.db, &books.local, |grant| {
        identity::revoke(grant, jti)
    })?;
    check_code(
        books.undo(&owner, &books.company, account_receipt),
        ErrorCode::Unauthorized,
    )?;
    Ok(())
}

fn assert_archived_account(books: &Books, transaction: TransactionId) -> TestResult {
    let scope = Company::bind(books.db.read(), books.company.clone())?;
    assert_eq!(account_status(&scope, "5000")?, AccountStatus::Archived);
    assert_eq!(account_status(&scope, "1000")?, AccountStatus::Active);
    assert_eq!(db::get_account(&scope, "5000")?.name, "5000");
    assert_eq!(db::get_transaction(&scope, transaction)?.1.len(), 2);
    assert_eq!(rules::list_rules(&scope)?.len(), 1);
    assert!(rules::list_active_rules(&scope)?.is_empty());
    let posting_failure = books
        .post(&books.db, "AFTER-ARCHIVE")
        .err()
        .ok_or("archived account accepted a post")?;
    assert!(posting_failure.to_string().contains("archived"));
    assert!(
        Granted::<Post>::with_db(&books.db, &books.local, |grant| {
            rules::create_rule(grant, &books.company, "another".parse()?, "5000", 10)
        })
        .is_err()
    );

    Ok(())
}

fn verify_ledger(path: &Path) -> TestResult {
    let output = Command::cargo_bin("bk")?
        .env_remove("BEANKEEPER_PASSPHRASE_CMD")
        .arg("--db")
        .arg(path)
        .args(["verify", "--json"])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let checked: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(checked["data"]["status"], "healthy");

    Ok(())
}

#[test]
fn owner_undo_respects_period_locks_and_reconciled_clearings() -> TestResult {
    use db::{audit::change_period, periods::PeriodChange};

    let directory = tempfile::tempdir()?;
    let path = directory.path().join("books.db");
    let books = Books::open(&path)?;
    Granted::<Admin>::with_db(&books.db, &books.local, identity::init_root)?;
    let owner = books.credential("owner", Capability::Admin)?;
    books.post(&books.db, "CLOSED")?;
    books.post(&books.db, "RECONCILED")?;
    let page = books.receipts()?;
    let find = |reference| {
        page.receipts()
            .iter()
            .find(|receipt| receipt.source_reference() == Some(reference))
            .map(beankeeper_cli::db::audit::receipts::AuditReceipt::id)
            .ok_or("missing posting receipt")
    };

    let invoke_period = |change| {
        Granted::<Admin>::with_db(&books.db, &books.local, |grant| {
            change_period(grant, &books.company, change)
        })
    };
    invoke_period(PeriodChange::Close("2099-12-31".parse()?))?;
    let locked = books.receipts()?.through();
    check_code(
        books.undo(&owner, &books.company, find("CLOSED")?),
        ErrorCode::UndoIneligible,
    )?;
    assert_eq!(books.receipts()?.through(), locked);
    invoke_period(PeriodChange::Reopen("Reopening fixture".parse()?))?;

    let clear = Command::cargo_bin("bk")?
        .env_remove("BEANKEEPER_PASSPHRASE_CMD")
        .arg("--db")
        .arg(&path)
        .args([
            "--company",
            "acme",
            "txn",
            "clear",
            "--reference",
            "RECONCILED",
            "--account",
            "1000",
            "--amount",
            "5",
            "--date",
            "2026-01-02",
            "--status",
            "reconciled",
        ])
        .output()?;
    assert!(
        clear.status.success(),
        "{}",
        String::from_utf8_lossy(&clear.stderr)
    );
    let reconciled = books.receipts()?.through();
    check_code(
        books.undo(&owner, &books.company, find("RECONCILED")?),
        ErrorCode::UndoIneligible,
    )?;
    assert_eq!(books.receipts()?.through(), reconciled);
    Ok(())
}

fn assert_owner_required(
    books: &Books,
    owner: &Actor,
    agent: &Actor,
    receipt: ReceiptId,
) -> TestResult {
    check_code(
        books.undo(&books.local, &books.company, receipt),
        ErrorCode::Unauthorized,
    )?;
    check_code(
        books.undo(agent, &books.company, receipt),
        ErrorCode::Forbidden,
    )?;
    check_code(
        books.undo(owner, &"other".parse()?, receipt),
        ErrorCode::NotFound,
    )?;

    Ok(())
}

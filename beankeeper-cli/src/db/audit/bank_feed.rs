//! Authorized mutations for provider staging and review.

use beankeeper::types::AccountType;
use rusqlite::{OptionalExtension, params};
use serde_json::json;

use super::{
    AuditAction, AuditChange, AuditEntity, grant::BankFeedAdmin, record, savepoint::AuditedTx,
    snapshot,
};
use crate::{
    db::{
        Actor, Db, accounts,
        bank_feed::{
            self, ConnectArgs, Connection, Cursor, FeedConnectionId, FeedError, FeedItemId,
            FeedTransaction, ItemStatus, ProviderTransactionKey, SyncDelta,
        },
        companies::{Company, CompanySlug},
    },
    error::CliError,
    identity::Granted,
};

pub(crate) fn connect(
    db: &Db,
    actor: &Actor,
    company: &CompanySlug,
    args: &ConnectArgs,
) -> Result<Connection, CliError> {
    Granted::<BankFeedAdmin>::with_db(db, actor, |grant| {
        grant.mutate("sp_connect_bank_feed", |tx, actor| {
            let scope = Company::bind(tx.read(), company.clone())?;
            let account = accounts::get_active_account(&scope, args.account.as_str())?;
            let has_native_fx: bool = tx.read().query_row(
                "SELECT EXISTS(SELECT 1 FROM fx_accounts WHERE company_slug = ?1 AND account_code = ?2)",
                params![company, account.code], |row| row.get(0),
            )?;
            if !matches!(account.account_type, AccountType::Asset | AccountType::Liability)
                || has_native_fx
                || crate::db::fx::functional_currency(tx.read(), company)?
                    .is_some_and(|currency| currency != args.currency)
            {
                return Err(FeedError::InvalidMapping.into());
            }
            let existing: Option<FeedConnectionId> = tx.connection().query_row(
                "SELECT id FROM bank_feed_connections WHERE company_slug = ?1 AND
                 (account_code = ?2 OR (environment = ?3 AND provider_account = ?4))",
                params![company, args.account.as_str(), args.environment.as_str(), args.provider_account.as_str()],
                |row| row.get(0),
            ).optional()?;
            if let Some(id) = existing {
                let current = bank_feed::connection(tx.read(), company, &id)?;
                if current.account != args.account.as_str() || current.currency != args.currency
                    || current.provider_account != args.provider_account || current.environment != args.environment
                {
                    return Err(FeedError::InvalidMapping.into());
                }
                return set_connected_in_scope(tx, actor, company, current, true);
            }
            let id = FeedConnectionId::generate();
            tx.connection().execute(
                "INSERT INTO bank_feed_connections (id, company_slug, account_code, provider_account, environment, currency)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id, company, args.account.as_str(), args.provider_account.as_str(), args.environment.as_str(), args.currency.code()],
            )?;
            let result = bank_feed::connection(tx.read(), company, &id)?;
            audit(tx, actor, company, &id.to_string(), None, &snapshot(&result))?;
            Ok(result)
        })
    })
}

pub(crate) fn disconnect(
    db: &Db,
    actor: &Actor,
    company: &CompanySlug,
    id: &FeedConnectionId,
) -> Result<Connection, CliError> {
    Granted::<BankFeedAdmin>::with_db(db, actor, |grant| {
        grant.mutate("sp_disconnect_bank_feed", |tx, actor| {
            let current = bank_feed::connection(tx.read(), company, id)?;
            set_connected_in_scope(tx, actor, company, current, false)
        })
    })
}

fn set_connected_in_scope(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &CompanySlug,
    before: Connection,
    connected: bool,
) -> Result<Connection, CliError> {
    if before.connected == connected {
        return Ok(before);
    }
    tx.connection().execute(
        "UPDATE bank_feed_connections SET connected = ?1, revision = revision + 1
         WHERE id = ?2 AND company_slug = ?3",
        params![connected, before.id, company],
    )?;
    let after = bank_feed::connection(tx.read(), company, &before.id)?;
    audit(
        tx,
        actor,
        company,
        &before.id.to_string(),
        Some(&snapshot(&before)),
        &snapshot(&after),
    )?;
    Ok(after)
}

pub(in crate::db) fn commit_sync(
    db: &Db,
    actor: &Actor,
    company: &CompanySlug,
    before: &Connection,
    cursor: &Cursor,
    delta: &SyncDelta,
) -> Result<Connection, CliError> {
    Granted::<BankFeedAdmin>::with_db(db, actor, |grant| {
        grant.mutate("sp_sync_bank_feed", |tx, actor| {
            let current = bank_feed::connection(tx.read(), company, &before.id)?;
            if !current.connected {
                return Err(FeedError::Disconnected.into());
            }
            if current.revision != before.revision || current.cursor != before.cursor {
                return Err(FeedError::Stale.into());
            }
            let mut changes = 0_u64;
            for row in delta
                .rows
                .iter()
                .filter(|row| row.account == current.provider_account)
            {
                changes += u64::from(upsert_row(tx, company, &current, row)?);
                if !row.pending
                    && let Some(key) = &row.pending_key
                    && key != &row.key
                {
                    changes += u64::from(remove_row(tx, company, &current.id, key)?);
                }
            }
            for row in delta.removed.iter().filter(|row| {
                row.account
                    .as_ref()
                    .is_none_or(|account| account == &current.provider_account)
            }) {
                changes += u64::from(remove_row(tx, company, &current.id, &row.key)?);
            }
            if changes == 0 && current.cursor.as_ref() == Some(cursor) {
                return Ok(current);
            }
            tx.connection().execute(
                "UPDATE bank_feed_connections SET cursor = ?1, revision = revision + 1
                 WHERE id = ?2 AND company_slug = ?3",
                params![cursor.as_str(), current.id, company],
            )?;
            let after = bank_feed::connection(tx.read(), company, &current.id)?;
            audit(
                tx,
                actor,
                company,
                &current.id.to_string(),
                Some(&snapshot(&current)),
                &snapshot(&json!({"connection": &after, "changed_items": changes})),
            )?;
            Ok(after)
        })
    })
}

fn upsert_row(
    tx: &AuditedTx<'_>,
    company: &CompanySlug,
    feed: &Connection,
    row: &FeedTransaction,
) -> Result<bool, CliError> {
    if row.currency != feed.currency {
        return Err(FeedError::InvalidMapping.into());
    }
    let item_id = FeedItemId::generate();
    let status = if row.pending {
        ItemStatus::Pending
    } else {
        ItemStatus::Posted
    };
    let changes = tx.connection().execute(
        "INSERT INTO bank_feed_items
             (id, connection_id, company_slug, provider_key, pending_key, date, description, amount_minor, status)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
         ON CONFLICT(connection_id, provider_key) DO UPDATE SET
             pending_key = excluded.pending_key, date = excluded.date, description = excluded.description,
             amount_minor = excluded.amount_minor, status = excluded.status, revision = revision + 1
         WHERE pending_key IS NOT excluded.pending_key OR date IS NOT excluded.date
             OR description IS NOT excluded.description OR amount_minor IS NOT excluded.amount_minor
             OR status IS NOT excluded.status",
        params![item_id, feed.id, company, row.key.as_str(),
            row.pending_key.as_ref().map(ProviderTransactionKey::as_str),
            row.date, row.description, row.amount.get(), status.as_str()],
    )?;
    Ok(changes > 0)
}

fn remove_row(
    tx: &AuditedTx<'_>,
    company: &CompanySlug,
    id: &FeedConnectionId,
    key: &ProviderTransactionKey,
) -> Result<bool, CliError> {
    let changed = tx.connection().execute(
        "UPDATE bank_feed_items SET status = 'removed', revision = revision + 1
         WHERE connection_id = ?1 AND company_slug = ?2 AND provider_key = ?3 AND status <> 'removed'",
        params![id, company, key.as_str()],
    )?;
    Ok(changed > 0)
}

fn audit(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &CompanySlug,
    id: &str,
    before: Option<&str>,
    after: &str,
) -> Result<(), CliError> {
    record(
        tx.connection(),
        actor,
        AuditChange {
            company_slug: Some(company),
            entity: AuditEntity::BankFeed,
            entity_id: id,
            action: AuditAction::BankFeedChange,
            snapshots: (before, Some(after)),
        },
    )
}

use super::grant::PostingGrant;
use crate::commands::reference::TransactionReference;
use crate::db::{
    bank_feed::{Item, ItemSelection, LinkArgs, PostArgs, Review},
    holds::{
        PostingPurpose,
        matching::{self, BankLine, BankMatch, HoldMatchOptions},
    },
    ids::TransactionId,
    proposals::{self, ProposalCreation, ProposalId},
    transactions::{
        self, ConflictStrategy, PostEntryParams, PostResult, PostTransactionParams, PostingSource,
    },
};
use crate::identity::Post;
use beankeeper::types::DebitOrCredit;

pub(crate) fn link(
    db: &Db,
    actor: &Actor,
    company: &CompanySlug,
    args: &LinkArgs,
) -> Result<Item, CliError> {
    if actor
        .identity()
        .map(crate::identity::VerifiedAgent::write_mode)
        == Some(crate::identity::WriteMode::ProposeOnly)
    {
        return Err(CliError::Forbidden(
            "a direct bookkeeper must confirm an existing bank feed match".into(),
        ));
    }
    Granted::<Post>::with_db(db, actor, |grant| {
        grant.mutate("sp_link_bank_feed", |tx, actor| {
            let (feed, item) = selected(tx, company, &args.selection)?;
            if item.transaction_id == Some(args.transaction)
                && item.resolved_revision == Some(item.revision)
            {
                return Ok(item);
            }
            require_unresolved(tx, company, &item)?;
            if !bank_feed::candidate_transactions(
                tx.read(),
                company,
                &feed,
                &item,
                Some(args.transaction),
            )?
            .into_iter()
            .any(|candidate| !candidate.already_linked)
            {
                return Err(FeedError::UnavailableItem.into());
            }
            resolve(
                tx,
                actor,
                company,
                &feed,
                &item,
                Resolution::Posted(args.transaction),
            )
        })
    })
}

pub(crate) fn post(
    db: &Db,
    actor: &Actor,
    company: &CompanySlug,
    args: &PostArgs,
) -> Result<Item, CliError> {
    PostingGrant::with_db(db, actor, |grant| match grant {
        PostingGrant::Direct(grant) => grant.mutate("sp_post_bank_feed", |tx, actor| {
            post_selected(tx, actor, company, args, PostingMode::Direct)
        }),
        PostingGrant::Proposal(grant) => grant.mutate("sp_propose_bank_feed", |tx, actor| {
            post_selected(tx, actor, company, args, PostingMode::Propose)
        }),
    })
}

fn selected(
    tx: &AuditedTx<'_>,
    company: &CompanySlug,
    selected: &ItemSelection,
) -> Result<(Connection, Item), CliError> {
    let feed = bank_feed::connection(tx.read(), company, &selected.feed)?;
    let item = bank_feed::item(tx.read(), company, &feed, &selected.item)?;
    if item.revision != selected.revision {
        return Err(FeedError::Stale.into());
    }
    Ok((feed, item))
}

fn require_unresolved(
    tx: &AuditedTx<'_>,
    company: &CompanySlug,
    item: &Item,
) -> Result<(), CliError> {
    if item.status != ItemStatus::Posted || item.transaction_id.is_some() {
        return Err(FeedError::UnavailableItem.into());
    }
    if let Some(id) = item.proposal_id.as_ref() {
        let retryable: bool = tx.connection().query_row(
            "SELECT status IN ('rejected', 'withdrawn') FROM txn_proposals WHERE id = ?1 AND company_slug = ?2",
            params![id.to_string(), company], |row| row.get(0),
        )?;
        if !retryable {
            return Err(FeedError::UnavailableItem.into());
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum PostingMode {
    Direct,
    Propose,
}

enum Resolution {
    Posted(TransactionId),
    Proposed(ProposalId),
}

impl From<PostResult> for Resolution {
    fn from(result: PostResult) -> Self {
        match result {
            PostResult::Created(id) | PostResult::Skipped(id) => Self::Posted(id),
        }
    }
}

impl From<ProposalCreation> for Resolution {
    fn from(result: ProposalCreation) -> Self {
        match result {
            ProposalCreation::Created(proposal) => Self::Proposed(proposal.id),
            ProposalCreation::Skipped(id) => Self::Posted(id),
        }
    }
}

fn post_selected(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &CompanySlug,
    args: &PostArgs,
    mode: PostingMode,
) -> Result<Item, CliError> {
    let (feed, item) = selected(tx, company, &args.selection)?;
    if item.transaction_id.is_some() && item.resolved_revision == Some(item.revision) {
        return Ok(item);
    }
    if let Some(proposal) = item.proposal_id.as_ref()
        && item.resolved_revision == Some(item.revision)
    {
        let still_open: bool = tx.connection().query_row(
            "SELECT status IN ('pending', 'approved') FROM txn_proposals WHERE id = ?1 AND company_slug = ?2",
            params![proposal.to_string(), company], |row| row.get(0),
        )?;
        if still_open {
            return Ok(item);
        }
    }
    require_unresolved(tx, company, &item)?;
    let review = bank_feed::review(tx.read(), company, &feed, item)?;
    if !args.confirm_new && !review.candidate_transactions.is_empty() {
        return Err(FeedError::ExistingCandidates.into());
    }
    let resolution = post_feed_item(tx, (actor, company), &feed, &review, args, mode)?;
    resolve(tx, actor, company, &feed, &review.item, resolution)
}

fn post_feed_item(
    tx: &AuditedTx<'_>,
    (actor, company): (&Actor, &CompanySlug),
    feed: &Connection,
    review: &Review,
    args: &PostArgs,
    mode: PostingMode,
) -> Result<Resolution, CliError> {
    let item = &review.item;
    let reference: TransactionReference = item.id.to_string().parse()?;
    let metadata = json!({"bank_feed_item": item.id, "payee": item.description}).to_string();
    let payee = item.description.parse().ok();
    let line = BankLine {
        reference: &reference,
        description: &item.description,
        payee: payee.as_ref(),
        date: item.date,
        currency: feed.currency,
        bank: &feed.account,
        direction: bank_feed::direction(item.amount),
        amount: item.amount_minor.abs().try_into()?,
        metadata: Some(&metadata),
    };
    let matching = HoldMatchOptions::default();
    let settlement = match mode {
        PostingMode::Direct => {
            matching::post(tx, actor, company, &line, matching)?.map(|result| match result {
                BankMatch::New(result) | BankMatch::Existing(result) => {
                    Resolution::Posted(result.transaction_id)
                }
            })
        }
        PostingMode::Propose => {
            matching::propose(tx, actor, company, &line, matching)?.map(|result| match result {
                BankMatch::New((id, _)) => Resolution::Proposed(id),
                BankMatch::Existing(result) => Resolution::Posted(result.transaction_id),
            })
        }
    };
    if let Some(resolution) = settlement {
        return Ok(resolution);
    }
    let entries = pair_entries(
        &line,
        args.contra
            .as_deref()
            .or(review.suggested_account.as_deref()),
    )?;
    let request = PostTransactionParams {
        company_slug: company,
        description: line.description,
        metadata: line.metadata,
        currency: line.currency,
        date: line.date,
        entries: &entries,
        correlate: None,
        reference: Some(line.reference.as_str()),
        on_conflict: ConflictStrategy::Error,
        actor,
    };
    match mode {
        PostingMode::Direct => {
            transactions::post_direct(tx, &request, PostingSource::Import).map(Into::into)
        }
        PostingMode::Propose => proposals::reserve_in_scope(
            tx,
            PostingPurpose::Posted,
            actor,
            &request,
            PostingSource::Command,
        )
        .map(Into::into),
    }
}

fn pair_entries(
    line: &BankLine<'_>,
    contra: Option<&str>,
) -> Result<[PostEntryParams; 2], CliError> {
    let contra = contra.filter(|value| *value != line.bank).ok_or_else(|| {
        CliError::Validation("choose an offset account different from the feed account".into())
    })?;
    let make_entry = |account: &str, direction| PostEntryParams {
        account_code: account.into(),
        direction,
        amount: line.amount,
        memo: None,
        tax_category: None,
    };
    let opposite = match line.direction {
        DebitOrCredit::Debit => DebitOrCredit::Credit,
        DebitOrCredit::Credit => DebitOrCredit::Debit,
    };
    Ok([
        make_entry(line.bank, line.direction),
        make_entry(contra, opposite),
    ])
}

fn resolve(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &CompanySlug,
    feed: &Connection,
    item: &Item,
    resolution: Resolution,
) -> Result<Item, CliError> {
    let (transaction, proposal) = match resolution {
        Resolution::Posted(id) => (Some(id), None),
        Resolution::Proposed(id) => (None, Some(id.to_string())),
    };
    tx.connection().execute(
        "UPDATE bank_feed_items SET transaction_id = ?1, proposal_id = ?2, resolved_revision = revision
         WHERE id = ?3 AND company_slug = ?4",
        params![transaction, proposal, item.id, company],
    )?;
    let after = bank_feed::item(tx.read(), company, feed, &item.id)?;
    audit(
        tx,
        actor,
        company,
        &item.id.to_string(),
        Some(&snapshot(item)),
        &snapshot(&after),
    )?;
    Ok(after)
}

#[cfg(test)]
mod tests {
    use crate::identity::Admin;
    use beankeeper::types::{AccountCode, Currency};

    use super::*;
    use crate::db::{self, CreateAccountParams, test_support, verification::VerifiedLedger};

    fn fixture(db: &Db, company: &CompanySlug, actor: &Actor) -> Result<Connection, CliError> {
        test_support::create_company(db, company.as_str(), "Feed tests")?;
        for (code, name, account_type) in [
            ("1000", "Bank", AccountType::Asset),
            ("5000", "Groceries", AccountType::Expense),
        ] {
            test_support::create_account(
                db,
                &CreateAccountParams {
                    company_slug: company,
                    code,
                    name,
                    account_type,
                    default_tax_category: None,
                },
            )?;
        }
        connect(
            db,
            actor,
            company,
            &ConnectArgs {
                account: AccountCode::new("1000")
                    .map_err(|error| CliError::Validation(error.to_string()))?,
                provider_account: "bank-key".parse()?,
                currency: Currency::USD,
                environment: bank_feed::PlaidEnvironment::Sandbox,
            },
        )
    }

    #[test]
    fn sync_checks_read_state_before_requesting_a_writer() -> Result<(), CliError> {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        use crate::{
            commands::bank_feed::BankFeedRequest, ledger_target::LedgerTarget,
            request::LedgerRequest,
        };

        let db = Db::open_in_memory()?;
        let company = "acme".parse()?;
        let actor = Actor::new("fixture".parse()?);
        let feed = fixture(&db, &company, &actor)?;
        disconnect(&db, &actor, &company, &feed.id)?;

        let admissions = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&admissions);
        let db = db.with_write_authorizer(LedgerTarget::default(), move |_: &LedgerTarget, _| {
            observed.fetch_add(1, Ordering::Relaxed);
            Ok(())
        });
        let result =
            LedgerRequest::BankFeed((company, BankFeedRequest::Sync(feed.id))).execute(&db, &actor);
        let Err(error) = result else {
            return Err(CliError::General(
                "a disconnected feed could not be rejected".into(),
            ));
        };
        assert!(error.to_string().contains("bank feed is disconnected"));
        assert_eq!(admissions.load(Ordering::Relaxed), 0);
        Ok(())
    }

    fn provider_row(key: &str, pending: bool, amount: i64) -> Result<FeedTransaction, CliError> {
        Ok(FeedTransaction {
            account: "bank-key".parse()?,
            key: key.parse()?,
            pending_key: None,
            date: chrono::NaiveDate::from_ymd_opt(2026, 11, 20)
                .ok_or_else(|| CliError::General("invalid test date".into()))?,
            description: "Groceries".into(),
            amount: bank_feed::SignedMinor::new(amount)?,
            currency: Currency::USD,
            pending,
        })
    }

    fn only_item(
        db: &Db,
        company: &CompanySlug,
        feed: &Connection,
        provider_key: &str,
    ) -> Result<Item, CliError> {
        let id = db.read().query_row(
            "SELECT id FROM bank_feed_items WHERE connection_id = ?1 AND provider_key = ?2",
            params![feed.id, provider_key],
            |row| row.get(0),
        )?;
        bank_feed::item(db.read(), company, feed, &id)
    }

    #[test]
    fn sync_discards_stale_workers_and_later_edits_leave_posting_intact() -> Result<(), CliError> {
        let db = Db::open_in_memory()?;
        let actor = test_support::fixture_actor("tester");
        let company = "acme".parse()?;
        let initial = fixture(&db, &company, &actor)?;

        let pending = provider_row("authorization", true, -4000)?;
        let first = SyncDelta {
            rows: vec![pending],
            removed: vec![],
        };
        let active = commit_sync(&db, &actor, &company, &initial, &"first".parse()?, &first)?;
        let pending = only_item(&db, &company, &active, "authorization")?;
        assert_eq!(pending.status, ItemStatus::Pending);
        assert!(commit_sync(&db, &actor, &company, &initial, &"stale".parse()?, &first).is_err());

        let mut posted = provider_row("posted", false, -4123)?;
        posted.pending_key = Some("authorization".parse()?);
        let second = SyncDelta {
            rows: vec![posted],
            removed: vec![],
        };
        let active = commit_sync(&db, &actor, &company, &active, &"second".parse()?, &second)?;
        assert_eq!(
            only_item(&db, &company, &active, "authorization")?.status,
            ItemStatus::Removed
        );
        let item = only_item(&db, &company, &active, "posted")?;
        let args = PostArgs {
            selection: ItemSelection {
                feed: active.id.clone(),
                item: item.id,
                revision: item.revision,
            },
            contra: Some("5000".into()),
            confirm_new: false,
        };
        let applied = post(&db, &actor, &company, &args)?;
        assert_eq!(
            applied.transaction_id,
            post(&db, &actor, &company, &args)?.transaction_id
        );

        let changed = SyncDelta {
            rows: vec![provider_row("posted", false, -5000)?],
            removed: vec![],
        };
        let active = commit_sync(&db, &actor, &company, &active, &"third".parse()?, &changed)?;
        let revised = only_item(&db, &company, &active, "posted")?;
        assert!(revised.correction_required);
        assert_eq!(revised.transaction_id, applied.transaction_id);
        assert_eq!(revised.amount_minor, -5000);
        let (count, debit): (i64, i64) = db.read().query_row(
            "SELECT COUNT(*), SUM(amount) FROM entries WHERE account_code = '5000'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!((count, debit), (1, 4123));
        assert!(post(&db, &actor, &company, &args).is_err());
        VerifiedLedger::read(&db)?;
        Ok(())
    }

    #[test]
    fn reviewed_file_import_is_linked_without_posting_once_more() -> Result<(), CliError> {
        let db = Db::open_in_memory()?;
        let actor = test_support::fixture_actor("tester");
        let company = "acme".parse()?;
        let feed = fixture(&db, &company, &actor)?;
        let row = provider_row("posted", false, -4200)?;
        let date = row.date;
        let feed = commit_sync(
            &db,
            &actor,
            &company,
            &feed,
            &"one".parse()?,
            &SyncDelta {
                rows: vec![row],
                removed: vec![],
            },
        )?;
        let item = only_item(&db, &company, &feed, "posted")?;
        let entries = [
            PostEntryParams {
                account_code: "1000".into(),
                direction: DebitOrCredit::Credit,
                amount: 4000_i64.try_into()?,
                memo: None,
                tax_category: None,
            },
            PostEntryParams {
                account_code: "1000".into(),
                direction: DebitOrCredit::Credit,
                amount: 200_i64.try_into()?,
                memo: None,
                tax_category: None,
            },
            PostEntryParams {
                account_code: "5000".into(),
                direction: DebitOrCredit::Debit,
                amount: 4200_i64.try_into()?,
                memo: None,
                tax_category: None,
            },
        ];
        let result = transactions::post_fixture(
            &db,
            &PostTransactionParams {
                company_slug: &company,
                description: "Imported groceries",
                metadata: None,
                currency: Currency::USD,
                date,
                entries: &entries,
                correlate: None,
                reference: Some("ofx-row"),
                on_conflict: ConflictStrategy::Error,
                actor: &actor,
            },
        )?;
        let PostResult::Created(existing) = result else {
            return Err(CliError::General(
                "test posting unexpectedly reused reference".into(),
            ));
        };
        let selection = ItemSelection {
            feed: feed.id.clone(),
            item: item.id,
            revision: item.revision,
        };
        let plan = PostArgs {
            selection: selection.clone(),
            contra: Some("5000".into()),
            confirm_new: false,
        };
        assert!(post(&db, &actor, &company, &plan).is_err());
        let linked = link(
            &db,
            &actor,
            &company,
            &LinkArgs {
                selection,
                transaction: existing,
            },
        )?;
        assert_eq!(linked.transaction_id, Some(existing));
        assert!(bank_feed::connection(db.read(), &"other".parse()?, &feed.id).is_err());
        let count: i64 = db
            .read()
            .query_row("SELECT COUNT(*) FROM transactions", [], |row| row.get(0))?;
        assert_eq!(count, 1);
        let log: i64 = db.read().query_row(
            "SELECT COUNT(*) FROM audit_log WHERE entity = 'bank_feed' AND action = 'bank_feed_change'",
            [], |row| row.get(0),
        )?;
        assert_eq!(log, 3);
        db::bank_feed::verify(db.read())?;
        Ok(())
    }

    #[test]
    fn approve_rejects_changed_feed_revision_without_posting() -> Result<(), CliError> {
        use crate::identity::{self, Approve, IssueRequest, WriteMode};
        use secrecy::{ExposeSecret, SecretString};

        let directory = tempfile::tempdir()?;
        let key = SecretString::from("test-only");
        let db = Db::open(&directory.path().join("ledger.db"), Some(&key))?;
        let owner = test_support::fixture_actor("owner");
        let company = "acme".parse()?;
        let feed = fixture(&db, &company, &owner)?;
        Granted::<Admin>::with_db(&db, &owner, identity::init_root)?;
        let propose_caps = ["ledger/read".into(), "ledger/propose".into()];
        let proposed_by = Granted::<Admin>::with_db(&db, &owner, |grant| {
            identity::issue_credential_with_mode(
                grant,
                &IssueRequest {
                    name: "proposal_agent",
                    capabilities: &propose_caps,
                    ttl: std::time::Duration::from_secs(3600),
                    public_key: None,
                    generate_key: true,
                },
                WriteMode::ProposeOnly,
            )
        })?;
        let approver_caps = ["ledger/read".into(), "ledger/approve".into()];
        let approved_by = Granted::<Admin>::with_db(&db, &owner, |grant| {
            identity::issue_credential(
                grant,
                &IssueRequest {
                    name: "approve_agent",
                    capabilities: &approver_caps,
                    ttl: std::time::Duration::from_secs(3600),
                    public_key: None,
                    generate_key: true,
                },
            )
        })?;
        let actor = test_support::fixture_actor("proposer").with_identity(identity::verify(
            db.read(),
            proposed_by.token().expose_secret(),
        )?);
        let approver = test_support::fixture_actor("approver").with_identity(identity::verify(
            db.read(),
            approved_by.token().expose_secret(),
        )?);
        let feed = commit_sync(
            &db,
            &owner,
            &company,
            &feed,
            &"first".parse()?,
            &SyncDelta {
                rows: vec![provider_row("posted", false, -2000)?],
                removed: vec![],
            },
        )?;
        let item = only_item(&db, &company, &feed, "posted")?;
        let args = PostArgs {
            selection: ItemSelection {
                feed: feed.id.clone(),
                item: item.id,
                revision: item.revision,
            },
            contra: Some("5000".into()),
            confirm_new: false,
        };
        let pending = post(&db, &actor, &company, &args)?;
        let id = pending
            .proposal_id
            .ok_or_else(|| CliError::General("expected a proposal".into()))?;
        assert!(pending.transaction_id.is_none());
        commit_sync(
            &db,
            &owner,
            &company,
            &feed,
            &"new".parse()?,
            &SyncDelta {
                rows: vec![provider_row("posted", false, -3000)?],
                removed: vec![],
            },
        )?;
        assert!(
            Granted::<Approve>::with_db(&db, &approver, |grant| {
                super::super::approve_proposal(grant, &company, &id)
            })
            .is_err()
        );
        let posted: i64 = db
            .read()
            .query_row("SELECT COUNT(*) FROM transactions", [], |row| row.get(0))?;
        assert_eq!(posted, 0);
        VerifiedLedger::read(&db)?;
        Ok(())
    }

    #[test]
    fn locked_period_leaves_feed_item_available_for_review() -> Result<(), CliError> {
        let db = Db::open_in_memory()?;
        let actor = test_support::fixture_actor("tester");
        let company = "acme".parse()?;
        let feed = fixture(&db, &company, &actor)?;
        let row = provider_row("posted", false, -500)?;
        Granted::<Admin>::with_db(&db, &actor, |grant| {
            super::super::change_period(
                grant,
                &company,
                db::periods::PeriodChange::Close(row.date.try_into()?),
            )
        })?;
        let feed = commit_sync(
            &db,
            &actor,
            &company,
            &feed,
            &"next".parse()?,
            &SyncDelta {
                rows: vec![row],
                removed: vec![],
            },
        )?;
        let item = only_item(&db, &company, &feed, "posted")?;
        let request = PostArgs {
            selection: ItemSelection {
                feed: feed.id.clone(),
                item: item.id,
                revision: item.revision,
            },
            contra: Some("5000".into()),
            confirm_new: false,
        };
        let attempted = post(&db, &actor, &company, &request);
        assert!(
            matches!(attempted, Err(CliError::Validation(message)) if message.contains("closed"))
        );
        let after = only_item(&db, &company, &feed, "posted")?;
        assert!(after.resolved_revision.is_none());
        assert!(after.transaction_id.is_none());
        VerifiedLedger::read(&db)?;
        Ok(())
    }
}

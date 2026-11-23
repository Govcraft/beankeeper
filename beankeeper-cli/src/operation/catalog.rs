//! Canonical ledger operation and wire contracts.

// The input type both decodes arguments and generates the advertised schema.
// Report variants share one wire tool to preserve the public report contract.
macro_rules! ledger_operations {
    ($consumer:ident) => {
        $consumer! {
            Summary => ("summary", Read, tool(financial_summary, FinancialSummaryInput, "Answer how the owner is doing: read-only financial summary, period comparisons, spending movers, attention counts and totals, brief highlights and optional Desktop targets. Returns the same full JSON envelope as bk summary; all amounts are in one currency.")),
            LedgerSetupStatus => ("ledger.setup.status", SetupStatus, tool_at("setup.status", ledger_setup_status, Empty, "Read setup counts and owner-key readiness for the selected ledger.")),
            LedgerSetupBookShow => ("ledger.setup.book.show", SetupStatus, tool_at("setup.book-show", ledger_setup_book_show, CompanyScope, "Read one book's current profile, fiscal-year start, currency, and accounts.")),
            LedgerSetupStatementPreview => ("ledger.setup.statement.preview", Setup, tool_at("setup.statement-preview", ledger_setup_statement_preview, LedgerSetupStatementPreview, "Freeze bounded CSV or OFX statement contents for owner review in an active setup generation.")),
            LedgerSetupStatementCommit => ("ledger.setup.statement.commit", Approve, tool_at("setup.statement-commit", ledger_setup_statement_commit, LedgerSetupStatementCommit, "Commit the exact reviewed statement batch with a live ledger/approve credential.")),
            LedgerSetupStatementReceipt => ("ledger.setup.statement.receipt", Read, tool_at("setup.statement-receipt", ledger_setup_statement_receipt, LedgerSetupStatementReceipt, "Read the imported and preexisting transaction IDs for a committed statement batch.")),
            LedgerSetupBegin => ("ledger.setup.begin", SetupOwner, tool_at("setup.begin", ledger_setup_begin, LedgerSetupBegin, "Issue a short-lived setup-only credential for the encrypted ledger. Requires a live owner Admin credential.")),
            LedgerSetupFinish => ("ledger.setup.finish", SetupOwner, tool_at("setup.finish", ledger_setup_finish, LedgerSetupEnd, "Finish a setup generation and revoke its credential. Requires a live owner Admin credential.")),
            LedgerSetupRevoke => ("ledger.setup.revoke", SetupOwner, tool_at("setup.revoke", ledger_setup_revoke, LedgerSetupEnd, "Revoke a setup generation and its credential. Requires a live owner Admin credential.")),
            LedgerSetupBookCreate => ("ledger.setup.book.create", Setup, tool_at("setup.book-create", ledger_setup_book_create, LedgerSetupBookCreate, "Create a personal or business book in an active setup generation.")),
            LedgerSetupBookInitialize => ("ledger.setup.book.initialize", Setup, tool_at("setup.book-initialize", ledger_setup_book_initialize, LedgerSetupBookInitialize, "Record the first profile for an existing book in an active setup generation.")),
            LedgerSetupBookUpdate => ("ledger.setup.book.update", Setup, tool_at("setup.book-update", ledger_setup_book_update, LedgerSetupBookUpdate, "Supersede a book profile during an active setup generation.")),
            LedgerSetupChartApply => ("ledger.setup.chart.apply", Setup, tool_at("setup.chart-apply", ledger_setup_chart_apply, LedgerSetupChartApply, "Apply a versioned chart template to a setup book and return the created accounts.")),
            LedgerSetupAccountAdd => ("ledger.setup.account.add", Setup, tool_at("setup.account-add", ledger_setup_account_add, LedgerSetupAccountAdd, "Add the owner's named account to a setup book.")),
            LedgerSetupOpeningPreview => ("ledger.setup.opening.preview", Setup, tool_at("setup.opening-preview", ledger_setup_opening_preview, LedgerSetupOpeningPreview, "Validate and preview a balanced opening journal for an active setup book.")),
            LedgerSetupOpeningReserve => ("ledger.setup.opening.reserve", Setup, tool_at("setup.opening-reserve", ledger_setup_opening_reserve, LedgerSetupOpeningReserve, "Reserve the exact previewed opening journal for the existing approval workflow.")),
            PeriodList => ("period.list", Read, tool(period_list, CompanyScope, "List a company's current posting cutoff and last close/reopen attribution.")),
            CompanyList => ("company.list", Read, tool(company_list, Empty, "List companies in the ledger.")),
            CompanyShow => ("company.show", Read, tool(company_show, CompanyShow, "Show one company by slug.")),
            AccountList => ("account.list", Read, tool(account_list, AccountList, "List accounts, optionally filtered by account_type or name. Set with_balances=true for debit, credit, and balance totals. Use from/to to restrict activity and supply currency when the company uses multiple currencies.")),
            AccountShow => ("account.show", Read, tool(account_show, AccountShow, "Show one account in a company.")),
            TxnList => ("txn.list", Read, tool(txn_list, TxnList, "List or count transactions using the CLI filters and pagination.")),
            TxnShow => ("txn.show", Read, tool(txn_show, TxnShow, "Show transaction entries, attachments, and clearing details.")),
            SecurityList => ("security.list", Read, tool(security_list, CompanyScope, "List company securities.")),
            InvestmentLots => ("investment.lots", Read, tool(investment_lots, InvestmentLots, "List acquisition lots with remaining share quantities and cost basis.")),
            InvestmentHoldings => ("investment.holdings", Read, tool(investment_holdings, InvestmentHoldings, "Value holdings at the latest price on or before as_of without posting unrealized gains. Monetary results use minor units.")),
            ScheduleList => ("schedule.list", Read, cli_only(PendingParity)),
            RuleList => ("rule.list", Read, tool(rule_list, CompanyScope, "List active categorization rules by descending priority, then ascending rule ID.")),
            RuleMatch => ("rule.match", Read, tool(rule_match, RuleMatch, "Check a candidate transaction description without posting. Returns outcome=match with the winning rule or outcome=no_match.")),
            BookkeepingPolicyShow => ("bookkeeping-policy.show", Read, tool(bookkeeping_policy_show, CompanyScope, "Read the company's standing sales-tax and processor-fee instructions.")),
            InformPreferenceShow => ("inform-preference.show", Read, tool(inform_preference_show, CompanyScope, "Read how often the owner wants to hear about completed changes.")),
            BudgetList => ("budget.list", Read, tool(budget_list, BudgetList, "List budget entries for a company and year.")),
            LoanSchedule => ("loan.schedule", Read, cli_only(PendingParity)),
            Export => ("export", Read, cli_only(LocalFiles)),
            Verify => ("verify", Read, tool(verify, Empty, "Verify every company in the ledger and return balance, audit-chain, and check evidence.")),
            AuditLog => ("audit.log", Read, tool(audit_log, AuditLog, "List audit changes in bounded pages, optionally filtered by actor, verified agent URI, company, entity, or action. A client_hint is unverified runtime context. Continue with _meta.pagination.next_offset and the same filters.")),
            TxnProposals => ("txn.proposals", ProposalList, tool(txn_proposals, TxnProposals, "List visible pending transaction proposals. Under enforcement, proposers see their own; approvers and administrators see all. include_rejected also includes approved, rejected and withdrawn proposals.")),
            AgentLimitShow => ("agent.limit.show", Read, cli_only(LocalAdministration)),
            AgentList => ("agent.list", Read, cli_only(LocalAdministration)),
            PolicyShow => ("policy.show", Read, cli_only(LocalAdministration)),
            Report => ("report", Read, mcp_only(ReportKinds, report, Report, "Run a financial report for company. Use chart=true for supported charts and chart_all=true for all series. Optional fields unless marked required:
- trend: metric (required), from, to, period, currency, compare, explain_change.
- trial-balance: from, to or as_of, account_type, currency, period, compare, explain_change.
- balance: account (required), from, to or as_of, currency, compare, explain_change.
- income-statement: from, to, currency, period, compare, explain_change.
- balance-sheet: to or as_of, currency, compare, explain_change.
- schedule-c and tax-summary: from, to, currency.
- 1099 (alias 1099-nec): from, to, currency, threshold.
- ledger: account, from, to, currency, filter_metadata.
- budget-variance: year (required), month or from/to (month numbers), account_type, currency, include_unbudgeted.
- aging: account (at least one required), as_of, include_children, buckets, group_by_metadata, currency, method, link_key, detail, group_across_accounts, filter_metadata.
- reconciliation: account (required), to or as_of, mode, link_key, statement_balance, statement, filter_metadata, group_by_metadata, currency.
- cash-flow: from, to, cash_account, currency, compare, explain_change.
Use only one of as_of and to. For date-range reports without as_of, use from and to.")),
            ReportAging => ("report.aging", Read, grouped(Report, "report.aging")),
            ReportBalance => ("report.balance", Read, grouped(Report, "report.balance")),
            ReportBalanceSheet => ("report.balance-sheet", Read, grouped(Report, "report.balance-sheet")),
            ReportBudgetVariance => ("report.budget-variance", Read, grouped(Report, "report.budget-variance")),
            ReportCashFlow => ("report.cash-flow", Read, grouped(Report, "report.cash-flow")),
            ReportIncomeStatement => ("report.income-statement", Read, grouped(Report, "report.income-statement")),
            ReportIncomeStatementCompare => ("report.income-statement.compare", Read, grouped(Report, "report.income-statement")),
            ReportIncomeStatementPeriods => ("report.income-statement.periods", Read, grouped(Report, "report.income-statement")),
            ReportLedger => ("report.ledger", Read, grouped(Report, "report.ledger")),
            ReportReconciliation => ("report.reconciliation", Read, grouped(Report, "report.reconciliation")),
            ReportScheduleC => ("report.schedule-c", Read, grouped(Report, "report.schedule-c")),
            ReportVendor1099 => ("report.1099", Read, grouped(Report, "report.1099")),
            ReportTaxSummary => ("report.tax-summary", Read, grouped(Report, "report.tax-summary")),
            ReportTrend => ("report.trend", Read, grouped(Report, "report.trend")),
            ReportTrialBalance => ("report.trial-balance", Read, grouped(Report, "report.trial-balance")),
            ReportTrialBalanceCompare => ("report.trial-balance.compare", Read, grouped(Report, "report.trial-balance")),
            ReportTrialBalancePeriods => ("report.trial-balance.periods", Read, grouped(Report, "report.trial-balance")),
            TxnPost => ("txn.post", Posting, tool(txn_post, TxnPost, "Record balanced debits and credits in decimal major units. Direct grants post immediately and return a transaction ID. A propose-only grant reserves the same input and returns a pending proposal ID for approval. Pass a stable reference and on_conflict=skip to skip an already posted reference. Use txn_proposals to find a pending reference.")),
            TxnImport => ("txn.import", Post, cli_only(LocalFiles)),
            TxnAttach => ("txn.attach", Post, cli_only(LocalFiles)),
            TxnMetaSet => ("txn.meta.set", Post, tool(txn_meta_set, TxnMetaSet, "Add or replace transaction annotations as key=value strings. Provide company and exactly one of id or reference. Relationship keys are reserved. No-op retries write no audit row.")),
            TxnMetaUnset => ("txn.meta.unset", Post, tool(txn_meta_unset, TxnMetaUnset, "Remove transaction annotation keys. Provide company and exactly one of id or reference. Relationship keys are reserved; absent keys are a no-op.")),
            InvestmentBuy => ("investment.buy", Post, tool(investment_buy, InvestmentBuy, "Acquire an exact share quantity and post total acquisition cost against cash. A duplicate reference fails atomically.")),
            InvestmentSell => ("investment.sell", Post, tool(investment_sell, InvestmentSell, "Sell shares with FIFO by default, LIFO, or one specific lot; post proceeds, basis relief and realized gain or loss atomically.")),
            SecurityCreate => ("security.create", Post, tool(security_create, SecurityCreate, "Create a company-scoped security with a fixed quotation currency.")),
            SecurityPrice => ("security.price", Post, tool(security_price, SecurityPrice, "Record a dated price per whole share in major currency units.")),
            LoanPay => ("loan.pay", Post, cli_only(PendingParity)),
            ScheduleCreate => ("schedule.create", Post, cli_only(PendingParity)),
            ScheduleCancel => ("schedule.cancel", Post, cli_only(PendingParity)),
            ScheduleRun => ("schedule.run", Post, cli_only(PendingParity)),
            RuleCreate => ("rule.create", Post, tool(rule_create, RuleCreate, "Create a case-insensitive literal description rule targeting an account in the company. Higher priority wins. Omitted priority defaults to zero.")),
            RuleAmend => ("rule.amend", Post, tool(rule_amend, RuleAmend, "Supersede an active categorization rule by ID with fully specified replacement terms. Returns a new rule ID and retains both versions in the audit trail.")),
            BookkeepingPolicySet => ("bookkeeping-policy.set", Post, tool(bookkeeping_policy_set, BookkeepingPolicySet, "Set or amend one standing sales-tax or processor-fee instruction. Use unset to clear it. Returns both current instructions.")),
            InformPreferenceSet => ("inform-preference.set", Post, tool(inform_preference_set, InformPreferenceSet, "Set or amend how often the owner hears about completed changes. Returns the saved preference.")),
            RuleUndo => ("rule.undo", OwnerUndo, tool(rule_undo, RuleIdInput, "Undo a rule creation by its returned rule ID. Appends an audit receipt and immediately stops future matching. Requires a signed ledger/admin credential. Retries return the same undo receipt.")),
            RuleDelete => ("rule.delete", Post, tool(rule_delete, RuleIdInput, "Remove an active rule from future matching and keep its full audit snapshot.")),
            TxnPropose => ("txn.propose", Propose, tool(txn_propose, TxnPost, "Explicitly reserve a balanced posting for sign-off. Use txn_post for ordinary bookkeeping; it follows the agent's write mode. This tool uses the same arguments and reserves the reference until resolution. Requires a verified proposer identity and ledger/propose when enforcement is enabled.")),
            TxnReject => ("txn.reject", ProposalReject, tool(txn_reject, TxnReject, "Reject a pending proposal with an optional reason. ledger/propose can withdraw the caller's own proposal; ledger/approve can reject any proposal. Resolved proposals cannot be resolved again.")),
            TxnApprove => ("txn.approve", Approve, tool(txn_approve, TxnApprove, "Approve a pending proposal and post it atomically as the approver. Requires ledger/approve when enforcement is enabled. Direct posts have already landed and can be undone with txn_reverse.")),
            TxnReverse => ("txn.reverse", Reverse, tool(txn_reverse, TxnReverse, "Undo a posted transaction in one step by appending an audited, linked mirror posting under the caller's identity. Requires ledger/reverse when enforcement is enabled. Pass the same explicit reference on retries to return the existing reversal. Reconciled transactions cannot be reversed; existing clearings stay on the original.")),
            TxnClear => ("txn.clear", Reconcile, tool(txn_clear, ClearInput, "Clear or reconcile entries by transaction and entry IDs, reference and account, or an atomic in-memory batch. Amounts use major-unit strings. Use a minus sign for withdrawals (credit), an explicit plus for deposits (debit), or an unsigned amount for either side.")),
            TxnReconcile => ("txn.reconcile", Reconcile, tool(txn_reconcile, Empty, "Find orphaned intercompany correlations across the ledger without changing it.")),
            BudgetSet => ("budget.set", Budget, tool(budget_set, BudgetSet, "Set a monthly amount or distribute an annual budget across twelve months.")),
            BudgetDelete => ("budget.delete", Budget, tool(budget_delete, BudgetDelete, "Delete a monthly or annual budget; this call confirms deletion.")),
            PeriodClose => ("period.close", Admin, tool(period_close, PeriodClose, "Close a company through an inclusive date, rejecting new postings on or before it. Requires ledger/admin when enforcement is on.")),
            PeriodReopen => ("period.reopen", Admin, tool(period_reopen, PeriodReopen, "Explicitly reopen all closed dates for a company with a required reason. Requires ledger/admin when enforcement is on.")),
            CompanyCreate => ("company.create", Admin, tool(company_create, CompanyCreate, "Create a company by slug, name and optional description. Requires ledger/admin when enforcement applies. Records an audit row.")),
            CompanyDelete => ("company.delete", Admin, tool(company_delete, CompanyShow, "Delete a company with no retained data and record an audit row. This call confirms deletion. Requires ledger/admin when enforcement applies.")),
            AccountCreate => ("account.create", Admin, tool(account_create, AccountCreate, "Create an account in a company.")),
            AccountDelete => ("account.delete", Admin, cli_only(LocalAdministration)),
            KeyInit => ("key.init", Admin, cli_only(LocalBootstrap)),
            AgentIssue => ("agent.issue", Admin, cli_only(LocalAdministration)),
            AgentRevoke => ("agent.revoke", Admin, cli_only(LocalAdministration)),
            PolicySet => ("policy.set", Admin, cli_only(LocalAdministration)),
            Init => ("init", Admin, cli_only(LocalBootstrap)),
            Backup => ("backup", Admin, cli_only(LocalFiles)),
            Restore => ("restore", Admin, cli_only(LocalFiles)),
            AgentLimitSet => ("agent.limit.set", Admin, cli_only(LocalAdministration)),
            AgentLimitClear => ("agent.limit.clear", Admin, cli_only(LocalAdministration)),
            McpServe => ("mcp.serve", Transport, cli_only(Transport)),
        }
    };
}

pub(crate) use ledger_operations;

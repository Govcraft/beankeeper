//! Synthetic journals for a validated interview.

use beankeeper::{
    error::BeanError,
    types::{AccountType, Currency, Money, NonNegativeMoney, PositiveMoney},
};
use chrono::NaiveDate;

use super::{cr_amount, dr_amount};
use crate::{
    db::{
        self, Actor, CreateAccountParams, Db, PostResult,
        amounts::PositiveMinorUnits,
        companies::CompanySlug,
        ids::{PartnerTransactionId, TransactionId},
        investments::{self, BuyParams, PriceParams, SecurityParams},
        transactions::{ConflictStrategy, PostTransactionParams},
    },
    demo::{CompanyKind, DemoAccountKind, DemoBookProfile, DemoIndustry, DemoProfile, WorkShape},
    error::CliError,
    identity::{Admin, Granted, Post},
};

pub(super) fn seed(db: &Db, initializer: &Actor, profile: &DemoProfile) -> Result<(), CliError> {
    let generator = Generator {
        db,
        actor: Actor::demo(),
    };
    let mut books = Vec::new();
    for (index, choices) in profile.books().iter().enumerate() {
        let number = index + 1;
        let slug = format!("demo-book-{number}").parse()?;
        let name = choices.kind.name(number).parse()?;
        Granted::<Admin>::with_db(db, initializer, |grant| {
            db::create_company(grant, &slug, &name, None)
        })?;
        for currency in profile.currencies() {
            let book = Book {
                generator: &generator,
                slug: &slug,
                currency,
            };
            let cash = book.seed_currency(profile, choices)?;
            if currency == profile.primary_currency() {
                books.push(PrimaryBook {
                    slug: slug.clone(),
                    kind: choices.kind,
                    cash,
                });
            }
        }
    }
    if profile.features().include_owner_draws {
        generator.draws(&books, profile.primary_currency())?;
    }
    Ok(())
}

impl CompanyKind {
    fn name(self, number: usize) -> String {
        match self {
            Self::Personal => format!("Fictional Household {number}"),
            Self::SoleProprietorship => format!("Fictional Juniper Workshop {number}"),
            Self::Llc => format!("Fictional Moonfern Studio {number} LLC"),
            Self::Corporation => format!("Fictional Copper Finch {number} Inc"),
        }
    }

    const fn equity_activity(self) -> &'static str {
        match self {
            Self::Personal => "Owner Contributions",
            Self::SoleProprietorship | Self::Llc => "Owner Draws",
            Self::Corporation => "Shareholder Distributions",
        }
    }
}

impl DemoAccountKind {
    const fn name(self) -> &'static str {
        match self {
            Self::Checking => "Sample Checking",
            Self::Savings => "Sample Savings",
            Self::CreditCard => "Sample Credit Card",
            Self::PaymentApp => "Sample Payment App",
            Self::Loan => "Sample Loan",
            Self::Investment => "Sample Brokerage",
            Self::Cash => "Sample Cash",
        }
    }

    const fn account_type(self) -> AccountType {
        match self {
            Self::CreditCard | Self::Loan => AccountType::Liability,
            _ => AccountType::Asset,
        }
    }

    const fn cash_class(self) -> beankeeper::types::CashClass {
        match self {
            Self::Checking | Self::Savings | Self::PaymentApp | Self::Cash => {
                beankeeper::types::CashClass::Cash
            }
            Self::CreditCard | Self::Loan | Self::Investment => {
                beankeeper::types::CashClass::NonCash
            }
        }
    }

    const fn is_cash_source(self) -> bool {
        matches!(self, Self::Checking | Self::Cash)
    }
}

struct Generator<'a> {
    db: &'a Db,
    actor: Actor,
}

struct PrimaryBook {
    slug: CompanySlug,
    kind: CompanyKind,
    cash: String,
}

impl Generator<'_> {
    fn draws(&self, books: &[PrimaryBook], currency: Currency) -> Result<(), CliError> {
        let Some(household) = books.iter().find(|book| book.kind == CompanyKind::Personal) else {
            return Ok(());
        };
        let receiving = Book {
            generator: self,
            slug: &household.slug,
            currency,
        };
        for company in books
            .iter()
            .filter(|book| book.kind != CompanyKind::Personal)
        {
            let sending = Book {
                generator: self,
                slug: &company.slug,
                currency,
            };
            let amount = sending.amount(475)?;
            let date = demo_date(3, 25)?;
            let (outgoing, incoming) = if company.kind == CompanyKind::Corporation {
                (
                    "Fictional shareholder distribution",
                    "Fictional shareholder distribution received",
                )
            } else {
                ("Fictional owner draw", "Fictional owner draw received")
            };
            let id = sending.post(date, outgoing, ("3100", &company.cash), amount, None)?;
            receiving.post(
                date,
                incoming,
                (&household.cash, "3100"),
                amount,
                Some(id.into()),
            )?;
        }
        Ok(())
    }
}

struct Book<'a> {
    generator: &'a Generator<'a>,
    slug: &'a CompanySlug,
    currency: Currency,
}

struct FinancialAccount {
    kind: DemoAccountKind,
    code: String,
}

impl Book<'_> {
    fn seed_currency(
        &self,
        profile: &DemoProfile,
        book: &DemoBookProfile,
    ) -> Result<String, CliError> {
        let primary = profile.primary_currency();
        let mut accounts = Vec::new();
        for (index, account) in book.accounts.iter().enumerate() {
            if account.currency_with_primary(primary) == self.currency {
                accounts.push(self.financial_account(account.kind(), Some(index + 1))?);
            }
        }
        let source = match accounts
            .iter()
            .find(|account| account.kind.is_cash_source())
        {
            Some(account) => account.code.clone(),
            None => {
                self.financial_account(DemoAccountKind::Checking, None)?
                    .code
            }
        };
        if profile.features().include_investments
            && self.currency == primary
            && !book
                .accounts
                .iter()
                .any(|account| account.kind() == DemoAccountKind::Investment)
        {
            accounts.push(self.financial_account(DemoAccountKind::Investment, None)?);
        }

        let scenario = Scenario::for_book(profile.work_shape(), book.kind, book.industry);
        self.seed_journal(book.kind, &source, &scenario, self.currency == primary)?;
        for account in accounts.iter().filter(|account| account.code != source) {
            self.seed_financial_activity(
                &source,
                account,
                profile.features().include_investments,
                scenario.expense,
            )?;
        }
        Ok(source)
    }

    fn financial_account(
        &self,
        kind: DemoAccountKind,
        number: Option<usize>,
    ) -> Result<FinancialAccount, CliError> {
        let slot = number.unwrap_or(if kind == DemoAccountKind::Investment {
            99
        } else {
            0
        });
        let code = format!("1.{:05}.{slot:02}", currency_slot(self.currency));
        let name = match number {
            Some(number) => format!("{} {number} ({})", kind.name(), self.currency),
            None => format!("{} ({})", kind.name(), self.currency),
        };
        let row = self.account(&code, &name, kind.account_type())?;
        db::audit::cash_classification::submit(
            self.generator.db,
            &self.generator.actor,
            self.slug,
            code.parse()
                .map_err(|error| CliError::Validation(format!("{error}")))?,
            kind.cash_class(),
        )?;
        Ok(FinancialAccount {
            kind,
            code: row.code,
        })
    }

    fn seed_journal(
        &self,
        kind: CompanyKind,
        cash: &str,
        scenario: &Scenario,
        primary: bool,
    ) -> Result<(), CliError> {
        if primary {
            for (code, name, account_type) in [
                ("3000", "Opening Equity", AccountType::Equity),
                ("3100", kind.equity_activity(), AccountType::Equity),
                ("4000", scenario.income_account, AccountType::Revenue),
                ("5000", scenario.expense_account, AccountType::Expense),
                ("5100", "Card Purchases", AccountType::Expense),
            ] {
                self.account(code, name, account_type)?;
            }
        }
        self.post(
            demo_date(1, 1)?,
            "Sample opening balance",
            (cash, "3000"),
            self.amount(3_400)?,
            None,
        )?;
        for (month, income, expense) in [(1, 3_680, 420), (2, 4_120, 510), (3, 3_940, 465)] {
            self.post(
                demo_date(month, 4)?,
                scenario.income,
                (cash, "4000"),
                self.amount(income)?,
                None,
            )?;
            self.post(
                demo_date(month, 9)?,
                scenario.expense,
                ("5000", cash),
                self.amount(expense)?,
                None,
            )?;
        }
        Ok(())
    }

    fn seed_financial_activity(
        &self,
        cash: &str,
        account: &FinancialAccount,
        investments: bool,
        expense: &str,
    ) -> Result<(), CliError> {
        let code = account.code.as_str();
        let (description, debit, credit, amount) = match account.kind {
            DemoAccountKind::Checking | DemoAccountKind::Cash => {
                ("Transfer to sample cash account", code, cash, 110)
            }
            DemoAccountKind::Savings => ("Transfer to sample savings", code, cash, 210),
            DemoAccountKind::PaymentApp => ("Transfer to sample payment wallet", code, cash, 95),
            DemoAccountKind::CreditCard => (expense, "5100", code, 128),
            DemoAccountKind::Loan => ("Synthetic loan proceeds", cash, code, 2_750),
            DemoAccountKind::Investment if investments => return self.investment(cash, code),
            DemoAccountKind::Investment => return Ok(()),
        };
        self.post(
            demo_date(2, 12)?,
            description,
            (debit, credit),
            self.amount(amount)?,
            None,
        )?;
        match account.kind {
            DemoAccountKind::CreditCard => {
                self.post(
                    demo_date(3, 12)?,
                    "Sample card payment",
                    (code, cash),
                    self.amount(100)?,
                    None,
                )?;
            }
            DemoAccountKind::Loan => {
                self.post(
                    demo_date(3, 12)?,
                    "Sample loan principal payment",
                    (code, cash),
                    self.amount(235)?,
                    None,
                )?;
            }
            _ => {}
        }
        Ok(())
    }

    fn investment(&self, cash: &str, asset: &str) -> Result<(), CliError> {
        let db = self.generator.db;
        let actor = &self.generator.actor;
        let symbol = format!("DEMO-{asset}");
        Granted::<Post>::with_db(db, actor, |grant| {
            investments::create_security(
                grant,
                self.slug,
                &SecurityParams {
                    symbol: symbol.parse()?,
                    name: "Fictional Paper Comet Fund".parse()?,
                    currency: self.currency,
                },
            )
        })?;
        let cost = PositiveMoney::new(self.money(296)).map_err(BeanError::from)?;
        Granted::<Post>::with_db(db, actor, |grant| {
            investments::buy(
                grant,
                self.slug,
                &BuyParams {
                    symbol: symbol.clone(),
                    quantity: "4".parse().map_err(BeanError::from)?,
                    cost,
                    date: demo_date(2, 16)?,
                    asset: asset.into(),
                    cash: cash.into(),
                    reference: None,
                },
            )
        })?;
        let price = NonNegativeMoney::new(self.money(81)).map_err(BeanError::from)?;
        Granted::<Post>::with_db(db, actor, |grant| {
            investments::record_price(
                grant,
                self.slug,
                &PriceParams {
                    symbol,
                    date: demo_date(3, 31)?,
                    price,
                },
            )
        })?;
        Ok(())
    }

    fn account(
        &self,
        code: &str,
        name: &str,
        account_type: AccountType,
    ) -> Result<db::AccountRow, CliError> {
        Granted::<Admin>::with_db(self.generator.db, &self.generator.actor, |grant| {
            db::create_account(
                grant,
                &CreateAccountParams {
                    company_slug: self.slug,
                    code,
                    name,
                    account_type,
                    default_tax_category: None,
                },
            )
        })
    }

    fn amount(&self, major: u16) -> Result<PositiveMinorUnits, CliError> {
        self.minor_units(major).try_into()
    }

    fn money(&self, major: u16) -> Money {
        Money::from_minor(i128::from(self.minor_units(major)), self.currency)
    }

    fn minor_units(&self, major: u16) -> i64 {
        let scale = match self.currency {
            Currency::JPY => 100,
            Currency::MXN => 15,
            _ => 1,
        };
        i64::from(major) * scale * 10_i64.pow(u32::from(self.currency.minor_units()))
    }

    fn post(
        &self,
        date: NaiveDate,
        description: &str,
        (debit, credit): (&str, &str),
        amount: PositiveMinorUnits,
        correlate: Option<PartnerTransactionId>,
    ) -> Result<TransactionId, CliError> {
        let actor = &self.generator.actor;
        Granted::<Post>::with_db(self.generator.db, actor, |grant| {
            db::post_transaction(
                grant,
                &PostTransactionParams {
                    company_slug: self.slug,
                    description,
                    metadata: None,
                    currency: self.currency,
                    date,
                    entries: &[dr_amount(debit, amount), cr_amount(credit, amount)],
                    correlate,
                    reference: None,
                    on_conflict: ConflictStrategy::Error,
                    actor,
                },
            )
        })
        .map(|result| match result {
            PostResult::Created(id) | PostResult::Skipped(id) => id,
        })
    }
}

fn demo_date(month: u32, day: u32) -> Result<NaiveDate, CliError> {
    NaiveDate::from_ymd_opt(2025, month, day)
        .ok_or_else(|| CliError::General("invalid synthetic posting date".into()))
}

fn currency_slot(currency: Currency) -> u32 {
    // Pack the ISO letters into a decimal account code. Each currency has its own range.
    let [a, b, c] = currency.code_bytes().map(u32::from);
    (a - 65) * 26 * 26 + (b - 65) * 26 + c - 65
}

struct Scenario {
    income_account: &'static str,
    income: &'static str,
    expense_account: &'static str,
    expense: &'static str,
}

impl Scenario {
    fn for_book(work: WorkShape, kind: CompanyKind, industry: DemoIndustry) -> Self {
        let ((income_account, income), (expense_account, expense)) = match (kind, industry) {
            (CompanyKind::Personal, _) => {
                let income = match work {
                    WorkShape::Employee => ("Wages", "Pay from Fictional Starling Works"),
                    WorkShape::SelfEmployed | WorkShape::BusinessOwner => (
                        "Other Household Income",
                        "Income from Fictional Willow Studio",
                    ),
                    WorkShape::Retired => ("Retirement Income", "Fictional pension payment"),
                    WorkShape::Other => ("Household Income", "Fictional household income"),
                };
                (
                    income,
                    (
                        "Everyday Spending",
                        "Groceries from Fictional Orchard Market",
                    ),
                )
            }
            (_, DemoIndustry::Retail) => (
                ("Merchandise Sales", "Sales at Fictional Ember Market"),
                ("Store Supplies", "Order from Fictional Lantern Supply"),
            ),
            (_, DemoIndustry::Trades) => (
                ("Project Revenue", "Fictional workshop installation"),
                ("Materials", "Materials from Fictional Pine Supply"),
            ),
            (_, DemoIndustry::ProfessionalServices) => (
                ("Service Revenue", "Fictional studio consulting invoice"),
                (
                    "Operating Expenses",
                    "Subscription from Fictional Quill Software",
                ),
            ),
            (_, DemoIndustry::General) => (
                ("Business Revenue", "Invoice to Fictional Kite Works"),
                ("Operating Expenses", "Order from Fictional Fern Supply"),
            ),
        };
        Self {
            income_account,
            income,
            expense_account,
            expense,
        }
    }
}

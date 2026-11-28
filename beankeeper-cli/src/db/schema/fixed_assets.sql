CREATE TABLE fixed_assets (
    id TEXT PRIMARY KEY NOT NULL,
    company_slug TEXT NOT NULL REFERENCES companies(slug),
    acquisition_id INTEGER NOT NULL UNIQUE REFERENCES transactions(id),
    acquired_on TEXT NOT NULL,
    cost_account TEXT NOT NULL,
    accumulated_account TEXT NOT NULL,
    expense_account TEXT NOT NULL,
    details TEXT NOT NULL CHECK(json_valid(details))
        CHECK(acquired_on IS json_extract(details,'$.acquired_on'))
        CHECK(cost_account IS json_extract(details,'$.accounts.cost'))
        CHECK(accumulated_account IS json_extract(details,'$.accounts.accumulated'))
        CHECK(expense_account IS json_extract(details,'$.accounts.expense')),
    FOREIGN KEY(company_slug,cost_account) REFERENCES accounts(company_slug,code),
    FOREIGN KEY(company_slug,accumulated_account) REFERENCES accounts(company_slug,code),
    FOREIGN KEY(company_slug,expense_account) REFERENCES accounts(company_slug,code),
    UNIQUE(company_slug,id)
) STRICT;

CREATE INDEX fixed_assets_company_date ON fixed_assets(company_slug,acquired_on,id);

CREATE TABLE fixed_asset_movements (
    transaction_id INTEGER PRIMARY KEY REFERENCES transactions(id),
    company_slug TEXT NOT NULL,
    asset_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('depreciation','disposal')),
    date TEXT NOT NULL,
    amount INTEGER NOT NULL CHECK(amount >= 0),
    FOREIGN KEY(company_slug,asset_id) REFERENCES fixed_assets(company_slug,id),
    UNIQUE(company_slug,asset_id,kind,date)
) STRICT;

CREATE INDEX fixed_asset_movements_company_date ON fixed_asset_movements(company_slug,date);

CREATE UNIQUE INDEX fixed_asset_one_disposal ON fixed_asset_movements(company_slug,asset_id)
    WHERE kind = 'disposal';

CREATE TRIGGER fixed_asset_no_update BEFORE UPDATE ON fixed_assets
BEGIN SELECT RAISE(ABORT, 'fixed asset registration is immutable'); END;

CREATE TRIGGER fixed_asset_no_delete BEFORE DELETE ON fixed_assets
BEGIN SELECT RAISE(ABORT, 'fixed asset registration is immutable'); END;

CREATE TRIGGER fixed_asset_movement_no_update BEFORE UPDATE ON fixed_asset_movements
BEGIN SELECT RAISE(ABORT, 'fixed asset movement is immutable'); END;

CREATE TRIGGER fixed_asset_movement_no_delete BEFORE DELETE ON fixed_asset_movements
BEGIN SELECT RAISE(ABORT, 'fixed asset movement is immutable'); END;

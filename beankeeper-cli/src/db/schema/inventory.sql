CREATE TABLE inventory_items (
    company_slug TEXT NOT NULL REFERENCES companies(slug),
    id TEXT NOT NULL,
    sku TEXT NOT NULL,
    stock_account TEXT NOT NULL,
    grni_account TEXT NOT NULL,
    currency TEXT NOT NULL,
    details TEXT NOT NULL CHECK(json_valid(details))
        CHECK(id IS json_extract(details,'$.id'))
        CHECK(sku IS json_extract(details,'$.terms.sku'))
        CHECK(stock_account IS json_extract(details,'$.terms.stock_account'))
        CHECK(grni_account IS json_extract(details,'$.terms.receiving_clearing'))
        CHECK(currency IS json_extract(details,'$.terms.currency')),
    receipt INTEGER NOT NULL UNIQUE REFERENCES audit_log(id),
    PRIMARY KEY(company_slug,id),
    UNIQUE(company_slug,sku),
    UNIQUE(id),
    FOREIGN KEY(company_slug,stock_account) REFERENCES accounts(company_slug,code),
    FOREIGN KEY(company_slug,grni_account) REFERENCES accounts(company_slug,code)
) STRICT;

CREATE TABLE stock_movements (
    company_slug TEXT NOT NULL,
    id TEXT NOT NULL,
    item_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('receipt','issue','customer_return','supplier_return','count_in','count_out')),
    movement_date TEXT NOT NULL,
    quantity INTEGER NOT NULL CHECK(quantity>0),
    cost_minor INTEGER NOT NULL CHECK(cost_minor>=0),
    transaction_id INTEGER UNIQUE,
    source_id TEXT,
    request_key TEXT NOT NULL,
    intent TEXT NOT NULL CHECK(json_valid(intent)),
    details TEXT NOT NULL CHECK(json_valid(details))
        CHECK(id IS json_extract(details,'$.id'))
        CHECK(item_id IS json_extract(details,'$.item'))
        CHECK(kind IS json_extract(details,'$.kind'))
        CHECK(movement_date IS json_extract(details,'$.on'))
        CHECK(cost_minor IS json_extract(details,'$.cost_minor'))
        CHECK(transaction_id IS json_extract(details,'$.posting_id'))
        CHECK(source_id IS json_extract(details,'$.source'))
        CHECK(request_key IS json_extract(details,'$.reference')),
    receipt INTEGER NOT NULL UNIQUE REFERENCES audit_log(id),
    PRIMARY KEY(company_slug,id),
    UNIQUE(company_slug,request_key),
    FOREIGN KEY(company_slug,item_id) REFERENCES inventory_items(company_slug,id),
    FOREIGN KEY(company_slug,transaction_id) REFERENCES transactions(company_slug,id),
    FOREIGN KEY(company_slug,source_id) REFERENCES stock_movements(company_slug,id),
    CHECK((transaction_id IS NOT NULL)=(cost_minor>0))
) STRICT;
CREATE INDEX stock_movements_item_date ON stock_movements(company_slug,item_id,movement_date,receipt);
CREATE INDEX stock_movements_source ON stock_movements(company_slug,source_id);

CREATE TABLE stock_lots (
    company_slug TEXT NOT NULL,
    id TEXT NOT NULL,
    movement_id TEXT NOT NULL,
    quantity INTEGER NOT NULL CHECK(quantity>0),
    cost_minor INTEGER NOT NULL CHECK(cost_minor>=0),
    PRIMARY KEY(company_slug,id),
    UNIQUE(company_slug,movement_id),
    FOREIGN KEY(company_slug,movement_id) REFERENCES stock_movements(company_slug,id)
) STRICT;

CREATE TABLE stock_partitions (
    company_slug TEXT NOT NULL,
    movement_id TEXT NOT NULL,
    lot_id TEXT NOT NULL,
    mode TEXT NOT NULL CHECK(mode IN ('consume','recover')),
    quantity INTEGER NOT NULL CHECK(quantity>0),
    cost_minor INTEGER NOT NULL CHECK(cost_minor>=0),
    PRIMARY KEY(company_slug,movement_id,lot_id),
    FOREIGN KEY(company_slug,movement_id) REFERENCES stock_movements(company_slug,id),
    FOREIGN KEY(company_slug,lot_id) REFERENCES stock_lots(company_slug,id)
) STRICT;
CREATE INDEX stock_partitions_lot ON stock_partitions(company_slug,lot_id,mode);

CREATE TABLE stock_document_links (
    company_slug TEXT NOT NULL,
    movement_id TEXT NOT NULL,
    document_id TEXT NOT NULL,
    line_number INTEGER NOT NULL CHECK(line_number BETWEEN 1 AND 1000),
    kind TEXT NOT NULL CHECK(kind IN ('bill','invoice')),
    linked_on TEXT NOT NULL,
    details TEXT NOT NULL CHECK(json_valid(details))
       CHECK(movement_id IS json_extract(details,'$.movement'))
       CHECK(document_id IS json_extract(details,'$.document'))
       CHECK(line_number IS json_extract(details,'$.line'))
       CHECK(kind IS json_extract(details,'$.kind'))
       CHECK(linked_on IS json_extract(details,'$.on')),
    receipt INTEGER NOT NULL UNIQUE REFERENCES audit_log(id),
    PRIMARY KEY(company_slug,movement_id),
    FOREIGN KEY(company_slug,movement_id) REFERENCES stock_movements(company_slug,id),
    FOREIGN KEY(company_slug,document_id) REFERENCES book_documents(company_slug,id)
) STRICT;
CREATE INDEX stock_links_document_line ON stock_document_links(company_slug,document_id,line_number);
CREATE UNIQUE INDEX stock_bill_line_once ON stock_document_links(company_slug,document_id,line_number) WHERE kind='bill';

CREATE TRIGGER inventory_items_immutable_update BEFORE UPDATE ON inventory_items
BEGIN SELECT RAISE(ABORT,'inventory history is immutable'); END;

CREATE TRIGGER inventory_items_immutable_delete BEFORE DELETE ON inventory_items
BEGIN SELECT RAISE(ABORT,'inventory history is immutable'); END;

CREATE TRIGGER stock_movements_immutable_update BEFORE UPDATE ON stock_movements
BEGIN SELECT RAISE(ABORT,'inventory history is immutable'); END;

CREATE TRIGGER stock_movements_immutable_delete BEFORE DELETE ON stock_movements
BEGIN SELECT RAISE(ABORT,'inventory history is immutable'); END;

CREATE TRIGGER stock_lots_immutable_update BEFORE UPDATE ON stock_lots
BEGIN SELECT RAISE(ABORT,'inventory history is immutable'); END;

CREATE TRIGGER stock_lots_immutable_delete BEFORE DELETE ON stock_lots
BEGIN SELECT RAISE(ABORT,'inventory history is immutable'); END;

CREATE TRIGGER stock_partitions_immutable_update BEFORE UPDATE ON stock_partitions
BEGIN SELECT RAISE(ABORT,'inventory history is immutable'); END;

CREATE TRIGGER stock_partitions_immutable_delete BEFORE DELETE ON stock_partitions
BEGIN SELECT RAISE(ABORT,'inventory history is immutable'); END;

CREATE TRIGGER stock_document_links_immutable_update BEFORE UPDATE ON stock_document_links
BEGIN SELECT RAISE(ABORT,'inventory history is immutable'); END;

CREATE TRIGGER stock_document_links_immutable_delete BEFORE DELETE ON stock_document_links
BEGIN SELECT RAISE(ABORT,'inventory history is immutable'); END;

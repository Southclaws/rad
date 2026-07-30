#![allow(dead_code)]

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::json;

use super::benchmark::{self, LoadStats, Manifest, QueryCase, TableData};
use super::http_process::RadProcess;
use super::s3::TestResult;

pub const FIXTURE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/benchmarks/commerce");
const GENERATOR_VERSION: &str = "commerce-dataset-v1";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
pub struct Counts {
    pub customers: usize,
    pub products: usize,
    pub orders: usize,
    pub order_items: usize,
}

impl Counts {
    pub const LATENCY: Self = Self {
        customers: 16,
        products: 12,
        orders: 64,
        order_items: 192,
    };

    fn validate(self) -> TestResult {
        if self.customers == 0 || self.products == 0 {
            return Err("commerce dataset requires customers and products".into());
        }
        if self.orders < self.customers {
            return Err("commerce dataset requires at least one order per customer".into());
        }
        if self.order_items < self.orders || self.order_items > self.orders * self.products {
            return Err(
                "commerce dataset order_items must be between orders and orders*products".into(),
            );
        }
        Ok(())
    }
}

pub fn load_benchmark() -> TestResult<Manifest> {
    benchmark::load_manifest(&root())
}

pub fn counts(benchmark: &Manifest) -> TestResult<Counts> {
    let rows = |name: &str| {
        benchmark
            .tables
            .iter()
            .find(|table| table.name == name)
            .map(|table| table.rows)
            .ok_or_else(|| format!("benchmark omitted table {name:?}"))
    };
    let counts = Counts {
        customers: rows("customers")?,
        products: rows("products")?,
        orders: rows("orders")?,
        order_items: rows("order_items")?,
    };
    counts.validate()?;
    Ok(counts)
}

pub fn schema(benchmark: &Manifest) -> TestResult<String> {
    benchmark::schema(&root(), benchmark)
}

pub fn queries(benchmark: &Manifest) -> TestResult<Vec<QueryCase>> {
    benchmark::queries(&root(), benchmark)
}

pub fn fixture_hash(benchmark: &Manifest) -> TestResult<String> {
    benchmark::fixture_hash(&root(), GENERATOR_VERSION, benchmark)
}

pub async fn load_dataset(
    server: &RadProcess,
    counts: Counts,
    batch_rows: usize,
) -> TestResult<LoadStats> {
    counts.validate()?;
    benchmark::load_tables(server, dataset(counts), batch_rows).await
}

fn dataset(counts: Counts) -> Vec<TableData> {
    let text = |name: &str| json!({ "name": name, "type": "text" });
    let int64 = |name: &str| json!({ "name": name, "type": "int64" });
    let boolean = |name: &str| json!({ "name": name, "type": "bool" });
    let customers = (0..counts.customers)
        .map(|index| {
            vec![
                json!(format!("customer-{index:04}")),
                json!(["north", "south", "east", "west"][index % 4]),
                json!(["standard", "plus", "enterprise"][index % 3]),
                json!(if index % 10 != 0 { "true" } else { "false" }),
            ]
        })
        .collect();
    let products = (0..counts.products)
        .map(|index| {
            vec![
                json!(format!("product-{index:04}")),
                json!(format!("category-{:02}", index % 10)),
                json!((100 + (index % 50) * 25).to_string()),
                json!(if index % 20 != 0 { "true" } else { "false" }),
            ]
        })
        .collect();
    let orders = (0..counts.orders)
        .map(|index| {
            let status = match index % 10 {
                0..=5 => "paid",
                6..=7 => "pending",
                8 => "shipped",
                _ => "cancelled",
            };
            vec![
                json!(format!("order-{index:06}")),
                json!(format!("customer-{:04}", index % counts.customers)),
                json!(status),
                json!((1_700_000_000_u64 + index as u64).to_string()),
                json!((1_000 + (index % 100) * 37).to_string()),
            ]
        })
        .collect();
    let order_items = (0..counts.order_items)
        .map(|index| {
            let order = index % counts.orders;
            let item = index / counts.orders;
            let product = (order * 7 + item * 13) % counts.products;
            vec![
                json!(format!("order-{order:06}")),
                json!(format!("product-{product:04}")),
                json!((1 + (index % 4)).to_string()),
                json!((100 + (product % 50) * 25).to_string()),
            ]
        })
        .collect();

    vec![
        TableData {
            name: "customers",
            columns: vec![
                text("customer_id"),
                text("customer_region"),
                text("customer_tier"),
                boolean("customer_active"),
            ],
            rows: customers,
        },
        TableData {
            name: "products",
            columns: vec![
                text("product_id"),
                text("product_category"),
                int64("product_price"),
                boolean("product_active"),
            ],
            rows: products,
        },
        TableData {
            name: "orders",
            columns: vec![
                text("order_id"),
                text("order_customer_id"),
                text("order_status"),
                int64("order_created_at"),
                int64("order_total"),
            ],
            rows: orders,
        },
        TableData {
            name: "order_items",
            columns: vec![
                text("item_order_id"),
                text("item_product_id"),
                int64("item_quantity"),
                int64("item_unit_price"),
            ],
            rows: order_items,
        },
    ]
}

fn root() -> PathBuf {
    Path::new(FIXTURE_ROOT).to_owned()
}

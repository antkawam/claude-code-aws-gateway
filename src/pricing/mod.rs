use std::collections::HashMap;

use anyhow::Result;
use chrono::Utc;
use serde::Serialize;
use tracing::{info, warn};

use crate::db::model_pricing::ModelPricing;

// ---------------------------------------------------------------------------
// Public helpers
// ---------------------------------------------------------------------------

/// Strip ` (Amazon Bedrock Edition)` suffix, lowercase, replace spaces with
/// dashes, replace periods with dashes.
///
/// Example: `"Claude Opus 4.7 (Amazon Bedrock Edition)"` → `"claude-opus-4-7"`
pub fn normalize_service_name(service_name: &str) -> String {
    let s = service_name
        .trim_end_matches(" (Amazon Bedrock Edition)")
        .trim();
    s.to_lowercase().replace([' ', '.'], "-")
}

// ---------------------------------------------------------------------------
// CSV parser
// ---------------------------------------------------------------------------

/// Intermediate accumulator while parsing rows for a single service/model.
#[derive(Debug, Default)]
struct ModelAccumulator {
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
    /// SKU from the first Input Tokens row.
    sku: Option<String>,
    /// Flag set when any row failed to parse its price.
    parse_error: bool,
}

/// Classify the last pipe-delimited segment of a `PriceDescription` field.
/// Returns `None` if the dimension should be skipped.
fn classify_description(price_description: &str) -> Option<&'static str> {
    let last = price_description
        .split('|')
        .next_back()
        .unwrap_or(price_description)
        .trim();

    match last {
        "Input Tokens - Standard, Global" => Some("input"),
        "Output Tokens - Standard, Global" => Some("output"),
        "Cache Read Tokens - Standard, Global" => Some("cache_read"),
        "Cache Write Tokens - Standard, Global" => Some("cache_write"),
        // 1h TTL and all other variants are skipped
        _ => None,
    }
}

/// Parse AWS Bedrock Foundation Models price list CSV.
///
/// Returns one [`ModelPricing`] per model that has **all 4 global dimensions**:
/// - `Input Tokens - Standard, Global`
/// - `Output Tokens - Standard, Global`
/// - `Cache Read Tokens - Standard, Global`
/// - `Cache Write Tokens - Standard, Global`  (the 5m one — not `(1h TTL)`)
///
/// Regional variants and 1h TTL cache writes are skipped.  Models with any
/// missing dimension or an unparseable `PricePerUnit` are also skipped.
///
/// `source` is always `"price_list_api"` on every returned row.
/// `aws_sku` is the SKU from the first `Input Tokens` row seen for the model.
pub fn parse_price_list_csv(csv: &str) -> Result<Vec<ModelPricing>> {
    // Column indices (0-based).  The test header is:
    // SKU | OfferTermCode | RateCode | TermType | PriceDescription |
    // EffectiveDate | StartingRange | EndingRange | Unit | PricePerUnit |
    // Currency | Location | Location Type | usageType | operation |
    // Region Code | serviceName
    const IDX_SKU: usize = 0;
    const IDX_PRICE_DESC: usize = 4;
    const IDX_PRICE_PER_UNIT: usize = 9;
    const IDX_SERVICE_NAME: usize = 16;

    let mut reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_reader(csv.as_bytes());

    // Keyed by normalized model prefix.
    let mut accumulators: HashMap<String, ModelAccumulator> = HashMap::new();

    for result in reader.records() {
        let record = match result {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "Skipping malformed CSV record");
                continue;
            }
        };

        let service_name = match record.get(IDX_SERVICE_NAME) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let model_prefix = normalize_service_name(&service_name);

        let price_desc = match record.get(IDX_PRICE_DESC) {
            Some(s) => s.to_string(),
            None => continue,
        };

        let dimension = match classify_description(&price_desc) {
            Some(d) => d,
            None => continue, // regional or 1h TTL or unknown — skip
        };

        let sku = record.get(IDX_SKU).unwrap_or("").to_string();
        let price_str = record.get(IDX_PRICE_PER_UNIT).unwrap_or("").to_string();

        let acc = accumulators.entry(model_prefix).or_default();

        // If this model already had a parse error, keep accumulating so that
        // we still emit nothing for it at the end.
        let price: f64 = match price_str.parse() {
            Ok(v) => v,
            Err(_) => {
                warn!(
                    sku = %sku,
                    price_str = %price_str,
                    "Unparseable PricePerUnit — marking model as errored"
                );
                acc.parse_error = true;
                continue;
            }
        };

        match dimension {
            "input" => {
                if acc.sku.is_none() {
                    acc.sku = Some(sku);
                }
                acc.input = Some(price);
            }
            "output" => {
                acc.output = Some(price);
            }
            "cache_read" => {
                acc.cache_read = Some(price);
            }
            "cache_write" => {
                acc.cache_write = Some(price);
            }
            _ => unreachable!(),
        }
    }

    let now = Utc::now();
    let mut results = Vec::with_capacity(accumulators.len());

    for (model_prefix, acc) in accumulators {
        if acc.parse_error {
            warn!(model_prefix = %model_prefix, "Skipping model due to parse error");
            continue;
        }
        match (acc.input, acc.output, acc.cache_read, acc.cache_write) {
            (Some(input), Some(output), Some(cache_read), Some(cache_write)) => {
                results.push(ModelPricing {
                    model_prefix,
                    input_rate: input,
                    output_rate: output,
                    cache_read_rate: cache_read,
                    cache_write_rate: cache_write,
                    source: "price_list_api".to_string(),
                    aws_sku: acc.sku,
                    updated_at: now,
                });
            }
            _ => {
                warn!(
                    model_prefix = %model_prefix,
                    "Skipping model — missing one or more dimensions"
                );
            }
        }
    }

    Ok(results)
}

// ---------------------------------------------------------------------------
// Refresh report
// ---------------------------------------------------------------------------

/// Summary of a single price list refresh run.
#[derive(Debug, Default, Serialize)]
pub struct RefreshReport {
    /// Rows written (new or updated).
    ///
    /// NOTE (Phase 1 simplification): To distinguish inserted vs updated we
    /// would need a pre-check SELECT per row.  For now every successful write
    /// is counted here and `inserted` is always 0.
    pub inserted: u32,
    pub updated: u32,
    pub unchanged: u32,
    pub skipped_manual: u32,
    pub errors: Vec<String>,
}

// ---------------------------------------------------------------------------
// refresh_pricing — AWS I/O layer
// ---------------------------------------------------------------------------

/// Fetch the latest us-east-1 Bedrock foundation models price list, parse it,
/// and upsert into the database.
///
/// Best-effort: individual row errors are logged and collected in
/// [`RefreshReport::errors`].  Never panics.
pub async fn refresh_pricing(
    pool: &sqlx::PgPool,
    pricing_client: &aws_sdk_pricing::Client,
    http_client: &reqwest::Client,
) -> anyhow::Result<RefreshReport> {
    let mut report = RefreshReport::default();

    // -----------------------------------------------------------------------
    // Step 1: list price lists for AmazonBedrockFoundationModels in us-east-1
    // -----------------------------------------------------------------------
    let now_secs = Utc::now().timestamp();
    let effective_date = aws_sdk_pricing::primitives::DateTime::from_secs(now_secs);

    let list_resp = pricing_client
        .list_price_lists()
        .service_code("AmazonBedrockFoundationModels")
        .currency_code("USD")
        .effective_date(effective_date)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("ListPriceLists failed: {:?}", e))?;

    let price_lists = list_resp.price_lists.unwrap_or_default();

    // -----------------------------------------------------------------------
    // Step 2: Find the us-east-1 price list ARN
    // -----------------------------------------------------------------------
    let arn = price_lists
        .iter()
        .find(|pl| pl.region_code.as_deref() == Some("us-east-1"))
        .and_then(|pl| pl.price_list_arn.as_deref())
        .ok_or_else(|| anyhow::anyhow!("No us-east-1 Bedrock price list found"))?
        .to_string();

    info!(arn = %arn, "Found us-east-1 Bedrock price list");

    // -----------------------------------------------------------------------
    // Step 3: Get the CSV download URL
    // -----------------------------------------------------------------------
    let url_resp = pricing_client
        .get_price_list_file_url()
        .price_list_arn(&arn)
        .file_format("csv")
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("GetPriceListFileUrl failed: {:?}", e))?;

    let download_url = url_resp
        .url
        .ok_or_else(|| anyhow::anyhow!("GetPriceListFileUrl returned no URL"))?;

    info!(url = %download_url, "Downloading price list CSV");

    // -----------------------------------------------------------------------
    // Step 4: HTTP GET the CSV
    // -----------------------------------------------------------------------
    let body = http_client
        .get(&download_url)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("HTTP GET price list failed: {:?}", e))?
        .text()
        .await
        .map_err(|e| anyhow::anyhow!("Reading price list body failed: {:?}", e))?;

    // -----------------------------------------------------------------------
    // Step 5: Parse CSV
    // -----------------------------------------------------------------------
    let rows = match parse_price_list_csv(&body) {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("parse_price_list_csv failed: {e:?}");
            warn!("{}", msg);
            report.errors.push(msg);
            return Ok(report);
        }
    };

    info!(count = rows.len(), "Parsed price list rows");

    // -----------------------------------------------------------------------
    // Step 6: Upsert into the DB
    // -----------------------------------------------------------------------
    for row in &rows {
        match crate::db::model_pricing::upsert_from_api(pool, row).await {
            Ok(true) => {
                // Phase 1 simplification: count all writes as "updated"
                report.updated += 1;
            }
            Ok(false) => {
                report.skipped_manual += 1;
            }
            Err(e) => {
                let msg = format!("upsert_from_api failed for {}: {:?}", row.model_prefix, e);
                warn!("{}", msg);
                report.errors.push(msg);
            }
        }
    }

    info!(
        updated = report.updated,
        skipped_manual = report.skipped_manual,
        errors = report.errors.len(),
        "Price list refresh complete"
    );

    Ok(report)
}

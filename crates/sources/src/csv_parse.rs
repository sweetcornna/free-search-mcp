//! CSV/TSV response parsing (`response = "csv"`).

use fsm_core::FsmError;
use fsm_core::manifest::CsvOpts;
use serde_json::{Map, Value};

/// Parse CSV/TSV text into a JSON array of row objects (`header = true`) or
/// string arrays (`header = false`).
pub fn csv_to_json(text: &str, opts: Option<&CsvOpts>, source_id: &str) -> Result<Value, FsmError> {
    let has_headers = opts.is_none_or(|c| c.header);
    let delimiter = match opts {
        Some(c) => {
            if c.delimiter.len() != 1 || !c.delimiter.is_ascii() {
                return Err(FsmError::invalid(format!(
                    "{source_id}: CSV delimiter {:?} must be a single ASCII byte",
                    c.delimiter
                )));
            }
            c.delimiter.as_bytes()[0]
        }
        None => b',',
    };

    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(has_headers)
        .delimiter(delimiter)
        .flexible(true)
        .from_reader(text.as_bytes());

    let mut rows = Vec::new();
    if has_headers {
        let headers: Vec<String> = rdr
            .headers()
            .map_err(|e| FsmError::upstream(source_id, None, format!("CSV headers error: {e}")))?
            .iter()
            .map(str::to_owned)
            .collect();
        for rec in rdr.records() {
            let rec = rec.map_err(|e| {
                FsmError::upstream(source_id, None, format!("CSV parse error: {e}"))
            })?;
            let mut obj = Map::with_capacity(headers.len());
            for (i, h) in headers.iter().enumerate() {
                let val = rec.get(i).unwrap_or("").to_owned();
                obj.insert(h.clone(), Value::String(val));
            }
            rows.push(Value::Object(obj));
        }
    } else {
        for rec in rdr.records() {
            let rec = rec.map_err(|e| {
                FsmError::upstream(source_id, None, format!("CSV parse error: {e}"))
            })?;
            let cols: Vec<Value> = rec.iter().map(|s| Value::String(s.to_owned())).collect();
            rows.push(Value::Array(cols));
        }
    }
    Ok(Value::Array(rows))
}

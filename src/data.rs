use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rand::RngExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};

/// External fixture source used by dynamic scenarios.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataSource {
    /// Fixture format.
    #[serde(rename = "type")]
    pub kind: DataSourceKind,
    /// Path to the fixture file.
    pub path: String,
    /// Optional JSON root path. Ignored for CSV sources.
    #[serde(default)]
    pub root: Option<String>,
    /// Row access strategy.
    #[serde(default)]
    pub access: DataAccessMode,
    /// Behavior when a finite access strategy runs past available rows.
    #[serde(default)]
    pub exhaustion: DataExhaustion,
    /// Optional deterministic seed for random access.
    #[serde(default)]
    pub seed: Option<u64>,
    /// Optional CSV column type map. Unspecified columns default to strings.
    #[serde(default)]
    pub columns: HashMap<String, CsvColumnType>,
}

impl DataSource {
    /// Create a CSV data source.
    pub fn csv<P: Into<String>>(path: P) -> Self {
        Self {
            kind: DataSourceKind::Csv,
            path: path.into(),
            root: None,
            access: DataAccessMode::default(),
            exhaustion: DataExhaustion::default(),
            seed: None,
            columns: HashMap::new(),
        }
    }

    /// Create a JSON data source.
    pub fn json<P: Into<String>>(path: P) -> Self {
        Self {
            kind: DataSourceKind::Json,
            path: path.into(),
            root: None,
            access: DataAccessMode::default(),
            exhaustion: DataExhaustion::default(),
            seed: None,
            columns: HashMap::new(),
        }
    }

    /// Set the optional JSON root path.
    pub fn root<S: Into<String>>(mut self, root: S) -> Self {
        self.root = Some(root.into());
        self
    }

    /// Set the row access strategy.
    pub fn access(mut self, access: DataAccessMode) -> Self {
        self.access = access;
        self
    }

    /// Set exhaustion behavior for finite access strategies.
    pub fn exhaustion(mut self, exhaustion: DataExhaustion) -> Self {
        self.exhaustion = exhaustion;
        self
    }

    /// Set a deterministic random seed.
    pub fn seed(mut self, seed: u64) -> Self {
        self.seed = Some(seed);
        self
    }

    /// Set a CSV column type.
    pub fn column_type<S: Into<String>>(mut self, column: S, value_type: CsvColumnType) -> Self {
        self.columns.insert(column.into(), value_type);
        self
    }
}

/// Fixture file format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DataSourceKind {
    /// Comma-separated value file with a header row.
    Csv,
    /// JSON file containing either an object row or an array of rows.
    Json,
}

/// Row access strategy for a data source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DataAccessMode {
    /// Bind row `vu_id` for the lifetime of each VU.
    #[default]
    PerVu,
    /// Advance one shared cursor per scenario run.
    Sequential,
    /// Pick a row randomly per VU iteration.
    Random,
}

/// Exhaustion behavior for finite row access strategies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DataExhaustion {
    /// Fail when no row is available.
    #[default]
    Fail,
    /// Wrap around to the beginning.
    Wrap,
}

/// CSV value coercion type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CsvColumnType {
    /// Keep values as strings.
    #[default]
    String,
    /// Parse values as signed integers.
    Integer,
    /// Parse values as JSON numbers.
    Number,
    /// Parse values as booleans.
    #[serde(alias = "bool")]
    Boolean,
    /// Parse values as JSON literals.
    Json,
}

#[derive(Debug)]
pub(crate) struct LoadedDataSource {
    id: String,
    config: DataSource,
    rows: Vec<Arc<Value>>,
    cursor: AtomicU64,
}

impl LoadedDataSource {
    fn new(id: String, config: DataSource, rows: Vec<Value>) -> Self {
        Self {
            id,
            config,
            rows: rows.into_iter().map(Arc::new).collect(),
            cursor: AtomicU64::new(0),
        }
    }

    fn row_count(&self) -> usize {
        self.rows.len()
    }

    fn row_at(&self, index: u64) -> Result<Arc<Value>> {
        let row_count = self.rows.len() as u64;
        if row_count == 0 {
            return Err(Error::config(format!(
                "data source '{}' did not load any rows",
                self.id
            )));
        }
        let index = match self.config.exhaustion {
            DataExhaustion::Fail if index >= row_count => {
                return Err(Error::validation(format!(
                    "data source '{}' exhausted at row index {index}",
                    self.id
                )));
            }
            DataExhaustion::Fail => index,
            DataExhaustion::Wrap => index % row_count,
        };
        Ok(Arc::clone(&self.rows[index as usize]))
    }

    fn bind_for_iteration(&self, vu_id: u32, iteration: u64) -> Result<Arc<Value>> {
        match self.config.access {
            DataAccessMode::PerVu => self.row_at(vu_id as u64),
            DataAccessMode::Sequential => {
                let index = self.cursor.fetch_add(1, Ordering::Relaxed);
                self.row_at(index)
            }
            DataAccessMode::Random => {
                let row_count = self.rows.len();
                if row_count == 0 {
                    return Err(Error::config(format!(
                        "data source '{}' did not load any rows",
                        self.id
                    )));
                }
                let index = match self.config.seed {
                    Some(seed) => stable_random_index(seed, &self.id, vu_id, iteration, row_count),
                    None => rand::rng().random_range(0..row_count),
                };
                Ok(Arc::clone(&self.rows[index]))
            }
        }
    }

    fn path_exists_for_every_row(&self, path: &str) -> bool {
        self.rows
            .iter()
            .all(|row| extract_relative_json_path(row.as_ref(), path).is_some())
    }
}

/// Loaded fixture rows for a scenario run.
#[derive(Debug, Default)]
pub(crate) struct LoadedDataSources {
    sources: HashMap<String, Arc<LoadedDataSource>>,
}

impl LoadedDataSources {
    pub(crate) fn load(
        sources: &HashMap<String, DataSource>,
        base_dir: &Path,
    ) -> Result<Arc<Self>> {
        let mut loaded = HashMap::new();
        for (id, source) in sources {
            validate_data_source_id(id)?;
            let rows = load_rows(id, source, base_dir)?;
            if rows.is_empty() {
                return Err(Error::config(format!(
                    "data source '{id}' must contain at least one row"
                )));
            }
            loaded.insert(
                id.clone(),
                Arc::new(LoadedDataSource::new(id.clone(), source.clone(), rows)),
            );
        }
        Ok(Arc::new(Self { sources: loaded }))
    }

    pub(crate) fn bind_iteration(
        &self,
        vu_id: u32,
        iteration: u64,
    ) -> Result<HashMap<String, Arc<Value>>> {
        let mut bound = HashMap::with_capacity(self.sources.len());
        for (id, source) in &self.sources {
            bound.insert(id.clone(), source.bind_for_iteration(vu_id, iteration)?);
        }
        Ok(bound)
    }

    pub(crate) fn row_count(&self, id: &str) -> Option<usize> {
        self.sources.get(id).map(|source| source.row_count())
    }

    pub(crate) fn has_source(&self, id: &str) -> bool {
        self.sources.contains_key(id)
    }

    pub(crate) fn path_exists_for_every_row(&self, id: &str, path: &str) -> bool {
        self.sources
            .get(id)
            .is_some_and(|source| source.path_exists_for_every_row(path))
    }
}

pub(crate) fn validate_data_source_id(id: &str) -> Result<()> {
    if id.is_empty() {
        return Err(Error::config("data source id cannot be empty"));
    }
    if id.contains('.') {
        return Err(Error::config(format!(
            "data source id '{id}' cannot contain '.' because templates use data.<source>.<path>"
        )));
    }
    if !id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err(Error::config(format!(
            "data source id '{id}' may only contain ASCII letters, digits, '_' or '-'"
        )));
    }
    Ok(())
}

fn load_rows(id: &str, source: &DataSource, base_dir: &Path) -> Result<Vec<Value>> {
    let path = resolve_source_path(base_dir, &source.path);
    match source.kind {
        DataSourceKind::Csv => load_csv_rows(id, source, &path),
        DataSourceKind::Json => load_json_rows(id, source, &path),
    }
}

fn resolve_source_path(base_dir: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    }
}

fn load_csv_rows(id: &str, source: &DataSource, path: &Path) -> Result<Vec<Value>> {
    if source.root.is_some() {
        return Err(Error::config(format!(
            "data source '{id}' is CSV and cannot set root"
        )));
    }

    let mut reader = csv::Reader::from_path(path).map_err(|e| {
        Error::config(format!(
            "failed to open CSV data source '{id}' at '{}': {e}",
            path.display()
        ))
    })?;

    let headers = reader
        .headers()
        .map_err(|e| Error::config(format!("failed to read CSV headers for '{id}': {e}")))?
        .clone();

    let mut seen = HashSet::new();
    for header in headers.iter() {
        if !seen.insert(header.to_string()) {
            return Err(Error::config(format!(
                "CSV data source '{id}' contains duplicate column '{header}'"
            )));
        }
    }

    for column in source.columns.keys() {
        if !headers.iter().any(|header| header == column) {
            return Err(Error::config(format!(
                "CSV data source '{id}' declares type for missing column '{column}'"
            )));
        }
    }

    let mut rows = Vec::new();
    for (row_index, record) in reader.records().enumerate() {
        let record = record.map_err(|e| {
            Error::config(format!(
                "failed to read CSV record {} for data source '{id}': {e}",
                row_index + 2
            ))
        })?;
        let mut object = serde_json::Map::new();
        for (column_index, header) in headers.iter().enumerate() {
            let raw = record.get(column_index).unwrap_or_default();
            let value_type = source
                .columns
                .get(header)
                .copied()
                .unwrap_or(CsvColumnType::String);
            let value = parse_csv_value(id, row_index + 2, header, raw, value_type)?;
            object.insert(header.to_string(), value);
        }
        rows.push(Value::Object(object));
    }

    Ok(rows)
}

fn parse_csv_value(
    source_id: &str,
    row_number: usize,
    column: &str,
    raw: &str,
    value_type: CsvColumnType,
) -> Result<Value> {
    match value_type {
        CsvColumnType::String => Ok(Value::String(raw.to_string())),
        CsvColumnType::Integer => {
            if raw.is_empty() {
                return Ok(Value::Null);
            }
            let value = raw.parse::<i64>().map_err(|e| {
                Error::config(format!(
                    "CSV data source '{source_id}' row {row_number} column '{column}' is not an integer: {e}"
                ))
            })?;
            Ok(Value::Number(value.into()))
        }
        CsvColumnType::Number => {
            if raw.is_empty() {
                return Ok(Value::Null);
            }
            let parsed = raw.parse::<f64>().map_err(|e| {
                Error::config(format!(
                    "CSV data source '{source_id}' row {row_number} column '{column}' is not a number: {e}"
                ))
            })?;
            let number = serde_json::Number::from_f64(parsed).ok_or_else(|| {
                Error::config(format!(
                    "CSV data source '{source_id}' row {row_number} column '{column}' is not a finite number"
                ))
            })?;
            Ok(Value::Number(number))
        }
        CsvColumnType::Boolean => {
            if raw.is_empty() {
                return Ok(Value::Null);
            }
            match raw.to_ascii_lowercase().as_str() {
                "true" | "1" => Ok(Value::Bool(true)),
                "false" | "0" => Ok(Value::Bool(false)),
                _ => Err(Error::config(format!(
                    "CSV data source '{source_id}' row {row_number} column '{column}' is not a boolean"
                ))),
            }
        }
        CsvColumnType::Json => {
            if raw.is_empty() {
                return Ok(Value::Null);
            }
            serde_json::from_str(raw).map_err(|e| {
                Error::config(format!(
                    "CSV data source '{source_id}' row {row_number} column '{column}' is not valid JSON: {e}"
                ))
            })
        }
    }
}

fn load_json_rows(id: &str, source: &DataSource, path: &Path) -> Result<Vec<Value>> {
    if !source.columns.is_empty() {
        return Err(Error::config(format!(
            "data source '{id}' is JSON and cannot set CSV columns"
        )));
    }

    let content = fs::read_to_string(path).map_err(|e| {
        Error::config(format!(
            "failed to read JSON data source '{id}' at '{}': {e}",
            path.display()
        ))
    })?;
    let value: Value = serde_json::from_str(&content).map_err(|e| {
        Error::config(format!(
            "failed to parse JSON data source '{id}' at '{}': {e}",
            path.display()
        ))
    })?;

    let root = source.root.as_deref().unwrap_or("$");
    validate_json_path(root)?;
    let selected = extract_json_path(&value, root).ok_or_else(|| {
        Error::config(format!(
            "JSON data source '{id}' root path '{root}' did not match"
        ))
    })?;

    match selected {
        Value::Array(rows) => Ok(rows),
        Value::Object(_) => Ok(vec![selected]),
        other => Err(Error::config(format!(
            "JSON data source '{id}' root path '{root}' selected {}, expected object or array",
            json_type_name(&other)
        ))),
    }
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Validate the documented JSON-path subset: `$`, `$.field`, nested dot
/// fields, and `[index]`.
pub(crate) fn validate_json_path(path: &str) -> Result<()> {
    parse_json_path(path).map(|_| ())
}

/// Validate the relative path used by `{{data.<source>.<path>}}`.
pub(crate) fn validate_relative_json_path(path: &str) -> Result<()> {
    if path == "$" || path.starts_with("$.") || path.starts_with("$[") {
        return validate_json_path(path);
    }
    if path.is_empty() {
        return Err(Error::config("data path cannot be empty"));
    }
    validate_json_path(&format!("$.{path}"))
}

/// Extract using the documented JSON-path subset.
pub(crate) fn extract_json_path(value: &Value, path: &str) -> Option<Value> {
    let tokens = parse_json_path(path).ok()?;
    extract_json_path_tokens(value, &tokens)
}

/// Extract using pre-parsed JSON-path tokens (hot path for extractors).
pub(crate) fn extract_json_path_tokens(value: &Value, tokens: &[JsonPathToken]) -> Option<Value> {
    extract_tokens(value, tokens).cloned()
}

/// Extract from a data row using a relative path.
pub(crate) fn extract_relative_json_path(value: &Value, path: &str) -> Option<Value> {
    if path == "$" || path.starts_with("$.") || path.starts_with("$[") {
        return extract_json_path(value, path);
    }
    let tokens = parse_json_path(&format!("$.{path}")).ok()?;
    extract_json_path_tokens(value, &tokens)
}

fn extract_tokens<'a>(value: &'a Value, tokens: &[JsonPathToken]) -> Option<&'a Value> {
    let mut current = value;
    for token in tokens {
        match token {
            JsonPathToken::Field(field) => {
                current = current.get(field)?;
            }
            JsonPathToken::Index(index) => {
                current = current.as_array()?.get(*index)?;
            }
        }
    }
    Some(current)
}

/// One segment of the supported JSON-path subset (`$.a[0].b`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum JsonPathToken {
    /// Object field access.
    Field(String),
    /// Array index access.
    Index(usize),
}

/// Parse the documented JSON-path subset into tokens.
pub(crate) fn parse_json_path(path: &str) -> Result<Vec<JsonPathToken>> {
    if path.is_empty() {
        return Err(Error::config("json path cannot be empty"));
    }
    if !path.starts_with('$') {
        return Err(Error::config(format!(
            "json path '{path}' must start with '$'"
        )));
    }
    if path == "$" {
        return Ok(Vec::new());
    }

    let bytes = path.as_bytes();
    let mut index = 1;
    let mut tokens = Vec::new();
    while index < bytes.len() {
        match bytes[index] {
            b'.' => {
                index += 1;
                let start = index;
                while index < bytes.len() && !matches!(bytes[index], b'.' | b'[' | b']') {
                    index += 1;
                }
                if start == index {
                    return Err(Error::config(format!(
                        "json path '{path}' contains an empty field"
                    )));
                }
                let field = &path[start..index];
                if field.contains('*')
                    || field.contains('?')
                    || field.contains('"')
                    || field.contains('\'')
                {
                    return Err(Error::config(format!(
                        "json path '{path}' uses unsupported field syntax"
                    )));
                }
                tokens.push(JsonPathToken::Field(field.to_string()));
            }
            b'[' => {
                index += 1;
                let start = index;
                while index < bytes.len() && bytes[index] != b']' {
                    index += 1;
                }
                if index >= bytes.len() {
                    return Err(Error::config(format!(
                        "json path '{path}' has an unclosed index"
                    )));
                }
                let raw_index = &path[start..index];
                if raw_index.is_empty() || !raw_index.chars().all(|ch| ch.is_ascii_digit()) {
                    return Err(Error::config(format!(
                        "json path '{path}' only supports non-negative numeric indexes"
                    )));
                }
                let parsed = raw_index.parse::<usize>().map_err(|e| {
                    Error::config(format!("json path '{path}' has an invalid index: {e}"))
                })?;
                tokens.push(JsonPathToken::Index(parsed));
                index += 1;
            }
            b']' => {
                return Err(Error::config(format!(
                    "json path '{path}' has an unopened index"
                )));
            }
            _ => {
                return Err(Error::config(format!(
                    "json path '{path}' expected '.' or '[' after '$'/segment"
                )));
            }
        }
    }

    Ok(tokens)
}

fn stable_random_index(
    seed: u64,
    source_id: &str,
    vu_id: u32,
    iteration: u64,
    len: usize,
) -> usize {
    let mut state = splitmix64(seed ^ 0x9e37_79b9_7f4a_7c15);
    for byte in source_id.bytes() {
        state = splitmix64(state ^ byte as u64);
    }
    state = splitmix64(state ^ vu_id as u64);
    state = splitmix64(state ^ iteration);
    (state % len as u64) as usize
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

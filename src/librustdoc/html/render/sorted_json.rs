use serde_json::Value;
use serde::{Serialize, Deserialize};
use std::fmt;
use itertools::Itertools as _;

/// Prerenedered json.
///
/// Arrays are sorted by their stringified entries, and objects are sorted by their stringified
/// keys.
///
/// Must use serde_json with the preserve_order feature.
///
/// Both the Display and serde_json::to_string implementations write the serialized json
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(from = "Value")]
#[serde(into = "Value")]
pub struct SortedJson(String);

impl SortedJson {
    /// If you pass in an array, it will not be sorted.
    pub fn serialize<T: Serialize>(item: T) -> Self {
        SortedJson(serde_json::to_string(&item).unwrap())
    }

    pub fn array<I: IntoIterator<Item=SortedJson>>(items: I) -> Self {
        let items = items.into_iter().sorted_unstable().format(",");
        SortedJson(format!("[{}]", items))
    }

    pub fn array_unsorted<I: IntoIterator<Item=SortedJson>>(items: I) -> Self {
        SortedJson(format!("[{}]", items.into_iter().format(",")))
    }

    pub fn object<I: IntoIterator<Item=(SortedJson, SortedJson)>>(items: I) -> Self {
        let items = items
            .into_iter()
            .sorted_unstable_by(|a, b| a.0.cmp(&b.0))
            .format_with(",", |(k, v), f| f(&format_args!("{k}:{v}")));
        SortedJson(format!("{{{}}}", items))
    }
}

impl fmt::Display for SortedJson {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<Value> for SortedJson {
    fn from(value: Value) -> Self {
        SortedJson(serde_json::to_string(&value).unwrap())
    }
}

impl From<SortedJson> for Value {
    fn from(json: SortedJson) -> Self {
        serde_json::from_str(&json.0).unwrap()
    }
}

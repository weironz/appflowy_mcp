//! A rich-text delta operation (`{"insert": ..., "attributes": {...}}`),
//! shared by the markdown parser (output) and the collab text map (input).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeltaOp {
    pub insert: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attributes: Option<serde_json::Map<String, serde_json::Value>>,
}

impl DeltaOp {
    pub fn plain(insert: impl Into<String>) -> Self {
        Self {
            insert: insert.into(),
            attributes: None,
        }
    }

    pub fn with_attributes(
        insert: impl Into<String>,
        attributes: serde_json::Map<String, serde_json::Value>,
    ) -> Self {
        Self {
            insert: insert.into(),
            attributes: Some(attributes),
        }
    }
}

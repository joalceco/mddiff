//! Private inventory of structured Atlassian identities, never a free-text PII classifier.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::AppError;

#[derive(Clone, Default, Deserialize, Serialize, PartialEq, Debug)]
#[serde(deny_unknown_fields)]
pub struct JiraData {
    pub accounts: BTreeMap<String, u64>,
    pub unclassified: bool,
}

impl JiraData {
    pub fn from_adf(document: &Value, retrieved_at: u64) -> Result<Self, AppError> {
        let mut result = Self::default();
        result.visit(document, retrieved_at, 0, &mut 0)?;
        Ok(result)
    }

    fn visit(
        &mut self,
        value: &Value,
        at: u64,
        depth: usize,
        nodes: &mut usize,
    ) -> Result<(), AppError> {
        *nodes += 1;
        if depth > 128 || *nodes > 262_144 {
            return Err(AppError::bad_request(
                "El inventario ADF supera los límites de análisis.",
            ));
        }
        match value {
            Value::Object(object) => {
                if object.get("type").and_then(Value::as_str) == Some("mention") {
                    let attrs = &value["attrs"];
                    // SPECIAL identifies collections; APP identifies apps rather than people.
                    if !matches!(attrs["userType"].as_str(), Some("SPECIAL" | "APP")) {
                        if attrs["userType"].as_str() != Some("DEFAULT") {
                            self.unclassified = true;
                        }
                        self.account(&attrs["id"], at);
                    }
                }
                if let Some(id) = object.get("accountId") {
                    self.account(id, at);
                }
                for child in object.values() {
                    self.visit(child, at, depth + 1, nodes)?;
                }
            }
            Value::Array(children) => {
                for child in children {
                    self.visit(child, at, depth + 1, nodes)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn account(&mut self, value: &Value, at: u64) {
        match value.as_str() {
            Some("unknown") => self.unclassified = true,
            Some(id) if valid_account_id(id) => {
                self.accounts.entry(id.to_owned()).or_insert(at);
            }
            _ => self.unclassified = true,
        }
    }

    pub(crate) fn merge(&mut self, other: &Self) {
        self.unclassified |= other.unclassified;
        for (id, at) in &other.accounts {
            self.accounts
                .entry(id.clone())
                .and_modify(|old| *old = (*old).min(*at))
                .or_insert(*at);
        }
    }

    pub(crate) fn valid(&self) -> bool {
        self.accounts.len() <= 32_768
            && self
                .accounts
                .iter()
                .all(|(id, at)| valid_account_id(id) && *at > 0)
    }
}

fn valid_account_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id != "unknown"
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-:".contains(&b))
}

#[derive(Clone, Default, Deserialize, Serialize, PartialEq, Debug)]
#[serde(deny_unknown_fields)]
pub struct WorkPrivacy {
    pub base: JiraData,
    pub candidate: JiraData,
}

impl WorkPrivacy {
    pub(crate) fn candidate_copy(&self) -> JiraData {
        let mut result = self.base.clone();
        result.merge(&self.candidate);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn identities_inside_unsupported_nodes_are_indexed_without_profile_text() {
        let adf = json!({"type":"doc","content":[{"type":"futureNode","content":[
            {"type":"mention","attrs":{"id":"synthetic:alice","text":"@Private Name"}},
            {"type":"mention","attrs":{"id":"synthetic:alice","text":"@Private Name"}},
            {"type":"mention","attrs":{"id":"all","userType":"SPECIAL"}},
            {"type":"mention","attrs":{"id":"app-id","userType":"APP"}},
            {"type":"mention","attrs":{"id":"unknown"}},
            {"type":"mention","attrs":{"id":"invalid id"}},
            {"attrs":{"accountId":"synthetic:bob","displayName":"Private Name"}}
        ]}]});
        let mut result = JiraData::from_adf(&adf, 100).unwrap();
        assert_eq!(
            result.accounts,
            BTreeMap::from([
                ("synthetic:alice".into(), 100),
                ("synthetic:bob".into(), 100)
            ])
        );
        assert!(result.unclassified);
        assert!(
            !serde_json::to_string(&result)
                .unwrap()
                .contains("Private Name")
        );
        result.merge(&JiraData::from_adf(&adf, 200).unwrap());
        assert_eq!(result.accounts["synthetic:alice"], 100);
    }

    #[test]
    fn free_text_is_not_guessed_and_analysis_is_bounded() {
        let text = json!({"type":"text","text":"Email alice@example.invalid and @Alice"});
        assert_eq!(JiraData::from_adf(&text, 100).unwrap(), JiraData::default());
        let mut deep = Value::Null;
        for _ in 0..130 {
            deep = json!([deep]);
        }
        assert!(JiraData::from_adf(&deep, 100).is_err());
    }

    #[test]
    fn ambiguous_mentions_cannot_claim_a_complete_inventory() {
        for attrs in [
            json!({"id":"group-name"}),
            json!({"id":null,"userType":"DEFAULT"}),
            json!({"id":"unknown","userType":"DEFAULT"}),
        ] {
            let data = JiraData::from_adf(&json!({"type":"mention","attrs":attrs}), 100).unwrap();
            assert!(data.unclassified);
        }
        let data = JiraData::from_adf(
            &json!({"type":"mention","attrs":{"id":"synthetic:alice","userType":"DEFAULT"}}),
            100,
        )
        .unwrap();
        assert!(!data.unclassified);
    }
}

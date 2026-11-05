//! Private inventory of structured Atlassian identities, never a free-text PII classifier.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::AppError;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AccountKind {
    Person,
    App,
}

impl AccountKind {
    pub fn from_api(value: &Value) -> Option<Self> {
        match value.as_str() {
            Some("atlassian" | "customer") => Some(Self::Person),
            Some("app") => Some(Self::App),
            _ => None,
        }
    }
}

#[derive(Clone, Default, Deserialize, Serialize, PartialEq, Debug)]
#[serde(deny_unknown_fields)]
pub struct JiraData {
    pub accounts: BTreeMap<String, u64>,
    pub unclassified: bool,
}

impl JiraData {
    pub fn from_adf(document: &Value, retrieved_at: u64) -> Result<Self, AppError> {
        Ok(Self::inspect_adf(document, retrieved_at, &BTreeMap::new())?.0)
    }

    pub fn inspect_adf(
        document: &Value,
        retrieved_at: u64,
        users: &BTreeMap<String, AccountKind>,
    ) -> Result<(Self, BTreeSet<String>), AppError> {
        let mut result = Self::default();
        let mut unresolved = BTreeSet::new();
        result.visit(document, retrieved_at, 0, &mut 0, users, &mut unresolved)?;
        Ok((result, unresolved))
    }

    fn visit(
        &mut self,
        value: &Value,
        at: u64,
        depth: usize,
        nodes: &mut usize,
        users: &BTreeMap<String, AccountKind>,
        unresolved: &mut BTreeSet<String>,
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
                        self.identity(
                            &attrs["id"],
                            (attrs["userType"].as_str() == Some("DEFAULT"))
                                .then_some(AccountKind::Person),
                            attrs["userType"].is_null(),
                            at,
                            users,
                            unresolved,
                        );
                    }
                }
                if let Some(id) = object.get("accountId") {
                    self.identity(
                        id,
                        AccountKind::from_api(&value["accountType"]),
                        value["accountType"].is_null(),
                        at,
                        users,
                        unresolved,
                    );
                }
                for child in object.values() {
                    self.visit(child, at, depth + 1, nodes, users, unresolved)?;
                }
            }
            Value::Array(children) => {
                for child in children {
                    self.visit(child, at, depth + 1, nodes, users, unresolved)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn identity(
        &mut self,
        value: &Value,
        kind: Option<AccountKind>,
        lookup: bool,
        at: u64,
        users: &BTreeMap<String, AccountKind>,
        unresolved: &mut BTreeSet<String>,
    ) {
        let Some(id) = value.as_str().filter(|id| valid_account_id(id)) else {
            self.unclassified = true;
            return;
        };
        let kind = match (kind, lookup) {
            (None, true) => users.get(id).copied(),
            _ => kind,
        };
        if kind != Some(AccountKind::App) {
            self.accounts.entry(id.to_owned()).or_insert(at);
        }
        if kind.is_none() {
            self.unclassified = true;
            if lookup {
                unresolved.insert(id.to_owned());
            }
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

pub(crate) fn valid_account_id(id: &str) -> bool {
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

    #[test]
    fn resolved_account_types_complete_the_inventory_without_changing_adf() {
        let adf = json!({"type":"doc","version":1,"content":[{"type":"futureNode","content":[
            {"type":"mention","attrs":{"id":"synthetic:alice","text":"@Original Name"}},
            {"type":"mention","attrs":{"id":"synthetic:alice","userType":null}},
            {"type":"mention","attrs":{"id":"synthetic:app"}},
            {"attrs":{"accountId":"synthetic:customer","accountType":"customer"}},
            {"attrs":{"accountId":"synthetic:other-app","accountType":"app"}},
            {"attrs":{"accountId":"synthetic:bob","displayName":"Original Bob"}}
        ]}]});
        let original = adf.clone();
        let (initial, unresolved) = JiraData::inspect_adf(&adf, 100, &BTreeMap::new()).unwrap();
        assert!(initial.unclassified);
        assert_eq!(
            unresolved,
            BTreeSet::from([
                "synthetic:alice".into(),
                "synthetic:app".into(),
                "synthetic:bob".into()
            ])
        );
        let users = BTreeMap::from([
            ("synthetic:alice".into(), AccountKind::Person),
            ("synthetic:app".into(), AccountKind::App),
            ("synthetic:bob".into(), AccountKind::Person),
        ]);
        let (resolved, missing) = JiraData::inspect_adf(&adf, 100, &users).unwrap();
        assert!(!resolved.unclassified && missing.is_empty());
        assert_eq!(
            resolved.accounts,
            BTreeMap::from([
                ("synthetic:alice".into(), 100),
                ("synthetic:customer".into(), 100),
                ("synthetic:bob".into(), 100),
            ])
        );
        assert_eq!(adf, original);
        assert_eq!(
            crate::adf::convert(&adf).unwrap().markdown,
            crate::adf::convert(&original).unwrap().markdown
        );
        assert!(
            !serde_json::to_string(&resolved)
                .unwrap()
                .contains("Original")
        );
    }

    #[test]
    fn lookup_cannot_override_invalid_or_explicit_mention_types() {
        let users = BTreeMap::from([("synthetic:alice".into(), AccountKind::Person)]);
        for kind in [json!("FUTURE"), json!(false)] {
            let adf = json!({"type":"mention","attrs":{"id":"synthetic:alice","userType":kind}});
            let (data, unresolved) = JiraData::inspect_adf(&adf, 100, &users).unwrap();
            assert!(data.unclassified && unresolved.is_empty());
        }
        for kind in ["APP", "SPECIAL"] {
            let (data, unresolved) = JiraData::inspect_adf(
                &json!({"type":"mention","attrs":{"id":"synthetic:alice","userType":kind}}),
                100,
                &users,
            )
            .unwrap();
            assert_eq!(data, JiraData::default());
            assert!(unresolved.is_empty());
        }
        for id in [json!(null), json!("unknown"), json!("bad id")] {
            let (data, unresolved) =
                JiraData::inspect_adf(&json!({"type":"mention","attrs":{"id":id}}), 100, &users)
                    .unwrap();
            assert!(data.unclassified && unresolved.is_empty());
        }
    }

    #[test]
    fn unknown_or_missing_account_kinds_never_default_to_person() {
        assert_eq!(
            AccountKind::from_api(&json!("atlassian")),
            Some(AccountKind::Person)
        );
        assert_eq!(
            AccountKind::from_api(&json!("customer")),
            Some(AccountKind::Person)
        );
        assert_eq!(AccountKind::from_api(&json!("app")), Some(AccountKind::App));
        for value in [
            Value::Null,
            json!("unknown"),
            json!("DEFAULT"),
            json!(false),
        ] {
            assert_eq!(AccountKind::from_api(&value), None);
        }
    }
}

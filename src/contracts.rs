//! Shared-contract loaders.
//!
//! The `shared/` git submodule pins forest-shared-resources at a tagged
//! release; the JSON files under `shared/contracts/` are the single source
//! of truth for platform rules and license knowledge across forest-backend,
//! forest-trust-gateway, and this CLI. The vectors files are asserted in
//! unit tests, so a submodule bump that changes behavior fails the build
//! loudly instead of drifting silently.
//!
//! A malformed contract is a build defect, not a runtime condition, so the
//! loaders panic on parse failure.

use serde::Deserialize;
use std::sync::OnceLock;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerseRules {
    pub reserved_words: Vec<String>,
    #[allow(dead_code)] // kept for contract completeness; gateway enforces path rules
    pub epic_path_roots: Vec<String>,
    pub packages_mount: String,
    // Runtime uses receipts::RECEIPT_FILE directly; a unit test asserts the
    // contract value matches it (the actual point of carrying the field).
    #[allow(dead_code)]
    pub receipt_file_name: String,
}

#[derive(Deserialize)]
struct VerseRulesFile {
    uefn: VerseRules,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Fingerprint {
    pub id: String,
    #[serde(default)]
    pub all_of: Vec<String>,
    #[serde(default)]
    pub any_of: Vec<String>,
}

// `ratings` deliberately not deserialized: license ratings are registry-side
// knowledge (license_helper::extract_license_info consumes API responses).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LicensesContract {
    pub spdx_licenses: Vec<String>,
    pub text_fingerprints: Vec<Fingerprint>,
}

pub fn verse_rules() -> &'static VerseRules {
    static RULES: OnceLock<VerseRules> = OnceLock::new();
    RULES.get_or_init(|| {
        let file: VerseRulesFile =
            serde_json::from_str(include_str!("../shared/contracts/verse-rules.json"))
                .expect("shared/contracts/verse-rules.json failed to parse");
        file.uefn
    })
}

pub fn licenses() -> &'static LicensesContract {
    static LICENSES: OnceLock<LicensesContract> = OnceLock::new();
    LICENSES.get_or_init(|| {
        serde_json::from_str(include_str!("../shared/contracts/licenses.json"))
            .expect("shared/contracts/licenses.json failed to parse")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verse_rules_parse_and_carry_the_platform_constants() {
        let rules = verse_rules();
        assert_eq!(rules.packages_mount, "ForestPackages");
        assert_eq!(rules.receipt_file_name, crate::receipts::RECEIPT_FILE);
        assert!(rules.reserved_words.iter().any(|w| w == "module"));
        assert_eq!(rules.epic_path_roots.len(), 3);
    }

    #[test]
    fn licenses_parse_with_fingerprint_order_preserved() {
        let contract = licenses();
        assert!(contract.spdx_licenses.iter().any(|l| l == "0BSD"));
        assert!(contract.spdx_licenses.iter().any(|l| l == "CC-BY-SA-4.0"));
        // Order sensitivity is part of the contract: AGPL must be evaluated
        // before GPL (its text contains "general public license" too).
        assert_eq!(contract.text_fingerprints[0].id, "AGPL-3.0");
    }
}

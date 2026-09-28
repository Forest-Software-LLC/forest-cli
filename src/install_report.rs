//! Private packages an install skipped because the registry refused access.
//! The download pool is the only writer. main is the only reader: it prints
//! one error for the run and exits 1, so a partial install never passes.

use std::collections::BTreeSet;
use std::sync::Mutex;

use colored::Colorize;

/// Why the registry refused a private package's download URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DenyReason {
    /// No login and no FOREST_TOKEN; the registry was never asked.
    NotLoggedIn,
    /// The stored login was rejected (401 after a failed refresh).
    SessionRejected,
    /// 403/404: the caller can't read the scope.
    NoAccess,
}

impl DenyReason {
    /// Registry statuses that mean the caller can't read the package.
    /// Anything else stays a hard error.
    pub fn from_status(status: reqwest::StatusCode) -> Option<Self> {
        match status.as_u16() {
            401 => Some(DenyReason::SessionRejected),
            403 | 404 => Some(DenyReason::NoAccess),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeniedPackage {
    /// `scope/name` as the lockfile has it.
    pub name: String,
    pub version: String,
    pub reason: DenyReason,
}

impl DeniedPackage {
    pub fn from_status(name: &str, version: &str, status: reqwest::StatusCode) -> Option<Self> {
        let reason = DenyReason::from_status(status)?;
        Some(DeniedPackage { name: name.to_string(), version: version.to_string(), reason })
    }

    fn scope(&self) -> &str {
        self.name.split('/').next().unwrap_or(&self.name)
    }
}

// Travels as an anyhow error out of fetch_signed_url, so the pool can tell a
// refusal from a real failure.
impl std::fmt::Display for DeniedPackage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Access to private package {}@{} was refused", self.name, self.version)
    }
}

impl std::error::Error for DeniedPackage {}

static DENIED: Mutex<Vec<DeniedPackage>> = Mutex::new(Vec::new());

pub fn record(denied: &[DeniedPackage]) {
    let mut all = DENIED.lock().expect("install report poisoned");
    for d in denied {
        if !all.iter().any(|seen| seen.name == d.name && seen.version == d.version) {
            all.push(d.clone());
        }
    }
}

pub fn count() -> usize {
    DENIED.lock().expect("install report poisoned").len()
}

pub fn take() -> Vec<DeniedPackage> {
    std::mem::take(&mut *DENIED.lock().expect("install report poisoned"))
}

/// The post-install error: a headline, one line per package, then what to do
/// about it for each reason present.
pub fn render(denied: &[DeniedPackage]) -> (String, Vec<String>) {
    let headline = format!(
        "{} private package{} failed to install:",
        denied.len(),
        if denied.len() == 1 { "" } else { "s" }
    );
    let mut sorted: Vec<&DeniedPackage> = denied.iter().collect();
    sorted.sort_by(|a, b| (&a.name, &a.version).cmp(&(&b.name, &b.version)));
    let mut lines: Vec<String> = sorted.iter().map(|d| format!("   {}@{}", d.name, d.version)).collect();

    let reasons: BTreeSet<DenyReason> = denied.iter().map(|d| d.reason).collect();
    for reason in reasons {
        lines.push(match reason {
            DenyReason::NotLoggedIn => {
                "   You're not logged in. Run `forest login`, or set FOREST_TOKEN to an API token in CI.".to_string()
            }
            DenyReason::SessionRejected => {
                "   Your login has expired. Run `forest login` and install again.".to_string()
            }
            DenyReason::NoAccess => {
                let scopes: BTreeSet<&str> = denied
                    .iter()
                    .filter(|d| d.reason == DenyReason::NoAccess)
                    .map(DeniedPackage::scope)
                    .collect();
                format!(
                    "   Make sure you're authorized to the scope{} that failed: {}. The package maintainer has to grant you access.",
                    if scopes.len() == 1 { "" } else { "s" },
                    scopes.into_iter().collect::<Vec<_>>().join(", ")
                )
            }
        });
    }
    (headline, lines)
}

/// Print the error for everything recorded this run. Returns whether there
/// was anything to report, so main can set the exit code.
pub fn report() -> bool {
    let denied = take();
    if denied.is_empty() {
        return false;
    }
    let (headline, lines) = render(&denied);
    crate::message::fail(&headline);
    for line in lines {
        println!("{}", line.red());
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::StatusCode;

    fn denied(name: &str, version: &str, reason: DenyReason) -> DeniedPackage {
        DeniedPackage { name: name.into(), version: version.into(), reason }
    }

    #[test]
    fn only_auth_statuses_count_as_refusals() {
        assert_eq!(
            DeniedPackage::from_status("acme/a", "1.0.0", StatusCode::NOT_FOUND).map(|d| d.reason),
            Some(DenyReason::NoAccess)
        );
        assert_eq!(
            DeniedPackage::from_status("acme/a", "1.0.0", StatusCode::UNAUTHORIZED).map(|d| d.reason),
            Some(DenyReason::SessionRejected)
        );
        assert!(DeniedPackage::from_status("acme/a", "1.0.0", StatusCode::INTERNAL_SERVER_ERROR).is_none());
        assert!(DeniedPackage::from_status("acme/a", "1.0.0", StatusCode::TOO_MANY_REQUESTS).is_none());
    }

    #[test]
    fn render_lists_packages_and_names_each_failed_scope_once() {
        let (headline, lines) = render(&[
            denied("zeta/tools", "2.0.0", DenyReason::NoAccess),
            denied("acme/core", "1.2.0", DenyReason::NoAccess),
            denied("acme/net", "0.3.1", DenyReason::NoAccess),
        ]);
        assert_eq!(headline, "3 private packages failed to install:");
        assert_eq!(
            lines,
            vec![
                "   acme/core@1.2.0",
                "   acme/net@0.3.1",
                "   zeta/tools@2.0.0",
                "   Make sure you're authorized to the scopes that failed: acme, zeta. The package maintainer has to grant you access.",
            ]
        );
    }

    #[test]
    fn render_gives_login_advice_when_not_logged_in() {
        let (headline, lines) = render(&[denied("acme/core", "1.2.0", DenyReason::NotLoggedIn)]);
        assert_eq!(headline, "1 private package failed to install:");
        assert!(lines.last().unwrap().contains("forest login"));
        assert!(!lines.iter().any(|l| l.contains("authorized")));
    }
}

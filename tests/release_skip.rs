use regex::Regex;

fn should_skip_release(subjects: &[&str]) -> bool {
    let release_worthy = Regex::new(r"^(chore|docs)(\(.*\))?:").expect("valid regex");
    let non_release = subjects
        .iter()
        .filter(|subject| !release_worthy.is_match(subject))
        .count();
    non_release == 0
}

#[test]
fn skips_release_for_docs_only() {
    let subjects = ["docs: update readme", "docs(agents): clarify routing"];
    assert!(should_skip_release(&subjects));
}

#[test]
fn skips_release_for_chore_only() {
    let subjects = ["chore: bump deps", "chore(ci): tweak cache"];
    assert!(should_skip_release(&subjects));
}

#[test]
fn skips_release_for_chore_and_docs_only() {
    let subjects = ["chore: update configs", "docs: add release note"];
    assert!(should_skip_release(&subjects));
}

#[test]
fn does_not_skip_release_for_fix() {
    let subjects = ["fix: handle nil token", "docs: update guide"];
    assert!(!should_skip_release(&subjects));
}

#[test]
fn does_not_skip_release_for_feat_or_breaking() {
    let subjects = [
        "feat: add router weights",
        "BREAKING CHANGE: drop legacy api",
    ];
    assert!(!should_skip_release(&subjects));
}

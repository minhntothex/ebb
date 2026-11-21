use ebb::updates::{
    CalVer, GITHUB_RELEASES_PAGE_URL, UpdateCheckResult, parse_github_release_json,
};

#[test]
fn calver_parsing_formatting_and_ordering() {
    let v26_0 = CalVer::parse("26.0").unwrap();
    let v26_0_2 = CalVer::parse("26.0.2").unwrap();
    let v26_0_12 = CalVer::parse("26.0.12").unwrap();
    let v26_1 = CalVer::parse("v26.1").unwrap();
    let v26_2 = CalVer::parse("26.2").unwrap();
    let v26_10 = CalVer::parse("26.10").unwrap();
    let v27_0 = CalVer::parse("27.0").unwrap();

    assert_eq!(v26_0.to_string(), "26.0");
    assert_eq!(v26_0_2.to_string(), "26.0.2");
    assert_eq!(v26_1.to_string(), "26.1");

    assert!(v26_0 < v26_0_2);
    assert!(v26_0_2 < v26_0_12);
    assert!(v26_0_12 < v26_1);
    assert!(v26_1 < v26_2);
    assert!(v26_2 < v26_10);
    assert!(v26_10 < v27_0);

    for invalid in ["", "26", "26.1.2.3", "26.1-rc1", "abc.1", "26.", ".1"] {
        assert!(
            CalVer::parse(invalid).is_none(),
            "expected None for {invalid:?}"
        );
    }
}

#[test]
fn parses_github_release_json_and_compares_versions() {
    let newer = br#"{
        "tag_name": "26.3",
        "html_url": "https://github.com/minhntothex/ebb/releases/tag/26.3",
        "draft": false,
        "prerelease": false
    }"#;
    assert_eq!(
        parse_github_release_json(newer, "26.2").unwrap(),
        UpdateCheckResult::UpdateAvailable {
            current: "26.2".into(),
            latest: "26.3".into(),
            release_url: "https://github.com/minhntothex/ebb/releases/tag/26.3".into(),
        }
    );

    // Same or older version reports UpToDate.
    let same = br#"{"tag_name": "26.2", "html_url": "https://github.com/minhntothex/ebb/releases/tag/26.2"}"#;
    assert_eq!(
        parse_github_release_json(same, "26.2").unwrap(),
        UpdateCheckResult::UpToDate {
            current: "26.2".into(),
            latest: "26.2".into(),
        }
    );

    // Draft/prerelease ignored safely.
    let prerelease = br#"{"tag_name": "26.9", "prerelease": true}"#;
    assert_eq!(
        parse_github_release_json(prerelease, "26.2").unwrap(),
        UpdateCheckResult::UpToDate {
            current: "26.2".into(),
            latest: "26.2".into(),
        }
    );

    // Untrusted non-GitHub URL falls back to canonical GitHub Releases page.
    let weird_url = br#"{"tag_name": "26.4", "html_url": "https://evil.example/ebb"}"#;
    assert_eq!(
        parse_github_release_json(weird_url, "26.2").unwrap(),
        UpdateCheckResult::UpdateAvailable {
            current: "26.2".into(),
            latest: "26.4".into(),
            release_url: GITHUB_RELEASES_PAGE_URL.into(),
        }
    );
}

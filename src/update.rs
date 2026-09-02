use semver::Version;
use serde::Deserialize;
use std::time::Duration;

const OWNER: &str = "mycode2025-ui";
const REPOSITORY: &str = "pcanwork";
const GITEE_LATEST_API: &str =
    "https://gitee.com/api/v5/repos/mycode2025-ui/pcanwork/releases/latest";
const GITHUB_LATEST_API: &str =
    "https://api.github.com/repos/mycode2025-ui/pcanwork/releases/latest";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UpdateInfo {
    pub version: String,
    pub notes: String,
    pub gitee_download: String,
    pub github_download: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CheckResult {
    Available(UpdateInfo),
    Current { latest: String },
}

#[derive(Debug, Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Debug, Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Clone, Copy)]
enum Source {
    Gitee,
    Github,
}

pub(crate) fn check(current: &str) -> Result<CheckResult, String> {
    match check_source(current, GITEE_LATEST_API, Source::Gitee) {
        Ok(CheckResult::Available(mut info)) => {
            // Gitee 决定最新版本；随后读取 GitHub 同版本 asset，确保两个按钮都优先使用 API 返回地址。
            if let Ok(release) = fetch_release(GITHUB_LATEST_API)
                && parse_version(&release.tag_name).ok().as_ref()
                    == parse_version(&info.version).ok().as_ref()
                && let Some(asset) = select_installer(&release.assets)
            {
                info.github_download = asset.browser_download_url.clone();
            }
            Ok(CheckResult::Available(info))
        }
        Ok(result) => Ok(result),
        Err(gitee_error) => check_source(current, GITHUB_LATEST_API, Source::Github)
            .map_err(|github_error| format!("Gitee: {gitee_error}; GitHub: {github_error}")),
    }
}

fn check_source(current: &str, url: &str, source: Source) -> Result<CheckResult, String> {
    evaluate_release(current, fetch_release(url)?, source)
}

fn fetch_release(url: &str) -> Result<ApiRelease, String> {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .https_only(true)
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                .build(),
        )
        .build();
    let agent = ureq::Agent::new_with_config(config);
    let mut response = agent
        .get(url)
        .header("Accept", "application/json")
        .header(
            "User-Agent",
            format!("PcanWork/{}", crate::product_version::current()),
        )
        .call()
        .map_err(|error| error.to_string())?;
    response
        .body_mut()
        .read_json::<ApiRelease>()
        .map_err(|error| error.to_string())
}

fn evaluate_release(
    current: &str,
    release: ApiRelease,
    source: Source,
) -> Result<CheckResult, String> {
    let current_version = parse_version(current)?;
    let latest_version = parse_version(&release.tag_name)?;
    let display_version = latest_version.to_string();
    if latest_version <= current_version {
        return Ok(CheckResult::Current {
            latest: display_version,
        });
    }

    let asset = select_installer(&release.assets)
        .ok_or_else(|| format!("{} 未包含 Windows 安装包", release.tag_name))?;
    let tag = &release.tag_name;
    let encoded_name = asset.name.replace(' ', "%20");
    let github_mirror =
        format!("https://github.com/{OWNER}/{REPOSITORY}/releases/download/{tag}/{encoded_name}");
    let gitee_mirror =
        format!("https://gitee.com/{OWNER}/{REPOSITORY}/releases/download/{tag}/{encoded_name}");
    let (gitee_download, github_download) = match source {
        Source::Gitee => (asset.browser_download_url.clone(), github_mirror),
        Source::Github => (gitee_mirror, asset.browser_download_url.clone()),
    };

    Ok(CheckResult::Available(UpdateInfo {
        version: display_version,
        notes: compact_notes(&release.body),
        gitee_download,
        github_download,
    }))
}

fn parse_version(value: &str) -> Result<Version, String> {
    Version::parse(value.trim().trim_start_matches(['v', 'V']))
        .map_err(|error| format!("无法解析版本 {value}: {error}"))
}

fn select_installer(assets: &[ApiAsset]) -> Option<&ApiAsset> {
    assets
        .iter()
        .find(|asset| {
            let name = asset.name.to_ascii_lowercase();
            name.starts_with("pcanwork-setup-") && name.ends_with(".exe")
        })
        .or_else(|| {
            assets
                .iter()
                .find(|asset| asset.name.to_ascii_lowercase().ends_with(".exe"))
        })
}

fn compact_notes(notes: &str) -> String {
    let items = notes
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter(|line| {
            !line.starts_with("安装包：")
                && !line.starts_with("SHA-256：")
                && !line.starts_with("签名状态：")
        })
        .map(|line| {
            line.strip_prefix("- ")
                .or_else(|| line.strip_prefix("* "))
                .unwrap_or(line)
        })
        .take(3)
        .map(|line| format!("• {line}"))
        .collect::<Vec<_>>();
    let text = items.join("\n");
    if text.chars().count() <= 220 {
        return text;
    }
    let mut shortened = text.chars().take(217).collect::<String>();
    shortened.push_str("...");
    shortened
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, assets: &[(&str, &str)]) -> ApiRelease {
        ApiRelease {
            tag_name: tag.to_string(),
            body: "修复问题\n改进体验".to_string(),
            assets: assets
                .iter()
                .map(|(name, url)| ApiAsset {
                    name: (*name).to_string(),
                    browser_download_url: (*url).to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn newer_version_uses_asset_and_builds_second_mirror() {
        let result = evaluate_release(
            "0.1.24",
            release(
                "v0.1.25",
                &[("PcanWork-Setup-0.1.25.exe", "https://gitee.test/setup.exe")],
            ),
            Source::Gitee,
        )
        .unwrap();
        let CheckResult::Available(info) = result else {
            panic!("expected available update");
        };
        assert_eq!(info.version, "0.1.25");
        assert_eq!(info.gitee_download, "https://gitee.test/setup.exe");
        assert_eq!(
            info.github_download,
            "https://github.com/mycode2025-ui/pcanwork/releases/download/v0.1.25/PcanWork-Setup-0.1.25.exe"
        );
    }

    #[test]
    fn equal_or_older_release_is_current() {
        assert!(matches!(
            evaluate_release("0.1.24", release("v0.1.24", &[]), Source::Github).unwrap(),
            CheckResult::Current { .. }
        ));
    }

    #[test]
    fn patch_versions_are_compared_numerically_not_lexically() {
        assert!(matches!(
            evaluate_release("0.3.20", release("v0.3.11", &[]), Source::Github).unwrap(),
            CheckResult::Current { latest } if latest == "0.3.11"
        ));
        assert!(matches!(
            evaluate_release("0.3.2", release("v0.3.11", &[("PcanWork-Setup-0.3.11.exe", "setup")]), Source::Github).unwrap(),
            CheckResult::Available(info) if info.version == "0.3.11"
        ));
    }

    #[test]
    fn installer_selection_prefers_named_setup() {
        let release = release(
            "v1.0.0",
            &[("helper.exe", "one"), ("PcanWork-Setup-1.0.0.exe", "two")],
        );
        assert_eq!(
            select_installer(&release.assets)
                .unwrap()
                .browser_download_url,
            "two"
        );
    }

    #[test]
    fn release_notes_keep_utf8_and_strip_markdown_metadata() {
        let notes = "## PcanWork v0.3.25\n\n- 改进 PCAN-USB FD 初始化兼容性。\n- PCAN ↔ ZLG 完成双向实机验证。\n- 每档波特率载荷校验正确。\n- 第四条不在弹窗显示。\n\n安装包：PcanWork-Setup.exe\nSHA-256：ABC";
        assert_eq!(
            compact_notes(notes),
            "• 改进 PCAN-USB FD 初始化兼容性。\n• PCAN ↔ ZLG 完成双向实机验证。\n• 每档波特率载荷校验正确。"
        );
    }

    #[test]
    #[ignore = "requires public release APIs"]
    fn live_release_api_returns_downloadable_update() {
        let CheckResult::Available(info) = check("0.0.0").unwrap() else {
            panic!("expected the published release to be newer than 0.0.0");
        };
        assert!(info.gitee_download.starts_with("https://"));
        assert!(info.github_download.starts_with("https://"));
    }
}

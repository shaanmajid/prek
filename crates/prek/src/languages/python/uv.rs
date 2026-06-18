use std::env::consts::EXE_EXTENSION;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

use anyhow::{Context, Result, bail};
use http::header::ACCEPT;
use semver::{Version, VersionReq};
use target_lexicon::HOST;
use tracing::{debug, trace, warn};

use prek_consts::env_vars::EnvVars;

use crate::fs::LockedFile;
use crate::http::{REQWEST_CLIENT, download_and_extract};
use crate::process::Cmd;
use crate::store::{CacheBucket, Store};

// The version range of `uv` we will install. Should update periodically.
const CUR_UV_VERSION: &str = "0.11.19";
const ASTRAL_BASE_URL: &str = "https://releases.astral.sh";
const GITHUB_UV_RELEASES_URL_PREFIX: &str = "https://github.com/astral-sh/uv/releases/download/";
const ASTRAL_UV_RELEASES_PATH: &str = "/github/uv/releases/download/";
const UV_MANIFEST_PATH: &str = "/github/versions/main/v1/uv.ndjson";
const PREK_UV_SOURCE: &str = "PREK_UV_SOURCE";
static UV_VERSION_RANGE: LazyLock<VersionReq> =
    LazyLock::new(|| VersionReq::parse(">=0.7.0").unwrap());

fn uv_archive_format() -> &'static str {
    if cfg!(windows) { "zip" } else { "tar.gz" }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AstralSource {
    base_url: String,
}

impl Default for AstralSource {
    fn default() -> Self {
        Self {
            base_url: ASTRAL_BASE_URL.to_string(),
        }
    }
}

impl AstralSource {
    fn mirror(base_url: &str) -> Result<Self> {
        let base_url = base_url.trim_end_matches('/').to_string();
        if base_url.is_empty() {
            bail!("{} must not be empty", EnvVars::UV_ASTRAL_MIRROR_URL);
        }

        Ok(Self { base_url })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    fn manifest_url(&self) -> String {
        self.url(UV_MANIFEST_PATH)
    }

    fn download_url(&self, artifact: &UvArtifact) -> Result<String> {
        let path = artifact.astral_path()?;
        Ok(self.url(&path))
    }
}

#[derive(Debug, serde::Deserialize)]
struct UvVersionManifest {
    version: String,
    artifacts: Vec<UvArtifact>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct UvArtifact {
    platform: String,
    variant: String,
    url: String,
    archive_format: String,
}

impl UvArtifact {
    fn archive_name(&self) -> String {
        format!("uv-{}.{}", self.platform, self.archive_format)
    }

    fn astral_path(&self) -> Result<String> {
        if let Some(path) = self.url.strip_prefix(ASTRAL_BASE_URL)
            && path.starts_with('/')
        {
            if !path.starts_with(ASTRAL_UV_RELEASES_PATH) {
                bail!("uv manifest artifact URL is not an Astral uv release URL");
            }
            return Ok(path.to_string());
        }

        if let Some(path) = self.url.strip_prefix(GITHUB_UV_RELEASES_URL_PREFIX) {
            return Ok(format!("{ASTRAL_UV_RELEASES_PATH}{path}"));
        }

        bail!("uv manifest artifact URL is not under a supported source")
    }

    fn matches(&self, version: &str, platform: &str, archive_format: &str) -> bool {
        let expected_suffix = format!("/{version}/{}", self.archive_name());
        self.platform == platform
            && self.variant == "default"
            && self.archive_format == archive_format
            && self.url.ends_with(&expected_suffix)
    }
}

fn uv_artifact_from_manifest(
    manifest: &str,
    version: &str,
    platform: &str,
    archive_format: &str,
) -> Result<UvArtifact> {
    for (line_index, line) in manifest.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let release: UvVersionManifest = serde_json::from_str(line)
            .with_context(|| format!("Failed to parse uv manifest line {}", line_index + 1))?;
        if release.version != version {
            continue;
        }

        return release
            .artifacts
            .into_iter()
            .find(|artifact| artifact.matches(version, platform, archive_format))
            .with_context(|| {
                format!(
                    "Could not find uv {version} artifact for platform `{platform}` \
                    and archive format `{archive_format}` in Astral's versions manifest. \
                    Install a compatible system uv or set {} to a mirror that provides \
                    the required artifact.",
                    EnvVars::UV_ASTRAL_MIRROR_URL
                )
            });
    }

    bail!("Could not find uv {version} in Astral's versions manifest")
}

fn get_uv_version(uv_path: &Path) -> Result<Version> {
    let output = Command::new(uv_path)
        .arg("--version")
        .output()
        .context("Failed to execute uv")?;

    if !output.status.success() {
        bail!("Failed to get uv version");
    }

    let version_output = String::from_utf8_lossy(&output.stdout);
    let version_str = version_output
        .split_whitespace()
        .nth(1)
        .context("Invalid version output format")?;

    Version::parse(version_str).map_err(Into::into)
}

fn validate_uv_binary(uv_path: &Path) -> Result<Version> {
    let version = get_uv_version(uv_path)?;
    if !UV_VERSION_RANGE.matches(&version) {
        bail!(
            "uv version `{version}` does not satisfy required range `{}`",
            &*UV_VERSION_RANGE
        );
    }
    Ok(version)
}

async fn replace_uv_binary(source: &Path, target_path: &Path) -> Result<()> {
    if let Some(parent) = target_path.parent() {
        fs_err::tokio::create_dir_all(parent).await?;
    }

    if target_path.exists() {
        debug!(target = %target_path.display(), "Removing existing uv binary");
        fs_err::tokio::remove_file(target_path).await?;
    }

    fs_err::tokio::rename(source, target_path).await?;
    Ok(())
}

static UV_EXE: LazyLock<Option<(PathBuf, Version)>> = LazyLock::new(|| {
    for uv_path in which::which_all("uv").ok()? {
        debug!("Found uv in PATH: {}", uv_path.display());

        match validate_uv_binary(&uv_path) {
            Ok(version) => return Some((uv_path, version)),
            Err(err) => warn!(uv = %uv_path.display(), error = %err, "Skipping incompatible uv"),
        }
    }

    None
});

impl AstralSource {
    async fn install(&self, store: &Store, target: &Path) -> Result<()> {
        let manifest_url = self.manifest_url();
        debug!("Fetching uv versions manifest");
        let response = REQWEST_CLIENT
            .get(&manifest_url)
            .header(ACCEPT, "*/*")
            .send()
            .await
            .context("Failed to fetch uv versions manifest")?;

        if !response.status().is_success() {
            bail!(
                "Failed to fetch uv versions manifest: {}",
                response.status()
            );
        }

        let manifest = response.text().await?;
        let artifact = uv_artifact_from_manifest(
            &manifest,
            CUR_UV_VERSION,
            &HOST.to_string(),
            uv_archive_format(),
        )?;
        let archive_name = artifact.archive_name();
        let download_url = self.download_url(&artifact)?;

        download_and_extract(&download_url, &archive_name, store, async |extracted| {
            let source = extracted.join("uv").with_extension(EXE_EXTENSION);
            let target_path = target.join("uv").with_extension(EXE_EXTENSION);

            debug!(?source, target = %target_path.display(), "Moving uv to target");
            // TODO: retry on Windows
            replace_uv_binary(&source, &target_path).await?;

            anyhow::Ok(())
        })
        .await
        .context("Failed to download and extract uv")?;

        Ok(())
    }
}

pub(crate) struct Uv {
    path: PathBuf,
}

impl Uv {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub(crate) fn cmd(&self, summary: &str, store: &Store) -> Cmd {
        let mut cmd = Cmd::new(&self.path, summary);
        cmd.env(EnvVars::UV_CACHE_DIR, store.cache_path(CacheBucket::Uv));
        cmd
    }

    pub(crate) async fn install(store: &Store, uv_dir: &Path) -> Result<Self> {
        // 1) Check `uv` alongside `prek` binary (e.g. `uv tool install prek --with uv`)
        let prek_exe = std::env::current_exe()?.canonicalize()?;
        if let Some(prek_dir) = prek_exe.parent() {
            let uv_path = prek_dir.join("uv").with_extension(EXE_EXTENSION);
            if uv_path.is_file() {
                match validate_uv_binary(&uv_path) {
                    Ok(_) => {
                        trace!(uv = %uv_path.display(), "Found compatible uv alongside prek binary");
                        return Ok(Self::new(uv_path));
                    }
                    Err(err) => {
                        warn!(uv = %uv_path.display(), error = %err, "Skipping incompatible uv");
                    }
                }
            }
        }

        // 2) Check if system `uv` meets minimum version requirement
        if let Some((uv_path, version)) = UV_EXE.as_ref() {
            trace!(
                "Using system uv version {} at {}",
                version,
                uv_path.display()
            );
            return Ok(Self::new(uv_path.clone()));
        }

        // 3) Use or install managed `uv`
        let uv_path = uv_dir.join("uv").with_extension(EXE_EXTENSION);

        if uv_path.is_file() {
            match validate_uv_binary(&uv_path) {
                Ok(_) => {
                    trace!(uv = %uv_path.display(), "Found compatible managed uv");
                    return Ok(Self::new(uv_path));
                }
                Err(err) => {
                    warn!(uv = %uv_path.display(), error = %err, "Skipping incompatible managed uv");
                }
            }
        }

        // Install new managed uv with proper locking
        fs_err::tokio::create_dir_all(&uv_dir).await?;
        let _lock = LockedFile::acquire(uv_dir.join(".lock"), "uv").await?;

        if uv_path.is_file() {
            match validate_uv_binary(&uv_path) {
                Ok(_) => {
                    trace!(uv = %uv_path.display(), "Found compatible managed uv");
                    return Ok(Self::new(uv_path));
                }
                Err(err) => {
                    warn!(uv = %uv_path.display(), error = %err, "Skipping incompatible managed uv");
                }
            }
        }

        let source = astral_source_from_env()?;
        trace!(?source, "Selected uv source");
        source.install(store, uv_dir).await?;

        // Downloaded `uv` binaries can be present on disk but still fail to execute in the
        // current runtime environment, such as when the libc variant or dynamic loader path
        // does not match the host. Validate immediately so we can surface a clear error here.
        match validate_uv_binary(&uv_path) {
            Ok(version) => trace!(version = %version, "Successfully installed uv"),
            Err(err) => bail!(
                "Installed uv at `{}` failed validation: {err}. \
                This usually means the downloaded uv binary is incompatible with the \
                current runtime environment, for example due to a libc mismatch or a \
                missing dynamic loader path. If this keeps happening, please report it \
                with details about your environment and the full error output.",
                uv_path.display()
            ),
        }

        Ok(Self::new(uv_path))
    }
}

fn astral_source_from_env() -> Result<AstralSource> {
    if EnvVars::is_set(PREK_UV_SOURCE) {
        warn!(
            "{PREK_UV_SOURCE} is no longer supported; prek installs managed uv from Astral releases. \
            Use {} to configure a releases-compatible mirror.",
            EnvVars::UV_ASTRAL_MIRROR_URL
        );
    }
    astral_source_from_mirror_url(EnvVars::var(EnvVars::UV_ASTRAL_MIRROR_URL).ok())
}

fn astral_source_from_mirror_url(mirror_url: Option<String>) -> Result<AstralSource> {
    if let Some(mirror_url) = mirror_url {
        AstralSource::mirror(&mirror_url)
    } else {
        Ok(AstralSource::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_cur_uv_version_in_range() {
        let version = Version::parse(CUR_UV_VERSION).expect("Invalid CUR_UV_VERSION");
        assert!(
            UV_VERSION_RANGE.matches(&version),
            "CUR_UV_VERSION {CUR_UV_VERSION} does not satisfy the version requirement {}",
            &*UV_VERSION_RANGE
        );
    }

    #[test]
    fn parses_manifest_and_selects_default_host_artifact() -> Result<()> {
        let manifest = r#"
{"version":"0.11.13","date":"2026-05-01T00:00:00Z","artifacts":[{"platform":"x86_64-unknown-linux-gnu","variant":"default","url":"https://github.com/astral-sh/uv/releases/download/0.11.13/uv-x86_64-unknown-linux-gnu.tar.gz","archive_format":"tar.gz","sha256":"ignored"}]}
{"version":"0.11.14","date":"2026-05-02T00:00:00Z","artifacts":[{"platform":"x86_64-unknown-linux-gnu","variant":"minimal","url":"https://github.com/astral-sh/uv/releases/download/0.11.14/uv-x86_64-unknown-linux-gnu.tar.gz","archive_format":"tar.gz","sha256":"ignored"},{"platform":"x86_64-unknown-linux-gnu","variant":"default","url":"https://github.com/astral-sh/uv/releases/download/0.11.14/uv-x86_64-unknown-linux-gnu.tar.gz","archive_format":"tar.gz","sha256":"ignored"},{"platform":"aarch64-apple-darwin","variant":"default","url":"https://github.com/astral-sh/uv/releases/download/0.11.14/uv-aarch64-apple-darwin.tar.gz","archive_format":"tar.gz","sha256":"ignored"},{"platform":"x86_64-pc-windows-msvc","variant":"default","url":"https://github.com/astral-sh/uv/releases/download/0.11.14/uv-x86_64-pc-windows-msvc.zip","archive_format":"zip","sha256":"ignored"}]}
"#;

        let artifact =
            uv_artifact_from_manifest(manifest, "0.11.14", "x86_64-unknown-linux-gnu", "tar.gz")?;

        assert_eq!(artifact.platform, "x86_64-unknown-linux-gnu");
        assert_eq!(artifact.variant, "default");
        assert_eq!(
            artifact.archive_name(),
            "uv-x86_64-unknown-linux-gnu.tar.gz"
        );
        Ok(())
    }

    #[test]
    fn manifest_selection_requires_matching_version_platform_and_format() {
        let manifest = r#"
{"version":"0.11.14","artifacts":[{"platform":"x86_64-unknown-linux-gnu","variant":"default","url":"https://github.com/astral-sh/uv/releases/download/0.11.14/uv-x86_64-unknown-linux-gnu.tar.gz","archive_format":"tar.gz","sha256":"ignored"}]}
"#;

        assert!(
            uv_artifact_from_manifest(manifest, "0.11.13", "x86_64-unknown-linux-gnu", "tar.gz")
                .is_err()
        );
        assert!(
            uv_artifact_from_manifest(manifest, "0.11.14", "aarch64-apple-darwin", "tar.gz")
                .is_err()
        );
        assert!(
            uv_artifact_from_manifest(manifest, "0.11.14", "x86_64-unknown-linux-gnu", "zip")
                .is_err()
        );
    }

    #[test]
    fn manifest_selection_requires_matching_artifact_url() {
        let wrong_filename_manifest = r#"
{"version":"0.11.14","artifacts":[{"platform":"x86_64-unknown-linux-gnu","variant":"default","url":"https://github.com/astral-sh/uv/releases/download/0.11.14/uv-aarch64-unknown-linux-gnu.tar.gz","archive_format":"tar.gz","sha256":"ignored"}]}
"#;
        let wrong_version_manifest = r#"
{"version":"0.11.14","artifacts":[{"platform":"x86_64-unknown-linux-gnu","variant":"default","url":"https://github.com/astral-sh/uv/releases/download/0.11.13/uv-x86_64-unknown-linux-gnu.tar.gz","archive_format":"tar.gz","sha256":"ignored"}]}
"#;

        assert!(
            uv_artifact_from_manifest(
                wrong_filename_manifest,
                "0.11.14",
                "x86_64-unknown-linux-gnu",
                "tar.gz",
            )
            .is_err()
        );
        assert!(
            uv_artifact_from_manifest(
                wrong_version_manifest,
                "0.11.14",
                "x86_64-unknown-linux-gnu",
                "tar.gz",
            )
            .is_err()
        );
    }

    #[test]
    fn astral_source_builds_default_urls() -> Result<()> {
        let source = AstralSource::default();
        let artifact = UvArtifact {
            platform: "x86_64-unknown-linux-gnu".to_string(),
            variant: "default".to_string(),
            url: "https://github.com/astral-sh/uv/releases/download/0.11.14/uv-x86_64-unknown-linux-gnu.tar.gz".to_string(),
            archive_format: "tar.gz".to_string(),
        };

        assert_eq!(
            source.manifest_url(),
            "https://releases.astral.sh/github/versions/main/v1/uv.ndjson"
        );
        assert_eq!(
            source.download_url(&artifact)?,
            "https://releases.astral.sh/github/uv/releases/download/0.11.14/uv-x86_64-unknown-linux-gnu.tar.gz"
        );
        Ok(())
    }

    #[test]
    fn astral_mirror_replaces_releases_base_url() -> Result<()> {
        let source = AstralSource::mirror("https://mirror.example.com/astral///")?;
        let artifact = UvArtifact {
            platform: "x86_64-unknown-linux-gnu".to_string(),
            variant: "default".to_string(),
            url: "https://github.com/astral-sh/uv/releases/download/0.11.14/uv-x86_64-unknown-linux-gnu.tar.gz".to_string(),
            archive_format: "tar.gz".to_string(),
        };

        assert_eq!(
            source.manifest_url(),
            "https://mirror.example.com/astral/github/versions/main/v1/uv.ndjson"
        );
        assert_eq!(
            source.download_url(&artifact)?,
            "https://mirror.example.com/astral/github/uv/releases/download/0.11.14/uv-x86_64-unknown-linux-gnu.tar.gz"
        );
        Ok(())
    }

    #[test]
    fn astral_mirror_rejects_non_astral_artifact_urls() -> Result<()> {
        let source = AstralSource::mirror("https://mirror.example.com/astral")?;
        let wrong_host_artifact = UvArtifact {
            platform: "x86_64-unknown-linux-gnu".to_string(),
            variant: "default".to_string(),
            url: "https://example.com/astral-sh/uv/releases/download/0.11.14/uv-x86_64-unknown-linux-gnu.tar.gz".to_string(),
            archive_format: "tar.gz".to_string(),
        };
        let wrong_github_repo_artifact = UvArtifact {
            platform: "x86_64-unknown-linux-gnu".to_string(),
            variant: "default".to_string(),
            url: "https://github.com/astral-sh/ruff/releases/download/0.11.14/uv-x86_64-unknown-linux-gnu.tar.gz".to_string(),
            archive_format: "tar.gz".to_string(),
        };
        let wrong_astral_repo_artifact = UvArtifact {
            platform: "x86_64-unknown-linux-gnu".to_string(),
            variant: "default".to_string(),
            url: "https://releases.astral.sh/github/ruff/releases/download/0.11.14/uv-x86_64-unknown-linux-gnu.tar.gz".to_string(),
            archive_format: "tar.gz".to_string(),
        };

        assert!(source.download_url(&wrong_host_artifact).is_err());
        assert!(source.download_url(&wrong_github_repo_artifact).is_err());
        assert!(source.download_url(&wrong_astral_repo_artifact).is_err());
        Ok(())
    }

    #[test]
    fn astral_source_preserves_already_mirrored_artifact_urls() -> Result<()> {
        let source = AstralSource::default();
        let artifact = UvArtifact {
            platform: "x86_64-unknown-linux-gnu".to_string(),
            variant: "default".to_string(),
            url: "https://releases.astral.sh/github/uv/releases/download/0.11.14/uv-x86_64-unknown-linux-gnu.tar.gz".to_string(),
            archive_format: "tar.gz".to_string(),
        };

        assert_eq!(
            source.download_url(&artifact)?,
            "https://releases.astral.sh/github/uv/releases/download/0.11.14/uv-x86_64-unknown-linux-gnu.tar.gz"
        );
        Ok(())
    }

    #[test]
    fn env_selection_uses_default_astral_source() -> Result<()> {
        let source = astral_source_from_mirror_url(None)?;

        assert!(source.base_url == "https://releases.astral.sh");
        Ok(())
    }

    #[test]
    fn env_selection_uses_astral_mirror() -> Result<()> {
        let source =
            astral_source_from_mirror_url(Some("https://mirror.example.com/astral/".to_string()))?;

        assert!(source.base_url == "https://mirror.example.com/astral");
        Ok(())
    }

    #[test]
    fn env_selection_rejects_empty_astral_mirror() {
        let source = astral_source_from_mirror_url(Some("///".to_string()));
        assert!(source.is_err());
    }

    #[test]
    fn parses_manifest_for_current_host_artifact() -> Result<()> {
        let manifest = format!(
            r#"{{"version":"{CUR_UV_VERSION}","date":"2026-05-02T00:00:00Z","artifacts":[{{"platform":"{}","variant":"default","url":"https://github.com/astral-sh/uv/releases/download/{CUR_UV_VERSION}/uv-{}.{}","archive_format":"{}","sha256":"ignored"}}]}}"#,
            HOST,
            HOST,
            uv_archive_format(),
            uv_archive_format(),
        );

        let artifact = uv_artifact_from_manifest(
            &manifest,
            CUR_UV_VERSION,
            &HOST.to_string(),
            uv_archive_format(),
        )?;
        assert_eq!(artifact.platform, HOST.to_string());
        assert_eq!(artifact.archive_format, uv_archive_format());
        Ok(())
    }

    #[tokio::test]
    async fn replace_uv_binary_overwrites_existing_file() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source-uv");
        let target_dir = temp.path().join("tools").join("uv");
        let target_path = target_dir.join("uv").with_extension(EXE_EXTENSION);

        fs_err::create_dir_all(&target_dir)?;
        fs_err::write(&source, b"new")?;
        fs_err::write(&target_path, b"old")?;

        replace_uv_binary(&source, &target_path).await?;

        assert!(!source.exists());
        assert_eq!(fs_err::read(&target_path)?, b"new");

        Ok(())
    }

    #[tokio::test]
    async fn replace_uv_binary_recreates_missing_parent_dir() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source-uv");
        let target_dir = temp.path().join("tools").join("uv");
        let target_path = target_dir.join("uv").with_extension(EXE_EXTENSION);

        fs_err::create_dir_all(&target_dir)?;
        fs_err::write(&target_path, b"old")?;
        fs_err::remove_dir_all(&target_dir)?;
        fs_err::write(&source, b"new")?;

        replace_uv_binary(&source, &target_path).await?;

        assert!(target_dir.exists());
        assert_eq!(fs_err::read(&target_path)?, b"new");

        Ok(())
    }
}

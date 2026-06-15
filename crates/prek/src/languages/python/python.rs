use std::env::consts::EXE_EXTENSION;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock};

use anyhow::{Context, Result};
use mea::once::OnceMap;
use prek_consts::env_vars::EnvVars;
use prek_consts::prepend_paths;
use regex::Regex;
use rustc_hash::FxBuildHasher;
use serde::Deserialize;
use tracing::{debug, trace};

use crate::cli::reporter::HookInstallReporter;
use crate::cli::run::HookRunReporter;
use crate::hook::InstalledHook;
use crate::hook::{Hook, InstallInfo};
use crate::languages::LanguageImpl;
use crate::languages::python::PythonRequest;
use crate::languages::python::uv::Uv;
use crate::languages::version::LanguageRequest;
use crate::process;
use crate::process::Cmd;
use crate::run::run_by_batch;
use crate::store::{Store, ToolBucket};

#[derive(Debug, Copy, Clone)]
pub(crate) struct Python;

pub(crate) struct PythonInfo {
    pub(crate) version: semver::Version,
    pub(crate) python_exec: PathBuf,
}

#[derive(Debug, Clone, thiserror::Error)]
pub(crate) enum PythonInfoError {
    #[error("Failed to parse Python info JSON: {0}")]
    Parse(String),
    #[error("Failed to query Python info: {0}")]
    Query(String),
    #[error("{0}")]
    Message(String),
}

static PYTHON_INFO_CACHE: LazyLock<OnceMap<PathBuf, Arc<PythonInfo>, FxBuildHasher>> =
    LazyLock::new(|| OnceMap::with_hasher(FxBuildHasher));

async fn query_python_info(python: &Path) -> Result<PythonInfo, PythonInfoError> {
    #[derive(Deserialize)]
    struct QueryPythonInfo {
        version: semver::Version,
        base_exec_prefix: PathBuf,
    }

    static QUERY_PYTHON_INFO: &str = indoc::indoc! {r#"
    import sys, json
    info = {
        "version": ".".join(map(str, sys.version_info[:3])),
        "base_exec_prefix": sys.base_exec_prefix,
    }
    print(json.dumps(info))
    "#};

    let stdout = Cmd::new(python, "python -c")
        .arg("-I")
        .arg("-c")
        .arg(QUERY_PYTHON_INFO)
        .check(true)
        .output()
        .await
        .map_err(|err| PythonInfoError::Query(err.to_string()))?
        .stdout;

    let info: QueryPythonInfo =
        serde_json::from_slice(&stdout).map_err(|err| PythonInfoError::Parse(err.to_string()))?;
    let python_exec = python_exec(&info.base_exec_prefix);

    Ok(PythonInfo {
        version: info.version,
        python_exec,
    })
}

pub(crate) async fn query_python_info_cached(
    python: &Path,
) -> Result<Arc<PythonInfo>, PythonInfoError> {
    let python = fs_err::canonicalize(python).unwrap_or_else(|_| python.to_path_buf());
    PYTHON_INFO_CACHE
        .try_compute(python.clone(), async move || {
            let info = query_python_info(&python).await?;
            Ok(Arc::new(info))
        })
        .await
}

impl LanguageImpl for Python {
    async fn install(
        &self,
        hook: Arc<Hook>,
        store: &Store,
        reporter: &HookInstallReporter,
    ) -> Result<InstalledHook> {
        let progress = reporter.on_install_start(&hook);

        let uv_dir = store.tools_path(ToolBucket::Uv);
        let uv = Uv::install(store, &uv_dir)
            .await
            .context("Failed to install uv")?;

        let mut info = InstallInfo::new(
            hook.language,
            hook.env_key_dependencies().clone(),
            &store.hooks_dir(),
        )?;

        debug!(%hook, target = %info.env_path.display(), "Installing environment");

        // Create venv (auto download Python if needed)
        Self::create_venv(&uv, store, &info, &hook.language_request)
            .await
            .context("Failed to create Python virtual environment")?;

        if let Err(err) = Self::install_dependencies(&uv, store, &info.env_path, &hook).await {
            let Some(inferred) = infer_retry_constraint_from_error(&err) else {
                return Err(err.into());
            };
            let Some(retry_request) =
                retry_request_for_language_request(&hook.language_request, &inferred)
            else {
                return Err(err.into());
            };

            debug!(
                constraint = %retry_request,
                "Retrying Python hook installation with inferred Python constraint",
            );

            Self::create_venv_with_python(
                &uv,
                store,
                &info,
                &hook.language_request,
                Some(&retry_request),
                true,
            )
            .await
            .with_context(|| {
                format!(
                    "Failed to recreate Python virtual environment with inferred Python constraint `{}`",
                    retry_request
                )
            })?;

            Self::install_dependencies(&uv, store, &info.env_path, &hook)
                .await
                .map_err(anyhow::Error::new)
                .with_context(|| {
                    format!(
                        "Retry with inferred Python constraint `{}` failed",
                        retry_request
                    )
                })?;
        }

        let python = python_exec(&info.env_path);
        let python_info = query_python_info(&python)
            .await
            .context("Failed to query Python info")?;

        info.with_language_version(python_info.version)
            .with_toolchain(python_info.python_exec);

        info.persist_env_path();

        reporter.on_install_complete(progress);

        Ok(InstalledHook::Installed {
            hook,
            info: Arc::new(info),
        })
    }

    async fn check_health(&self, info: &InstallInfo) -> Result<()> {
        let python = python_exec(&info.env_path);
        let python_info = query_python_info_cached(&python)
            .await
            .context("Failed to query Python info")?;

        if python_info.version != info.language_version {
            anyhow::bail!(
                "Python version mismatch: expected {}, found {}",
                info.language_version,
                python_info.version
            );
        }

        Ok(())
    }

    async fn run(
        &self,
        hook: &InstalledHook,
        filenames: &[&Path],
        store: &Store,
        reporter: &HookRunReporter,
    ) -> Result<(i32, Vec<u8>)> {
        let progress = reporter.on_run_start(hook, filenames.len());

        let env_dir = hook.env_path().expect("Python must have env path");
        let new_path = prepend_paths(&[&bin_dir(env_dir)]).context("Failed to join PATH")?;
        let entry = hook.entry.resolve(Some(&new_path), store)?;

        let run = async |batch: &[&Path]| {
            let mut output = Cmd::new(&entry[0], "python hook")
                .current_dir(hook.work_dir())
                .args(&entry[1..])
                .env(EnvVars::VIRTUAL_ENV, env_dir)
                .env(EnvVars::PATH, &new_path)
                .env_remove(EnvVars::PYTHONHOME)
                .envs(&hook.env)
                .args(&hook.args)
                .args(batch)
                .check(false)
                .stdin(Stdio::null())
                .pty_output_with_sink(reporter.output_sink(progress))
                .await?;

            reporter.on_run_progress(progress, batch.len() as u64);

            output.stdout.extend(output.stderr);
            let code = output.status.code().unwrap_or(1);
            anyhow::Ok((code, output.stdout))
        };

        let results = run_by_batch(hook, filenames, entry.argv(), run).await?;

        // Collect results
        let mut combined_status = 0;
        let mut combined_output = Vec::new();

        for (code, output) in results {
            combined_status |= code;
            combined_output.extend(output);
        }

        reporter.on_run_complete(progress);

        Ok((combined_status, combined_output))
    }
}

fn to_uv_python_request(request: &LanguageRequest) -> Option<String> {
    match request {
        LanguageRequest::Any { .. } => None,
        LanguageRequest::Python(request) => match request {
            PythonRequest::Any => None,
            PythonRequest::Major(major) => Some(format!("{major}")),
            PythonRequest::MajorMinor(major, minor) => Some(format!("{major}.{minor}")),
            PythonRequest::MajorMinorPatch(major, minor, patch) => {
                Some(format!("{major}.{minor}.{patch}"))
            }
            PythonRequest::Range(_, raw) => Some(raw.clone()),
        },
        _ => unreachable!(),
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum VersionPrecision {
    Major,
    MajorMinor,
    MajorMinorPatch,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct InferredLowerBound {
    version: semver::Version,
    inclusive: bool,
    precision: VersionPrecision,
}

impl InferredLowerBound {
    fn operator(&self) -> &'static str {
        if self.inclusive { ">=" } else { ">" }
    }

    fn is_stricter_than(&self, other: &Self) -> bool {
        match self.version.cmp(&other.version) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal => !self.inclusive && other.inclusive,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct InferredUpperBound {
    version: semver::Version,
    inclusive: bool,
    precision: VersionPrecision,
}

impl InferredUpperBound {
    fn operator(&self) -> &'static str {
        if self.inclusive { "<=" } else { "<" }
    }

    fn is_stricter_than(&self, other: &Self) -> bool {
        match self.version.cmp(&other.version) {
            std::cmp::Ordering::Less => true,
            std::cmp::Ordering::Greater => false,
            std::cmp::Ordering::Equal => !self.inclusive && other.inclusive,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct InferredRetryConstraint {
    request: String,
    requirement: semver::VersionReq,
    candidate: semver::Version,
    lower: InferredLowerBound,
    upper: InferredUpperBound,
}

fn infer_retry_constraint_from_error(error: &process::Error) -> Option<InferredRetryConstraint> {
    let process::Error::Status {
        error: process::StatusError {
            output: Some(output),
            ..
        },
        ..
    } = error
    else {
        return None;
    };

    infer_retry_constraint(&String::from_utf8_lossy(&output.stderr))
}

fn infer_retry_constraint(stderr: &str) -> Option<InferredRetryConstraint> {
    static PYTHON_BOUND: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"does not satisfy\s+Python\s*(>=|>)\s*([0-9]+(?:\.[0-9]+){0,2})")
            .expect("inferred Python bound regex must be valid")
    });

    PYTHON_BOUND
        .captures_iter(stderr)
        .filter_map(|captures| {
            let op = captures.get(1)?.as_str();
            let version = captures.get(2)?.as_str();
            parse_inferred_lower_bound(op, version)
        })
        .reduce(|strictest, candidate| {
            if candidate.is_stricter_than(&strictest) {
                candidate
            } else {
                strictest
            }
        })
        .and_then(|lower| build_inferred_retry_constraint(&lower))
}

fn parse_inferred_lower_bound(op: &str, raw_version: &str) -> Option<InferredLowerBound> {
    let (version, precision) = parse_python_version(raw_version)?;
    let inclusive = match op {
        ">=" => true,
        ">" => false,
        _ => return None,
    };
    Some(InferredLowerBound {
        version,
        inclusive,
        precision,
    })
}

fn parse_python_version(raw: &str) -> Option<(semver::Version, VersionPrecision)> {
    let mut parts = raw.split('.');
    let major = parts.next()?.parse::<u64>().ok()?;
    let minor = match parts.next() {
        Some(part) => Some(part.parse::<u64>().ok()?),
        None => None,
    };
    let patch = match parts.next() {
        Some(part) => Some(part.parse::<u64>().ok()?),
        None => None,
    };
    if parts.next().is_some() {
        return None;
    }

    let precision = match (minor, patch) {
        (None, None) => VersionPrecision::Major,
        (Some(_), None) => VersionPrecision::MajorMinor,
        (Some(_), Some(_)) => VersionPrecision::MajorMinorPatch,
        (None, Some(_)) => return None,
    };
    let version = semver::Version::new(major, minor.unwrap_or(0), patch.unwrap_or(0));
    Some((version, precision))
}

fn build_inferred_retry_constraint(lower: &InferredLowerBound) -> Option<InferredRetryConstraint> {
    let upper_version =
        semver::Version::new(lower.version.major, lower.version.minor.checked_add(1)?, 0);
    let upper = InferredUpperBound {
        version: upper_version,
        inclusive: false,
        precision: VersionPrecision::MajorMinor,
    };

    build_retry_constraint(lower.clone(), upper)
}

fn build_retry_constraint(
    lower: InferredLowerBound,
    upper: InferredUpperBound,
) -> Option<InferredRetryConstraint> {
    let request = format!(
        "{}{},{}{}",
        lower.operator(),
        format_lower_bound_version(&lower),
        upper.operator(),
        format_upper_bound_version(&upper)
    );
    let requirement = semver::VersionReq::parse(&format!(
        "{}{},{}{}",
        lower.operator(),
        format_version_for_semver(&lower.version),
        upper.operator(),
        format_version_for_semver(&upper.version)
    ))
    .ok()?;
    let candidate = retry_candidate(&lower)?;

    if !requirement.matches(&candidate) {
        return None;
    }

    Some(InferredRetryConstraint {
        request,
        requirement,
        candidate,
        lower,
        upper,
    })
}

fn retry_candidate(lower: &InferredLowerBound) -> Option<semver::Version> {
    if lower.inclusive {
        return Some(lower.version.clone());
    }

    Some(semver::Version::new(
        lower.version.major,
        lower.version.minor,
        lower.version.patch.checked_add(1)?,
    ))
}

fn format_lower_bound_version(lower: &InferredLowerBound) -> String {
    match lower.precision {
        VersionPrecision::Major => lower.version.major.to_string(),
        VersionPrecision::MajorMinor => format!("{}.{}", lower.version.major, lower.version.minor),
        VersionPrecision::MajorMinorPatch => format!(
            "{}.{}.{}",
            lower.version.major, lower.version.minor, lower.version.patch
        ),
    }
}

fn format_upper_bound_version(upper: &InferredUpperBound) -> String {
    match upper.precision {
        VersionPrecision::Major => upper.version.major.to_string(),
        VersionPrecision::MajorMinor => {
            format!("{}.{}", upper.version.major, upper.version.minor)
        }
        VersionPrecision::MajorMinorPatch => {
            format!(
                "{}.{}.{}",
                upper.version.major, upper.version.minor, upper.version.patch
            )
        }
    }
}

fn format_version_for_semver(version: &semver::Version) -> String {
    format!("{}.{}.{}", version.major, version.minor, version.patch)
}

fn retry_request_for_language_request(
    language_request: &LanguageRequest,
    inferred: &InferredRetryConstraint,
) -> Option<String> {
    match language_request {
        LanguageRequest::Any { system_only: false }
        | LanguageRequest::Python(PythonRequest::Any) => Some(inferred.request.clone()),
        LanguageRequest::Any { system_only: true } => None,
        LanguageRequest::Python(PythonRequest::Major(major)) => {
            (inferred.candidate.major == *major).then(|| inferred.request.clone())
        }
        LanguageRequest::Python(PythonRequest::MajorMinor(major, minor)) => {
            (inferred.candidate.major == *major && inferred.candidate.minor == *minor)
                .then(|| inferred.request.clone())
        }
        LanguageRequest::Python(PythonRequest::MajorMinorPatch(major, minor, patch)) => {
            let request = format!("{major}.{minor}.{patch}");
            let version = semver::Version::new(*major, *minor, *patch);
            inferred.requirement.matches(&version).then_some(request)
        }
        LanguageRequest::Python(PythonRequest::Range(requirement, _)) => {
            let (lower, upper) = semver_range_bounds(requirement)?;
            let lower = lower
                .filter(|bound| bound.is_stricter_than(&inferred.lower))
                .unwrap_or_else(|| inferred.lower.clone());
            let upper = upper
                .filter(|bound| bound.is_stricter_than(&inferred.upper))
                .unwrap_or_else(|| inferred.upper.clone());
            build_retry_constraint(lower, upper).map(|constraint| constraint.request)
        }
        _ => None,
    }
}

fn semver_range_bounds(
    requirement: &semver::VersionReq,
) -> Option<(Option<InferredLowerBound>, Option<InferredUpperBound>)> {
    let mut lower = None;
    let mut upper = None;

    for comparator in &requirement.comparators {
        match comparator.op {
            semver::Op::Greater | semver::Op::GreaterEq => {
                let bound = comparator_lower_bound(comparator)?;
                if lower
                    .as_ref()
                    .is_none_or(|current| bound.is_stricter_than(current))
                {
                    lower = Some(bound);
                }
            }
            semver::Op::Less | semver::Op::LessEq => {
                let bound = comparator_upper_bound(comparator)?;
                if upper
                    .as_ref()
                    .is_none_or(|current| bound.is_stricter_than(current))
                {
                    upper = Some(bound);
                }
            }
            _ => return None,
        }
    }

    Some((lower, upper))
}

fn comparator_version(
    comparator: &semver::Comparator,
) -> Option<(semver::Version, VersionPrecision)> {
    let precision = match (comparator.minor, comparator.patch) {
        (None, None) => VersionPrecision::Major,
        (Some(_), None) => VersionPrecision::MajorMinor,
        (Some(_), Some(_)) => VersionPrecision::MajorMinorPatch,
        (None, Some(_)) => return None,
    };
    Some((
        semver::Version::new(
            comparator.major,
            comparator.minor.unwrap_or(0),
            comparator.patch.unwrap_or(0),
        ),
        precision,
    ))
}

fn comparator_lower_bound(comparator: &semver::Comparator) -> Option<InferredLowerBound> {
    let (version, precision) = comparator_version(comparator)?;
    Some(InferredLowerBound {
        version,
        inclusive: comparator.op == semver::Op::GreaterEq,
        precision,
    })
}

fn comparator_upper_bound(comparator: &semver::Comparator) -> Option<InferredUpperBound> {
    let (version, precision) = comparator_version(comparator)?;
    Some(InferredUpperBound {
        version,
        inclusive: comparator.op == semver::Op::LessEq,
        precision,
    })
}

impl Python {
    fn remove_uv_python_override_envs(cmd: &mut Cmd) -> &mut Cmd {
        // Ensure uv selects the hook virtualenv interpreter.
        cmd.env_remove(EnvVars::UV_PYTHON)
            .env_remove(EnvVars::UV_SYSTEM_PYTHON)
            // `--managed-python` and `--no-managed-python` conflict with our explicit preference.
            .env_remove(EnvVars::UV_MANAGED_PYTHON)
            .env_remove(EnvVars::UV_NO_MANAGED_PYTHON)
    }

    fn pip_install_command(uv: &Uv, store: &Store, env_path: &Path) -> Cmd {
        let mut cmd = uv.cmd("uv pip", store);
        cmd.arg("pip")
            .arg("install")
            // Explicitly set project to root to avoid uv searching for project-level configs.
            // `--project` has no other effect on `uv pip` subcommands.
            .args(["--project", "/"])
            .env(EnvVars::VIRTUAL_ENV, env_path);
        Self::remove_uv_python_override_envs(&mut cmd)
            // Remove GIT environment variables that may leak from git hooks (e.g., in worktrees).
            // These can break packages using setuptools_scm for file discovery.
            .remove_git_envs()
            .check(true);
        cmd
    }

    async fn install_dependencies(
        uv: &Uv,
        store: &Store,
        env_path: &Path,
        hook: &Hook,
    ) -> std::result::Result<(), process::Error> {
        let mut pip_install = Self::pip_install_command(uv, store, env_path);

        if let Some(repo_path) = hook.repo_path() {
            trace!(
                "Installing dependencies from repo path: {}",
                repo_path.display()
            );
            pip_install
                .arg("--directory")
                .arg(repo_path)
                .arg(".")
                .args(&hook.additional_dependencies)
                .output()
                .await?;
        } else if !hook.additional_dependencies.is_empty() {
            trace!(
                "Installing additional dependencies: {:?}",
                hook.additional_dependencies
            );
            pip_install
                .args(&hook.additional_dependencies)
                .output()
                .await?;
        } else {
            debug!("No dependencies to install");
        }

        Ok(())
    }

    async fn create_venv(
        uv: &Uv,
        store: &Store,
        info: &InstallInfo,
        python_request: &LanguageRequest,
    ) -> Result<()> {
        let python = to_uv_python_request(python_request);
        Self::create_venv_with_python(uv, store, info, python_request, python.as_deref(), false)
            .await
    }

    async fn create_venv_with_python(
        uv: &Uv,
        store: &Store,
        info: &InstallInfo,
        language_request: &LanguageRequest,
        python_request: Option<&str>,
        clear: bool,
    ) -> Result<()> {
        // Try creating venv without downloads first
        match Self::create_venv_command(uv, store, info, python_request, false, false, clear)
            .check(true)
            .output()
            .await
        {
            Ok(_) => {
                debug!(
                    "Venv created successfully with no downloads: `{}`",
                    info.env_path.display()
                );
                Ok(())
            }
            Err(e @ process::Error::Status { .. }) => {
                // Check if we can retry with downloads
                if Self::can_retry_with_downloads(&e) {
                    if !language_request.allows_download() {
                        anyhow::bail!(
                            "No suitable system Python version found and downloads are disabled"
                        );
                    }

                    debug!(
                        "Retrying venv creation with managed Python downloads: `{}`",
                        info.env_path.display()
                    );
                    Self::create_venv_command(uv, store, info, python_request, true, true, clear)
                        .check(true)
                        .output()
                        .await?;
                    return Ok(());
                }
                // If we can't retry, return the original error
                Err(e.into())
            }
            Err(e) => {
                debug!("Failed to create venv `{}`: {e}", info.env_path.display());
                Err(e.into())
            }
        }
    }

    fn create_venv_command(
        uv: &Uv,
        store: &Store,
        info: &InstallInfo,
        python_request: Option<&str>,
        set_install_dir: bool,
        allow_downloads: bool,
        clear: bool,
    ) -> Cmd {
        let mut cmd = uv.cmd("create venv", store);
        cmd.arg("venv")
            .arg(&info.env_path)
            .args(["--python-preference", "managed"])
            // Avoid discovering a project or workspace
            .arg("--no-project")
            // Explicitly set project to root to avoid uv searching for project-level configs
            .args(["--project", "/"]);
        if clear {
            cmd.arg("--clear");
        }
        Self::remove_uv_python_override_envs(&mut cmd);
        if set_install_dir {
            cmd.env(
                EnvVars::UV_PYTHON_INSTALL_DIR,
                store.tools_path(ToolBucket::Python),
            );
        }
        if allow_downloads {
            cmd.arg("--allow-python-downloads");
        } else {
            cmd.arg("--no-python-downloads");
        }

        if let Some(python) = python_request {
            cmd.arg("--python").arg(python);
        }

        cmd
    }

    fn can_retry_with_downloads(error: &process::Error) -> bool {
        let process::Error::Status {
            error:
                process::StatusError {
                    output: Some(output),
                    ..
                },
            ..
        } = error
        else {
            return false;
        };

        let stderr = String::from_utf8_lossy(&output.stderr);
        stderr.contains("A managed Python download is available")
    }
}

fn bin_dir(venv: &Path) -> PathBuf {
    if cfg!(windows) {
        venv.join("Scripts")
    } else {
        venv.join("bin")
    }
}

pub(crate) fn python_exec(venv: &Path) -> PathBuf {
    bin_dir(venv).join("python").with_extension(EXE_EXTENSION)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use prek_consts::env_vars::EnvVars;
    use rustc_hash::FxHashSet;

    use super::Python;
    use crate::config::Language;
    use crate::hook::InstallInfo;
    use crate::languages::python::uv::Uv;
    use crate::languages::version::LanguageRequest;
    use crate::store::Store;

    fn setup_test_install() -> (tempfile::TempDir, Uv, Store, InstallInfo) {
        let temp = tempfile::tempdir().expect("create tempdir");
        let hooks_dir = temp.path().join("hooks");
        fs_err::create_dir_all(&hooks_dir).expect("create hooks dir");

        let info = InstallInfo::new(Language::Python, FxHashSet::default(), &hooks_dir)
            .expect("create install info");
        let store = Store::from_path(temp.path().join("store"));
        let uv = Uv::new(PathBuf::from("uv"));

        (temp, uv, store, info)
    }

    fn env_map(cmd: &crate::process::Cmd) -> HashMap<String, Option<String>> {
        cmd.get_envs()
            .map(|(key, val)| {
                (
                    key.to_string_lossy().into_owned(),
                    val.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    #[test]
    fn create_venv_command_removes_uv_system_python_override() {
        let (_temp, uv, store, info) = setup_test_install();
        let request = LanguageRequest::Any { system_only: false };
        let python = super::to_uv_python_request(&request);
        let cmd =
            Python::create_venv_command(&uv, &store, &info, python.as_deref(), false, false, false);
        let envs = env_map(&cmd);

        assert_eq!(envs.get(EnvVars::UV_SYSTEM_PYTHON), Some(&None));
        assert_eq!(envs.get(EnvVars::UV_PYTHON), Some(&None));
        assert_eq!(envs.get(EnvVars::UV_MANAGED_PYTHON), Some(&None));
        assert_eq!(envs.get(EnvVars::UV_NO_MANAGED_PYTHON), Some(&None));
    }

    #[test]
    fn pip_install_command_removes_uv_system_python_override() {
        let (_temp, uv, store, info) = setup_test_install();
        let cmd = Python::pip_install_command(&uv, &store, &info.env_path);
        let envs = env_map(&cmd);

        assert_eq!(envs.get(EnvVars::UV_SYSTEM_PYTHON), Some(&None));
        assert_eq!(envs.get(EnvVars::UV_PYTHON), Some(&None));
        assert_eq!(envs.get(EnvVars::UV_MANAGED_PYTHON), Some(&None));
        assert_eq!(envs.get(EnvVars::UV_NO_MANAGED_PYTHON), Some(&None));
    }

    #[test]
    fn infer_retry_constraint_parses_python_mismatch() {
        let stderr = indoc::indoc! {r"
            × No solution found when resolving dependencies:
            ╰─▶ Because the current Python version (3.9.6) does not satisfy Python>=3.10 and example==0.0.0 depends on Python>=3.10, we can conclude that example==0.0.0 cannot be used.
        "};

        let inferred =
            super::infer_retry_constraint(stderr).expect("should infer retry constraint");
        assert_eq!(inferred.request, ">=3.10,<3.11");
    }

    #[test]
    fn infer_retry_constraint_parses_wrapped_python_mismatch() {
        let stderr = indoc::indoc! {r"
            × No solution found when resolving dependencies:
            ╰─▶ Because the current Python version (3.9.6) does not satisfy
                  Python>=3.10 and example==0.0.0 depends on Python>=3.10, we can conclude that example==0.0.0 cannot be used.
        "};

        let inferred =
            super::infer_retry_constraint(stderr).expect("should infer retry constraint");
        assert_eq!(inferred.request, ">=3.10,<3.11");
    }

    #[test]
    fn infer_retry_constraint_uses_next_minor_cap_for_major_only_bound() {
        let stderr = indoc::indoc! {r"
            × No solution found when resolving dependencies:
            ╰─▶ Because the current Python version (2.7.18) does not satisfy Python>=3 and example==0.0.0 depends on Python>=3, we can conclude that example==0.0.0 cannot be used.
        "};

        let inferred =
            super::infer_retry_constraint(stderr).expect("should infer retry constraint");
        assert_eq!(inferred.request, ">=3,<3.1");
    }

    #[test]
    fn infer_retry_constraint_uses_strictest_lower_bound() {
        let stderr = indoc::indoc! {r"
            × No solution found when resolving dependencies:
            ╰─▶ Because the current Python version (3.9.6) does not satisfy Python>=3.10 and package-a==1.0.0 depends on Python>=3.10, we can conclude that package-a==1.0.0 cannot be used.
                Because the current Python version (3.9.6) does not satisfy Python>3.11 and package-b==2.0.0 depends on Python>3.11, we can conclude that package-b==2.0.0 cannot be used.
        "};

        let inferred =
            super::infer_retry_constraint(stderr).expect("should infer retry constraint");
        assert_eq!(inferred.request, ">3.11,<3.12");
    }

    #[test]
    fn infer_retry_constraint_ignores_non_python_resolution_errors() {
        let stderr = indoc::indoc! {r"
            × No solution found when resolving dependencies:
            ╰─▶ Because package-a==1.0.0 depends on package-b==1.0.0 and package-b==2.0.0, we can conclude that package-a==1.0.0 cannot be used.
        "};

        assert!(super::infer_retry_constraint(stderr).is_none());
    }

    #[test]
    fn retry_request_respects_configured_python_request() {
        let inferred = super::infer_retry_constraint(
            "Because the current Python version (3.9.6) does not satisfy Python>=3.10 and x depends on Python>=3.10.",
        )
        .expect("should infer retry constraint");

        let any = LanguageRequest::Any { system_only: false };
        assert_eq!(
            super::retry_request_for_language_request(&any, &inferred),
            Some(">=3.10,<3.11".to_string())
        );

        let system = LanguageRequest::Any { system_only: true };
        assert_eq!(
            super::retry_request_for_language_request(&system, &inferred),
            None
        );

        let compatible_major = LanguageRequest::parse(Language::Python, "3").expect("valid major");
        assert_eq!(
            super::retry_request_for_language_request(&compatible_major, &inferred),
            Some(">=3.10,<3.11".to_string())
        );

        let incompatible_pin =
            LanguageRequest::parse(Language::Python, "3.9").expect("valid major.minor");
        assert_eq!(
            super::retry_request_for_language_request(&incompatible_pin, &inferred),
            None
        );

        let compatible_range =
            LanguageRequest::parse(Language::Python, ">=3.10,<3.10.5").expect("valid range");
        assert_eq!(
            super::retry_request_for_language_request(&compatible_range, &inferred),
            Some(">=3.10,<3.10.5".to_string())
        );

        let incompatible_range =
            LanguageRequest::parse(Language::Python, "<3.10").expect("valid range");
        assert_eq!(
            super::retry_request_for_language_request(&incompatible_range, &inferred),
            None
        );
    }

    #[test]
    fn retry_request_treats_major_minor_as_range_for_exclusive_bound() {
        let inferred = super::infer_retry_constraint(
            "Because the current Python version (3.9.6) does not satisfy Python>3.10 and x depends on Python>3.10.",
        )
        .expect("should infer retry constraint");

        let request = LanguageRequest::parse(Language::Python, "3.10").expect("valid request");
        assert_eq!(
            super::retry_request_for_language_request(&request, &inferred),
            Some(">3.10,<3.11".to_string())
        );
    }

    #[test]
    fn retry_request_keeps_explicit_patch_pin() {
        let inferred = super::infer_retry_constraint(
            "Because the current Python version (3.9.6) does not satisfy Python>=3.10 and x depends on Python>=3.10.",
        )
        .expect("should infer retry constraint");

        let request = LanguageRequest::parse(Language::Python, "3.10.5").expect("valid request");
        assert_eq!(
            super::retry_request_for_language_request(&request, &inferred),
            Some("3.10.5".to_string())
        );
    }

    #[test]
    fn retry_request_refuses_unsupported_range_intersection() {
        let inferred = super::infer_retry_constraint(
            "Because the current Python version (3.9.6) does not satisfy Python>=3.10 and x depends on Python>=3.10.",
        )
        .expect("should infer retry constraint");

        let request = LanguageRequest::parse(Language::Python, "^3.10").expect("valid range");
        assert_eq!(
            super::retry_request_for_language_request(&request, &inferred),
            None
        );
    }
}

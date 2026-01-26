use std::borrow::Cow;
use std::collections::HashSet;
use std::error::Error as StdError;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str::Utf8Error;
use std::sync::LazyLock;

use anyhow::Result;
use owo_colors::OwoColorize;
use path_clean::PathClean;
use prek_consts::env_vars::EnvVars;
use rustc_hash::FxHashSet;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, instrument, warn};

use crate::process;
use crate::process::{Cmd, StatusError};

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error(transparent)]
    Command(#[from] process::Error),

    #[error("Failed to find git: {0}")]
    GitNotFound(#[from] which::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    UTF8(#[from] Utf8Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthHintKind {
    Auth,
    RepoNotFound,
}

/// FAQ URL for private repository authentication.
pub(crate) const PRIVATE_REPO_FAQ_URL: &str =
    "https://prek.j178.dev/faq#how-do-i-use-hooks-from-private-repositories";

/// Hint message for authentication failures.
pub(crate) const AUTH_HINT: &str = "For private repos, see:";

/// Hint message for repository not found errors.
pub(crate) const REPO_NOT_FOUND_HINT: &str =
    "Repo not found. The URL or revision may be wrong, or the repo may be private. See:";

/// Format an auth hint suffix for error messages.
pub(crate) fn auth_hint_suffix(kind: Option<AuthHintKind>) -> String {
    match kind {
        Some(AuthHintKind::Auth) => {
            format!(
                "\n\n{} {} {}",
                "hint:".yellow().bold(),
                AUTH_HINT,
                PRIVATE_REPO_FAQ_URL,
            )
        }
        Some(AuthHintKind::RepoNotFound) => {
            format!(
                "\n\n{} {} {}",
                "hint:".yellow().bold(),
                REPO_NOT_FOUND_HINT,
                PRIVATE_REPO_FAQ_URL,
            )
        }
        None => String::new(),
    }
}

impl Error {
    pub(crate) fn auth_hint_kind(&self) -> Option<AuthHintKind> {
        match self {
            Self::Command(error) => error.auth_hint_kind(),
            _ => None,
        }
    }

    pub(crate) fn is_auth_error(&self) -> bool {
        matches!(self.auth_hint_kind(), Some(AuthHintKind::Auth))
    }

    pub(crate) fn is_repo_not_found(&self) -> bool {
        matches!(self.auth_hint_kind(), Some(AuthHintKind::RepoNotFound))
    }
}

pub(crate) static GIT: LazyLock<Result<PathBuf, which::Error>> =
    LazyLock::new(|| which::which("git"));

pub(crate) static GIT_ROOT: LazyLock<Result<PathBuf, Error>> = LazyLock::new(|| {
    get_root().inspect(|root| {
        debug!("Git root: {}", root.display());
    })
});

/// Remove some `GIT_` environment variables exposed by `git`.
///
/// For some commands, like `git commit -a` or `git commit -p`, git creates a `.git/index.lock` file
/// and set `GIT_INDEX_FILE` to point to it.
/// We need to keep the `GIT_INDEX_FILE` env var to make sure `git write-tree` works correctly.
/// <https://stackoverflow.com/questions/65639403/git-pre-commit-hook-how-can-i-get-added-modified-files-when-commit-with-a-flag/65647202#65647202>
pub(crate) static GIT_ENV_TO_REMOVE: LazyLock<Vec<(String, String)>> = LazyLock::new(|| {
    let keep = &[
        "GIT_EXEC_PATH",
        "GIT_SSH",
        "GIT_SSH_COMMAND",
        "GIT_SSL_CAINFO",
        "GIT_SSL_NO_VERIFY",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_PARAMETERS",
        "GIT_HTTP_PROXY_AUTHMETHOD",
        "GIT_ALLOW_PROTOCOL",
        "GIT_ASKPASS",
    ];

    std::env::vars()
        .filter(|(k, _)| {
            k.starts_with("GIT_")
                && !k.starts_with("GIT_CONFIG_KEY_")
                && !k.starts_with("GIT_CONFIG_VALUE_")
                && !keep.contains(&k.as_str())
        })
        .collect()
});

pub(crate) fn auth_hint_kind_from_output(output: &std::process::Output) -> Option<AuthHintKind> {
    let stderr = String::from_utf8_lossy(&output.stderr);
    if is_auth_error_message(&stderr) {
        Some(AuthHintKind::Auth)
    } else if is_repo_not_found_message(&stderr) {
        Some(AuthHintKind::RepoNotFound)
    } else {
        None
    }
}

pub(crate) fn auth_hint_kind_from_error(error: &(dyn StdError + 'static)) -> Option<AuthHintKind> {
    let mut current = Some(error);
    while let Some(err) = current {
        if let Some(git_error) = err.downcast_ref::<Error>() {
            if let Some(kind) = git_error.auth_hint_kind() {
                return Some(kind);
            }
        }

        if let Some(process_error) = err.downcast_ref::<crate::process::Error>() {
            if let Some(kind) = process_error.auth_hint_kind() {
                return Some(kind);
            }
        }

        current = err.source();
    }

    None
}

pub(crate) fn is_auth_error_message(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    let patterns = [
        "terminal prompts disabled",
        "could not read username",
        "could not read password",
        "authentication failed",
        "permission denied (publickey)",
        "invalid username or token",
        "returned error: 401",
        "returned error: 403",
    ];

    patterns.iter().any(|pattern| stderr.contains(pattern))
}

pub(crate) fn is_repo_not_found_message(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    stderr.contains("returned error: 404")
        || (stderr.contains("repository") && stderr.contains("not found"))
}

pub(crate) fn git_cmd(summary: &str) -> Result<Cmd, Error> {
    let mut cmd = Cmd::new(GIT.as_ref().map_err(|&e| Error::GitNotFound(e))?, summary);
    cmd.arg("-c").arg("core.useBuiltinFSMonitor=false");

    Ok(cmd)
}

fn zsplit(s: &[u8]) -> Result<Vec<PathBuf>, Utf8Error> {
    s.split(|&b| b == b'\0')
        .filter(|slice| !slice.is_empty())
        .map(|slice| str::from_utf8(slice).map(PathBuf::from))
        .collect()
}

pub(crate) async fn intent_to_add_files(root: &Path) -> Result<Vec<PathBuf>, Error> {
    let output = git_cmd("get intent to add files")?
        .arg("diff")
        .arg("--no-ext-diff")
        .arg("--ignore-submodules")
        .arg("--diff-filter=A")
        .arg("--name-only")
        .arg("-z")
        .arg("--")
        .arg(root)
        .check(true)
        .output()
        .await?;
    Ok(zsplit(&output.stdout)?)
}

pub(crate) async fn get_added_files(root: &Path) -> Result<Vec<PathBuf>, Error> {
    let output = git_cmd("get added files")?
        .current_dir(root)
        .arg("diff")
        .arg("--staged")
        .arg("--name-only")
        .arg("--diff-filter=A")
        .arg("-z") // Use NUL as line terminator
        .check(true)
        .output()
        .await?;
    Ok(zsplit(&output.stdout)?)
}

pub(crate) async fn get_changed_files(
    old: &str,
    new: &str,
    root: &Path,
) -> Result<Vec<PathBuf>, Error> {
    let build_cmd = |range: String| -> Result<Cmd, Error> {
        let mut cmd = git_cmd("get changed files")?;
        cmd.arg("diff")
            .arg("--name-only")
            .arg("--diff-filter=ACMRT")
            .arg("--no-ext-diff") // Disable external diff drivers
            .arg("-z") // Use NUL as line terminator
            .arg(range)
            .arg("--")
            .arg(root);
        Ok(cmd)
    };

    // Try three-dot syntax first (merge-base diff), which works for commits
    let output = build_cmd(format!("{old}...{new}"))?
        .check(false)
        .output()
        .await?;

    if output.status.success() {
        return Ok(zsplit(&output.stdout)?);
    }

    // Fall back to two-dot syntax, which works with both commits and trees
    let output = build_cmd(format!("{old}..{new}"))?
        .check(true)
        .output()
        .await?;
    Ok(zsplit(&output.stdout)?)
}

#[instrument(level = "trace")]
pub(crate) async fn ls_files(cwd: &Path, path: &Path) -> Result<Vec<PathBuf>, Error> {
    let output = git_cmd("git ls-files")?
        .current_dir(cwd)
        .arg("ls-files")
        .arg("-z")
        .arg("--")
        .arg(path)
        .check(true)
        .output()
        .await?;

    Ok(zsplit(&output.stdout)?)
}

pub(crate) async fn get_git_dir() -> Result<PathBuf, Error> {
    let output = git_cmd("get git dir")?
        .arg("rev-parse")
        .arg("--git-dir")
        .check(true)
        .output()
        .await?;
    Ok(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim_ascii(),
    ))
}

pub(crate) async fn get_git_common_dir() -> Result<PathBuf, Error> {
    let output = git_cmd("get git common dir")?
        .arg("rev-parse")
        .arg("--git-common-dir")
        .check(true)
        .output()
        .await?;
    if output.stdout.trim_ascii().is_empty() {
        Ok(get_git_dir().await?)
    } else {
        Ok(PathBuf::from(
            String::from_utf8_lossy(&output.stdout).trim_ascii(),
        ))
    }
}

pub(crate) async fn get_staged_files(root: &Path) -> Result<Vec<PathBuf>, Error> {
    let output = git_cmd("get staged files")?
        .current_dir(root)
        .arg("diff")
        .arg("--cached")
        .arg("--name-only")
        .arg("--diff-filter=ACMRTUXB") // Everything except for D
        .arg("--no-ext-diff") // Disable external diff drivers
        .arg("-z") // Use NUL as line terminator
        .check(true)
        .output()
        .await?;
    Ok(zsplit(&output.stdout)?)
}

pub(crate) async fn files_not_staged(files: &[&Path]) -> Result<Vec<PathBuf>> {
    let output = git_cmd("git diff")?
        .arg("diff")
        .arg("--exit-code")
        .arg("--name-only")
        .arg("--no-ext-diff")
        .arg("-z") // Use NUL as line terminator
        .args(files)
        .check(false)
        .output()
        .await?;

    if output.status.code().is_some_and(|code| code == 1) {
        return Ok(zsplit(&output.stdout)?);
    }

    Ok(vec![])
}

pub(crate) async fn has_unmerged_paths() -> Result<bool, Error> {
    let output = git_cmd("check has unmerged paths")?
        .arg("ls-files")
        .arg("--unmerged")
        .check(true)
        .output()
        .await?;
    Ok(!output.stdout.trim_ascii().is_empty())
}

pub(crate) async fn has_diff(rev: &str, path: &Path) -> Result<bool> {
    let status = git_cmd("check diff")?
        .arg("diff")
        .arg("--quiet")
        .arg(rev)
        .current_dir(path)
        .check(false)
        .status()
        .await?;
    Ok(status.code() == Some(1))
}

pub(crate) async fn is_in_merge_conflict() -> Result<bool, Error> {
    let git_dir = get_git_dir().await?;
    Ok(git_dir.join("MERGE_HEAD").try_exists()? && git_dir.join("MERGE_MSG").try_exists()?)
}

pub(crate) async fn get_conflicted_files(root: &Path) -> Result<Vec<PathBuf>, Error> {
    let tree = git_cmd("git write-tree")?
        .arg("write-tree")
        .check(true)
        .output()
        .await?;

    let output = git_cmd("get conflicted files")?
        .arg("diff")
        .arg("--name-only")
        .arg("--no-ext-diff") // Disable external diff drivers
        .arg("-z") // Use NUL as line terminator
        .arg("-m") // Show diffs for merge commits in the default format.
        .arg(String::from_utf8_lossy(&tree.stdout).trim_ascii())
        .arg("HEAD")
        .arg("MERGE_HEAD")
        .arg("--")
        .arg(root)
        .check(true)
        .output()
        .await?;

    Ok(zsplit(&output.stdout)?
        .into_iter()
        .chain(parse_merge_msg_for_conflicts().await?)
        .collect::<HashSet<PathBuf>>()
        .into_iter()
        .collect())
}

async fn parse_merge_msg_for_conflicts() -> Result<Vec<PathBuf>, Error> {
    let git_dir = get_git_dir().await?;
    let merge_msg = git_dir.join("MERGE_MSG");
    let content = fs_err::tokio::read_to_string(&merge_msg).await?;
    let conflicts = content
        .lines()
        // Conflicted files start with tabs
        .filter(|line| line.starts_with('\t') || line.starts_with("#\t"))
        .map(|line| line.trim_start_matches('#').trim().to_string())
        .map(PathBuf::from)
        .collect();

    Ok(conflicts)
}

#[instrument(level = "trace")]
pub(crate) async fn get_diff(path: &Path) -> Result<Vec<u8>, Error> {
    let output = git_cmd("git diff")?
        .arg("diff")
        .arg("--no-ext-diff") // Disable external diff drivers
        .arg("--no-textconv")
        .arg("--ignore-submodules")
        .arg("--")
        .arg(path)
        .check(true)
        .output()
        .await?;
    Ok(output.stdout)
}

/// Create a tree object from the current index.
///
/// The name of the new tree object is printed to standard output.
/// The index must be in a fully merged state.
pub(crate) async fn write_tree() -> Result<String, Error> {
    let output = git_cmd("git write-tree")?
        .arg("write-tree")
        .check(true)
        .output()
        .await?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_ascii()
        .to_string())
}

/// Get the path of the top-level directory of the working tree.
#[instrument(level = "trace")]
pub(crate) fn get_root() -> Result<PathBuf, Error> {
    let git = GIT.as_ref().map_err(|&e| Error::GitNotFound(e))?;
    let output = std::process::Command::new(git)
        .arg("rev-parse")
        .arg("--show-toplevel")
        .output()?;
    if !output.status.success() {
        return Err(Error::Command(process::Error::Status {
            summary: "get git root".to_string(),
            error: StatusError {
                status: output.status,
                output: Some(output),
            },
        }));
    }

    Ok(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim_ascii(),
    ))
}

pub(crate) async fn init_repo(url: &str, path: &Path) -> Result<(), Error> {
    let url = if Path::new(url).is_dir() {
        // If the URL is a local path, convert it to an absolute path
        Cow::Owned(
            std::path::absolute(url)?
                .clean()
                .to_string_lossy()
                .to_string(),
        )
    } else {
        Cow::Borrowed(url)
    };

    git_cmd("init git repo")?
        // Unset `extensions.objectFormat` if set, just follow what hash the remote uses.
        .arg("-c")
        .arg("init.defaultObjectFormat=")
        .arg("init")
        .arg("--template=")
        .arg(path)
        .remove_git_envs()
        .check(true)
        .output()
        .await?;

    git_cmd("add git remote")?
        .current_dir(path)
        .arg("remote")
        .arg("add")
        .arg("origin")
        .arg(&*url)
        .remove_git_envs()
        .check(true)
        .output()
        .await?;

    Ok(())
}

async fn shallow_clone(rev: &str, path: &Path) -> Result<(), Error> {
    git_cmd("git shallow clone")?
        .current_dir(path)
        .arg("-c")
        .arg("protocol.version=2")
        .arg("fetch")
        .arg("origin")
        .arg(rev)
        .arg("--depth=1")
        // Disable interactive prompts in the terminal, as they'll be erased by the progress bar
        // animation and the process will "hang".
        .env(EnvVars::GIT_TERMINAL_PROMPT, "0")
        .remove_git_envs()
        .check(true)
        .output()
        .await?;

    git_cmd("git checkout")?
        .current_dir(path)
        .arg("checkout")
        .arg("FETCH_HEAD")
        .remove_git_envs()
        .env(EnvVars::PREK_INTERNAL__SKIP_POST_CHECKOUT, "1")
        .check(true)
        .output()
        .await?;

    git_cmd("update git submodules")?
        .current_dir(path)
        .arg("-c")
        .arg("protocol.version=2")
        .arg("submodule")
        .arg("update")
        .arg("--init")
        .arg("--recursive")
        .arg("--depth=1")
        .env(EnvVars::GIT_TERMINAL_PROMPT, "0")
        .remove_git_envs()
        .check(true)
        .output()
        .await?;

    Ok(())
}

async fn full_clone(rev: &str, path: &Path) -> Result<(), Error> {
    git_cmd("git full clone")?
        .current_dir(path)
        .arg("fetch")
        .arg("origin")
        .arg("--tags")
        .env(EnvVars::GIT_TERMINAL_PROMPT, "0")
        .remove_git_envs()
        .check(true)
        .output()
        .await?;

    git_cmd("git checkout")?
        .current_dir(path)
        .arg("checkout")
        .arg(rev)
        .env(EnvVars::PREK_INTERNAL__SKIP_POST_CHECKOUT, "1")
        .remove_git_envs()
        .check(true)
        .output()
        .await?;

    git_cmd("update git submodules")?
        .current_dir(path)
        .arg("submodule")
        .arg("update")
        .arg("--init")
        .arg("--recursive")
        .env(EnvVars::GIT_TERMINAL_PROMPT, "0")
        .remove_git_envs()
        .check(true)
        .output()
        .await?;

    Ok(())
}

pub(crate) async fn clone_repo(url: &str, rev: &str, path: &Path) -> Result<(), Error> {
    init_repo(url, path).await?;

    if let Err(err) = shallow_clone(rev, path).await {
        warn!(?err, "Failed to shallow clone, falling back to full clone");
        full_clone(rev, path).await
    } else {
        Ok(())
    }
}

pub(crate) async fn has_hooks_path_set() -> Result<bool> {
    let output = git_cmd("get git hooks path")?
        .arg("config")
        .arg("--get")
        .arg("core.hooksPath")
        .check(false)
        .output()
        .await?;
    if output.status.success() {
        Ok(!output.stdout.trim_ascii().is_empty())
    } else {
        Ok(false)
    }
}

pub(crate) async fn get_lfs_files(paths: &[&Path]) -> Result<FxHashSet<PathBuf>, Error> {
    if paths.is_empty() {
        return Ok(FxHashSet::default());
    }

    let mut child = git_cmd("git check-attr")?
        .arg("check-attr")
        .arg("filter")
        .arg("-z")
        .arg("--stdin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .check(true)
        .spawn()?;

    let mut stdout = child.stdout.take().expect("failed to open stdout");
    let mut stdin = child.stdin.take().expect("failed to open stdin");

    let writer = async move {
        for path in paths {
            stdin.write_all(path.to_string_lossy().as_bytes()).await?;
            stdin.write_all(b"\0").await?;
        }
        stdin.shutdown().await?;
        Ok::<(), std::io::Error>(())
    };
    let reader = async move {
        let mut out = Vec::new();
        stdout.read_to_end(&mut out).await?;
        Ok::<_, std::io::Error>(out)
    };

    let (read_result, _write_result) = tokio::try_join!(biased; reader, writer)?;

    let status = child.wait().await?;
    if !status.success() {
        return Err(Error::Command(process::Error::Status {
            summary: "git check-attr".to_string(),
            error: StatusError {
                status,
                output: None,
            },
        }));
    }

    let mut lfs_files = FxHashSet::default();
    let read_result = String::from_utf8_lossy(&read_result);
    let mut it = read_result.split_terminator('\0');
    loop {
        let (Some(file), Some(_attr), Some(value)) = (it.next(), it.next(), it.next()) else {
            break;
        };
        if value == "lfs" {
            lfs_files.insert(PathBuf::from(file));
        }
    }

    Ok(lfs_files)
}

/// Check if a git revision exists
pub(crate) async fn rev_exists(rev: &str) -> Result<bool, Error> {
    let output = git_cmd("git cat-file")?
        .arg("cat-file")
        // Exit with zero status if <object> exists and is a valid object.
        .arg("-e")
        .arg(rev)
        .check(false)
        .output()
        .await?;
    Ok(output.status.success())
}

/// Get commits that are ancestors of the given commit but not in the specified remote
pub(crate) async fn get_ancestors_not_in_remote(
    local_sha: &str,
    remote_name: &str,
) -> Result<Vec<String>, Error> {
    let output = git_cmd("get ancestors not in remote")?
        .arg("rev-list")
        .arg(local_sha)
        .arg("--topo-order")
        .arg("--reverse")
        .arg("--not")
        .arg(format!("--remotes={remote_name}"))
        .check(true)
        .output()
        .await?;
    Ok(str::from_utf8(&output.stdout)?
        .trim_ascii()
        .lines()
        .map(ToString::to_string)
        .collect())
}

/// Get root commits (commits with no parents) for the given commit
pub(crate) async fn get_root_commits(local_sha: &str) -> Result<FxHashSet<String>, Error> {
    let output = git_cmd("get root commits")?
        .arg("rev-list")
        .arg("--max-parents=0")
        .arg(local_sha)
        .check(true)
        .output()
        .await?;
    Ok(str::from_utf8(&output.stdout)?
        .trim_ascii()
        .lines()
        .map(ToString::to_string)
        .collect())
}

/// Get the parent commit of the given commit
pub(crate) async fn get_parent_commit(commit: &str) -> Result<Option<String>, Error> {
    let output = git_cmd("get parent commit")?
        .arg("rev-parse")
        .arg(format!("{commit}^"))
        .check(false)
        .output()
        .await?;
    if output.status.success() {
        Ok(Some(
            str::from_utf8(&output.stdout)?.trim_ascii().to_string(),
        ))
    } else {
        Ok(None)
    }
}

/// Return a list of absolute paths of all git submodules in the repository.
#[instrument(level = "trace")]
pub(crate) fn list_submodules(git_root: &Path) -> Result<Vec<PathBuf>, Error> {
    if !git_root.join(".gitmodules").exists() {
        return Ok(vec![]);
    }

    let git = GIT.as_ref().map_err(|&e| Error::GitNotFound(e))?;
    let output = std::process::Command::new(git)
        .current_dir(git_root)
        .arg("config")
        .arg("--file")
        .arg(".gitmodules")
        .arg("--get-regexp")
        .arg(r"^submodule\..*\.path$")
        .output()?;

    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_ascii()
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .map(|submodule| git_root.join(submodule))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_auth_error_message_matches_common_failures() {
        let samples = [
            "fatal: could not read Username for 'https://example.com': terminal prompts disabled",
            "fatal: could not read Password for 'https://example.com': terminal prompts disabled",
            "remote: Authentication failed for 'https://example.com/org/repo'",
            "Permission denied (publickey).",
            "remote: Invalid username or token.",
            "The requested URL returned error: 401",
            "The requested URL returned error: 403",
        ];

        for sample in samples {
            assert!(
                is_auth_error_message(sample),
                "sample should match: {sample}"
            );
        }
    }

    #[test]
    fn is_repo_not_found_message_matches_common_failures() {
        let samples = [
            "fatal: repository 'https://example.com/org/repo' not found",
            "The requested URL returned error: 404",
            "Repository not found",
        ];

        for sample in samples {
            assert!(
                is_repo_not_found_message(sample),
                "sample should match: {sample}"
            );
        }
    }

    #[test]
    fn is_auth_error_message_ignores_unrelated_errors() {
        assert!(!is_auth_error_message(
            "fatal: bad object deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
        ));
    }

    #[test]
    fn is_repo_not_found_message_ignores_unrelated_errors() {
        assert!(!is_repo_not_found_message(
            "fatal: could not read Username for 'https://example.com': terminal prompts disabled"
        ));
    }
}

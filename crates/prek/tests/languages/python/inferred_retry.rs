use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::LazyLock;

use anyhow::Result;
use assert_fs::fixture::{FileWriteStr, PathChild};
use prek_consts::PRE_COMMIT_HOOKS_YAML;
use prek_consts::env_vars::EnvVars;
use prek_consts::prepend_paths;

use crate::common::TestContext;

static FAKE_UV_BIN_DIR: LazyLock<PathBuf> = LazyLock::new(|| {
    let dir = std::env::temp_dir().join(format!(
        "prek-fake-uv-inferred-retry-{}",
        std::process::id()
    ));
    fs_err::create_dir_all(&dir).expect("create fake uv dir");

    let uv = dir.join("uv");
    fs_err::write(
        &uv,
        indoc::indoc! {r#"
            #!/bin/sh
            set -eu

            : "${PREK_FAKE_UV_LOG:?missing PREK_FAKE_UV_LOG}"
            : "${PREK_FAKE_UV_STATE:?missing PREK_FAKE_UV_STATE}"

            {
              for arg in "$@"; do
                printf '%s\t' "$arg"
              done
              printf '\n'
            } >> "$PREK_FAKE_UV_LOG"

            if [ "${1-}" = "--version" ]; then
              echo "uv 0.11.14"
              exit 0
            fi

            command="${1-}"
            if [ "$#" -gt 0 ]; then
              shift
            fi

            case "$command" in
              venv)
                env_path=""
                while [ "$#" -gt 0 ]; do
                  case "$1" in
                    --python)
                      shift
                      ;;
                    --*)
                      ;;
                    *)
                      if [ -z "$env_path" ]; then
                        env_path="$1"
                      fi
                      ;;
                  esac
                  if [ "$#" -gt 0 ]; then
                    shift
                  fi
                done

                mkdir -p "$env_path/bin"
                cat > "$env_path/bin/python" <<'PYEOF'
            #!/bin/sh
            set -eu
            base_exec_prefix="$(cd "$(dirname "$0")/.." && pwd)"
            if [ "${1-}" = "-I" ]; then
              shift
            fi
            if [ "${1-}" = "-c" ]; then
              echo "{\"version\":\"3.10.0\",\"base_exec_prefix\":\"$base_exec_prefix\"}"
              exit 0
            fi
            echo "unsupported fake python invocation: $*" >&2
            exit 2
            PYEOF
                chmod +x "$env_path/bin/python"
                ;;
              pip)
                count=0
                if [ -f "$PREK_FAKE_UV_STATE" ]; then
                  count="$(cat "$PREK_FAKE_UV_STATE")"
                fi
                count=$((count + 1))
                printf '%s' "$count" > "$PREK_FAKE_UV_STATE"

                if [ "${PREK_FAKE_UV_MODE:-retry-succeeds}" = "retry-succeeds" ] && [ "$count" -gt 1 ]; then
                  exit 0
                fi

                bound="${PREK_FAKE_UV_ERROR_BOUND:->=3.10}"
                if [ "${PREK_FAKE_UV_WRAP_BOUND:-0}" = "1" ]; then
                  cat >&2 <<ERREOF
            × No solution found when resolving dependencies:
            ╰─▶ Because the current Python version (3.9.6) does not satisfy
                  Python${bound} and example==0.0.0 depends on Python${bound}, we can conclude that example==0.0.0 cannot be used.
            ERREOF
                else
                  cat >&2 <<ERREOF
            × No solution found when resolving dependencies:
            ╰─▶ Because the current Python version (3.9.6) does not satisfy Python${bound} and example==0.0.0 depends on Python${bound}, we can conclude that example==0.0.0 cannot be used.
            ERREOF
                fi
                exit 1
                ;;
              *)
                echo "unsupported fake uv command: $command" >&2
                exit 2
                ;;
            esac
        "#},
    )
    .expect("write fake uv");

    let mut perms = fs_err::metadata(&uv)
        .expect("fake uv metadata")
        .permissions();
    perms.set_mode(0o755);
    fs_err::set_permissions(&uv, perms).expect("make fake uv executable");

    dir
});

fn configure_hook(context: &TestContext, language_version: Option<&str>) {
    let language_version = language_version
        .map(|version| format!("        language_version: '{version}'\n"))
        .unwrap_or_default();
    context.write_pre_commit_config(&format!(
        r#"
repos:
  - repo: local
    hooks:
      - id: local
        name: local
        language: python
{language_version}        entry: python -c 'print("ok")'
        additional_dependencies: ["example==0.0.0"]
        always_run: true
        pass_filenames: false
"#
    ));
}

fn configure_remote_hook(context: &TestContext, repo: &TestContext) -> Result<()> {
    repo.work_dir()
        .child(PRE_COMMIT_HOOKS_YAML)
        .write_str(indoc::indoc! {r#"
            - id: remote-python
              name: remote-python
              language: python
              entry: python -c 'print("ok")'
              additional_dependencies: ["example==0.0.0"]
              always_run: true
              pass_filenames: false
        "#})?;
    repo.git_add(".");
    repo.git_commit("Add python hook");
    repo.git_tag("v0.1.0");

    context.write_pre_commit_config(&indoc::formatdoc! {r#"
        repos:
          - repo: {}
            rev: v0.1.0
            hooks:
              - id: remote-python
    "#, repo.work_dir().display()});

    Ok(())
}

struct FakeUv {
    path: OsString,
    log_path: PathBuf,
    state_path: PathBuf,
}

impl FakeUv {
    fn new(context: &TestContext) -> Result<Self> {
        let log_path = context.work_dir().child("fake-uv.log").to_path_buf();
        let state_path = context.work_dir().child("fake-uv.state").to_path_buf();

        fs_err::write(&log_path, "")?;
        fs_err::write(&state_path, "0")?;

        Ok(Self {
            path: prepend_paths(&[FAKE_UV_BIN_DIR.as_path()])?,
            log_path,
            state_path,
        })
    }

    fn command_log(&self) -> Result<Vec<String>> {
        Ok(fs_err::read_to_string(&self.log_path)?
            .lines()
            .map(ToString::to_string)
            .collect())
    }
}

#[test]
fn inferred_retry_recreates_venv_and_retries_install() -> Result<()> {
    let context = TestContext::new();
    context.init_project();
    configure_hook(&context, None);
    context.git_add(".");

    let fake_uv = FakeUv::new(&context)?;
    let output = context
        .command()
        .arg("install-hooks")
        .env(EnvVars::PATH, &fake_uv.path)
        .env("PREK_FAKE_UV_LOG", &fake_uv.log_path)
        .env("PREK_FAKE_UV_STATE", &fake_uv.state_path)
        .env("PREK_FAKE_UV_WRAP_BOUND", "1")
        .output()?;

    assert!(
        output.status.success(),
        "expected install-hooks success, stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let commands = fake_uv.command_log()?;
    let pip_install_calls = commands
        .iter()
        .filter(|command| command.starts_with("pip\tinstall\t"))
        .count();
    assert_eq!(pip_install_calls, 2, "commands: {commands:?}");
    assert!(
        commands.iter().any(|command| {
            command.starts_with("venv\t")
                && command.contains("\t--clear\t")
                && command.contains("\t--python\t>=3.10,<3.11\t")
        }),
        "commands: {commands:?}"
    );

    Ok(())
}

#[test]
fn inferred_retry_retries_remote_repo_install() -> Result<()> {
    let repo = TestContext::new();
    repo.init_project();

    let context = TestContext::new();
    context.init_project();
    configure_remote_hook(&context, &repo)?;
    context.git_add(".");

    let fake_uv = FakeUv::new(&context)?;
    let output = context
        .command()
        .arg("install-hooks")
        .env(EnvVars::PATH, &fake_uv.path)
        .env("PREK_FAKE_UV_LOG", &fake_uv.log_path)
        .env("PREK_FAKE_UV_STATE", &fake_uv.state_path)
        .output()?;

    assert!(
        output.status.success(),
        "expected install-hooks success, stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let commands = fake_uv.command_log()?;
    let remote_installs = commands
        .iter()
        .filter(|command| {
            command.starts_with("pip\tinstall\t")
                && command.contains("\t--directory\t")
                && command.contains("\t.\t")
        })
        .count();
    assert_eq!(remote_installs, 2, "commands: {commands:?}");
    assert!(
        commands.iter().any(|command| {
            command.starts_with("venv\t")
                && command.contains("\t--clear\t")
                && command.contains("\t--python\t>=3.10,<3.11\t")
        }),
        "commands: {commands:?}"
    );

    Ok(())
}

#[test]
fn system_language_version_does_not_retry_with_inferred_constraint() -> Result<()> {
    let context = TestContext::new();
    context.init_project();
    configure_hook(&context, Some("system"));
    context.git_add(".");

    let fake_uv = FakeUv::new(&context)?;
    let output = context
        .command()
        .arg("install-hooks")
        .env(EnvVars::PATH, &fake_uv.path)
        .env("PREK_FAKE_UV_LOG", &fake_uv.log_path)
        .env("PREK_FAKE_UV_STATE", &fake_uv.state_path)
        .output()?;

    assert!(
        !output.status.success(),
        "expected install-hooks failure, stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let commands = fake_uv.command_log()?;
    let pip_install_calls = commands
        .iter()
        .filter(|command| command.starts_with("pip\tinstall\t"))
        .count();
    assert_eq!(pip_install_calls, 1, "commands: {commands:?}");
    assert!(
        !commands
            .iter()
            .any(|command| command.starts_with("venv\t") && command.contains("\t--clear\t")),
        "commands: {commands:?}"
    );
    assert!(
        !commands
            .iter()
            .any(|command| command.contains("\t--python\t>=3.10,<3.11\t")),
        "commands: {commands:?}"
    );

    Ok(())
}

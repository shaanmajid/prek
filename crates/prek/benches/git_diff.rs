use std::path::Path;
use std::process::Command;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use tempfile::TempDir;

fn create_repo_with_changes(num_files: usize, lines_per_file: usize) -> TempDir {
    let dir = TempDir::new().unwrap();
    let path = dir.path();

    Command::new("git").args(["init", "-q"]).current_dir(path).output().unwrap();
    Command::new("git").args(["config", "user.email", "b@b.com"]).current_dir(path).output().unwrap();
    Command::new("git").args(["config", "user.name", "B"]).current_dir(path).output().unwrap();

    for i in 0..num_files {
        std::fs::write(path.join(format!("file_{i}.txt")), "x\n".repeat(lines_per_file)).unwrap();
    }
    Command::new("git").args(["add", "."]).current_dir(path).output().unwrap();
    Command::new("git").args(["commit", "-m", "init"]).current_dir(path).output().unwrap();

    for i in 0..num_files {
        std::fs::write(path.join(format!("file_{i}.txt")), "y\n".repeat(lines_per_file)).unwrap();
    }
    dir
}

fn diff_full(path: &Path) -> Vec<u8> {
    Command::new("git")
        .args(["-c", "core.useBuiltinFSMonitor=false", "diff", "--no-ext-diff", "--no-textconv", "--ignore-submodules"])
        .current_dir(path)
        .output()
        .unwrap()
        .stdout
}

fn diff_name_only(path: &Path) -> Vec<u8> {
    Command::new("git")
        .args(["-c", "core.useBuiltinFSMonitor=false", "diff", "--name-only", "-z", "--no-ext-diff", "--ignore-submodules"])
        .current_dir(path)
        .output()
        .unwrap()
        .stdout
}

fn bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("git_diff");

    for (files, lines) in [(5, 100), (10, 500), (20, 1000)] {
        let repo = create_repo_with_changes(files, lines);
        let path = repo.path();

        let full = diff_full(path);
        let names = diff_name_only(path);
        eprintln!("[{files} files, {lines} lines] full: {} bytes, name-only: {} bytes", full.len(), names.len());

        let id = format!("{files}f_{lines}l");
        group.bench_with_input(BenchmarkId::new("full", &id), path, |b, p| b.iter(|| diff_full(p)));
        group.bench_with_input(BenchmarkId::new("name_only", &id), path, |b, p| b.iter(|| diff_name_only(p)));
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);

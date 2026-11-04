use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

struct Worktrees {
    repo: PathBuf,
    paths: Vec<PathBuf>,
}

impl Worktrees {
    fn add(&mut self, path: &Path, commit: &str) -> Result<()> {
        let status = Command::new("git")
            .args(["-C", self.repo.to_str().ok_or("non-UTF-8 repository path")?])
            .args(["worktree", "add", "--detach", "--"])
            .arg(path)
            .arg(commit)
            .status()?;
        if !status.success() {
            return Err(format!("git worktree add failed for {commit}").into());
        }
        self.paths.push(path.to_path_buf());
        Ok(())
    }
}

impl Drop for Worktrees {
    fn drop(&mut self) {
        for path in self.paths.iter().rev() {
            let status = Command::new("git")
                .arg("-C")
                .arg(&self.repo)
                .args(["worktree", "remove", "--force", "--"])
                .arg(path)
                .status();
            if !matches!(status, Ok(s) if s.success()) {
                eprintln!(
                    "warning: could not remove temporary worktree {}",
                    path.display()
                );
            }
        }
    }
}

fn git_commit(repo: &Path, reference: &str) -> Result<String> {
    if reference.starts_with('-') {
        return Err("Git refs must not start with '-'".into());
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-list", "-n", "1"])
        .arg(reference)
        .output()?;
    if !output.status.success() {
        return Err(format!("cannot resolve Git ref {reference}").into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn bench_binaries(worktree: &Path) -> Result<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    for entry in fs::read_dir(worktree.join("benches"))? {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "rs") {
            names.insert(
                path.file_stem()
                    .ok_or("benchmark file without stem")?
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    if names.is_empty() {
        return Err("no benchmark binaries found in benches/*.rs".into());
    }
    Ok(names)
}

fn list_cases(worktree: &Path, target: &Path, binary: &str) -> Result<Vec<String>> {
    eprintln!("Discovering {binary} in {}", worktree.display());
    let output = Command::new("cargo")
        .current_dir(worktree)
        .env("CARGO_TARGET_DIR", target)
        .args(["bench", "--bench", binary, "--", "--list"])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()?;
    if !output.status.success() {
        return Err(format!("could not list benchmarks in {binary}").into());
    }
    let listing = String::from_utf8(output.stdout)?;
    let cases: Vec<String> = listing
        .lines()
        .filter_map(|line| line.trim().strip_suffix(": benchmark"))
        .map(str::to_owned)
        .collect();
    if cases.is_empty() {
        return Err(format!("Criterion returned no benchmark cases for {binary}").into());
    }
    Ok(cases)
}

fn run_case(worktree: &Path, target: &Path, binary: &str, case: &str, label: &str) -> Result<()> {
    for attempt in 1..=2 {
        eprintln!("[{label}] {binary}: {case} (attempt {attempt}/2)");
        let status = Command::new("cargo")
            .current_dir(worktree)
            .env("CARGO_TARGET_DIR", target)
            .args(["bench", "--bench", binary, "--"])
            .arg(case)
            .args(["--exact", "--noplot", "--save-baseline", label])
            .status()?;
        if status.success() {
            return Ok(());
        }
    }
    Err(format!("benchmark failed twice: {binary} / {case} ({label})").into())
}

fn run_version(
    worktree: &Path,
    target: &Path,
    label: &str,
    filter: &str,
) -> Result<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    for binary in bench_binaries(worktree)? {
        for case in list_cases(worktree, target, &binary)? {
            if !case.contains(filter) {
                continue;
            }
            if !names.insert(case.clone()) {
                return Err(format!("duplicate benchmark name: {case}").into());
            }
            run_case(worktree, target, &binary, &case, label)?;
        }
    }
    if names.is_empty() {
        return Err(format!("no benchmark names matched filter {filter:?}").into());
    }
    Ok(names)
}

fn scan_results(dir: &Path, label: &str, values: &mut BTreeMap<String, f64>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if !path.is_dir() {
            continue;
        }
        if path.file_name().is_some_and(|name| name == label)
            && path.join("benchmark.json").is_file()
            && path.join("estimates.json").is_file()
        {
            let benchmark: Value = serde_json::from_slice(&fs::read(path.join("benchmark.json"))?)?;
            let estimates: Value = serde_json::from_slice(&fs::read(path.join("estimates.json"))?)?;
            let name = benchmark["full_id"]
                .as_str()
                .ok_or("Criterion benchmark.json has no full_id")?;
            let mean = estimates["mean"]["point_estimate"]
                .as_f64()
                .ok_or("Criterion estimates.json has no mean.point_estimate")?;
            if values.insert(name.to_owned(), mean).is_some() {
                return Err(format!("duplicate Criterion result: {name}").into());
            }
        } else {
            scan_results(&path, label, values)?;
        }
    }
    Ok(())
}

fn results(target: &Path, label: &str) -> Result<BTreeMap<String, f64>> {
    let criterion = target.join("criterion");
    if !criterion.is_dir() {
        return Err(format!("Criterion did not write results to {}", criterion.display()).into());
    }
    let mut values = BTreeMap::new();
    scan_results(&criterion, label, &mut values)?;
    Ok(values)
}

fn version(program: &str) -> String {
    Command::new(program)
        .arg("--version")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .unwrap_or_else(|| "unknown".to_owned())
        .trim()
        .to_owned()
}

struct ReportInput<'a> {
    base_ref: &'a str,
    candidate_ref: &'a str,
    base_commit: &'a str,
    candidate_commit: &'a str,
    base_locked: bool,
    candidate_locked: bool,
    base_cases: &'a BTreeSet<String>,
    candidate_cases: &'a BTreeSet<String>,
    base: &'a BTreeMap<String, f64>,
    candidate: &'a BTreeMap<String, f64>,
}

fn report(
    ReportInput {
        base_ref,
        candidate_ref,
        base_commit,
        candidate_commit,
        base_locked,
        candidate_locked,
        base_cases,
        candidate_cases,
        base,
        candidate,
    }: ReportInput<'_>,
) -> Result<String> {
    let common: Vec<_> = base_cases.intersection(candidate_cases).collect();
    if common.is_empty() {
        return Err("the two versions have no benchmark cases in common".into());
    }
    let mut text = String::new();
    writeln!(text, "# Benchmark comparison")?;
    writeln!(text)?;
    writeln!(text, "- Base: `{base_ref}` (`{base_commit}`)")?;
    writeln!(
        text,
        "- Candidate: `{candidate_ref}` (`{candidate_commit}`)"
    )?;
    writeln!(
        text,
        "- Platform: {} / {}",
        env::consts::OS,
        env::consts::ARCH
    )?;
    writeln!(text, "- Compiler: {}", version("rustc"))?;
    writeln!(text, "- Cargo: {}", version("cargo"))?;
    writeln!(text, "- Cargo.lock in base commit: {base_locked}")?;
    writeln!(text, "- Cargo.lock in candidate commit: {candidate_locked}")?;
    writeln!(
        text,
        "- Each case ran in a separate process. Each version used its own fresh temporary Cargo target directory."
    )?;
    writeln!(text)?;
    writeln!(
        text,
        "Criterion mean time per iteration in microseconds (lower is faster). Change = `(candidate / base - 1) × 100%`."
    )?;
    writeln!(text)?;
    writeln!(text, "| Benchmark | Base (µs) | Candidate (µs) | Change |")?;
    writeln!(text, "| --- | ---: | ---: | ---: |")?;
    let mut faster = 0;
    let mut slower = 0;
    for name in common {
        let a = base
            .get(name)
            .ok_or(format!("missing base result for {name}"))?;
        let b = candidate
            .get(name)
            .ok_or(format!("missing candidate result for {name}"))?;
        let change = (b / a - 1.0) * 100.0;
        if change < 0.0 {
            faster += 1;
        } else if change > 0.0 {
            slower += 1;
        }
        writeln!(
            text,
            "| {name} | {:.1} | {:.1} | {change:+.1}% |",
            a / 1000.0,
            b / 1000.0
        )?;
    }
    writeln!(text)?;
    writeln!(
        text,
        "Candidate faster in {faster} cases, slower in {slower} cases."
    )?;
    let base_only: Vec<_> = base_cases.difference(candidate_cases).collect();
    let candidate_only: Vec<_> = candidate_cases.difference(base_cases).collect();
    if !base_only.is_empty() || !candidate_only.is_empty() {
        writeln!(text)?;
        writeln!(text, "Unmatched cases (not compared):")?;
        for name in base_only {
            writeln!(text, "- Base only: `{name}`")?;
        }
        for name in candidate_only {
            writeln!(text, "- Candidate only: `{name}`")?;
        }
    }
    writeln!(text)?;
    writeln!(
        text,
        "These are local measurements. Runtime, client, server, dependency and machine differences can all affect them."
    )?;
    if !base_locked || !candidate_locked {
        writeln!(
            text,
            "A version without Cargo.lock resolved its dependencies at run time; future runs may select different versions."
        )?;
    }
    Ok(text)
}

fn run() -> Result<()> {
    let mut args = env::args().skip(1);
    let repo = PathBuf::from(args.next().ok_or("missing repository path")?);
    let base_ref = args.next().unwrap_or_else(|| "main".to_owned());
    let candidate_ref = args.next().unwrap_or_else(|| "HEAD".to_owned());
    let output_arg = args.next().unwrap_or_default();
    let filter = args.next().unwrap_or_default();
    if args.next().is_some() {
        return Err(
            "usage: compare-benchmarks [BASE_REF] [CANDIDATE_REF] [OUTPUT.md] [NAME_FILTER]".into(),
        );
    }
    let base_commit = git_commit(&repo, &base_ref)?;
    let candidate_commit = git_commit(&repo, &candidate_ref)?;
    if base_commit == candidate_commit {
        return Err("base and candidate resolve to the same commit".into());
    }
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let output = if output_arg.is_empty() {
        env::current_dir()?.join(format!(
            "benchmark-comparison-{}-{}-{stamp}.md",
            &base_commit[..8],
            &candidate_commit[..8]
        ))
    } else {
        PathBuf::from(output_arg)
    };
    let temporary = tempfile::Builder::new()
        .prefix("oxidinetd-benchmark-")
        .tempdir()?;
    let base_dir = temporary.path().join("base");
    let candidate_dir = temporary.path().join("candidate");
    let base_target = temporary.path().join("base-target");
    let candidate_target = temporary.path().join("candidate-target");
    let mut worktrees = Worktrees {
        repo,
        paths: Vec::new(),
    };
    worktrees.add(&base_dir, &base_commit)?;
    worktrees.add(&candidate_dir, &candidate_commit)?;
    let base_locked = base_dir.join("Cargo.lock").is_file();
    let candidate_locked = candidate_dir.join("Cargo.lock").is_file();
    eprintln!("Base build and result directory: {}", base_target.display());
    eprintln!(
        "Candidate build and result directory: {}",
        candidate_target.display()
    );
    let base_cases = run_version(&base_dir, &base_target, "base_ref", &filter)?;
    let candidate_cases = run_version(&candidate_dir, &candidate_target, "candidate_ref", &filter)?;
    let base = results(&base_target, "base_ref")?;
    let candidate = results(&candidate_target, "candidate_ref")?;
    let content = report(ReportInput {
        base_ref: &base_ref,
        candidate_ref: &candidate_ref,
        base_commit: &base_commit,
        candidate_commit: &candidate_commit,
        base_locked,
        candidate_locked,
        base_cases: &base_cases,
        candidate_cases: &candidate_cases,
        base: &base,
        candidate: &candidate,
    })?;
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&output, content)?;
    println!("Comparison written to {}", output.display());
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("benchmark comparison failed: {error}");
        std::process::exit(1);
    }
}

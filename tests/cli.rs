use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

struct Fixture {
    workspace: PathBuf,
    root: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let workspace = std::env::temp_dir().join(format!(
            "rcm-cli-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&workspace).unwrap();
        let root = workspace.join("chunks");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("segment001.m4s"), b"unchanged fixture").unwrap();
        Self { workspace, root }
    }

    fn with_files(label: &str, names: &[&str]) -> Self {
        let fixture = Self::new(label);
        std::fs::remove_file(fixture.root.join("segment001.m4s")).unwrap();
        for name in names {
            std::fs::write(fixture.root.join(name), b"unchanged fixture").unwrap();
        }
        fixture
    }

    fn assert_files_unchanged(&self, names: &[&str]) {
        assert_eq!(std::fs::read_dir(&self.root).unwrap().count(), names.len());
        for name in names {
            assert_eq!(
                std::fs::read(self.root.join(name)).unwrap(),
                b"unchanged fixture"
            );
        }
        assert_eq!(std::fs::read_dir(&self.workspace).unwrap().count(), 1);
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rs_chunkmerger"))
            .args(args)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn assert_unchanged(&self) {
        assert_eq!(
            std::fs::read(self.root.join("segment001.m4s")).unwrap(),
            b"unchanged fixture"
        );
        assert_eq!(std::fs::read_dir(&self.root).unwrap().count(), 1);
        assert_eq!(std::fs::read_dir(&self.workspace).unwrap().count(), 1);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.workspace);
    }
}

#[test]
fn help_exits_without_scanning_or_resolving_the_input_directory() {
    let fixture = Fixture::new("help");
    for flag in ["--help", "-h"] {
        for args in [
            vec![flag, "--input-dir", "does-not-exist"],
            vec!["--input-dir", "does-not-exist", flag],
            vec![flag, "--input-dir"],
        ] {
            let output = fixture.run(&args);
            assert!(output.status.success(), "{output:?}");
            assert!(output.stderr.is_empty(), "{output:?}");
            let stdout = String::from_utf8(output.stdout).unwrap();
            assert!(stdout.starts_with(&format!("rcm {}", env!("CARGO_PKG_VERSION"))));
            assert!(stdout.contains("用法"));
            for option in [
                "--help",
                "--version",
                "--input-dir",
                "--this-dir",
                "--recursive",
                "--include-hidden",
                "--start",
                "--end",
                "--ts",
            ] {
                assert!(stdout.contains(option), "缺少帮助选项：{option}");
            }
            assert!(!stdout.contains("扫描目录:"));
            assert!(!stdout.contains("确认目录组"));
            fixture.assert_unchanged();
        }
    }
}

#[test]
fn filename_range_preview_shows_resolved_boundaries_and_selected_count() {
    let names = ["segment201.m4s", "segment202.m4s", "segment203.m4s"];
    let fixture = Fixture::with_files("filename-range", &names);
    let output = fixture.run(&[
        "--start",
        "segment202.m4s",
        "--end",
        "segment203.m4s",
        "--ts",
    ]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("文件名匹配"));
    assert!(stdout.contains("原列表第 2 个"));
    assert!(stdout.contains("本次文件范围：segment202.m4s → segment203.m4s"));
    assert!(stdout.contains("待合并 2 个"));
    fixture.assert_files_unchanged(&names);
}

#[test]
fn numeric_range_ignores_padding_and_takes_priority_over_order() {
    let names = ["segment002.m4s", "segment003.m4s", "segment004.m4s"];
    let fixture = Fixture::with_files("numeric-range", &names);
    let output = fixture.run(&["--start=0002", "--end", "003", "--ts"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("数字匹配（完整字段）"));
    assert!(stdout.contains("原列表第 1 个"));
    assert!(stdout.contains("本次文件范围：segment002.m4s → segment003.m4s"));
    assert!(stdout.contains("待合并 2 个"));
    fixture.assert_files_unchanged(&names);
}

#[test]
fn order_range_fallback_does_not_treat_numeric_substrings_as_matches() {
    let names = ["segment101.m4s", "segment102.m4s", "segment103.m4s"];
    let fixture = Fixture::with_files("order-range", &names);
    let output = fixture.run(&["--start", "1", "--end=0002", "--ts"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("文件顺序匹配（从 1 开始）"));
    assert!(!stdout.contains("数字匹配（完整字段）"));
    assert!(stdout.contains("本次文件范围：segment101.m4s → segment102.m4s"));
    fixture.assert_files_unchanged(&names);
}

#[test]
fn mixed_matching_can_select_one_file_without_losing_its_known_pattern() {
    let names = ["segment002.m4s", "segment003.m4s", "segment004.m4s"];
    let fixture = Fixture::with_files("mixed-range", &names);
    let output = fixture.run(&["--start", "2", "--end", "1", "--ts"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("--start 2：数字匹配（完整字段）"));
    assert!(stdout.contains("--end 1：文件顺序匹配（从 1 开始）"));
    assert!(stdout.contains("本次文件范围：segment002.m4s → segment002.m4s"));
    assert!(stdout.contains("待合并 1 个"));
    assert!(!stdout.contains("文件名规律不足"));
    fixture.assert_files_unchanged(&names);
}

#[test]
fn invalid_ranges_fail_before_confirmation_without_creating_output() {
    let names = ["segment101.m4s", "segment102.m4s", "segment103.m4s"];
    let fixture = Fixture::with_files("invalid-range", &names);
    for args in [
        vec!["--start", "4"],
        vec!["--start", "0"],
        vec!["--end", "missing.m4s"],
        vec!["--start", "103", "--end", "101"],
    ] {
        let output = fixture.run(&args);
        assert!(!output.status.success(), "{output:?}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(!stdout.contains("确认目录组"));
        assert!(!stdout.contains("合并完成"));
        fixture.assert_files_unchanged(&names);
    }
}

#[test]
fn ambiguous_numeric_boundaries_fail_instead_of_using_valid_order_positions() {
    let names = [
        "00_002090_index_2_2185.m4s",
        "00_002091_index_2_2186.m4s",
        "00_002092_index_2_2187.m4s",
    ];
    let fixture = Fixture::with_files("ambiguous-range", &names);
    let output = fixture.run(&["--start", "2", "--ts"]);
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("匹配多个文件")
    );
    assert!(
        !String::from_utf8(output.stdout)
            .unwrap()
            .contains("确认目录组")
    );
    fixture.assert_files_unchanged(&names);
}

#[test]
fn ranges_on_empty_or_filtered_file_lists_fail_without_output() {
    for names in [vec![], vec!["app.ts"]] {
        let fixture = Fixture::with_files("empty-range", &names);
        let output = fixture.run(&["--start", "1", "--ts"]);
        assert!(!output.status.success(), "{output:?}");
        assert!(
            !String::from_utf8(output.stdout)
                .unwrap()
                .contains("确认目录组")
        );
        fixture.assert_files_unchanged(&names);
    }
}

#[test]
fn recursive_range_failure_prevents_execution_of_previously_planned_directories() {
    let names = ["segment201.m4s", "segment202.m4s", "segment203.m4s"];
    let fixture = Fixture::with_files("recursive-range", &names);
    let nested = fixture.root.join("nested");
    std::fs::create_dir(&nested).unwrap();
    std::fs::write(nested.join("segment900.m4s"), b"nested fixture").unwrap();
    let output = fixture.run(&["--recursive", "--start", "201", "--ts"]);
    assert!(!output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("合并计划"));
    assert!(!stdout.contains("确认目录组"));
    assert!(!stdout.contains("合并完成"));
    assert_eq!(std::fs::read_dir(&fixture.workspace).unwrap().count(), 1);
    assert_eq!(std::fs::read_dir(&fixture.root).unwrap().count(), 4);
    assert_eq!(std::fs::read_dir(&nested).unwrap().count(), 1);
}

#[test]
fn confirmed_range_merge_contains_only_the_selected_files() {
    let names = ["segment201.m4s", "segment202.m4s", "segment203.m4s"];
    let fixture = Fixture::with_files("confirmed-range", &names);
    for (name, payload) in names.iter().zip([b"first".as_slice(), b"second", b"third"]) {
        std::fs::write(fixture.root.join(name), payload).unwrap();
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_rs_chunkmerger"))
        .args(["--start", "segment202.m4s", "--end", "203", "--ts"])
        .current_dir(&fixture.root)
        .env("PATH", &fixture.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"y\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let output_file = fixture.workspace.join(format!(
        "chunks[v{}][segment202-segment203].ts",
        env!("CARGO_PKG_VERSION")
    ));
    assert_eq!(std::fs::read(output_file).unwrap(), b"secondthird");
    assert_eq!(std::fs::read_dir(&fixture.workspace).unwrap().count(), 2);
    assert_eq!(
        std::fs::read(fixture.root.join(names[0])).unwrap(),
        b"first"
    );
    assert_eq!(
        std::fs::read(fixture.root.join(names[1])).unwrap(),
        b"second"
    );
    assert_eq!(
        std::fs::read(fixture.root.join(names[2])).unwrap(),
        b"third"
    );
}

#[test]
fn version_exits_without_scanning_or_resolving_the_input_directory() {
    let fixture = Fixture::new("version");
    for flag in ["--version", "-V"] {
        for args in [
            vec![flag, "--input-dir", "does-not-exist"],
            vec!["--input-dir", "does-not-exist", flag],
            vec![flag, "--input-dir"],
        ] {
            let output = fixture.run(&args);
            assert!(output.status.success(), "{output:?}");
            assert!(output.stderr.is_empty(), "{output:?}");
            let stdout = String::from_utf8(output.stdout).unwrap();
            assert_eq!(stdout.trim(), format!("rcm {}", env!("CARGO_PKG_VERSION")));
            fixture.assert_unchanged();
        }
    }
}

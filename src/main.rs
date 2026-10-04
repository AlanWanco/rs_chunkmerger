use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions, remove_file};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write, copy};
use std::path::{Path, PathBuf};
use std::process::Command;

use natord::compare;
use walkdir::{DirEntry, WalkDir};

fn main() -> io::Result<()> {
    let options = parse_args(env::args().skip(1))?;
    let current_dir = env::current_dir()?;
    let input_dir = resolve_input_dir(&current_dir, options.input_dir.as_deref())?;
    let version = env!("CARGO_PKG_VERSION");

    println!("扫描目录: {:?}", input_dir);
    if !options.include_hidden {
        println!("默认跳过隐藏文件和 dotfile（如需包含请使用 --include-hidden）");
    }
    if !options.recursive {
        print_unscanned_subdirectories(&input_dir, options.include_hidden);
    }

    let candidates = collect_media_files(&input_dir, options.include_hidden, options.recursive);
    let directories = group_candidates_by_directory(candidates);
    if directories.is_empty() {
        println!("当前扫描范围内没有可合并的媒体分片。");
        if !options.recursive {
            println!("如分片位于子目录，请加 --recursive 扫描；递归时会按目录分别处理。");
        }
        return Ok(());
    }

    let candidate_count: usize = directories.values().map(Vec::len).sum();
    println!(
        "发现 {} 个目录组，共 {} 个候选文件；每个目录单独预览和确认，不会跨目录拼接。",
        directories.len(),
        candidate_count
    );

    let mut plans = Vec::new();
    for (directory, candidates) in directories {
        println!("\n=== 目录组：{:?} ===", directory);
        let inspection = inspect_candidate_files(&directory, candidates)?;
        print_inspection(&inspection);
        if inspection.files.is_empty() {
            println!("该目录没有可合并的媒体分片，跳过。");
            continue;
        }

        let split_at_gaps = if inspection.gaps.is_empty() {
            false
        } else {
            ask_yes_no("是否在缺片处拆分成多个视频？ [y/N]")?
        };
        let groups = build_merge_groups(&inspection.files, &inspection.gaps, split_at_gaps);
        let output_files = build_output_paths(&input_dir, &directory, version, &groups);
        print_merge_plan(&groups, &output_files);
        plans.push(DirectoryPlan {
            directory,
            inspection,
            groups,
            output_files,
        });
    }

    if plans.is_empty() {
        println!("没有可执行的合并任务。");
        return Ok(());
    }

    let ffmpeg = if options.skip_ffmpeg {
        None
    } else {
        which_ffmpeg()
    };
    if !options.skip_ffmpeg && ffmpeg.is_none() {
        println!("未找到 ffmpeg；合并后将保留 TS 文件。");
    }

    for plan in plans {
        let irregular_names = !irregular_filename_samples(&plan.inspection).is_empty();
        let prompt = if irregular_names {
            format!(
                "⚠️ {:?} 文件名规律不足，可能选错目录；仍合并这 {} 个文件？ [y/N]",
                plan.directory,
                plan.inspection.files.len()
            )
        } else {
            format!(
                "确认目录 {:?} 的文件范围和数量（{} 个），开始合并？ [y/N]",
                plan.directory,
                plan.inspection.files.len()
            )
        };
        if !ask_yes_no(&prompt)? {
            println!("已跳过目录 {:?}。", plan.directory);
            continue;
        }

        for (index, (group, output_file)) in plan.groups.iter().zip(&plan.output_files).enumerate()
        {
            merge_files(group, output_file)?;
            println!("✅ 合并完成: {:?}", output_file);

            if options.skip_ffmpeg {
                println!("已指定 --ts，跳过 ffmpeg 转换。");
            } else if let Some(ffmpeg) = &ffmpeg {
                convert_to_mp4(ffmpeg, output_file);
            }

            if index + 1 < plan.groups.len() {
                println!("完成分段 {}/{}。", index + 1, plan.groups.len());
            }
        }
    }

    Ok(())
}

#[derive(Default)]
struct Options {
    skip_ffmpeg: bool,
    include_hidden: bool,
    recursive: bool,
    input_dir: Option<PathBuf>,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> io::Result<Options> {
    let mut options = Options::default();
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--ts" => options.skip_ffmpeg = true,
            "--include-hidden" => options.include_hidden = true,
            "--recursive" => options.recursive = true,
            "--this-dir" => options.input_dir = Some(PathBuf::from(".")),
            "-i" | "--input-dir" => {
                let value = args.next().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("参数 {arg} 需要指定输入目录"),
                    )
                })?;
                options.input_dir = Some(PathBuf::from(value));
            }
            // 为兼容现有行为，忽略未知参数。
            _ => {}
        }
    }

    Ok(options)
}

/// 相对输入路径基于启动命令时的工作目录解析，而非可执行文件所在目录。
fn resolve_input_dir(current_dir: &Path, input_dir: Option<&Path>) -> io::Result<PathBuf> {
    let input_dir = match input_dir {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => current_dir.join(path),
        None => current_dir.to_path_buf(),
    }
    .canonicalize()?;

    if !input_dir.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("输入路径不是目录: {:?}", input_dir),
        ));
    }

    Ok(input_dir)
}

#[derive(Clone, Copy)]
struct TsLayout {
    stride: usize,
    sync_offset: usize,
}

#[derive(Clone)]
struct MediaFile {
    path: PathBuf,
    size: u64,
    sequence: Option<u64>,
    series_key: Option<String>,
}

struct SequenceGap {
    series_key: String,
    missing_start: u64,
    missing_end: u64,
    next_file: PathBuf,
}

struct Inspection {
    candidate_count: usize,
    files: Vec<MediaFile>,
    skipped_typescript: Vec<PathBuf>,
    duplicate_count: usize,
    warnings: Vec<String>,
    gaps: Vec<SequenceGap>,
}

struct DirectoryPlan {
    directory: PathBuf,
    inspection: Inspection,
    groups: Vec<Vec<MediaFile>>,
    output_files: Vec<PathBuf>,
}

fn group_candidates_by_directory(candidates: Vec<PathBuf>) -> BTreeMap<PathBuf, Vec<PathBuf>> {
    let mut groups = BTreeMap::new();
    for path in candidates {
        if let Some(directory) = path.parent() {
            groups
                .entry(directory.to_path_buf())
                .or_insert_with(Vec::new)
                .push(path);
        }
    }
    groups
}

fn print_unscanned_subdirectories(root: &Path, include_hidden: bool) {
    let directories: Vec<PathBuf> = WalkDir::new(root)
        .max_depth(1)
        .into_iter()
        .filter_entry(|entry| include_hidden || !is_hidden_entry(entry))
        .filter_map(Result::ok)
        .filter(|entry| entry.depth() == 1 && entry.file_type().is_dir())
        .map(DirEntry::into_path)
        .collect();

    if directories.is_empty() {
        return;
    }

    println!(
        "默认不扫描 {} 个子目录；需要时加 --recursive（递归后会分目录独立确认和合并）：",
        directories.len()
    );
    for directory in directories.iter().take(10) {
        println!("  - {:?}", directory);
    }
    if directories.len() > 10 {
        println!("  …另有 {} 个子目录。", directories.len() - 10);
    }
}

#[cfg(test)]
fn inspect_media_files(root: &Path, include_hidden: bool) -> io::Result<Inspection> {
    let candidates = collect_media_files(root, include_hidden, false);
    inspect_candidate_files(root, candidates)
}

fn inspect_candidate_files(root: &Path, candidates: Vec<PathBuf>) -> io::Result<Inspection> {
    let candidate_count = candidates.len();
    let mut files = Vec::new();
    let mut skipped_typescript = Vec::new();
    let mut warnings = Vec::new();

    for path in candidates {
        let metadata = match path.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                warnings.push(format!("无法读取 {:?}: {error}", path));
                continue;
            }
        };
        let size = metadata.len();
        let is_ts = path
            .extension()
            .and_then(OsStr::to_str)
            .is_some_and(|extension| extension.eq_ignore_ascii_case("ts"));

        if is_ts {
            let Some(layout) = detect_ts_layout(&path, size)? else {
                skipped_typescript.push(path);
                continue;
            };
            if size % layout.stride as u64 != 0 {
                let (checked_packets, invalid_syncs) = inspect_ts_packets(&path, size, layout)?;
                warnings.push(format!(
                    "TS 大小异常：{:?} 为 {} 字节（包长 {}），检查了 {} 个二进制包，发现 {} 个同步字节异常。",
                    path, size, layout.stride, checked_packets, invalid_syncs
                ));
            }
        }

        if size == 0 {
            warnings.push(format!("空文件仍在待合并范围内：{:?}", path));
        }

        let (sequence, series_key) = sequence_details(root, &path);
        files.push(MediaFile {
            path,
            size,
            sequence,
            series_key,
        });
    }

    files.sort_by(compare_media_files);
    let before_dedup = files.len();
    files = remove_identical_duplicates(files, &mut warnings)?;
    let duplicate_count = before_dedup - files.len();
    let gaps = find_sequence_gaps(&files);

    Ok(Inspection {
        candidate_count,
        files,
        skipped_typescript,
        duplicate_count,
        warnings,
        gaps,
    })
}

fn sequence_details(root: &Path, path: &Path) -> (Option<u64>, Option<String>) {
    let Some(stem) = path.file_stem().and_then(OsStr::to_str) else {
        return (None, None);
    };
    let Some(start) = stem
        .char_indices()
        .rev()
        .find(|(_, character)| !character.is_ascii_digit())
        .map(|(index, character)| index + character.len_utf8())
        .or_else(|| (!stem.is_empty()).then_some(0))
    else {
        return (None, None);
    };
    let Ok(sequence) = stem[start..].parse::<u64>() else {
        return (None, None);
    };
    let relative_parent = path
        .parent()
        .and_then(|parent| parent.strip_prefix(root).ok())
        .unwrap_or(Path::new(""));
    let series_key = format!(
        "{}::{}",
        relative_parent.to_string_lossy(),
        stem[..start].to_lowercase()
    );

    (Some(sequence), Some(series_key))
}

fn compare_media_files(left: &MediaFile, right: &MediaFile) -> Ordering {
    let left_init = is_init_file(&left.path);
    let right_init = is_init_file(&right.path);
    match (left_init, right_init) {
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        _ => {}
    }

    if let (Some(left_key), Some(right_key), Some(left_number), Some(right_number)) = (
        left.series_key.as_deref(),
        right.series_key.as_deref(),
        left.sequence,
        right.sequence,
    ) {
        let series_order = compare(left_key, right_key);
        if series_order != Ordering::Equal {
            return series_order;
        }
        let number_order = left_number.cmp(&right_number);
        if number_order != Ordering::Equal {
            return number_order;
        }
    }

    compare(&left.path.to_string_lossy(), &right.path.to_string_lossy())
}

fn is_init_file(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.to_ascii_lowercase().contains("init"))
}

fn find_sequence_gaps(files: &[MediaFile]) -> Vec<SequenceGap> {
    let mut sequences: BTreeMap<&str, Vec<(u64, &Path)>> = BTreeMap::new();
    for file in files {
        if let (Some(series_key), Some(sequence)) = (file.series_key.as_deref(), file.sequence) {
            sequences
                .entry(series_key)
                .or_default()
                .push((sequence, &file.path));
        }
    }

    let mut gaps = Vec::new();
    for (series_key, mut entries) in sequences {
        entries.sort_by_key(|(sequence, _)| *sequence);
        let mut previous: Option<(u64, &Path)> = None;
        for (sequence, path) in entries {
            if let Some((previous_sequence, _)) = previous
                && sequence > previous_sequence.saturating_add(1)
            {
                gaps.push(SequenceGap {
                    series_key: series_key.to_string(),
                    missing_start: previous_sequence + 1,
                    missing_end: sequence - 1,
                    next_file: path.to_path_buf(),
                });
            }
            if previous.is_none_or(|(previous_sequence, _)| sequence > previous_sequence) {
                previous = Some((sequence, path));
            }
        }
    }
    gaps
}

fn remove_identical_duplicates(
    files: Vec<MediaFile>,
    warnings: &mut Vec<String>,
) -> io::Result<Vec<MediaFile>> {
    let mut kept: Vec<MediaFile> = Vec::with_capacity(files.len());
    for file in files {
        let matching: Vec<&MediaFile> = match (&file.series_key, file.sequence) {
            (Some(series_key), Some(sequence)) => kept
                .iter()
                .filter(|existing| {
                    existing.series_key.as_ref() == Some(series_key)
                        && existing.sequence == Some(sequence)
                })
                .collect(),
            _ => Vec::new(),
        };

        let mut identical_to: Option<PathBuf> = None;
        let mut has_conflict = false;
        for existing in matching {
            if files_are_identical(&file.path, &existing.path)? {
                identical_to = Some(existing.path.clone());
                break;
            }
            has_conflict = true;
        }

        if let Some(existing_path) = identical_to {
            warnings.push(format!(
                "相同编号且二进制内容完全一致，跳过重复文件：{:?} == {:?}",
                file.path, existing_path
            ));
            continue;
        }
        if has_conflict {
            warnings.push(format!("编号重复但二进制内容不同，请检查：{:?}", file.path));
        }
        kept.push(file);
    }
    Ok(kept)
}

fn files_are_identical(left: &Path, right: &Path) -> io::Result<bool> {
    let mut left = File::open(left)?;
    let mut right = File::open(right)?;
    let mut left_buffer = [0u8; 64 * 1024];
    let mut right_buffer = [0u8; 64 * 1024];

    loop {
        let left_read = left.read(&mut left_buffer)?;
        let right_read = right.read(&mut right_buffer)?;
        if left_read != right_read || left_buffer[..left_read] != right_buffer[..right_read] {
            return Ok(false);
        }
        if left_read == 0 {
            return Ok(true);
        }
    }
}

fn detect_ts_layout(path: &Path, file_size: u64) -> io::Result<Option<TsLayout>> {
    const STRIDES: [usize; 3] = [188, 192, 204];
    if file_size < 376 {
        return Ok(None);
    }

    let probe_size = (204 * 6) as usize;
    let mut probe = vec![0u8; file_size.min(probe_size as u64) as usize];
    let mut file = File::open(path)?;
    let read = file.read(&mut probe)?;
    probe.truncate(read);

    for stride in STRIDES {
        for sync_offset in 0..stride.min(probe.len()) {
            let packet_count = ((probe.len() - 1 - sync_offset) / stride + 1).min(4);
            if packet_count >= 2
                && (0..packet_count).all(|packet| probe[sync_offset + packet * stride] == 0x47)
            {
                return Ok(Some(TsLayout {
                    stride,
                    sync_offset,
                }));
            }
        }
    }
    Ok(None)
}

fn inspect_ts_packets(path: &Path, file_size: u64, layout: TsLayout) -> io::Result<(u64, u64)> {
    let mut reader = BufReader::new(File::open(path)?);
    reader.seek(SeekFrom::Start(layout.sync_offset as u64))?;
    let packet_count = if file_size > layout.sync_offset as u64 {
        (file_size - 1 - layout.sync_offset as u64) / layout.stride as u64 + 1
    } else {
        0
    };
    let mut checked = 0;
    let mut invalid = 0;
    let mut packet = vec![0u8; layout.stride];

    for index in 0..packet_count {
        let position = layout.sync_offset as u64 + index * layout.stride as u64;
        let bytes_to_read = (file_size - position).min(layout.stride as u64) as usize;
        reader.read_exact(&mut packet[..bytes_to_read])?;
        checked += 1;
        if packet[0] != 0x47 {
            invalid += 1;
        }
    }
    Ok((checked, invalid))
}

fn print_inspection(inspection: &Inspection) {
    let total_bytes: u64 = inspection.files.iter().map(|file| file.size).sum();
    println!(
        "检查完成：扫描候选文件 {} 个，待合并 {} 个（{}），跳过非 MPEG-TS 的 .ts 文件 {} 个。",
        inspection.candidate_count,
        inspection.files.len(),
        format_bytes(total_bytes),
        inspection.skipped_typescript.len()
    );
    if let (Some(first), Some(last)) = (inspection.files.first(), inspection.files.last()) {
        println!("文件范围：{:?} → {:?}", first.path, last.path);
    }
    if inspection.duplicate_count > 0 {
        println!(
            "已从待合并列表排除 {} 个完全重复分片。",
            inspection.duplicate_count
        );
    }
    if !inspection.skipped_typescript.is_empty() {
        println!("已跳过文本/非 MPEG-TS .ts 文件（TypeScript 源码不会参与合并）：");
        for path in inspection.skipped_typescript.iter().take(10) {
            println!("  - {:?}", path);
        }
        if inspection.skipped_typescript.len() > 10 {
            println!("  …另有 {} 个。", inspection.skipped_typescript.len() - 10);
        }
    }
    for warning in &inspection.warnings {
        println!("⚠️ {warning}");
    }
    let irregular_samples = irregular_filename_samples(inspection);
    if !irregular_samples.is_empty() {
        println!("⚠️ 文件名规律不足，可能选错目录；这些文件无法归入至少包含两片的编号序列：");
        for path in irregular_samples.iter().take(8) {
            println!("  - {:?}", path);
        }
        if irregular_samples.len() > 8 {
            println!("  …另有 {} 个文件。", irregular_samples.len() - 8);
        }
    }
    let indexed_count = inspection
        .files
        .iter()
        .filter(|file| file.sequence.is_some())
        .count();
    let mut series_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for file in &inspection.files {
        if let (Some(series_key), Some(_)) = (file.series_key.as_deref(), file.sequence) {
            *series_counts.entry(series_key).or_default() += 1;
        }
    }
    let checked_series_count = series_counts.values().filter(|count| **count >= 2).count();
    if indexed_count < inspection.files.len() {
        println!(
            "编号连续性检查覆盖 {}/{} 个文件；未识别编号的文件无法判断是否缺片。",
            indexed_count,
            inspection.files.len()
        );
    }
    if inspection.gaps.is_empty() {
        if checked_series_count == 0 {
            println!("编号检查：没有至少包含两个分片的编号序列，无法判断是否缺片。");
        } else {
            println!("编号检查：未发现可识别编号序列中的缺片。");
        }
    } else {
        println!("编号检查：发现以下缺片：");
        for gap in &inspection.gaps {
            if gap.missing_start == gap.missing_end {
                println!(
                    "  - {}：缺少 {}（下一片：{:?}）",
                    gap.series_key, gap.missing_start, gap.next_file
                );
            } else {
                println!(
                    "  - {}：缺少 {}-{}（下一片：{:?}）",
                    gap.series_key, gap.missing_start, gap.missing_end, gap.next_file
                );
            }
        }
    }
}

fn irregular_filename_samples(inspection: &Inspection) -> Vec<PathBuf> {
    if inspection.files.is_empty() {
        return Vec::new();
    }

    let mut series_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for file in &inspection.files {
        if let (Some(series_key), Some(_)) = (file.series_key.as_deref(), file.sequence) {
            *series_counts.entry(series_key).or_default() += 1;
        }
    }

    inspection
        .files
        .iter()
        .filter(|file| {
            if is_init_file(&file.path) {
                return false;
            }
            match (file.series_key.as_deref(), file.sequence) {
                (Some(series_key), Some(_)) => {
                    series_counts.get(series_key).copied().unwrap_or(0) < 2
                }
                _ => true,
            }
        })
        .map(|file| file.path.clone())
        .collect()
}

fn build_merge_groups(
    files: &[MediaFile],
    gaps: &[SequenceGap],
    split_at_gaps: bool,
) -> Vec<Vec<MediaFile>> {
    if !split_at_gaps || gaps.is_empty() {
        return vec![files.to_vec()];
    }

    let break_before: BTreeSet<&Path> = gaps.iter().map(|gap| gap.next_file.as_path()).collect();
    let mut groups = Vec::new();
    let mut current = Vec::new();
    for file in files {
        if !current.is_empty() && break_before.contains(file.path.as_path()) {
            groups.push(std::mem::take(&mut current));
        }
        current.push(file.clone());
    }
    if !current.is_empty() {
        groups.push(current);
    }
    groups
}

fn build_output_paths(
    scan_root: &Path,
    directory: &Path,
    version: &str,
    groups: &[Vec<MediaFile>],
) -> Vec<PathBuf> {
    let parent_dir = scan_root.parent().unwrap_or(Path::new("."));
    let root_name = scan_root
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("output");
    let relative_directory = directory.strip_prefix(scan_root).unwrap_or(Path::new(""));
    let subdirectory_label = relative_directory
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("_");
    let output_prefix = if subdirectory_label.is_empty() {
        root_name.to_string()
    } else {
        format!("{root_name}[{subdirectory_label}]")
    };

    groups
        .iter()
        .enumerate()
        .map(|(index, group)| {
            let start = group
                .first()
                .and_then(|file| file.path.file_stem())
                .and_then(OsStr::to_str)
                .unwrap_or("start");
            let end = group
                .last()
                .and_then(|file| file.path.file_stem())
                .and_then(OsStr::to_str)
                .unwrap_or("end");
            let part = if groups.len() > 1 {
                format!("[part-{}-of-{}]", index + 1, groups.len())
            } else {
                String::new()
            };
            parent_dir.join(format!(
                "{}[v{}][{}-{}]{}.ts",
                output_prefix, version, start, end, part
            ))
        })
        .collect()
}

fn print_merge_plan(groups: &[Vec<MediaFile>], output_files: &[PathBuf]) {
    println!("合并计划：共 {} 个输出视频。", groups.len());
    for (index, (group, output)) in groups.iter().zip(output_files).enumerate() {
        let bytes: u64 = group.iter().map(|file| file.size).sum();
        let first = group.first().map(|file| &file.path);
        let last = group.last().map(|file| &file.path);
        println!(
            "  {}. {} 个文件，{}，范围 {:?} → {:?}",
            index + 1,
            group.len(),
            format_bytes(bytes),
            first,
            last
        );
        println!(
            "     输出：{:?}{}",
            output,
            if output.exists() {
                "（将覆盖已有文件）"
            } else {
                ""
            }
        );
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.2} {}", UNITS[unit])
}

fn ask_yes_no(prompt: &str) -> io::Result<bool> {
    print!("{prompt} ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes" | "是"
    ))
}

fn merge_files(files: &[MediaFile], output_file: &Path) -> io::Result<()> {
    let mut outfile = BufWriter::new(
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(output_file)?,
    );
    for file in files {
        println!("Merging {:?}", file.path);
        let mut reader = BufReader::new(File::open(&file.path)?);
        copy(&mut reader, &mut outfile)?;
    }
    outfile.flush()
}

fn convert_to_mp4(ffmpeg: &Path, output_file: &Path) {
    let mp4_path = output_file.with_extension("mp4");
    println!("正在调用 ffmpeg 转换为 mp4...");
    let status = Command::new(ffmpeg)
        .arg("-y")
        .arg("-i")
        .arg(output_file)
        .arg("-c")
        .arg("copy")
        .arg(&mp4_path)
        .status();

    match status {
        Ok(status) if status.success() => {
            println!("✅ 转换完成: {:?}", mp4_path);
            match remove_file(output_file) {
                Ok(()) => println!("✅ 已删除旧文件: {:?}", output_file),
                Err(error) => println!("❌ 删除旧文件失败: {error}，文件: {:?}", output_file),
            }
        }
        Ok(status) => println!(
            "❌ ffmpeg 运行失败，退出码: {status}；保留 TS 文件: {:?}",
            output_file
        ),
        Err(error) => println!(
            "❌ 调用 ffmpeg 失败: {error}；保留 TS 文件: {:?}",
            output_file
        ),
    }
}

/// 收集媒体分片；默认仅当前目录，递归模式遍历所有可见子目录。
fn collect_media_files(root: &Path, include_hidden: bool, recursive: bool) -> Vec<PathBuf> {
    const SUPPORTED_EXTS: [&str; 4] = ["ts", "decrypt", "mp4", "m4s"];

    WalkDir::new(root)
        .max_depth(if recursive { usize::MAX } else { 1 })
        .into_iter()
        // filter_entry 不仅跳过隐藏文件，也会阻止继续遍历隐藏目录。
        .filter_entry(|entry| include_hidden || !is_hidden_entry(entry))
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| {
            entry
                .path()
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| {
                    SUPPORTED_EXTS
                        .iter()
                        .any(|supported| supported.eq_ignore_ascii_case(ext))
                })
                .unwrap_or(false)
        })
        .map(DirEntry::into_path)
        .collect()
}

/// 判断目录项是否为隐藏项。
///
/// dotfile（名称以 `.` 开头）在所有平台上都会被跳过；macOS 还会检查
/// Finder/`chflags hidden` 使用的 `UF_HIDDEN` 文件标记。
fn is_hidden_entry(entry: &DirEntry) -> bool {
    // WalkDir 的根目录可能是 `.`，不能因为它的名称以 `.` 开头而跳过整个目录。
    if entry.depth() == 0 {
        return false;
    }

    if is_dotfile(entry.file_name()) {
        return true;
    }

    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt;

        entry
            .metadata()
            .map(|metadata| metadata.st_flags() & libc::UF_HIDDEN != 0)
            .unwrap_or(false)
    }

    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

fn is_dotfile(name: &OsStr) -> bool {
    name.to_string_lossy().starts_with('.')
}

/// 检查 ffmpeg 是否可用，优先系统 PATH，其次当前目录。
/// macOS/Linux 的本地文件名为 `ffmpeg`，Windows 则通常为 `ffmpeg.exe`。
fn which_ffmpeg() -> Option<PathBuf> {
    // 1. 检查系统 PATH
    if let Ok(ffmpeg_in_path) = which::which("ffmpeg") {
        return Some(ffmpeg_in_path);
    }

    // 2. 检查当前目录下的本地 ffmpeg
    for candidate in ["./ffmpeg", "./ffmpeg.exe"] {
        let path = Path::new(candidate);
        if path.is_file() {
            return Some(path.to_path_buf());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::{
        build_merge_groups, build_output_paths, collect_media_files, compare_media_files,
        group_candidates_by_directory, inspect_candidate_files, inspect_media_files,
        irregular_filename_samples, is_dotfile, parse_args, resolve_input_dir,
    };
    use std::ffi::OsStr;
    use std::path::{Path, PathBuf};

    fn temp_dir(test_name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "rs_chunkmerger-{test_name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn recognizes_dotfiles() {
        assert!(is_dotfile(OsStr::new(".DS_Store")));
        assert!(is_dotfile(OsStr::new(".segment.ts")));
        assert!(!is_dotfile(OsStr::new("segment.ts")));
    }

    #[test]
    fn skips_hidden_files_and_directories_by_default() {
        let root = temp_dir("hidden-test");
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::write(root.join("part1.ts"), b"visible").unwrap();
        std::fs::write(root.join(".part2.ts"), b"dotfile").unwrap();
        std::fs::write(root.join(".hidden/part3.ts"), b"hidden directory").unwrap();

        let files = collect_media_files(&root, false, false);
        assert_eq!(files, vec![root.join("part1.ts")]);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parses_input_dir_option() {
        let options = parse_args([
            "--input-dir".to_string(),
            "chunks".to_string(),
            "--ts".to_string(),
            "--include-hidden".to_string(),
            "--recursive".to_string(),
        ])
        .unwrap();

        assert_eq!(options.input_dir, Some(PathBuf::from("chunks")));
        assert!(options.skip_ffmpeg);
        assert!(options.include_hidden);
        assert!(options.recursive);
        assert!(parse_args(["--input-dir".to_string()]).is_err());

        let this_dir = parse_args(["--this-dir".to_string()]).unwrap();
        assert_eq!(this_dir.input_dir, Some(PathBuf::from(".")));
    }

    #[test]
    fn resolves_relative_input_dir_from_invocation_directory() {
        let invocation_dir = temp_dir("input-dir-test");
        std::fs::create_dir_all(invocation_dir.join("chunks")).unwrap();

        let input_dir = resolve_input_dir(&invocation_dir, Some(Path::new("chunks"))).unwrap();
        assert_eq!(
            input_dir,
            invocation_dir.join("chunks").canonicalize().unwrap()
        );
        let this_dir = resolve_input_dir(&invocation_dir, Some(Path::new("."))).unwrap();
        assert_eq!(this_dir, invocation_dir.canonicalize().unwrap());

        std::fs::remove_dir_all(invocation_dir).unwrap();
    }

    #[test]
    fn recursive_scan_groups_candidates_by_containing_directory() {
        let root = temp_dir("recursive-scan-test");
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::write(root.join("root001.m4s"), b"root").unwrap();
        std::fs::write(root.join("nested/child001.m4s"), b"child").unwrap();

        let shallow = collect_media_files(&root, false, false);
        assert_eq!(shallow, vec![root.join("root001.m4s")]);

        let recursive = collect_media_files(&root, false, true);
        assert_eq!(recursive.len(), 2);
        let grouped = group_candidates_by_directory(recursive);
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[&root].len(), 1);
        assert_eq!(grouped[&root.join("nested")].len(), 1);

        let nested_directory = root.join("nested");
        let nested_inspection =
            inspect_candidate_files(&nested_directory, grouped[&nested_directory].clone()).unwrap();
        let outputs =
            build_output_paths(&root, &nested_directory, "test", &[nested_inspection.files]);
        assert!(!outputs[0].starts_with(&root));
        assert!(
            outputs[0]
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains("[nested]")
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn warns_when_filenames_have_no_shared_numbered_pattern() {
        let root = temp_dir("irregular-name-test");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("capture-final.m4s"), b"one").unwrap();
        std::fs::write(root.join("random-data.m4s"), b"two").unwrap();

        let inspection = inspect_media_files(&root, false).unwrap();
        assert_eq!(irregular_filename_samples(&inspection).len(), 2);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn numeric_sort_handles_different_zero_padding() {
        let root = temp_dir("numeric-sort-test");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("segment0679.m4s"), b"b").unwrap();
        std::fs::write(root.join("segment678.m4s"), b"a").unwrap();
        std::fs::write(root.join("segment5342.m4s"), b"c").unwrap();

        let inspection = inspect_media_files(&root, false).unwrap();
        let numbers: Vec<_> = inspection
            .files
            .iter()
            .map(|file| file.sequence.unwrap())
            .collect();
        assert_eq!(numbers, vec![678, 679, 5342]);
        assert_eq!(
            compare_media_files(&inspection.files[0], &inspection.files[1]),
            std::cmp::Ordering::Less
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reports_missing_chunks_and_deduplicates_identical_payloads() {
        let root = temp_dir("gap-test");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("segment334.m4s"), b"same payload").unwrap();
        std::fs::write(root.join("segment334.decrypt"), b"same payload").unwrap();
        std::fs::write(root.join("segment336.m4s"), b"next payload").unwrap();

        let inspection = inspect_media_files(&root, false).unwrap();
        assert_eq!(inspection.candidate_count, 3);
        assert_eq!(inspection.duplicate_count, 1);
        assert_eq!(inspection.files.len(), 2);
        assert_eq!(inspection.gaps.len(), 1);
        assert_eq!(inspection.gaps[0].missing_start, 335);
        assert_eq!(inspection.gaps[0].missing_end, 335);

        let groups = build_merge_groups(&inspection.files, &inspection.gaps, true);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].len(), 1);
        assert_eq!(groups[1].len(), 1);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn warns_when_duplicate_number_has_different_binary_content() {
        let root = temp_dir("duplicate-conflict-test");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("segment334.m4s"), b"payload one").unwrap();
        std::fs::write(root.join("segment334.decrypt"), b"payload two").unwrap();

        let inspection = inspect_media_files(&root, false).unwrap();
        assert_eq!(inspection.files.len(), 2);
        assert!(
            inspection
                .warnings
                .iter()
                .any(|warning| warning.contains("编号重复但二进制内容不同"))
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn skips_typescript_and_detects_misaligned_transport_stream_data() {
        let root = temp_dir("ts-inspection-test");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("app.ts"),
            b"const value: string = 'not a media stream';",
        )
        .unwrap();

        let mut transport_stream = vec![0xff; 188 * 4 + 1];
        for offset in (0..188 * 4).step_by(188) {
            transport_stream[offset] = 0x47;
        }
        std::fs::write(root.join("segment001.ts"), transport_stream).unwrap();

        let inspection = inspect_media_files(&root, false).unwrap();
        assert_eq!(inspection.candidate_count, 2);
        assert_eq!(inspection.files.len(), 1);
        assert_eq!(inspection.skipped_typescript, vec![root.join("app.ts")]);
        assert!(
            inspection
                .warnings
                .iter()
                .any(|warning| warning.contains("检查了 5 个二进制包"))
        );
        assert!(
            inspection
                .warnings
                .iter()
                .any(|warning| warning.contains("1 个同步字节异常"))
        );

        std::fs::remove_dir_all(root).unwrap();
    }
}

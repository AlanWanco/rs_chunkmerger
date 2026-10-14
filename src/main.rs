use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::env;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions, remove_file};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write, copy};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

use natord::compare;
use walkdir::{DirEntry, WalkDir};

fn main() -> io::Result<()> {
    let options = parse_args(env::args().skip(1))?;
    let version = env!("CARGO_PKG_VERSION");
    if options.show_help {
        print_help(version);
        return Ok(());
    }
    if options.show_version {
        println!("rcm {version}");
        return Ok(());
    }
    let current_dir = env::current_dir()?;
    let input_dir = resolve_input_dir(&current_dir, options.input_dir.as_deref())?;

    println!("扫描目录: {}", compact_path(&input_dir, 80));
    if !options.include_hidden {
        println!("默认跳过隐藏文件和 dotfile（如需包含请使用 --include-hidden）");
    }
    if !options.recursive {
        print_unscanned_subdirectories(&input_dir, options.include_hidden);
    }

    let candidates = collect_media_files(&input_dir, options.include_hidden, options.recursive);
    let directories = group_candidates_by_directory(candidates);
    if directories.is_empty() {
        if options.start.is_some() || options.end.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "已指定 --start/--end，但扫描范围内没有可合并的媒体分片。",
            ));
        }
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

    let ffmpeg = which_ffmpeg();
    let ffprobe = which_ffprobe();
    let mut plans = Vec::new();
    for (directory, candidates) in directories {
        let label = directory_label(&input_dir, &directory);
        println!("\n=== 目录组：{} ===", compact_text(&label, 72));
        let mut inspection = inspect_candidate_files(&directory, candidates)?;
        if options.start.is_some() || options.end.is_some() {
            let range = resolve_file_range(
                &inspection.files,
                options.start.as_deref(),
                options.end.as_deref(),
            )
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("目录组「{}」范围无效：{error}", compact_text(&label, 72)),
                )
            })?;
            print_file_range(
                &inspection.files,
                &range,
                options.start.as_deref(),
                options.end.as_deref(),
            );
            apply_file_range(&mut inspection, &range);
        }
        print_inspection(&inspection);
        if inspection.files.is_empty() {
            println!("该目录没有可合并的媒体分片，跳过。");
            continue;
        }

        match assess_audio_video_tracks(&inspection, ffprobe.as_deref()) {
            AvAssessment::Pair(pair) => {
                if ffmpeg.is_none() {
                    println!("⚠️ 已识别独立音视频轨，但未找到 ffmpeg；不会将两轨直接拼接。");
                    continue;
                }
                let groups = vec![inspection.files.clone()];
                let mut output_file = build_output_paths(&input_dir, &directory, version, &groups)
                    .into_iter()
                    .next()
                    .expect("one output path for one AV pair");
                if !options.skip_ffmpeg {
                    output_file.set_extension("mp4");
                }
                print_audio_video_plan(&pair, &output_file);
                plans.push(DirectoryPlan {
                    label,
                    inspection,
                    action: DirectoryAction::MuxTracks { pair, output_file },
                });
            }
            AvAssessment::Unsafe(reason) => {
                println!("⚠️ {reason}");
            }
            AvAssessment::SingleProgram => {
                let split_at_gaps = if inspection.gaps.is_empty() {
                    false
                } else {
                    ask_yes_no("是否在缺片处拆分成多个视频？ [y/N]")?
                };
                let groups = build_merge_groups(&inspection.files, &inspection.gaps, split_at_gaps);
                let output_files = build_output_paths(&input_dir, &directory, version, &groups);
                print_merge_plan(&groups, &output_files);
                plans.push(DirectoryPlan {
                    label,
                    inspection,
                    action: DirectoryAction::Concatenate {
                        groups,
                        output_files,
                    },
                });
            }
        }
    }

    if plans.is_empty() {
        println!("没有可执行的合并任务。");
        return Ok(());
    }
    if !options.skip_ffmpeg && ffmpeg.is_none() {
        println!("未找到 ffmpeg；普通分片合并后将保留 TS 文件。");
    }

    for plan in plans {
        let irregular_names = !irregular_filename_samples(&plan.inspection).is_empty();
        let prompt = match &plan.action {
            DirectoryAction::Concatenate { .. } if irregular_names => format!(
                "⚠️ 目录组「{}」文件名规律不足，可能选错目录；仍合并这 {} 个文件？ [y/N]",
                compact_text(&plan.label, 48),
                plan.inspection.files.len()
            ),
            DirectoryAction::Concatenate { .. } => format!(
                "确认目录组「{}」的文件范围和数量（{} 个），开始合并？ [y/N]",
                compact_text(&plan.label, 48),
                plan.inspection.files.len()
            ),
            DirectoryAction::MuxTracks { pair, .. } => format!(
                "确认目录组「{}」的音视频轨时间线对齐；复用 {} 个视频片段与 {} 个音频片段？ [y/N]",
                compact_text(&plan.label, 40),
                pair.video.files.len(),
                pair.audio.files.len()
            ),
        };
        if !ask_yes_no(&prompt)? {
            println!("已跳过目录组「{}」。", compact_text(&plan.label, 48));
            continue;
        }

        match plan.action {
            DirectoryAction::Concatenate {
                groups,
                output_files,
            } => {
                for (index, (group, output_file)) in groups.iter().zip(&output_files).enumerate() {
                    merge_files(group, output_file)?;
                    println!(
                        "✅ 合并完成: {}",
                        compact_text(&file_name_display(output_file), 88)
                    );
                    if options.skip_ffmpeg {
                        println!("已指定 --ts，跳过 ffmpeg 转换。");
                    } else if let Some(ffmpeg) = &ffmpeg {
                        convert_to_mp4(ffmpeg, output_file);
                    }
                    if index + 1 < groups.len() {
                        println!("完成分段 {}/{}。", index + 1, groups.len());
                    }
                }
            }
            DirectoryAction::MuxTracks { pair, output_file } => {
                match (ffmpeg.as_deref(), ffprobe.as_deref()) {
                    (Some(ffmpeg), Some(ffprobe)) => {
                        if let Err(error) = mux_audio_video(
                            ffmpeg,
                            ffprobe,
                            &pair,
                            &output_file,
                            options.skip_ffmpeg,
                        ) {
                            println!("❌ 音视频复用失败，未发布输出文件：{error}");
                        }
                    }
                    _ => println!("❌ ffmpeg/ffprobe 不可用；不会将音视频轨直接拼接。"),
                }
            }
        }
    }

    Ok(())
}

fn print_help(version: &str) {
    println!(
        r#"rcm {version} - 媒体分片合并与音视频分轨复用工具

用法：
  rcm [选项]

选项：
  -h, --help              显示帮助并退出
  -V, --version           显示版本并退出
  -i, --input-dir <目录>  指定输入目录；相对路径基于当前工作目录
      --this-dir          扫描当前工作目录（默认行为）
      --recursive         扫描子目录，各目录独立确认和合并
      --include-hidden    包含隐藏文件和 dotfile
      --start <边界>      从指定文件开始（包含该文件）
      --end <边界>        到指定文件结束（包含该文件）
      --ts                输出 TS，不转换为 MP4

说明：
  默认扫描启动命令时的工作目录，而非程序所在目录；默认不扫描子目录。
  普通分片默认尝试用 FFmpeg 转为 MP4；未找到 FFmpeg 时保留 TS。
  独立音视频分轨需要 FFmpeg 和 ffprobe，使用 --ts 时也不例外。
  展示文件范围、编号检查和合并计划后，用户确认才执行合并。
  输出保存在扫描根目录的上一级。
  范围边界先匹配完整文件名（含扩展名）；纯数字再匹配完整数字字段。
  数字忽略前导零、不做子串匹配；未匹配数字字段时才按文件顺序号匹配。
  顺序号从 1 开始，基于排序、去重后的有效文件列表；支持只指定一个边界。
  多个文件匹配同一数字、越界或起点晚于终点时会报错，不自动猜测。
  发现初始化片段时需将其包含在范围内，不会自动追加范围外文件。
  使用 --recursive 时，各目录分别解析同一组范围；任一目录无效则停止。

示例：
  rcm
  rcm --input-dir "路径/分片"
  rcm --this-dir --recursive --ts
  rcm --start "segment001.ts" --end "segment050.ts"
  rcm --start 2090 --end 2139"#
    );
}

#[derive(Default)]
struct Options {
    show_help: bool,
    show_version: bool,
    skip_ffmpeg: bool,
    include_hidden: bool,
    recursive: bool,
    input_dir: Option<PathBuf>,
    start: Option<String>,
    end: Option<String>,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> io::Result<Options> {
    let mut options = Options::default();
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                options.show_help = true;
                return Ok(options);
            }
            "-V" | "--version" => {
                options.show_version = true;
                return Ok(options);
            }
            "--ts" => options.skip_ffmpeg = true,
            "--include-hidden" => options.include_hidden = true,
            "--recursive" => options.recursive = true,
            "--this-dir" => options.input_dir = Some(PathBuf::from(".")),
            "--start" => options.start = Some(parse_range_value(args.next(), "--start")?),
            "--end" => options.end = Some(parse_range_value(args.next(), "--end")?),
            value if value.starts_with("--start=") => {
                options.start = Some(parse_range_value(
                    Some(value["--start=".len()..].to_string()),
                    "--start",
                )?);
            }
            value if value.starts_with("--end=") => {
                options.end = Some(parse_range_value(
                    Some(value["--end=".len()..].to_string()),
                    "--end",
                )?);
            }
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

fn parse_range_value(value: Option<String>, option: &str) -> io::Result<String> {
    value
        .filter(|value| {
            !value.is_empty()
                && !matches!(
                    value.as_str(),
                    "-h" | "--help"
                        | "-V"
                        | "--version"
                        | "-i"
                        | "--input-dir"
                        | "--this-dir"
                        | "--recursive"
                        | "--include-hidden"
                        | "--ts"
                        | "--start"
                        | "--end"
                )
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "参数 {option} 需要指定文件名、文件名数字或文件顺序号；使用 --help 查看用法。"
                ),
            )
        })
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
            format!("输入路径不是目录: {}", compact_path(&input_dir, 80)),
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
    ts_layout: Option<TsLayout>,
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
    recognized_series: BTreeSet<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BoundaryMatch {
    FileName,
    Number,
    Order,
    Default,
}

#[derive(Clone, Copy, Debug)]
struct ResolvedBoundary {
    index: usize,
    kind: BoundaryMatch,
}

#[derive(Clone, Copy, Debug)]
struct ResolvedRange {
    start: ResolvedBoundary,
    end: ResolvedBoundary,
}

fn normalized_digits(value: &str) -> &str {
    let normalized = value.trim_start_matches('0');
    if normalized.is_empty() {
        "0"
    } else {
        normalized
    }
}

fn filename_contains_number(path: &Path, number: &str) -> bool {
    path.file_stem()
        .and_then(OsStr::to_str)
        .is_some_and(|stem| {
            stem.split(|character: char| !character.is_ascii_digit())
                .any(|digits| !digits.is_empty() && normalized_digits(digits) == number)
        })
}

fn unique_boundary_match(
    files: &[MediaFile],
    matches: &[usize],
    option: &str,
    value: &str,
    kind: BoundaryMatch,
) -> io::Result<Option<ResolvedBoundary>> {
    match matches {
        [] => Ok(None),
        [index] => Ok(Some(ResolvedBoundary {
            index: *index,
            kind,
        })),
        _ => {
            let names = matches
                .iter()
                .take(6)
                .map(|index| compact_text(&file_name_display(&files[*index].path), 72))
                .collect::<Vec<_>>()
                .join("、");
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "参数 {option}「{}」匹配多个文件（共 {} 个）：{names}{}。请指定完整文件名，不会猜测或改按顺序号匹配。",
                    compact_text(value, 72),
                    matches.len(),
                    if matches.len() > 6 { "等" } else { "" }
                ),
            ))
        }
    }
}

fn resolve_boundary(
    files: &[MediaFile],
    value: &str,
    option: &str,
) -> io::Result<ResolvedBoundary> {
    let name_matches: Vec<usize> = files
        .iter()
        .enumerate()
        .filter(|(_, file)| {
            file.path
                .file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| {
                    name == value || (cfg!(windows) && name.eq_ignore_ascii_case(value))
                })
        })
        .map(|(index, _)| index)
        .collect();
    if let Some(boundary) =
        unique_boundary_match(files, &name_matches, option, value, BoundaryMatch::FileName)?
    {
        return Ok(boundary);
    }

    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "参数 {option} 的文件名「{}」不在当前目录的有效文件列表中；非纯数字不会按顺序号匹配。",
                compact_text(value, 72)
            ),
        ));
    }
    let number = normalized_digits(value);
    let number_matches: Vec<usize> = files
        .iter()
        .enumerate()
        .filter(|(_, file)| filename_contains_number(&file.path, number))
        .map(|(index, _)| index)
        .collect();
    if let Some(boundary) =
        unique_boundary_match(files, &number_matches, option, value, BoundaryMatch::Number)?
    {
        return Ok(boundary);
    }

    if let Some(position) = number
        .parse::<usize>()
        .ok()
        .filter(|position| *position >= 1 && *position <= files.len())
    {
        return Ok(ResolvedBoundary {
            index: position - 1,
            kind: BoundaryMatch::Order,
        });
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "参数 {option}「{}」没有匹配文件名中的完整数字字段，且超出文件顺序范围 1–{}。",
            compact_text(value, 72),
            files.len()
        ),
    ))
}

fn resolve_file_range(
    files: &[MediaFile],
    start: Option<&str>,
    end: Option<&str>,
) -> io::Result<ResolvedRange> {
    if files.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "没有有效文件，无法匹配 --start/--end 范围。",
        ));
    }
    let start = start
        .map(|value| resolve_boundary(files, value, "--start"))
        .transpose()?
        .unwrap_or(ResolvedBoundary {
            index: 0,
            kind: BoundaryMatch::Default,
        });
    let end = end
        .map(|value| resolve_boundary(files, value, "--end"))
        .transpose()?
        .unwrap_or(ResolvedBoundary {
            index: files.len() - 1,
            kind: BoundaryMatch::Default,
        });
    if start.index > end.index {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "起点 {}（第 {} 个）晚于终点 {}（第 {} 个）；范围必须按文件列表顺序从前到后。",
                file_name_display(&files[start.index].path),
                start.index + 1,
                file_name_display(&files[end.index].path),
                end.index + 1
            ),
        ));
    }
    if let Some((index, file)) = files.iter().enumerate().find(|(index, file)| {
        (*index < start.index || *index > end.index) && is_init_file(&file.path)
    }) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "范围排除了初始化片段 {}（第 {} 个），无法保证输出可解码；请将初始化片段包含在范围内，不会自动追加范围外文件。",
                file_name_display(&file.path),
                index + 1
            ),
        ));
    }
    Ok(ResolvedRange { start, end })
}

fn print_file_range(
    files: &[MediaFile],
    range: &ResolvedRange,
    start: Option<&str>,
    end: Option<&str>,
) {
    println!(
        "范围选择：原列表 {} 个文件（已排序、去重），本次 {} 个。",
        files.len(),
        range.end.index - range.start.index + 1
    );
    for (option, value, boundary) in [("--start", start, range.start), ("--end", end, range.end)] {
        let method = match boundary.kind {
            BoundaryMatch::FileName => "文件名匹配",
            BoundaryMatch::Number => "数字匹配（完整字段）",
            BoundaryMatch::Order => "文件顺序匹配（从 1 开始）",
            BoundaryMatch::Default => "默认边界",
        };
        println!(
            "  {option} {}：{method} → {}（原列表第 {} 个）",
            compact_text(value.unwrap_or("未指定"), 72),
            file_name_display(&files[boundary.index].path),
            boundary.index + 1
        );
    }
    println!(
        "  本次文件范围：{} → {}",
        file_name_display(&files[range.start.index].path),
        file_name_display(&files[range.end.index].path)
    );
}

fn apply_file_range(inspection: &mut Inspection, range: &ResolvedRange) {
    inspection.files.truncate(range.end.index + 1);
    inspection.files.drain(..range.start.index);
    inspection.gaps = find_sequence_gaps(&inspection.files);
}

#[derive(Clone)]
struct ProbeStream {
    codec_type: String,
    codec_name: String,
    stream_id: Option<String>,
    start_time: Option<f64>,
    duration: Option<f64>,
}

#[derive(Clone)]
struct ProbeInfo {
    streams: Vec<ProbeStream>,
    start_time: Option<f64>,
    duration: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrackKind {
    Audio,
    Video,
    Muxed,
    Other,
}

struct TrackProbe {
    kind: TrackKind,
    codec: String,
    files: Vec<MediaFile>,
    segment_durations: Vec<f64>,
    start_time: f64,
    end_time: f64,
}

struct AudioVideoPair {
    audio: TrackProbe,
    video: TrackProbe,
    start_delta: f64,
    end_delta: f64,
}

enum AvAssessment {
    SingleProgram,
    Pair(AudioVideoPair),
    Unsafe(String),
}

enum DirectoryAction {
    Concatenate {
        groups: Vec<Vec<MediaFile>>,
        output_files: Vec<PathBuf>,
    },
    MuxTracks {
        pair: AudioVideoPair,
        output_file: PathBuf,
    },
}

struct DirectoryPlan {
    label: String,
    inspection: Inspection,
    action: DirectoryAction,
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
        println!("  - {}", file_name_display(directory));
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
                warnings.push(format!("无法读取 {}: {error}", compact_path(&path, 72)));
                continue;
            }
        };
        let size = metadata.len();
        let is_ts = path
            .extension()
            .and_then(OsStr::to_str)
            .is_some_and(|extension| extension.eq_ignore_ascii_case("ts"));

        let mut ts_layout = None;
        if is_ts {
            let Some(layout) = detect_ts_layout(&path, size)? else {
                skipped_typescript.push(path);
                continue;
            };
            ts_layout = Some(layout);
            if size % layout.stride as u64 != 0 {
                let (checked_packets, invalid_syncs) = inspect_ts_packets(&path, size, layout)?;
                warnings.push(format!(
                    "TS 大小异常：{} 为 {} 字节（包长 {}），检查了 {} 个二进制包，发现 {} 个同步字节异常。",
                    file_name_display(&path), size, layout.stride, checked_packets, invalid_syncs
                ));
            }
        }

        if size == 0 {
            warnings.push(format!(
                "空文件仍在待合并范围内：{}",
                file_name_display(&path)
            ));
        }

        let (sequence, series_key) = sequence_details(root, &path);
        files.push(MediaFile {
            path,
            size,
            sequence,
            series_key,
            ts_layout,
        });
    }

    infer_correlated_sequences(root, &mut files);
    files.sort_by(compare_media_files);
    let before_dedup = files.len();
    files = remove_identical_duplicates(files, &mut warnings)?;
    let duplicate_count = before_dedup - files.len();
    let gaps = find_sequence_gaps(&files);
    let recognized_series = numbered_series_counts(&files)
        .into_iter()
        .filter(|(_, count)| *count >= 2)
        .map(|(key, _)| key.to_string())
        .collect();

    Ok(Inspection {
        candidate_count,
        files,
        skipped_typescript,
        duplicate_count,
        warnings,
        gaps,
        recognized_series,
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

struct NumberedFilename {
    stem: String,
    literals: Vec<String>,
    numbers: Vec<(Range<usize>, u64)>,
}

fn parse_numbered_filename(path: &Path) -> Option<NumberedFilename> {
    let stem = path.file_stem()?.to_str()?.to_lowercase();
    if !stem.as_bytes().last()?.is_ascii_digit() {
        return None;
    }
    let mut literals = Vec::new();
    let mut numbers = Vec::new();
    let mut cursor = 0;
    let mut literal_start = 0;
    while cursor < stem.len() {
        if !stem.as_bytes()[cursor].is_ascii_digit() {
            cursor += 1;
            continue;
        }
        literals.push(stem[literal_start..cursor].to_string());
        let start = cursor;
        while cursor < stem.len() && stem.as_bytes()[cursor].is_ascii_digit() {
            cursor += 1;
        }
        numbers.push((start..cursor, stem[start..cursor].parse().ok()?));
        literal_start = cursor;
    }
    literals.push(stem[literal_start..].to_string());
    Some(NumberedFilename {
        stem,
        literals,
        numbers,
    })
}

fn infer_correlated_sequences(root: &Path, files: &mut [MediaFile]) {
    let mut shapes: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for (index, file) in files.iter().enumerate() {
        if file.sequence.is_none() || is_init_file(&file.path) {
            continue;
        }
        let Some(parsed) = parse_numbered_filename(&file.path) else {
            continue;
        };
        if parsed.numbers.len() < 2 {
            continue;
        }
        let relative_parent = file
            .path
            .parent()
            .and_then(|parent| parent.strip_prefix(root).ok())
            .unwrap_or(Path::new(""))
            .to_path_buf();
        shapes
            .entry((relative_parent, parsed.literals.clone()))
            .or_default()
            .push((index, parsed));
    }

    for ((relative_parent, _), entries) in shapes {
        let last_column = entries[0].1.numbers.len() - 1;
        let sequence_values: BTreeSet<u64> = entries
            .iter()
            .map(|(_, parsed)| parsed.numbers[last_column].1)
            .collect();
        // Two singleton tracks can happen to have matching offsets. Require more evidence
        // before interpreting a changing prefix number as another segment counter.
        if sequence_values.len() < 3 {
            continue;
        }
        let first = &entries[0].1;
        let offsets: Vec<Option<i128>> = (0..last_column)
            .map(|column| {
                let offset = first.numbers[column].1 as i128 - first.numbers[last_column].1 as i128;
                entries
                    .iter()
                    .all(|(_, parsed)| {
                        parsed.numbers[column].1 as i128 - parsed.numbers[last_column].1 as i128
                            == offset
                    })
                    .then_some(offset)
            })
            .collect();
        if offsets.iter().all(Option::is_none) {
            continue;
        }

        for (index, parsed) in entries {
            let mut prefix = String::new();
            for (column, offset) in offsets.iter().enumerate() {
                prefix.push_str(&parsed.literals[column]);
                if let Some(offset) = offset {
                    prefix.push_str(&format!("{{seq{offset:+}}}"));
                } else {
                    prefix.push_str(&parsed.stem[parsed.numbers[column].0.clone()]);
                }
            }
            prefix.push_str(&parsed.literals[last_column]);
            files[index].series_key =
                Some(format!("{}::{prefix}", relative_parent.to_string_lossy()));
        }
    }
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
                "相同编号且二进制内容完全一致，跳过重复文件：{} == {}",
                file_name_display(&file.path),
                file_name_display(&existing_path)
            ));
            continue;
        }
        if has_conflict {
            warnings.push(format!(
                "编号重复但二进制内容不同，请检查：{}",
                file_name_display(&file.path)
            ));
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
        println!(
            "文件范围：{} → {}",
            file_name_display(&first.path),
            file_name_display(&last.path)
        );
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
            println!("  - {}", file_name_display(path));
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
            println!("  - {}", file_name_display(path));
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
    let series_counts = numbered_series_counts(&inspection.files);
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
                    "  - {}：缺少 {}（下一片：{}）",
                    display_series_key(&gap.series_key),
                    gap.missing_start,
                    file_name_display(&gap.next_file)
                );
            } else {
                println!(
                    "  - {}：缺少 {}-{}（下一片：{}）",
                    display_series_key(&gap.series_key),
                    gap.missing_start,
                    gap.missing_end,
                    file_name_display(&gap.next_file)
                );
            }
        }
    }
}

fn numbered_series_counts(files: &[MediaFile]) -> BTreeMap<&str, usize> {
    let mut counts = BTreeMap::new();
    for file in files {
        if let (Some(key), Some(_)) = (file.series_key.as_deref(), file.sequence) {
            *counts.entry(key).or_default() += 1;
        }
    }
    counts
}

fn irregular_filename_samples(inspection: &Inspection) -> Vec<PathBuf> {
    inspection
        .files
        .iter()
        .filter(|file| {
            if is_init_file(&file.path) {
                return false;
            }
            match (file.series_key.as_deref(), file.sequence) {
                (Some(series_key), Some(_)) => !inspection.recognized_series.contains(series_key),
                _ => true,
            }
        })
        .map(|file| file.path.clone())
        .collect()
}

const AV_SYNC_TOLERANCE_SECONDS: f64 = 0.25;

fn assess_audio_video_tracks(inspection: &Inspection, ffprobe: Option<&Path>) -> AvAssessment {
    let mut series: BTreeMap<String, Vec<MediaFile>> = BTreeMap::new();
    for file in &inspection.files {
        if let (Some(key), Some(_)) = (&file.series_key, file.sequence) {
            series.entry(key.clone()).or_default().push(file.clone());
        }
    }

    if series.len() == 1 {
        let Some(ffprobe) = ffprobe else {
            return AvAssessment::SingleProgram;
        };
        let (key, files) = series.into_iter().next().unwrap();
        match probe_series_kind(ffprobe, &files) {
            Ok(TrackKind::Audio) => {
                return AvAssessment::Unsafe(
                    "只识别到音频轨，没有可配对的视频轨；不会把音频分片伪装成视频合并。"
                        .to_string(),
                );
            }
            Ok(TrackKind::Video | TrackKind::Muxed) | Err(_) => {
                return AvAssessment::SingleProgram;
            }
            Ok(TrackKind::Other) => {
                return AvAssessment::Unsafe(format!(
                    "编号序列「{}」不包含可识别的音视频流，无法安全合并。",
                    display_series_key(&key)
                ));
            }
        }
    }

    if series.is_empty() {
        if inspection.files.len() == 1 {
            return AvAssessment::SingleProgram;
        }
        if inspection.files.len() != 2 {
            return AvAssessment::Unsafe(
                "文件名没有可识别的编号序列，无法安全分组音视频轨。".to_string(),
            );
        }
        let Some(ffprobe) = ffprobe else {
            return AvAssessment::Unsafe(
                "文件名没有编号序列，且未找到 ffprobe；无法判定是否为独立音视频轨。".to_string(),
            );
        };
        let mut files = inspection.files.clone();
        files.sort_by(compare_media_files);
        let mut probes = Vec::new();
        for file in files {
            match probe_track_series(ffprobe, vec![file]) {
                Ok(track) => probes.push(track),
                Err(error) => {
                    return AvAssessment::Unsafe(format!("无法探测候选音视频文件：{error}"));
                }
            }
        }
        return assemble_audio_video_pair(probes);
    }

    if !inspection.gaps.is_empty() {
        return AvAssessment::Unsafe(
            "候选音视频分轨存在缺片；先补齐分片再复用，避免跨缺口错配。".to_string(),
        );
    }
    let Some(ffprobe) = ffprobe else {
        return AvAssessment::Unsafe(
            "检测到多个编号序列，但未找到 ffprobe；为避免把独立音视频轨直接拼接，已停止。"
                .to_string(),
        );
    };

    if inspection
        .files
        .iter()
        .any(|file| file.sequence.is_none() && !is_init_file(&file.path))
    {
        return AvAssessment::Unsafe(
            "存在无法归入编号序列的文件，无法安全判定音视频配对。".to_string(),
        );
    }

    let mut tracks = Vec::new();
    for (key, mut files) in series {
        files.sort_by_key(|file| file.sequence.unwrap_or_default());
        match probe_track_series(ffprobe, files) {
            Ok(track) => tracks.push((key, track)),
            Err(error) => {
                return AvAssessment::Unsafe(format!(
                    "无法可靠探测编号序列「{}」：{error}。不会把多个序列直接拼接。",
                    display_series_key(&key)
                ));
            }
        }
    }

    let mut audio_tracks = Vec::new();
    let mut video_tracks = Vec::new();
    let mut other_track_count = 0;
    for (key, track) in tracks {
        match track.kind {
            TrackKind::Audio => audio_tracks.push(track),
            TrackKind::Video => video_tracks.push(track),
            TrackKind::Muxed => {
                return AvAssessment::Unsafe(format!(
                    "编号序列「{}」本身已包含音视频流，但目录还有其他序列；无法判断应如何配对。",
                    display_series_key(&key)
                ));
            }
            TrackKind::Other => other_track_count += 1,
        }
    }

    if audio_tracks.len() != 1 || video_tracks.len() != 1 || other_track_count != 0 {
        return AvAssessment::Unsafe(format!(
            "检测到多个序列，但无法唯一识别一条音轨和一条视频轨（音轨 {} 组、视频轨 {} 组、其他 {} 组）；不会猜测配对。",
            audio_tracks.len(),
            video_tracks.len(),
            other_track_count
        ));
    }
    assemble_audio_video_pair([audio_tracks.pop().unwrap(), video_tracks.pop().unwrap()].into())
}

fn probe_series_kind(ffprobe: &Path, files: &[MediaFile]) -> io::Result<TrackKind> {
    let first = files
        .first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "编号序列为空"))?;
    let last = files.last().unwrap();
    let first_probe = probe_media_file(ffprobe, &first.path)?;
    let last_probe = if first.path == last.path {
        first_probe.clone()
    } else {
        probe_media_file(ffprobe, &last.path)?
    };
    let kind = classify_probe(&first_probe);
    if classify_probe(&last_probe) != kind {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "首尾片段的流类型不一致",
        ));
    }
    Ok(kind)
}

fn assemble_audio_video_pair(mut tracks: Vec<TrackProbe>) -> AvAssessment {
    let mut audio = Vec::new();
    let mut video = Vec::new();
    let mut other_count = 0;
    for track in tracks.drain(..) {
        match track.kind {
            TrackKind::Audio => audio.push(track),
            TrackKind::Video => video.push(track),
            TrackKind::Muxed | TrackKind::Other => other_count += 1,
        }
    }
    if audio.len() != 1 || video.len() != 1 || other_count != 0 {
        return AvAssessment::Unsafe(format!(
            "无法唯一识别一条音轨和一条视频轨（音轨 {} 组、视频轨 {} 组、其他 {} 组）；不会猜测配对。",
            audio.len(),
            video.len(),
            other_count
        ));
    }
    let audio = audio.pop().unwrap();
    let video = video.pop().unwrap();
    let (start_delta, end_delta) = match compare_track_time_ranges(
        video.start_time,
        video.end_time,
        audio.start_time,
        audio.end_time,
    ) {
        Ok(deltas) => deltas,
        Err(error) => return AvAssessment::Unsafe(error.to_string()),
    };
    AvAssessment::Pair(AudioVideoPair {
        audio,
        video,
        start_delta,
        end_delta,
    })
}

fn parse_ts_pid(stream_id: &str) -> Option<u16> {
    let pid = stream_id
        .strip_prefix("0x")
        .and_then(|value| u16::from_str_radix(value, 16).ok())
        .or_else(|| stream_id.parse::<u16>().ok())?;
    Some(pid)
}

fn find_first_pes_pts(path: &Path, layout: TsLayout, target_pid: u16) -> io::Result<u64> {
    let mut reader = BufReader::new(File::open(path)?);
    reader.seek(SeekFrom::Start(layout.sync_offset as u64))?;
    let mut packet = vec![0u8; layout.stride];

    loop {
        let bytes = reader.read(&mut packet)?;
        if bytes < 188 || packet[0] != 0x47 {
            if bytes == 0 {
                break;
            }
            continue;
        }

        let pid = (((packet[1] & 0x1f) as u16) << 8) | packet[2] as u16;
        let payload_start = packet[1] & 0x40 != 0;
        let adaptation_control = (packet[3] >> 4) & 0x03;
        if pid != target_pid || !payload_start || adaptation_control & 0x01 == 0 {
            continue;
        }

        let payload_offset = if adaptation_control & 0x02 != 0 {
            5 + packet[4] as usize
        } else {
            4
        };
        if payload_offset + 14 > bytes || packet[payload_offset..payload_offset + 3] != [0, 0, 1] {
            continue;
        }
        let flags = (packet[payload_offset + 7] >> 6) & 0x03;
        if flags != 0x02 && flags != 0x03 {
            continue;
        }
        let pts = &packet[payload_offset + 9..payload_offset + 14];
        return Ok((((pts[0] as u64 >> 1) & 0x07) << 30)
            | ((pts[1] as u64) << 22)
            | (((pts[2] as u64 >> 1) & 0x7f) << 15)
            | ((pts[3] as u64) << 7)
            | ((pts[4] as u64 >> 1) & 0x7f));
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{} 中未找到目标 TS 流的 PTS", file_name_display(path)),
    ))
}

fn normalize_pts_ticks(timestamps: &[u64]) -> io::Result<Vec<f64>> {
    const PTS_WRAP: u64 = 1 << 33;
    const PTS_HALF_WRAP: u64 = 1 << 32;
    let mut offset = 0u64;
    let mut previous: Option<u64> = None;
    let mut normalized = Vec::with_capacity(timestamps.len());

    for &timestamp in timestamps {
        let mut current = timestamp.saturating_add(offset);
        if let Some(previous) = previous
            && current < previous
        {
            if previous - current > PTS_HALF_WRAP {
                offset = offset.saturating_add(PTS_WRAP);
                current = timestamp.saturating_add(offset);
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "TS 分片的 PTS 没有递增",
                ));
            }
        }
        previous = Some(current);
        normalized.push(current as f64 / 90_000.0);
    }
    Ok(normalized)
}

fn probe_track_series(ffprobe: &Path, files: Vec<MediaFile>) -> io::Result<TrackProbe> {
    let first_file = files
        .first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "编号序列为空"))?;
    let last_file = files.last().unwrap();
    let first = probe_media_file(ffprobe, &first_file.path)?;
    let last = if first_file.path == last_file.path {
        first.clone()
    } else {
        probe_media_file(ffprobe, &last_file.path)?
    };
    let kind = classify_probe(&first);
    if !matches!(kind, TrackKind::Audio | TrackKind::Video) || classify_probe(&last) != kind {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "首尾片段的流类型不一致，或片段同时包含音频和视频",
        ));
    }

    let first_stream = first
        .streams
        .iter()
        .find(|stream| stream.codec_type == track_kind_name(kind))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "首片缺少目标媒体流"))?;
    let last_stream = last
        .streams
        .iter()
        .find(|stream| stream.codec_type == track_kind_name(kind))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "尾片缺少目标媒体流"))?;
    if first_stream.codec_name != last_stream.codec_name {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "首尾片段编码不一致",
        ));
    }

    let all_ts = files.iter().all(|file| file.ts_layout.is_some());
    let no_ts = files.iter().all(|file| file.ts_layout.is_none());
    if !all_ts && !no_ts {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "同一轨道混有 TS 与其他容器，无法统一计算分片时间线",
        ));
    }

    let segment_starts = if all_ts {
        let first_pid = first_stream
            .stream_id
            .as_deref()
            .and_then(parse_ts_pid)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "ffprobe 未提供 TS 流 PID")
            })?;
        let last_pid = last_stream
            .stream_id
            .as_deref()
            .and_then(parse_ts_pid)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "无法解析尾片 TS 流 PID"))?;
        if first_pid != last_pid {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "首尾 TS 片段的流 PID 不一致",
            ));
        }
        let raw_timestamps = files
            .iter()
            .map(|file| find_first_pes_pts(&file.path, file.ts_layout.unwrap(), first_pid))
            .collect::<io::Result<Vec<_>>>()?;
        normalize_pts_ticks(&raw_timestamps)?
    } else {
        const MAX_NON_TS_SEGMENTS_TO_PROBE: usize = 128;
        if files.len() > MAX_NON_TS_SEGMENTS_TO_PROBE {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "非 TS 分轨有 {} 个片段；为避免逐片启动 ffprobe 造成大量开销，当前上限为 {}",
                    files.len(),
                    MAX_NON_TS_SEGMENTS_TO_PROBE
                ),
            ));
        }
        let mut starts = Vec::with_capacity(files.len());
        for (index, file) in files.iter().enumerate() {
            let probe = if index == 0 {
                first.clone()
            } else if index + 1 == files.len() {
                last.clone()
            } else {
                probe_media_file_start(ffprobe, &file.path)?
            };
            if classify_probe(&probe) != kind {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "分片 {} 的流类型与首片不一致",
                        file_name_display(&file.path)
                    ),
                ));
            }
            let stream = probe
                .streams
                .iter()
                .find(|stream| stream.codec_type == track_kind_name(kind))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "分片缺少目标媒体流"))?;
            if stream.codec_name != first_stream.codec_name {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("分片 {} 的编码与首片不一致", file_name_display(&file.path)),
                ));
            }
            starts.push(probe.start_time.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "无法读取分片 {} 的起始时间戳",
                        file_name_display(&file.path)
                    ),
                )
            })?);
        }
        starts
    };

    let mut segment_durations = Vec::with_capacity(files.len());
    for pair in segment_starts.windows(2) {
        let duration = pair[1] - pair[0];
        if !duration.is_finite() || duration <= 0.0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "分片起始时间没有递增",
            ));
        }
        segment_durations.push(duration);
    }
    let last_duration = last
        .duration
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "无法读取尾片时长"))?;
    if !last_duration.is_finite() || last_duration <= 0.0 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "尾片时长无效"));
    }
    segment_durations.push(last_duration);

    let start_time = segment_starts[0];
    let last_start = *segment_starts.last().unwrap();
    let end_time = last_start + last_duration;
    if !start_time.is_finite() || !end_time.is_finite() || end_time <= start_time {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "片段起止时间无效",
        ));
    }

    Ok(TrackProbe {
        kind,
        codec: first_stream.codec_name.clone(),
        files,
        segment_durations,
        start_time,
        end_time,
    })
}

fn classify_probe(probe: &ProbeInfo) -> TrackKind {
    let audio_count = probe
        .streams
        .iter()
        .filter(|stream| stream.codec_type == "audio")
        .count();
    let video_count = probe
        .streams
        .iter()
        .filter(|stream| stream.codec_type == "video")
        .count();
    match (audio_count, video_count) {
        (1, 0) => TrackKind::Audio,
        (0, 1) => TrackKind::Video,
        (audio, video) if audio > 0 && video > 0 => TrackKind::Muxed,
        _ => TrackKind::Other,
    }
}

fn track_kind_name(kind: TrackKind) -> &'static str {
    match kind {
        TrackKind::Audio => "audio",
        TrackKind::Video => "video",
        TrackKind::Muxed | TrackKind::Other => "",
    }
}

fn probe_media_file(ffprobe: &Path, path: &Path) -> io::Result<ProbeInfo> {
    let output = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=start_time,duration:stream=codec_type,codec_name,id,start_time,duration",
            "-of",
            "flat",
        ])
        .arg(path)
        .output()?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            compact_text(message.trim(), 160),
        ));
    }

    let output = String::from_utf8_lossy(&output.stdout);
    let probe = parse_ffprobe_flat(&output);
    if probe.streams.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "ffprobe 未识别到音频或视频流",
        ));
    }
    Ok(probe)
}

fn probe_media_streams(ffprobe: &Path, path: &Path) -> io::Result<ProbeInfo> {
    let output = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type,codec_name,id",
            "-of",
            "flat",
        ])
        .arg(path)
        .output()?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            compact_text(message.trim(), 160),
        ));
    }
    let probe = parse_ffprobe_flat(&String::from_utf8_lossy(&output.stdout));
    if probe.streams.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "ffprobe 未识别到输出音视频流",
        ));
    }
    Ok(probe)
}

fn probe_media_file_start(ffprobe: &Path, path: &Path) -> io::Result<ProbeInfo> {
    let output = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=start_time:stream=codec_type,codec_name,id,start_time",
            "-of",
            "flat",
        ])
        .arg(path)
        .output()?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            compact_text(message.trim(), 160),
        ));
    }
    Ok(parse_ffprobe_flat(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_ffprobe_flat(output: &str) -> ProbeInfo {
    let mut streams: BTreeMap<usize, BTreeMap<String, String>> = BTreeMap::new();
    let mut format_start = None;
    let mut format_duration = None;

    for line in output.lines() {
        let Some((key, raw_value)) = line.split_once('=') else {
            continue;
        };
        let value = raw_value.trim().trim_matches('"').to_string();
        if key == "format.start_time" {
            format_start = parse_seconds(Some(&value));
        } else if key == "format.duration" {
            format_duration = parse_seconds(Some(&value));
        } else if let Some(rest) = key.strip_prefix("streams.stream.")
            && let Some((index, field)) = rest.split_once('.')
            && let Ok(index) = index.parse::<usize>()
        {
            streams
                .entry(index)
                .or_default()
                .insert(field.to_string(), value);
        }
    }

    let streams: Vec<ProbeStream> = streams
        .into_values()
        .map(|fields| ProbeStream {
            codec_type: fields.get("codec_type").cloned().unwrap_or_default(),
            codec_name: fields.get("codec_name").cloned().unwrap_or_default(),
            stream_id: fields.get("id").cloned(),
            start_time: fields
                .get("start_time")
                .and_then(|value| parse_seconds(Some(value))),
            duration: fields
                .get("duration")
                .and_then(|value| parse_seconds(Some(value))),
        })
        .collect();
    let start_time = format_start.or_else(|| {
        streams
            .iter()
            .filter_map(|stream| stream.start_time)
            .min_by(f64::total_cmp)
    });
    let duration = format_duration.or_else(|| {
        streams
            .iter()
            .filter_map(|stream| stream.duration)
            .max_by(f64::total_cmp)
    });

    ProbeInfo {
        streams,
        start_time,
        duration,
    }
}

fn parse_seconds(value: Option<&String>) -> Option<f64> {
    let seconds = value?.parse::<f64>().ok()?;
    seconds.is_finite().then_some(seconds)
}

fn print_audio_video_plan(pair: &AudioVideoPair, output: &Path) {
    println!("识别到一组可配对的独立音视频轨：");
    println!(
        "  视频：{} 个文件（{} → {}），{}，时间 {:.3}–{:.3}s",
        pair.video.files.len(),
        file_name_display(&pair.video.files.first().unwrap().path),
        file_name_display(&pair.video.files.last().unwrap().path),
        pair.video.codec,
        pair.video.start_time,
        pair.video.end_time
    );
    println!(
        "  音频：{} 个文件（{} → {}），{}，时间 {:.3}–{:.3}s",
        pair.audio.files.len(),
        file_name_display(&pair.audio.files.first().unwrap().path),
        file_name_display(&pair.audio.files.last().unwrap().path),
        pair.audio.codec,
        pair.audio.start_time,
        pair.audio.end_time
    );
    println!(
        "  起止偏差：{:.3}s / {:.3}s（限制 {:.3}s）",
        pair.start_delta, pair.end_delta, AV_SYNC_TOLERANCE_SECONDS
    );
    println!("  输出：{}", compact_text(&file_name_display(output), 88));
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
    if let Some(output_dir) = output_files.first().and_then(|path| path.parent()) {
        println!("输出目录：{}", compact_path(output_dir, 80));
    }
    for (index, (group, output)) in groups.iter().zip(output_files).enumerate() {
        let bytes: u64 = group.iter().map(|file| file.size).sum();
        let first = group
            .first()
            .map(|file| file_name_display(&file.path))
            .unwrap_or_else(|| "start".to_string());
        let last = group
            .last()
            .map(|file| file_name_display(&file.path))
            .unwrap_or_else(|| "end".to_string());
        println!(
            "  {}. {} 个文件，{}，范围 {} → {}",
            index + 1,
            group.len(),
            format_bytes(bytes),
            first,
            last
        );
        println!(
            "     输出：{}{}",
            compact_text(&file_name_display(output), 88),
            if output.exists() {
                "（将覆盖已有文件）"
            } else {
                ""
            }
        );
    }
}

fn directory_label(scan_root: &Path, directory: &Path) -> String {
    let relative = directory.strip_prefix(scan_root).unwrap_or(directory);
    if relative.as_os_str().is_empty() {
        ".".to_string()
    } else {
        relative.display().to_string()
    }
}

fn file_name_display(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

fn compact_path(path: &Path, max_chars: usize) -> String {
    let displayed = path.display().to_string();
    let displayed = displayed.strip_prefix("\\\\?\\").unwrap_or(&displayed);
    compact_text(displayed, max_chars)
}

fn compact_text(text: &str, max_chars: usize) -> String {
    let max_chars = max_chars.max(2);
    if text.chars().count() <= max_chars {
        return text.to_string();
    }

    let suffix: String = text
        .chars()
        .rev()
        .take(max_chars - 1)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{suffix}")
}

fn display_series_key(series_key: &str) -> &str {
    series_key.strip_prefix("::").unwrap_or(series_key)
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

struct TempWorkspace {
    path: PathBuf,
}

impl TempWorkspace {
    fn new(parent: &Path) -> io::Result<Self> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for attempt in 0..10 {
            let path = parent.join(format!(".rcm-mux-{}-{now}-{attempt}", std::process::id()));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "无法创建音视频复用临时目录",
        ))
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn write_concat_playlist(path: &Path, track: &TrackProbe) -> io::Result<()> {
    let mut playlist = BufWriter::new(File::create(path)?);
    for (file, duration) in track.files.iter().zip(&track.segment_durations) {
        let path = file.path.display().to_string();
        let path = path.strip_prefix("\\\\?\\").unwrap_or(&path);
        let escaped = path.replace('\\', "/").replace('\'', "'\\''");
        writeln!(playlist, "file '{escaped}'")?;
        writeln!(playlist, "duration {duration:.6}")?;
    }
    playlist.flush()
}

#[derive(Default)]
struct PacketTimeline {
    packets: u64,
    pts_packets: u64,
    dts_packets: u64,
    min_pts: Option<f64>,
    max_pts_end: Option<f64>,
    last_dts: Option<f64>,
    dts_regressions: u64,
}

#[derive(Default)]
struct RuntimeTimeline {
    video: PacketTimeline,
    audio: PacketTimeline,
}

struct DebugPacket {
    input_index: usize,
    kind: TrackKind,
    pts: Option<f64>,
    dts: Option<f64>,
    duration: Option<f64>,
}

fn parse_debug_packet(line: &str) -> Option<DebugPacket> {
    let rest = line.split_once("demuxer -> ist_index:")?.1;
    let mut tokens = rest.split_whitespace();
    let input_index = tokens.next()?.split(':').next()?.parse().ok()?;
    let mut kind = None;
    let mut pts = None;
    let mut dts = None;
    let mut duration = None;
    for token in tokens {
        let Some((key, value)) = token.split_once(':') else {
            continue;
        };
        match key {
            "type" => {
                kind = match value {
                    "audio" => Some(TrackKind::Audio),
                    "video" => Some(TrackKind::Video),
                    _ => None,
                };
            }
            "pkt_pts_time" => pts = parse_debug_seconds(value),
            "pkt_dts_time" => dts = parse_debug_seconds(value),
            "duration_time" => duration = parse_debug_seconds(value),
            _ => {}
        }
    }
    Some(DebugPacket {
        input_index,
        kind: kind?,
        pts,
        dts,
        duration,
    })
}

fn parse_debug_seconds(value: &str) -> Option<f64> {
    let seconds = value.parse::<f64>().ok()?;
    seconds.is_finite().then_some(seconds)
}

fn display_ffmpeg_progress(output: impl Read, total_duration: f64) -> io::Result<()> {
    let mut out_time_us = None;
    let mut last_reported_percent = 0i32;

    for line in BufReader::new(output).lines() {
        let line = line?;
        if let Some(value) = line.strip_prefix("out_time_us=") {
            out_time_us = value.parse::<i64>().ok().filter(|value| *value >= 0);
        } else if line == "progress=continue" {
            if let Some(out_time_us) = out_time_us {
                let percent = if total_duration > 0.0 {
                    ((out_time_us as f64 / 1_000_000.0 / total_duration) * 100.0).clamp(0.0, 99.0)
                        as i32
                } else {
                    0
                };
                if percent >= last_reported_percent + 5 {
                    println!("音视频复用进度：{percent}%");
                    last_reported_percent = percent;
                }
            }
        } else if line == "progress=end" {
            println!("FFmpeg 已读完输入，正在校验临时输出…");
        }
    }
    Ok(())
}

fn run_ffmpeg_with_timeline(command: &mut Command, total_duration: f64) -> io::Result<()> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let progress = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("无法读取 ffmpeg 进度输出"))?;
    let progress_thread = thread::spawn(move || display_ffmpeg_progress(progress, total_duration));
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("无法读取 ffmpeg 时间戳检查输出"))?;
    let mut timeline = RuntimeTimeline::default();
    let mut diagnostics = VecDeque::with_capacity(8);

    for line in BufReader::new(stderr).lines() {
        let line = line?;
        if let Some(packet) = parse_debug_packet(&line) {
            let expected_input = match packet.kind {
                TrackKind::Video => 0,
                TrackKind::Audio => 1,
                TrackKind::Muxed | TrackKind::Other => continue,
            };
            if packet.input_index != expected_input {
                continue;
            }
            let track = match packet.kind {
                TrackKind::Video => &mut timeline.video,
                TrackKind::Audio => &mut timeline.audio,
                TrackKind::Muxed | TrackKind::Other => continue,
            };
            track.packets += 1;
            if let Some(pts) = packet.pts {
                track.pts_packets += 1;
                track.min_pts = Some(track.min_pts.map_or(pts, |current| current.min(pts)));
                let packet_end = pts + packet.duration.unwrap_or(0.0).max(0.0);
                track.max_pts_end = Some(
                    track
                        .max_pts_end
                        .map_or(packet_end, |current| current.max(packet_end)),
                );
            }
            if let Some(dts) = packet.dts {
                track.dts_packets += 1;
                if track
                    .last_dts
                    .is_some_and(|previous| dts + 0.001 < previous)
                {
                    track.dts_regressions += 1;
                }
                track.last_dts = Some(dts);
            }
        } else if line.contains("Non-monotonous DTS")
            || line.contains("Invalid data found")
            || line.contains("Error while decoding")
        {
            if diagnostics.len() == 8 {
                diagnostics.pop_front();
            }
            diagnostics.push_back(line);
        }
    }

    let status = child.wait()?;
    progress_thread
        .join()
        .map_err(|_| io::Error::other("ffmpeg 进度读取线程异常"))??;
    if !status.success() {
        let detail = diagnostics.into_iter().collect::<Vec<_>>().join(" | ");
        return Err(io::Error::other(format!(
            "ffmpeg 退出码 {}{}",
            status.code().unwrap_or(-1),
            if detail.is_empty() {
                String::new()
            } else {
                format!("：{detail}")
            }
        )));
    }
    if !diagnostics.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            diagnostics.into_iter().collect::<Vec<_>>().join(" | "),
        ));
    }

    validate_runtime_timeline(&timeline)?;
    let video_start = timeline.video.min_pts.unwrap();
    let video_end = timeline.video.max_pts_end.unwrap();
    let audio_start = timeline.audio.min_pts.unwrap();
    let audio_end = timeline.audio.max_pts_end.unwrap();
    let (start_delta, end_delta) =
        compare_track_time_ranges(video_start, video_end, audio_start, audio_end).map_err(
            |error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("完整流时间戳不对齐：{error}"),
                )
            },
        )?;
    println!(
        "PTS/DTS 单次流检查通过：视频 {} 包、音频 {} 包；起止偏差 {:.3}s / {:.3}s。",
        timeline.video.packets, timeline.audio.packets, start_delta, end_delta
    );
    Ok(())
}

fn compare_track_time_ranges(
    video_start: f64,
    video_end: f64,
    audio_start: f64,
    audio_end: f64,
) -> io::Result<(f64, f64)> {
    if ![video_start, video_end, audio_start, audio_end]
        .into_iter()
        .all(f64::is_finite)
        || video_end <= video_start
        || audio_end <= audio_start
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "音视频起止时间无效，无法确认是否对齐。",
        ));
    }
    let start_delta = (video_start - audio_start).abs();
    let end_delta = (video_end - audio_end).abs();
    if start_delta > AV_SYNC_TOLERANCE_SECONDS || end_delta > AV_SYNC_TOLERANCE_SECONDS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "音视频时间范围未对齐：起点相差 {:.3}s、终点相差 {:.3}s（容差 {:.3}s）",
                start_delta, end_delta, AV_SYNC_TOLERANCE_SECONDS
            ),
        ));
    }
    Ok((start_delta, end_delta))
}

fn validate_runtime_timeline(timeline: &RuntimeTimeline) -> io::Result<()> {
    for (name, track) in [("视频", &timeline.video), ("音频", &timeline.audio)] {
        if track.packets == 0 || track.pts_packets == 0 || track.dts_packets == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{name}流没有可校验的完整 PTS/DTS 时间戳"),
            ));
        }
        if track.dts_regressions > 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{name}流发现 {} 次 DTS 倒退", track.dts_regressions),
            ));
        }
        if track.min_pts.is_none() || track.max_pts_end.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{name}流缺少可比较的 PTS 范围"),
            ));
        }
    }
    Ok(())
}

fn mux_audio_video(
    ffmpeg: &Path,
    ffprobe: &Path,
    pair: &AudioVideoPair,
    output_file: &Path,
    output_ts: bool,
) -> io::Result<()> {
    let output_dir = output_file.parent().unwrap_or(Path::new("."));
    let workspace = TempWorkspace::new(output_dir)?;
    let video_playlist = workspace.path.join("video.ffconcat");
    let audio_playlist = workspace.path.join("audio.ffconcat");
    write_concat_playlist(&video_playlist, &pair.video)?;
    write_concat_playlist(&audio_playlist, &pair.audio)?;

    let temp_output = workspace.path.join(if output_ts {
        "muxed.partial.ts"
    } else {
        "muxed.partial.mp4"
    });
    let mut command = Command::new(ffmpeg);
    command
        .arg("-y")
        .arg("-loglevel")
        .arg("info")
        .arg("-debug_ts")
        .arg("-progress")
        .arg("pipe:1")
        .arg("-stats_period")
        .arg("5")
        .arg("-xerror")
        .arg("-copyts")
        .arg("-f")
        .arg("concat")
        .arg("-safe")
        .arg("0")
        .arg("-i")
        .arg(&video_playlist)
        .arg("-f")
        .arg("concat")
        .arg("-safe")
        .arg("0")
        .arg("-i")
        .arg(&audio_playlist)
        .arg("-map")
        .arg("0:v:0")
        .arg("-map")
        .arg("1:a:0")
        .arg("-c")
        .arg("copy");
    if output_ts {
        command.arg("-f").arg("mpegts");
    } else {
        command.arg("-movflags").arg("+faststart");
    }
    let total_duration = pair.video.end_time.max(pair.audio.end_time)
        - pair.video.start_time.min(pair.audio.start_time);
    run_ffmpeg_with_timeline(command.arg(&temp_output), total_duration)?;

    let output_probe = probe_media_streams(ffprobe, &temp_output)?;
    if classify_probe(&output_probe) != TrackKind::Muxed {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "临时输出未同时包含音频流和视频流",
        ));
    }

    publish_output(&temp_output, output_file, &workspace.path)?;
    Ok(())
}

fn publish_output(temp_output: &Path, output_file: &Path, workspace: &Path) -> io::Result<()> {
    if !output_file.exists() {
        return std::fs::rename(temp_output, output_file);
    }

    let backup = workspace.join("previous-output");
    std::fs::rename(output_file, &backup)?;
    if let Err(error) = std::fs::rename(temp_output, output_file) {
        let _ = std::fs::rename(&backup, output_file);
        return Err(error);
    }
    std::fs::remove_file(backup)
}

fn merge_files(files: &[MediaFile], output_file: &Path) -> io::Result<()> {
    let mut outfile = BufWriter::new(
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(output_file)?,
    );
    for (index, file) in files.iter().enumerate() {
        if files.len() <= 20 || index == 0 || (index + 1) % 100 == 0 || index + 1 == files.len() {
            println!(
                "合并进度 {}/{}：{}",
                index + 1,
                files.len(),
                compact_text(&file_name_display(&file.path), 64)
            );
        }
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
            println!(
                "✅ 转换完成: {}",
                compact_text(&file_name_display(&mp4_path), 88)
            );
            match remove_file(output_file) {
                Ok(()) => println!("✅ 已删除旧 TS 文件。"),
                Err(error) => println!("❌ 删除旧 TS 文件失败: {error}"),
            }
        }
        Ok(status) => println!(
            "❌ ffmpeg 运行失败，退出码: {status}；保留 TS 文件: {}",
            compact_text(&file_name_display(output_file), 88)
        ),
        Err(error) => println!(
            "❌ 调用 ffmpeg 失败: {error}；保留 TS 文件: {}",
            compact_text(&file_name_display(output_file), 88)
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

fn which_ffprobe() -> Option<PathBuf> {
    if let Ok(ffprobe_in_path) = which::which("ffprobe") {
        return Some(ffprobe_in_path);
    }

    for candidate in ["./ffprobe", "./ffprobe.exe"] {
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
        TsLayout, build_merge_groups, build_output_paths, classify_probe, collect_media_files,
        compact_text, compare_media_files, compare_track_time_ranges, file_name_display,
        find_first_pes_pts, group_candidates_by_directory, inspect_candidate_files,
        inspect_media_files, irregular_filename_samples, is_dotfile, normalize_pts_ticks,
        parse_args, parse_debug_packet, parse_ffprobe_flat, resolve_input_dir,
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

    fn range_files(names: &[&str]) -> Vec<super::MediaFile> {
        names
            .iter()
            .map(|name| {
                let path = PathBuf::from(name);
                let (sequence, series_key) = super::sequence_details(Path::new("."), &path);
                super::MediaFile {
                    path,
                    size: 1,
                    sequence,
                    series_key,
                    ts_layout: None,
                }
            })
            .collect()
    }

    #[test]
    fn resolves_range_boundaries_by_exact_filename() {
        let files = range_files(&["切片 201.ts", "切片 202.ts", "切片 203.ts"]);
        let range =
            super::resolve_file_range(&files, Some("切片 202.ts"), Some("切片 203.ts")).unwrap();
        assert_eq!(range.start.index, 1);
        assert_eq!(range.end.index, 2);
        assert_eq!(range.start.kind, super::BoundaryMatch::FileName);
        assert_eq!(range.end.kind, super::BoundaryMatch::FileName);
        assert!(super::resolve_file_range(&files, Some("切片 202"), None).is_err());
    }

    #[test]
    fn resolves_whole_numeric_fields_with_leading_zeros_and_multiple_counters() {
        let files = range_files(&[
            "00_002090_index_4_2185.ts",
            "00_002091_index_4_2186.ts",
            "00_002092_index_4_2187.ts",
        ]);
        let range = super::resolve_file_range(&files, Some("0002091"), Some("002187")).unwrap();
        assert_eq!(range.start.index, 1);
        assert_eq!(range.end.index, 2);
        assert_eq!(range.start.kind, super::BoundaryMatch::Number);
        assert_eq!(range.end.kind, super::BoundaryMatch::Number);
    }

    #[test]
    fn prioritizes_filename_numbers_over_file_order() {
        let files = range_files(&["segment002.ts", "segment003.ts", "segment004.ts"]);
        let boundary = super::resolve_boundary(&files, "0002", "--start").unwrap();
        assert_eq!(boundary.index, 0);
        assert_eq!(boundary.kind, super::BoundaryMatch::Number);

        let files = range_files(&["002_0002_0002.ts", "003_0003_0003.ts"]);
        assert_eq!(
            super::resolve_boundary(&files, "2", "--start")
                .unwrap()
                .index,
            0
        );
    }

    #[test]
    fn falls_back_to_one_based_order_without_matching_numeric_substrings() {
        let files = range_files(&["segment101.ts", "segment102.ts", "segment103.ts"]);
        let range = super::resolve_file_range(&files, Some("0001"), Some("0002")).unwrap();
        assert_eq!(range.start.index, 0);
        assert_eq!(range.end.index, 1);
        assert_eq!(range.start.kind, super::BoundaryMatch::Order);
        assert_eq!(range.end.kind, super::BoundaryMatch::Order);
        for value in ["0", "4", "10", "1010", "999999999999999999999999999"] {
            let error = super::resolve_boundary(&files, value, "--start").unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
            assert!(error.to_string().contains("超出文件顺序范围"));
        }
        let files = range_files(&["segment201.mp4", "segment202.mp4", "segment203.mp4"]);
        assert!(super::resolve_boundary(&files, "4", "--start").is_err());
    }

    #[test]
    fn rejects_ambiguous_numeric_matches_instead_of_falling_back_to_order() {
        let files = range_files(&[
            "00_002090_index_2_2185.ts",
            "00_002091_index_2_2186.ts",
            "00_002092_index_2_2187.ts",
        ]);
        let error = super::resolve_boundary(&files, "2", "--start").unwrap_err();
        assert!(error.to_string().contains("匹配多个文件"));
        assert!(error.to_string().contains("00_002090_index_2_2185.ts"));

        let files = range_files(&["segment002_count3.ts", "segment003_count4.ts"]);
        assert!(super::resolve_boundary(&files, "3", "--end").is_err());
        let files = range_files(&["segment101.m4s", "segment101.decrypt"]);
        assert!(super::resolve_boundary(&files, "101", "--start").is_err());
        assert_eq!(
            super::resolve_boundary(&files, "segment101.decrypt", "--start")
                .unwrap()
                .index,
            1
        );
    }

    #[test]
    fn numeric_range_matching_handles_zero_and_values_larger_than_u64() {
        let files = range_files(&["0000.ts", "999999999999999999999999999.ts"]);
        assert_eq!(
            super::resolve_boundary(&files, "00", "--start")
                .unwrap()
                .index,
            0
        );
        assert_eq!(
            super::resolve_boundary(&files, "000999999999999999999999999999", "--end")
                .unwrap()
                .index,
            1
        );
    }

    #[test]
    fn validates_range_order_and_supports_open_or_single_file_ranges() {
        let files = range_files(&["segment101.ts", "segment102.ts", "segment103.ts"]);
        let range = super::resolve_file_range(&files, None, Some("102")).unwrap();
        assert_eq!(range.start.index, 0);
        assert_eq!(range.start.kind, super::BoundaryMatch::Default);
        assert_eq!(range.end.index, 1);
        let range = super::resolve_file_range(&files, Some("102"), None).unwrap();
        assert_eq!(range.end.index, 2);
        assert_eq!(range.end.kind, super::BoundaryMatch::Default);
        let range = super::resolve_file_range(&files, Some("102"), Some("segment102.ts")).unwrap();
        assert_eq!(range.start.index, range.end.index);
        let error = super::resolve_file_range(&files, Some("103"), Some("101")).unwrap_err();
        assert!(error.to_string().contains("晚于终点"));
        assert!(super::resolve_file_range(&files, Some("missing.ts"), None).is_err());
        assert!(super::resolve_file_range(&[], Some("1"), None).is_err());
    }

    #[test]
    fn range_selection_rechecks_gaps_and_preserves_known_filename_patterns() {
        let files = range_files(&["segment101.ts", "segment103.ts", "segment104.ts"]);
        let recognized_series = super::numbered_series_counts(&files)
            .into_keys()
            .map(str::to_string)
            .collect();
        let gaps = super::find_sequence_gaps(&files);
        let mut inspection = super::Inspection {
            candidate_count: files.len(),
            files,
            skipped_typescript: Vec::new(),
            duplicate_count: 0,
            warnings: Vec::new(),
            gaps,
            recognized_series,
        };
        assert_eq!(inspection.gaps.len(), 1);
        let range = super::resolve_file_range(&inspection.files, Some("103"), Some("104")).unwrap();
        super::apply_file_range(&mut inspection, &range);
        assert_eq!(inspection.files.len(), 2);
        assert!(inspection.gaps.is_empty());
        let range = super::resolve_file_range(&inspection.files, Some("104"), None).unwrap();
        super::apply_file_range(&mut inspection, &range);
        assert_eq!(inspection.files.len(), 1);
        assert!(irregular_filename_samples(&inspection).is_empty());
        assert_eq!(inspection.candidate_count, 3);
    }

    #[test]
    fn refuses_to_drop_initialization_fragments_from_a_selected_range() {
        let files = range_files(&["init.mp4", "segment201.m4s", "segment202.m4s"]);
        let error = super::resolve_file_range(&files, Some("201"), Some("202")).unwrap_err();
        assert!(error.to_string().contains("排除了初始化片段"));
        assert!(super::resolve_file_range(&files, Some("init.mp4"), Some("202")).is_ok());
    }

    #[test]
    fn parses_start_and_end_options_and_rejects_missing_values() {
        let options = parse_args([
            "--start".to_string(),
            "切片 201.ts".to_string(),
            "--end=000203".to_string(),
        ])
        .unwrap();
        assert_eq!(options.start.as_deref(), Some("切片 201.ts"));
        assert_eq!(options.end.as_deref(), Some("000203"));
        let options = parse_args([
            "--start=2".to_string(),
            "--end".to_string(),
            "3".to_string(),
        ])
        .unwrap();
        assert_eq!(options.start.as_deref(), Some("2"));
        assert_eq!(options.end.as_deref(), Some("3"));
        for args in [
            vec!["--start"],
            vec!["--end"],
            vec!["--start", ""],
            vec!["--end="],
            vec!["--start="],
            vec!["--start", "--end", "3"],
        ] {
            assert!(parse_args(args.into_iter().map(str::to_string)).is_err());
        }
    }

    fn series_count(inspection: &super::Inspection) -> usize {
        inspection
            .files
            .iter()
            .filter_map(|file| file.series_key.as_deref())
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    }

    #[test]
    fn recognizes_synchronized_filename_counters_as_one_sequence() {
        let root = temp_dir("synchronized-counters-test");
        std::fs::create_dir_all(&root).unwrap();
        let mut transport_stream = vec![0xff; 188 * 4];
        for offset in (0..transport_stream.len()).step_by(188) {
            transport_stream[offset] = 0x47;
        }
        for counter in (2090..=2139).rev() {
            std::fs::write(
                root.join(format!("00_{counter:06}_index_4_{}.ts", counter + 95)),
                &transport_stream,
            )
            .unwrap();
        }

        let inspection = inspect_media_files(&root, false).unwrap();
        assert_eq!(inspection.files.len(), 50);
        assert_eq!(series_count(&inspection), 1);
        assert!(irregular_filename_samples(&inspection).is_empty());
        assert!(inspection.gaps.is_empty());
        assert_eq!(
            inspection
                .files
                .iter()
                .map(|file| file.sequence.unwrap())
                .collect::<Vec<_>>(),
            (2185..=2234).collect::<Vec<_>>()
        );
        assert!(matches!(
            super::assess_audio_video_tracks(&inspection, None),
            super::AvAssessment::SingleProgram
        ));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn checks_gaps_and_duplicates_with_synchronized_counters() {
        let root = temp_dir("synchronized-gap-test");
        std::fs::create_dir_all(&root).unwrap();
        for counter in [2090, 2091, 2093] {
            std::fs::write(
                root.join(format!("00_{counter:06}_index_4_{}.m4s", counter + 95)),
                b"payload",
            )
            .unwrap();
        }
        std::fs::write(root.join("00_002091_index_4_2186.decrypt"), b"payload").unwrap();
        std::fs::write(root.join("00_002090_index_4_2185.decrypt"), b"different").unwrap();

        let inspection = inspect_media_files(&root, false).unwrap();
        assert_eq!(series_count(&inspection), 1);
        assert_eq!(inspection.duplicate_count, 1);
        assert_eq!(inspection.files.len(), 4);
        assert_eq!(inspection.gaps.len(), 1);
        assert_eq!(inspection.gaps[0].missing_start, 2187);
        assert_eq!(inspection.gaps[0].missing_end, 2187);
        assert!(
            inspection
                .warnings
                .iter()
                .any(|warning| warning.contains("编号重复但二进制内容不同"))
        );
        assert_eq!(
            build_merge_groups(&inspection.files, &inspection.gaps, true).len(),
            2
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preserves_numeric_track_identifiers_when_inferring_counters() {
        let root = temp_dir("synchronized-tracks-test");
        std::fs::create_dir_all(&root).unwrap();
        for counter in 2090..=2092 {
            for (track, index) in [(0, 4), (1, 5)] {
                std::fs::write(
                    root.join(format!(
                        "{track:02}_{counter:06}_index_{index}_{}.m4s",
                        counter + 95
                    )),
                    b"payload",
                )
                .unwrap();
            }
        }

        let inspection = inspect_media_files(&root, false).unwrap();
        assert_eq!(inspection.files.len(), 6);
        assert_eq!(series_count(&inspection), 2);
        assert!(irregular_filename_samples(&inspection).is_empty());
        assert!(inspection.gaps.is_empty());
        assert!(matches!(
            super::assess_audio_video_tracks(&inspection, None),
            super::AvAssessment::Unsafe(_)
        ));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn does_not_infer_counters_with_inconsistent_offsets() {
        let root = temp_dir("inconsistent-counters-test");
        std::fs::create_dir_all(&root).unwrap();
        for (counter, sequence) in [(2090, 2185), (2091, 2186), (2100, 2187)] {
            std::fs::write(
                root.join(format!("00_{counter:06}_index_4_{sequence}.m4s")),
                b"payload",
            )
            .unwrap();
        }

        let inspection = inspect_media_files(&root, false).unwrap();
        assert_eq!(series_count(&inspection), 3);
        assert_eq!(irregular_filename_samples(&inspection).len(), 3);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn does_not_coalesce_two_singleton_tracks_with_matching_offsets() {
        let root = temp_dir("singleton-track-test");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("track1_001.m4s"), b"first").unwrap();
        std::fs::write(root.join("track2_002.m4s"), b"second").unwrap();

        let inspection = inspect_media_files(&root, false).unwrap();
        assert_eq!(series_count(&inspection), 2);
        assert_eq!(irregular_filename_samples(&inspection).len(), 2);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn handles_unicode_and_padding_in_multiple_synchronized_counters() {
        let root = temp_dir("unicode-counters-test");
        std::fs::create_dir_all(&root).unwrap();
        for name in [
            "切片00_2098_index_4_2118_2193.m4s",
            "切片00_002099_index_4_002119_2194.m4s",
            "切片00_2100_index_4_2120_2195.m4s",
        ] {
            std::fs::write(root.join(name), b"payload").unwrap();
        }

        let inspection = inspect_media_files(&root, false).unwrap();
        assert_eq!(series_count(&inspection), 1);
        assert!(irregular_filename_samples(&inspection).is_empty());
        assert!(inspection.gaps.is_empty());
        assert_eq!(inspection.files[0].sequence, Some(2193));
        assert_eq!(inspection.files[2].sequence, Some(2195));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn checks_audio_video_timeline_with_small_boundary_tolerance() {
        assert!(compare_track_time_ranges(1.0, 3.0, 1.1, 2.95).is_ok());
        assert!(compare_track_time_ranges(1.0, 3.0, 1.5, 2.95).is_err());
        assert!(compare_track_time_ranges(1.0, 1.0, 1.0, 1.0).is_err());
    }

    #[test]
    fn parses_ffprobe_stream_metadata_and_timeline() {
        let probe = parse_ffprobe_flat(
            r#"streams.stream.0.codec_type="video"
streams.stream.0.codec_name="h264"
streams.stream.0.start_time="1.250000"
format.start_time="1.250000"
format.duration="2.000000""#,
        );
        assert_eq!(classify_probe(&probe), super::TrackKind::Video);
        assert_eq!(probe.start_time, Some(1.25));
        assert_eq!(probe.duration, Some(2.0));
        assert_eq!(probe.streams[0].codec_name, "h264");
    }

    #[test]
    fn extracts_segment_pts_from_transport_stream_packets() {
        let root = temp_dir("ts-pts-test");
        std::fs::create_dir_all(&root).unwrap();
        let pid = 0x100u16;
        let ticks = 126_000u64;
        let pts = [
            0x21 | ((((ticks >> 30) & 0x07) as u8) << 1),
            (ticks >> 22) as u8,
            ((((ticks >> 15) & 0x7f) as u8) << 1) | 1,
            (ticks >> 7) as u8,
            (((ticks & 0x7f) as u8) << 1) | 1,
        ];
        let mut packet = [0xffu8; 188];
        packet[0] = 0x47;
        packet[1] = 0x40 | ((pid >> 8) as u8 & 0x1f);
        packet[2] = pid as u8;
        packet[3] = 0x10;
        packet[4..13].copy_from_slice(&[0, 0, 1, 0xe0, 0, 0, 0x80, 0x80, 5]);
        packet[13..18].copy_from_slice(&pts);
        let path = root.join("segment.ts");
        std::fs::write(&path, packet).unwrap();

        assert_eq!(
            find_first_pes_pts(
                &path,
                TsLayout {
                    stride: 188,
                    sync_offset: 0,
                },
                pid
            )
            .unwrap(),
            ticks
        );
        assert_eq!(
            normalize_pts_ticks(&[90_000, 180_000]).unwrap(),
            vec![1.0, 2.0]
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parses_ffmpeg_debug_pts_dts_packet_lines() {
        let packet = parse_debug_packet(
            "[aist#1:0/aac @ 0x123] demuxer -> ist_index:1:0 type:audio pkt_pts:126000 pkt_pts_time:1.400000 pkt_dts:126000 pkt_dts_time:1.400000 duration:1920 duration_time:0.021333",
        )
        .unwrap();
        assert_eq!(packet.input_index, 1);
        assert_eq!(packet.kind, super::TrackKind::Audio);
        assert_eq!(packet.pts, Some(1.4));
        assert_eq!(packet.dts, Some(1.4));
        assert_eq!(packet.duration, Some(0.021333));
    }

    #[test]
    fn formats_console_labels_without_debug_paths() {
        assert_eq!(
            file_name_display(Path::new("segment001.ts")),
            "segment001.ts"
        );
        let compact = compact_text("a very long path component for terminal output", 20);
        assert!(compact.starts_with('…'));
        assert_eq!(compact.chars().count(), 20);
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
    fn parses_help_and_version_flags_without_requiring_input_options() {
        for flag in ["-h", "--help"] {
            let options = parse_args([flag.to_string(), "--input-dir".to_string()]).unwrap();
            assert!(options.show_help);
            assert!(!options.show_version);
        }
        for flag in ["-V", "--version"] {
            let options = parse_args([flag.to_string(), "--input-dir".to_string()]).unwrap();
            assert!(options.show_version);
            assert!(!options.show_help);
        }
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

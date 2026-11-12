use super::*;

pub struct GuiRequest {
    pub paths: Vec<PathBuf>,
    pub start: Option<String>,
    pub end: Option<String>,
    pub ts: bool,
    pub recursive: bool,
    pub include_hidden: bool,
    pub split_gaps: bool,
}

pub struct GuiPlan {
    pub summary: Vec<String>,
    pub files: Vec<String>,
    pub outputs: Vec<PathBuf>,
    actions: Vec<DirectoryAction>,
    ffmpeg: Option<PathBuf>,
    ffprobe: Option<PathBuf>,
    ts: bool,
}

pub fn prepare(request: GuiRequest) -> io::Result<GuiPlan> {
    let mut candidates = BTreeSet::new();
    for path in request.paths {
        let path = path.canonicalize()?;
        if path.is_dir() {
            candidates.extend(collect_media_files(
                &path,
                request.include_hidden,
                request.recursive,
            ));
        } else if path.is_file() {
            if !path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                ["ts", "decrypt", "mp4", "m4s"]
                    .iter()
                    .any(|supported| e.eq_ignore_ascii_case(supported))
            }) {
                return Err(io::Error::other(format!(
                    "不支持的分片格式：{}",
                    path.display()
                )));
            }
            candidates.insert(path);
        } else {
            return Err(io::Error::other("请选择文件或目录"));
        }
    }
    let ffmpeg = which_ffmpeg();
    let ffprobe = which_ffprobe();
    if !request.ts && ffmpeg.is_none() {
        return Err(io::Error::other(
            "MP4 输出需要 FFmpeg，请安装 FFmpeg 或选择 TS。",
        ));
    }
    let mut plan = GuiPlan {
        summary: vec![],
        files: vec![],
        outputs: vec![],
        actions: vec![],
        ffmpeg,
        ffprobe,
        ts: request.ts,
    };
    for (directory, candidates) in group_candidates_by_directory(candidates.into_iter().collect()) {
        let mut inspection = inspect_candidate_files(&directory, candidates)?;
        if inspection.files.is_empty() {
            return Err(io::Error::other("所选内容中没有可合并的媒体分片"));
        }
        if request.start.is_some() || request.end.is_some() {
            let range = resolve_file_range(
                &inspection.files,
                request.start.as_deref(),
                request.end.as_deref(),
            )?;
            for (label, boundary) in [("起始", range.start), ("结束", range.end)] {
                let method = match boundary.kind {
                    BoundaryMatch::FileName => "文件名匹配",
                    BoundaryMatch::Number => "数字字段匹配",
                    BoundaryMatch::Order => "文件顺序匹配",
                    BoundaryMatch::Default => "默认边界",
                };
                plan.summary.push(format!(
                    "{label}：{method} · {}（第 {} 个）",
                    file_name_display(&inspection.files[boundary.index].path),
                    boundary.index + 1
                ));
            }
            apply_file_range(&mut inspection, &range);
        }
        plan.summary.push(format!(
            "{} · {} 个分片 · {}",
            directory.display(),
            inspection.files.len(),
            format_bytes(inspection.files.iter().map(|f| f.size).sum())
        ));
        plan.summary.extend(inspection.warnings.iter().cloned());
        if !inspection.gaps.is_empty() {
            plan.summary.push(format!(
                "发现 {} 处缺片；{}",
                inspection.gaps.len(),
                if request.split_gaps {
                    "将在缺片处拆分"
                } else {
                    "将按当前顺序拼接，可能出现跳跃"
                }
            ));
        }
        if !irregular_filename_samples(&inspection).is_empty() {
            plan.summary
                .push("文件名规律不足，请核对下方完整文件列表。".into());
        }
        plan.files.extend(
            inspection
                .files
                .iter()
                .map(|f| f.path.display().to_string()),
        );
        let action = match assess_audio_video_tracks(&inspection, plan.ffprobe.as_deref()) {
            AvAssessment::Unsafe(reason) => return Err(io::Error::other(reason)),
            AvAssessment::Pair(pair) => {
                if plan.ffmpeg.is_none() {
                    return Err(io::Error::other("音视频分轨复用需要 FFmpeg"));
                }
                plan.summary.push(format!(
                    "独立音视频轨：起点偏差 {:.3}s，终点偏差 {:.3}s",
                    pair.start_delta, pair.end_delta
                ));
                let mut output_file = build_output_paths(
                    &directory,
                    &directory,
                    env!("CARGO_PKG_VERSION"),
                    &[inspection.files.clone()],
                )
                .remove(0);
                if !request.ts {
                    output_file.set_extension("mp4");
                }
                plan.outputs.push(output_file.clone());
                DirectoryAction::MuxTracks { pair, output_file }
            }
            AvAssessment::SingleProgram => {
                let groups =
                    build_merge_groups(&inspection.files, &inspection.gaps, request.split_gaps);
                let output_files =
                    build_output_paths(&directory, &directory, env!("CARGO_PKG_VERSION"), &groups);
                plan.outputs.extend(output_files.iter().map(|p| {
                    if request.ts {
                        p.clone()
                    } else {
                        p.with_extension("mp4")
                    }
                }));
                for path in &output_files {
                    reject_existing(path)?;
                }
                DirectoryAction::Concatenate {
                    groups,
                    output_files,
                }
            }
        };
        plan.actions.push(action);
    }
    if plan.actions.is_empty() {
        return Err(io::Error::other("扫描范围内没有可合并的媒体分片"));
    }
    for path in &plan.outputs {
        reject_existing(path)?;
    }
    Ok(plan)
}

fn reject_existing(path: &Path) -> io::Result<()> {
    if path.exists() {
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("输出已存在，请先移动或重命名：{}", path.display()),
        ))
    } else {
        Ok(())
    }
}

pub fn execute(plan: GuiPlan) -> io::Result<Vec<PathBuf>> {
    for path in &plan.outputs {
        reject_existing(path)?;
    }
    for action in plan.actions {
        match action {
            DirectoryAction::MuxTracks { pair, output_file } => mux_audio_video(
                plan.ffmpeg
                    .as_deref()
                    .ok_or_else(|| io::Error::other("找不到 FFmpeg"))?,
                plan.ffprobe
                    .as_deref()
                    .ok_or_else(|| io::Error::other("找不到 ffprobe"))?,
                &pair,
                &output_file,
                plan.ts,
            )?,
            DirectoryAction::Concatenate {
                groups,
                output_files,
            } => {
                for (group, output) in groups.iter().zip(&output_files) {
                    reject_existing(output)?;
                    merge_files(group, output)?;
                    if !plan.ts {
                        let workspace =
                            TempWorkspace::new(output.parent().unwrap_or(Path::new(".")))?;
                        let temp = workspace.path.join("output.mp4");
                        let result = Command::new(
                            plan.ffmpeg
                                .as_ref()
                                .ok_or_else(|| io::Error::other("找不到 FFmpeg"))?,
                        )
                        .args(["-nostdin", "-v", "error", "-n", "-i"])
                        .arg(output)
                        .args(["-c", "copy"])
                        .arg(&temp)
                        .output()?;
                        if !result.status.success() {
                            return Err(io::Error::other(format!(
                                "转换失败，保留 TS {}：{}",
                                output.display(),
                                String::from_utf8_lossy(&result.stderr)
                            )));
                        }
                        publish_output(&temp, &output.with_extension("mp4"), &workspace.path)?;
                        remove_file(output)?;
                    }
                }
            }
        }
    }
    Ok(plan.outputs)
}

#[cfg(test)]
mod tests {
    use super::{GuiRequest, execute, prepare};
    #[test]
    fn selected_files_do_not_include_siblings_and_existing_outputs_are_rejected() {
        let root = std::env::temp_dir().join(format!(
            "rcm-gui-selected-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let input = root.join("chunks");
        std::fs::create_dir_all(&input).unwrap();
        for (name, bytes) in [
            ("segment201.m4s", b"first".as_slice()),
            ("segment202.m4s", b"excluded"),
            ("segment203.m4s", b"last"),
        ] {
            std::fs::write(input.join(name), bytes).unwrap();
        }
        let request = || GuiRequest {
            paths: vec![input.join("segment201.m4s"), input.join("segment203.m4s")],
            start: None,
            end: None,
            ts: true,
            recursive: false,
            include_hidden: false,
            split_gaps: false,
        };
        let plan = prepare(request()).unwrap();
        assert_eq!(plan.files.len(), 2);
        assert!(!plan.files.iter().any(|p| p.ends_with("segment202.m4s")));
        let paths = execute(plan).unwrap();
        assert_eq!(std::fs::read(&paths[0]).unwrap(), b"firstlast");
        assert_eq!(
            std::fs::read(input.join("segment202.m4s")).unwrap(),
            b"excluded"
        );
        assert!(prepare(request()).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}

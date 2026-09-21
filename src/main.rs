use std::env;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions, remove_file};
use std::io::{self, BufReader, BufWriter, copy};
use std::path::{Path, PathBuf};
use std::process::Command;

use natord::compare;
use walkdir::{DirEntry, WalkDir};

fn main() -> io::Result<()> {
    // ----------------------------
    // 1. 获取当前文件夹名
    // ----------------------------
    let current_dir = env::current_dir()?;
    let folder_name = current_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("output");

    // 编译时包版本，用于输出文件名区分
    let version = env!("CARGO_PKG_VERSION");

    // ----------------------------
    // 额外命令行选项
    //    --ts             : 生成 .ts 并跳过 ffmpeg 转换为 mp4
    //    --include-hidden : 包含隐藏文件和 dotfile
    // ----------------------------
    let args: Vec<String> = env::args().collect();
    let skip_ffmpeg = args.iter().any(|a| a == "--ts");
    let include_hidden = args.iter().any(|a| a == "--include-hidden");

    // ----------------------------
    // 2. 收集支持的文件类型
    //    支持: ts, decrypt, mp4, m4s
    //    默认跳过隐藏文件和 dotfile；可用 --include-hidden 覆盖
    // ----------------------------
    let mut files = collect_media_files(Path::new("./"), include_hidden);

    if !include_hidden {
        println!("默认跳过隐藏文件和 dotfile（如需包含请使用 --include-hidden）");
    }

    // ----------------------------
    // 3. 自然排序
    // ----------------------------
    files.sort_by(|a, b| {
        let sa = a.file_name().unwrap().to_string_lossy();
        let sb = b.file_name().unwrap().to_string_lossy();
        compare(&sa, &sb)
    });

    // ----------------------------
    // MPD 特殊处理：若存在文件名包含 "init" 的支持文件，
    // 则将第一个匹配的 init 文件移动到合并列表的首位
    // ----------------------------
    if let Some(pos) = files.iter().position(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_lowercase().contains("init"))
            .unwrap_or(false)
    }) && pos != 0
    {
        let init_path = files.remove(pos);
        files.insert(0, init_path);
    }

    // ----------------------------
    // 4. 获取首尾文件名
    // ----------------------------
    let start_name = files
        .first()
        .and_then(|p| p.file_stem())
        .and_then(|s| s.to_str())
        .unwrap_or("start");

    let end_name = files
        .last()
        .and_then(|p| p.file_stem())
        .and_then(|s| s.to_str())
        .unwrap_or("end");

    // ----------------------------
    // 5. 构建输出路径 -> 上一级文件夹
    // ----------------------------
    let parent_dir = current_dir.parent().unwrap_or(Path::new("."));
    let output_file = parent_dir.join(format!(
        "{}[v{}][{}-{}].ts",
        folder_name, version, start_name, end_name
    ));

    let mut outfile = BufWriter::new(
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&output_file)?,
    );

    // ----------------------------
    // 6. 按顺序合并
    // ----------------------------
    for path in &files {
        println!("Merging {:?}", path);
        let infile = File::open(path)?;
        let mut reader = BufReader::new(infile);
        copy(&mut reader, &mut outfile)?;
    }

    println!("✅ Merge finished -> {:?}", output_file);

    // ----------------------------
    // 7. 根据参数决定是否使用 ffmpeg 转换为 mp4
    // ----------------------------
    if skip_ffmpeg {
        println!(
            "已指定 --ts，跳过 ffmpeg 转换，保留 ts 文件: {:?}",
            output_file
        );
    } else {
        let ffmpeg_path = which_ffmpeg();
        match ffmpeg_path {
            Some(ffmpeg) => {
                let mp4_path = output_file.with_extension("mp4");
                println!("正在调用 ffmpeg 转换为 mp4...");
                let status = Command::new(ffmpeg)
                    .args([
                        "-y", // 覆盖输出
                        "-i",
                        output_file.to_str().unwrap(),
                        "-c",
                        "copy",
                        mp4_path.to_str().unwrap(),
                    ])
                    .status();

                match status {
                    Ok(s) if s.success() => {
                        println!("✅ 转换完成: {:?}", mp4_path);

                        // 尝试删除旧的输出文件（例如 .ts），若删除失败则给出相应提示
                        match remove_file(&output_file) {
                            Ok(_) => println!("✅ 已删除旧文件: {:?}", output_file),
                            Err(e) => {
                                if e.kind() == io::ErrorKind::PermissionDenied
                                    || e.raw_os_error() == Some(32)
                                {
                                    println!("❌ 无法删除旧文件，文件被占用: {:?}", output_file);
                                } else {
                                    println!("❌ 删除旧文件失败: {}，文件: {:?}", e, output_file);
                                }
                            }
                        }
                    }
                    Ok(s) => {
                        println!("❌ ffmpeg 运行失败，退出码: {}", s);
                        println!("输出 ts 文件: {:?}", output_file);
                    }
                    Err(e) => {
                        println!("❌ 调用 ffmpeg 失败: {}", e);
                        println!("输出 ts 文件: {:?}", output_file);
                    }
                }
            }
            None => {
                println!("❌ 未找到 ffmpeg，请将 ffmpeg 添加到系统 PATH 或放在当前目录下。");
                println!("输出 ts 文件: {:?}", output_file);
            }
        }
    }

    // ----------------------------
    // 8. 按回车继续
    // ----------------------------
    println!("按回车键退出...");
    let mut input = String::new();
    io::stdin().read_line(&mut input).unwrap();

    Ok(())
}

/// 收集媒体分片，并在默认情况下跳过隐藏文件和隐藏目录。
fn collect_media_files(root: &Path, include_hidden: bool) -> Vec<PathBuf> {
    const SUPPORTED_EXTS: [&str; 4] = ["ts", "decrypt", "mp4", "m4s"];

    WalkDir::new(root)
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
    use super::{collect_media_files, is_dotfile};
    use std::ffi::OsStr;

    #[test]
    fn recognizes_dotfiles() {
        assert!(is_dotfile(OsStr::new(".DS_Store")));
        assert!(is_dotfile(OsStr::new(".segment.ts")));
        assert!(!is_dotfile(OsStr::new("segment.ts")));
    }

    #[test]
    fn skips_hidden_files_and_directories_by_default() {
        let root = std::env::temp_dir().join(format!(
            "rs_chunkmerger-hidden-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::write(root.join("part1.ts"), b"visible").unwrap();
        std::fs::write(root.join(".part2.ts"), b"dotfile").unwrap();
        std::fs::write(root.join(".hidden/part3.ts"), b"hidden directory").unwrap();

        let files = collect_media_files(&root, false);
        assert_eq!(files, vec![root.join("part1.ts")]);

        std::fs::remove_dir_all(root).unwrap();
    }
}

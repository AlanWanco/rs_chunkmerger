use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, copy};
use walkdir::WalkDir;
use std::env;
use natord::compare;
use std::path::Path;

fn main() -> io::Result<()> {
    // ----------------------------
    // 1. 获取当前文件夹名
    // ----------------------------
    let current_dir = env::current_dir()?;
    let folder_name = current_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("output");

    // ----------------------------
    // 2. 收集所有 .ts 文件
    // ----------------------------
    let mut files: Vec<_> = WalkDir::new("./")
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| e.path().extension().map(|ext| ext == "ts").unwrap_or(false))
        .map(|e| e.into_path())
        .collect();

    // ----------------------------
    // 3. 自然排序
    // ----------------------------
    files.sort_by(|a, b| {
        let sa = a.file_name().unwrap().to_string_lossy();
        let sb = b.file_name().unwrap().to_string_lossy();
        compare(&sa, &sb)
    });

    // ----------------------------
    // 4. 获取首尾文件名
    // ----------------------------
    let start_name = files.first()
        .and_then(|p| p.file_stem())
        .and_then(|s| s.to_str())
        .unwrap_or("start");

    let end_name = files.last()
        .and_then(|p| p.file_stem())
        .and_then(|s| s.to_str())
        .unwrap_or("end");

    // ----------------------------
    // 5. 构建输出路径 -> 上一级文件夹
    // ----------------------------
    let parent_dir = current_dir.parent().unwrap_or(Path::new("."));
    let output_file = parent_dir.join(format!("{}[{}-{}].ts", folder_name, start_name, end_name));

    let mut outfile = BufWriter::new(
        OpenOptions::new().create(true).write(true).truncate(true).open(&output_file)?
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
    // 7. 按回车继续
    // ----------------------------
    println!("按回车键退出...");
    let mut input = String::new();
    io::stdin().read_line(&mut input).unwrap();

    Ok(())
}

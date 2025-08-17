// 引入需要用到的标准库模块
use std::fs::{File, OpenOptions};   // 处理文件读写
use std::io::{self, Write, Read};    // 处理输入输出、读取和写入
use walkdir::WalkDir;                // 用于遍历文件夹
use std::env;                        // 获取当前目录
use std::path::Path;                 // 处理路径相关

fn main() -> io::Result<()> {
    // ----------------------------
    // 1. 获取当前工作目录
    // ----------------------------
    let current_dir = env::current_dir()?; // 返回的是一个 PathBuf
    // 从路径里取出文件夹名（最后一级）
    let folder_name = current_dir
        .file_name()                    // 拿到文件夹名（OsStr 类型）
        .and_then(|name| name.to_str()) // 转成 Rust &str
        .unwrap_or("output");           // 如果失败就用 "output"

    // ----------------------------
    // 2. 定义输出文件名：文件夹名 + ".ts"
    // ----------------------------
    let output_file = format!("{}.ts", folder_name);

    // 创建/覆盖一个新的输出文件
    let mut outfile = OpenOptions::new()
        .create(true)   // 如果文件不存在就创建
        .write(true)    // 打开写入权限
        .truncate(true) // 如果文件已经存在就清空
        .open(&output_file)?; // 可能失败，所以用 ?

    // ----------------------------
    // 3. 遍历当前目录下所有文件
    // ----------------------------
    for entry in WalkDir::new("./") {
        let entry = entry?; // 每个 entry 是一个文件或文件夹
        if entry.file_type().is_file() {
            let path = entry.path(); // 获取路径

            // 检查文件扩展名是不是 "ts"
            if let Some(ext) = path.extension() {
                if ext == "ts" {
                    println!("正在合并 {:?}", path);

                    // 打开输入文件
                    let mut infile = File::open(path)?;
                    let mut buffer = Vec::new();

                    // 把文件内容读到 buffer 里
                    infile.read_to_end(&mut buffer)?;

                    // 把内容写入输出文件
                    outfile.write_all(&buffer)?;
                }
            }
        }
    }

    // ----------------------------
    // 4. 合并完成提示
    // ----------------------------
    println!("✅ 合并完成，输出文件：{}", output_file);

    Ok(())
}

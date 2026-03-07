# rs_chunkmerger

一个用 Rust 编写的高效媒体分片合并工具，支持自动排序、DASH/HLS 分片处理及可选的 FFmpeg 转码。

~~其实就是给minyami、N_m3u8DL-RE这些工具报错后处理现有切片啦~~

## 功能特性

- **多格式支持**：支持 `.ts`, `.decrypt`, `.mp4`, `.m4s` 等常见媒体分片格式。
- **智能排序**：采用自然排序（Natural Sort），确保分片按 `1, 2, 10` 而非 `1, 10, 2` 的顺序合并。
- **DASH 支持**：自动识别并优先放置包含 `init` 字样的初始化文件。
- **自动转码**：合并完成后可自动调用 FFmpeg 将 TS 转换为 MP4（需系统已安装 FFmpeg）。
- **简单易用**：直接在包含分片的文件夹中运行即可，输出文件会自动存放在上级目录。

## 安装说明

### 从 Release 下载
前往 [Releases](https://github.com/your-username/rs_chunkmerger/releases) 页面下载适用于您系统的预编译二进制文件。

### 自行编译
确保已安装 [Rust](https://www.rust-lang.org/) 环境，然后运行：

```bash
cargo build --release
```

编译产物位于 `target/release/rs_chunkmerger`（Windows 下为 `.exe`）。

## 使用方法

1. 将编译好的程序（或下载的二进制文件）放到存放视频分片的文件夹中。
2. 直接运行程序。
3. （可选）如果不想转换为 MP4，可以带参数运行：
   ```bash
   rs_chunkmerger --ts
   ```

## 注意事项

- **FFmpeg**：若需自动转换为 MP4，请确保 `ffmpeg` 已添加到系统 PATH 中，或将 `ffmpeg.exe` 放在程序同级目录下。
- **文件命名**：输出文件名将结合当前文件夹名、版本号以及分片的起始/结束编号。

## 现有问题
- 目前用的是二进制合并，效果比ffmpeg的concat要差一点。

## 许可证

[Apache-2.0](LICENSE)

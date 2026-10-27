# Yike CLI · 一刻原件备份

A Rust CLI for resumable original-media transfers to your own Baidu Photo (一刻相册) account.

用于本人一刻相册的原片上传、检索、原件下载与强校验。任务状态持久化，支持中断恢复与重复提交防护。使用网页私有接口，非官方客户端；协议变化集中在适配层维护。

## 下载与运行

从 [GitHub Releases](https://github.com/OpenMedia-Lab/yike-cli/releases) 下载对应系统的压缩包；解压后直接运行，不需要安装 Rust。每次发布提供 `SHA256SUMS` 用于核对下载文件。

| 系统 | Rust target | 文件 |
|---|---|---|
| Windows x64 | `x86_64-pc-windows-msvc` | `.zip`，运行 `yike.exe` |
| WSL / Linux x64 | `x86_64-unknown-linux-gnu` | `.tar.gz`，运行 `yike` |
| Linux ARM64 | `aarch64-unknown-linux-gnu` | `.tar.gz` |
| macOS Apple Silicon | `aarch64-apple-darwin` | `.tar.gz` |
| macOS Intel | `x86_64-apple-darwin` | `.tar.gz` |

WSL 使用 Linux 版本。构建环境、系统要求和发布验证见 [发布说明](docs/releasing.md)。各平台产物由 CI 构建和测试，按需下载即可，本地不必保留全部产物。

```sh
./yike --version
./yike doctor --offline
./yike --help
```

PowerShell 中使用 `.\yike.exe`。也可从源码构建：

```sh
cargo build --locked --release
cargo install --path . --locked
```

Windows 源码构建需要 MSVC C++ 工具链；使用发布包不需要编译工具。SQLite 随程序构建，无需另装数据库。

## 登录

```sh
yike auth login
yike auth status
yike doctor
```

在专用浏览器窗口正常扫码登录。WSL 通过本地 PowerShell 子进程连接 Windows Edge；macOS/Linux 使用专用浏览器配置。调试端口只监听回环地址，不扫描或解密日常浏览器配置。

已有专用会话可用 `yike auth login --attach --port 19227`。密码和 Cookie 不需要放进命令行或聊天。

会话和 SQLite 状态默认存入平台本地数据目录下的 `yike`，可通过 `--state-dir` 或 `YIKE_STATE_DIR` 指定专用私有目录。WSL 状态库须放原生 Linux 文件系统，素材可在挂载的 Windows 硬盘。原片、凭据、状态库、文件清单和报告不应提交到仓库或公开。

## 上传：先计划，再执行

```sh
yike upload plan '/media/originals' --album 'Journey·Originals·{date}'
yike upload run PLAN_ID
yike jobs status JOB_ID
yike jobs resume JOB_ID
yike report JOB_ID --json > report.json
```

计划阶段递归扫描 MP4（大小写不敏感），流式计算完整 MD5/SHA-256 和分片摘要，不创建云端对象。也可传入一个 MP4 路径，或用 `--max-files 1` 选择最小可传样本。`--expect-files N` 在哈希前确认库存，避免静默漏扫。

`{date}` 来自明确的日期目录，例如 `/media/originals/20000102/`；不依靠修改时间猜测拍摄日期。相册也可使用不带占位符的固定名称。

默认上传上限为 **5 GiB/视频**，若账号报告更小上限则取更小值。超限或不可确认的项保留在计划中并标为 blocked，不压缩或分割原片。存在 blocked、失败或未确认项时，整批仍是部分完成。

无限容量与单文件上限是两项不同的能力。网页与账号配置可能报告 5 GiB，而服务端预创建仍接受更大的原片；预创建成功不能证明最终建档或原件读回成功。无限容量账号可在**新计划**中显式指定尝试上限（正整数，单位 bytes）：

```sh
# 6 GiB 尝试上限；先用一条原片验证完整上传与原件读回
yike upload plan '/media/originals/sample.mp4' --album 'Originals' \
  --attempt-upload-limit 6442450944 --expect-files 1
yike upload run PLAN_ID
```

该值只表示愿意尝试的大小，不是服务保证。需要在线确认账号具有无限容量且报告了有效上传能力；执行时会重新检查。上限随不可变计划保存，旧计划不会自动放宽。超过 10 GiB 的原件读回也需实际验证，不能由较小样本推断。原片仍保留，只有完整读回 SHA-256 一致才算备份成功。

每项原件在扫描和传输中核对大小、修改时间、原生文件身份与分片摘要；源变化要求重新计划。恢复时以服务器返回的缺失分片为准，不能只信本地进度。

默认上传后在执行电脑**流式读回云端原件，SHA-256 与不可变源计划完全一致**才记为 verified，不另存第二份整片。完整读回会产生约一份原片大小的下载流量。`uploaded` 表示已上传但尚未完成全部验证条件，与 `verified` 分开统计。

## 恢复、校验和相册

```sh
yike jobs list
yike jobs cancel JOB_ID
yike jobs resume JOB_ID
yike verify JOB_ID
yike albums list
yike albums create 'Originals'
```

Ctrl-C 保存进度；取消只停止传输。显式 `verify` 仅重新读取已经上传的原件，不启动待上传项。一个账号只允许一个传输进程；precreate 或最终提交结果不确定时保留意图、先做完整身份核对，不能盲目重试制造重复对象。

相册允许重名，以稳定 ID 定位。相册创建响应不明时，人工核对后可用 `albums bind --title 'Originals' --id ALBUM_ID` 绑定。工具不删除云端对象或本地原片，不做镜像删除同步。

## 检索和原件下载

```sh
yike files search 'camera' --kind video
yike files list --album ALBUM_ID
yike files info FILE_ID
yike download plan --id FILE_ID --to '/chosen/output'
yike download run PLAN_ID
```

下载也可显式选择 `--album ALBUM_ID` 或 `--query TEXT`。日期筛选 `--after/--before YYYY-MM-DD` 指云端创建日期 UTC，不等同于相机拍摄日期。

使用原件接口，校验 Range、ETag、完整大小和 MD5；`.part` 与成品同目录，验证后无覆盖原子发布。重名按稳定 ID 区分。独立下载记录计算得到的 SHA-256；只有具备原始源计划的读回，才证明与那份本地原片相同。目标文件系统需要支持硬链接。

## 输出与后台运行

`--json` 输出紧凑 JSON；默认输出格式化 JSON。进度和脱敏错误写 stderr。退出码：`0` 完成，`2` 输入/鉴权/协议错误，`3` 部分完成，`130` 中断。

可选 POSIX/WSL 启动器把计划和上传串起来，状态保存在 `STATE_DIR/runs/RUN_ID`。同一 run-id 的配置不可变，子进程继承运行锁，SSH 断开后仍可配合进程管理器运行。Rust CLI 本身不依赖 Python；Windows 原生直接使用 upload/jobs 命令。

```sh
python3 scripts/run-backup.py --cli /absolute/path/yike --state-dir /native/private/yike \
  --root /media/originals --album 'Journey·Originals·{date}' --expect-files 12
yike completions zsh > _yike
```

开发和测试见 [贡献指南](CONTRIBUTING.md)，协议边界见 [协议记录](docs/protocol.md)。许可证：MIT。

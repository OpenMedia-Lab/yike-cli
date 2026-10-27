<p align="center">
  <img src="docs/assets/readme-hero.svg" alt="Yike CLI：本地原片、断点传输、云端原件流式哈希校验" width="100%" />
</p>

<h1 align="center">Yike CLI · 一刻原件备份</h1>
<p align="center"><strong>原片上传，断点续传，原件读回验证。</strong></p>
<p align="center">用 Rust CLI 管理本人一刻相册的原件。</p>
<p align="center">
  <a href="README.md">English</a> ·
  <a href="#快速开始">快速开始</a> ·
  <a href="https://github.com/OpenMedia-Lab/yike-cli/releases/latest">下载</a> ·
  <a href="docs/usage.md">完整命令指南</a> ·
  <a href="CONTRIBUTING.md">参与开发</a>
</p>

上传原始 MP4，中断后恢复任务，检索并下载云端原件。只有完整读回云端原件、与不可变本地
计划中的 **SHA-256 一致**，备份才记为 `verified`；上传成功与验证完成分开统计。

- **保留原片：** 不转码、不拆分、不删除源文件。
- **接着上次传：** 持久化计划与任务，按服务端缺片信息恢复。
- **看清完成了什么：** 区分已上传、blocked 和 verified，导出 JSON 报告。

首图为工作流示意，不是用户账号或传输任务截图。

## 快速开始

### 1. 下载，先做本地检查

从 [Releases](https://github.com/OpenMedia-Lab/yike-cli/releases/latest) 下载对应系统的压缩包，
用同次发布的 `SHA256SUMS` 核对后解压。运行预编译程序不需要安装 Rust。

```sh
./yike --version
./yike doctor --offline
./yike --help
```

离线检查不登录、不访问百度。全新状态下出现 `session_saved: false` 和
`local_verified_receipts: 0` 是预期结果。此命令会创建 CLI 的本地状态目录。

<details>
<summary>选择系统，或从源码安装</summary>

| 系统 | 压缩包 target | 运行方式 |
| --- | --- | --- |
| Windows x64 | `x86_64-pc-windows-msvc` · `.zip` | PowerShell 中 `.\yike.exe` |
| Linux / WSL x64 | `x86_64-unknown-linux-gnu` · `.tar.gz` | `./yike` |
| Linux ARM64 | `aarch64-unknown-linux-gnu` · `.tar.gz` | `./yike` |
| macOS Apple Silicon | `aarch64-apple-darwin` · `.tar.gz` | `./yike` |
| macOS Intel | `x86_64-apple-darwin` · `.tar.gz` | `./yike` |

WSL 使用 Linux 版本。从源码安装需使用[构建与发布说明](docs/releasing.md)中的 Rust 工具链：

```sh
git clone https://github.com/OpenMedia-Lab/yike-cli.git
cd yike-cli
cargo install --path . --locked
yike doctor --offline
```

SQLite 随程序构建，无需另装数据库。Windows 源码构建需要 MSVC C++ 工具链；
使用发布包不需要编译工具。平台兼容性、校验命令与原生构建验证见[发布说明](docs/releasing.md)。

</details>

### 2. 登录，先浏览自己的素材

把 `yike` 加入 PATH 后执行以下命令；也可在解压目录中使用 `./yike`：

```sh
yike auth login
yike auth status
yike albums list
yike files search 'camera' --kind video
```

在专用浏览器窗口正常扫码。macOS/Linux 使用独立配置，WSL 连接 Windows Edge。
会话保存在 CLI 私有状态目录。[登录与状态目录说明 →](docs/usage.md#登录)

### 3. 先计划一条原片，再执行

```sh
yike upload plan '/media/originals/sample.mp4' --album 'Originals' --expect-files 1
# 使用上一步返回的 plan_id：
yike upload run PLAN_ID
# 使用执行返回的 job ID：
yike report JOB_ID --json > report.json
```

计划阶段扫描并计算哈希，不创建云端对象。`upload run` 才执行上传，默认在完成后流式读回
云端原件、比对 SHA-256，不另存第二份整片。完整读回会产生约一份原片大小的下载流量。

先用一条小 MP4 验证流程。默认上限为 **5 GiB/视频**；账号报告更小上限时取更小值。
存在 blocked 或未确认项时，整批仍是部分完成。超大原件的显式尝试选项及边界见
[上传指南](docs/usage.md#上传先计划再执行)。

## 按任务继续

| 你要做什么 | 从这里开始 |
| --- | --- |
| 恢复中断的传输 | `yike jobs status JOB_ID` → `yike jobs resume JOB_ID` |
| 重新校验已上传原件 | `yike verify JOB_ID` |
| 找到并下载原件 | `yike files search 'camera' --kind video` → `yike download plan --id FILE_ID --to '/chosen/output'` → `yike download run PLAN_ID` |
| 按日期目录分相册，或后台跑备份 | [上传与后台运行指南](docs/usage.md) |
| 了解接口与验证边界 | [协议记录](docs/protocol.md) |
| 开发、测试或发布新版本 | [贡献指南](CONTRIBUTING.md) · [发布说明](docs/releasing.md) |

下载写入 `.part`，核对原件大小和 MD5 后无覆盖地发布成品；有原始源计划的读回还会比对
那份本地原片的 SHA-256。[下载细节 →](docs/usage.md#检索和原件下载)

## 适合这些工作流

- **命令行与脚本：** JSON 标准输出、stderr 进度、持久化任务和补全脚本。
- **个人原片归档：** 保留 MP4 原始字节，明确区分已上传和已验证。
- **长时间传输：** 接着已有任务恢复，不重新发起整批任务。

这是独立实现的非官方客户端，使用一刻相册网页接口；服务变化可能需要更新适配层。
工具不删除云端对象或本地原片，不做镜像删除同步。

[OpenMedia-Lab](https://github.com/OpenMedia-Lab) 项目 · 自有代码采用 [MIT](LICENSE) 协议。

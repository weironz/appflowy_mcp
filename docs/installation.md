# 安装 appflowy（一键脚本 / 手动 / 源码）

`appflowy` 是一个单文件静态二进制：同一个文件既是 CLI，也是 MCP server（`appflowy serve`；改名/复制为 `appflowy-mcp` 则启动即 MCP 模式）。本页覆盖三种装法，生产用推荐第一种。

## 一键安装（推荐）

Linux / macOS（bash/sh）:

```bash
curl -fsSL https://raw.githubusercontent.com/weironz/appflowy_mcp/main/install.sh | sh
```

Windows（PowerShell）:

```powershell
irm https://raw.githubusercontent.com/weironz/appflowy_mcp/main/install.ps1 | iex
```

> ExecutionPolicy 拦脚本时，用单行绕过（只对本次执行生效）：
> `Set-ExecutionPolicy Bypass -Scope Process -Force; irm ... | iex`

**更新 = 重跑同一行命令**（脚本会把最新 release 覆盖装上去）。固定版本：

```bash
APPFLOWY_VERSION=0.8.0 curl -fsSL https://raw.githubusercontent.com/weironz/appflowy_mcp/main/install.sh | sh
```
```powershell
$env:APPFLOWY_VERSION='0.8.0'; irm https://raw.githubusercontent.com/weironz/appflowy_mcp/main/install.ps1 | iex
```

自定义安装目录（默认 `~/.local/bin` 与 `%LOCALAPPDATA%\Appflowy\bin`）:`APPFLOWY_BIN_DIR`。

### 脚本做了什么（不该是黑盒的每一步）

1. 识别平台资产名：Linux x64 → `appflowy-linux-x64`,macOS arm64 → `appflowy-macos-aarch64`,macOS x64 → `appflowy-macos-x64`,Windows x64 → `appflowy-windows-x64.exe`。
2. 解析版本：显式 `APPFLOWY_VERSION` 优先，否则查 GitHub latest release。
3. 下载到临时文件，**先跑 `--version` 验证再安装**——下载被截断、架构不匹配会死在这一步，而不是装上一个坏文件。
4. 落两个名字：`appflowy`(CLI)和 `appflowy-mcp`（同一份字节，argv[0] 分发：MCP 客户端可以直接指它，免配 `serve` 参数）。
5. Windows 会把 bin 目录加进用户 PATH（提示"开新终端生效");Linux/macOS 若 bin 不在 PATH 会打印提示。

### ⚠️ 预编译资产现状（重要）

release 资产由 CI 在**打 tag 时**构建，当前覆盖 **Windows x64 / Linux x64 / macOS arm64(Apple Silicon)** 三平台。v0.8.0 只有 Windows 资产;**Intel Mac(x86_64)暂不提供预编译资产**——GitHub 的退役代 Intel runner 池长期排队拒绝调度,已在 CI 矩阵中撤下;Intel Mac 请用源码安装(`cargo build --release`,见下)。

## 卸载

删除两个二进制（以及可选的配置目录，里面是 CLI 的登录凭证）:

```bash
rm -f ~/.local/bin/appflowy ~/.local/bin/appflowy-mcp
rm -rf ~/.config/appflowy-cli        # 可选:登录凭证
```
```powershell
rm "$env:LOCALAPPDATA\Appflowy\bin\appflowy*.exe"
rm -r "$env:USERPROFILE\.config\appflowy-cli"   # 可选
```

## 手动安装

去 [GitHub Releases](https://github.com/weironz/appflowy_mcp/releases) 下载对应平台的 `appflowy-*` 文件，加执行权限，放进 PATH 中的任意目录。想要 MCP 名字分发，就再复制一份改名 `appflowy-mcp`。

## 源码安装

需要 Rust 工具链（[rustup](https://rustup.rs/)):

```bash
git clone https://github.com/weironz/appflowy_mcp.git
cd appflowy_mcp
cargo build --release        # 产物: target/release/appflowy (Windows 为 .exe)
```

纯 rustls(webpki-roots 内嵌 CA)，不依赖 OpenSSL，交叉编译友好。

## 排错

- **下载 404**:release 里还没有该平台资产（见上面"预编译资产现状")，或 tag 写错——换 `APPFLOWY_VERSION` 或走手动/源码。
- **Windows SmartScreen 警告**:二进制未做代码签名，属预期；"更多信息 → 仍要运行"。企业机器被策略拦截时需自签或加白。
- **装完 `appflowy` 找不到命令**:Windows 需要开**新的**终端（PATH 变更只对新进程生效）;Linux/macOS 按脚本末尾提示把 `~/.local/bin` 加进 PATH。
- **公司代理/GitHub 慢**：脚本不会用镜像或代理变量；手动下载后用代理工具，或 `cargo build`。
- **想确认装的是真的它**:`appflowy --version`(`appflowy-cli 0.x.y`)；名字带 `-cli` 是历史包名留下的打印约定，不是装错了东西。

## 下一步

- 接 MCP 客户端（Codex / Claude / Cursor)：[mcp-connect.md](mcp-connect.md)
- CLI 完整使用参考：[cli.md](cli.md)

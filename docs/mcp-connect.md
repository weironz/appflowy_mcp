# 把 appflowy 接入 Codex / Claude / Cursor(MCP)

`appflowy serve` 把 AppFlowy Cloud 的能力以 **114 个 MCP 工具**暴露给任何 MCP 客户端:
工作区/空间/页面增删改查、Markdown 导入导出、PDF 导出、数据库与 CSV 导入、行操作、
发布、文档评论、页面分享、通知、文件上传、AI 问答与会议转写。一个典型用法:
**让 agent 把对话和笔记直接写进你的 AppFlowy 工作区**,或让 agent 把整库导出成 Markdown 做备份。

先装好二进制(一键脚本见 [installation.md](installation.md)),再按你的客户端选一段。

## 凭证真相(先读这一段,三个客户端通用)

MCP server 的认证**只认环境变量**(或工作目录下的 `.env`):

```text
APPFLOWY_EMAIL=you@example.com
APPFLOWY_PASSWORD=your-password
APPFLOWY_BASE_URL=https://beta.appflowy.cloud   # 自托管才需要改
```

两条容易踩的岔路,提前说清:

- **`appflowy login` 存的 `~/.config/appflowy-cli/credentials.json` 是给 CLI 用的,
  MCP server 不读它**。在客户端里启动的 server 进程没有 CLI 的登录态——所以客户端
  配置里必须给 env。给不了 env 的场合,也可以在 MCP 里先调 `appflowy_login` 工具登录
  (凭证会写进 shell/对话历史,斟酌)。
- **别把 env 凭证写进会被 git 跟踪的项目级配置**(比如项目目录的 `.mcp.json`、
  `.cursor/mcp.json`)——仓库一旦公开,账号密码就在提交历史里永久留下了。
  项目级配置只配无凭证的东西;带凭证的配置一律放**用户级**(见下各节)。

## Codex

命令行一把加(用户级,所有项目可用):

```bash
codex mcp add appflowy \
  --env APPFLOWY_EMAIL=you@example.com \
  --env APPFLOWY_PASSWORD=your-password \
  -- appflowy serve
```

手写配置 `~/.codex/config.toml` 的等价写法:

```toml
[mcp_servers.appflowy]
command = "appflowy"
args = ["serve"]
env = { APPFLOWY_EMAIL = "you@example.com", APPFLOWY_PASSWORD = "your-password" }
```

验证:新开会话让 Codex 列出工作区(会走 `appflowy_list_workspaces`)。

## Claude Code

```bash
claude mcp add --scope user appflowy \
  -e APPFLOWY_EMAIL=you@example.com \
  -e APPFLOWY_PASSWORD=your-password \
  -- appflowy serve
```

### ⚠️ `--scope user` 不能省 —— 尤其在 Windows 上

省掉它是默认的 `local`(项目级)。Windows 上 `claude mcp add` 把 project key
写成**正斜杠**(`C:/Users/you/proj`),应用自己建的 key 却是**反斜杠**
(`C:\Users\you\proj`),两个 key 对不上,配置永远加载不到。

症状极具迷惑性:`claude mcp list` 报 `✓ Connected`(它读的是自己写的那个
正斜杠 key),但**重启后会话里一个 appflowy 工具都没有** —— 两边都没说谎,
只是在看不同的配置。别信 `✓ Connected`,以「重启后会话里有没有 `appflowy_*`
工具」为准(2026 年在同类项目上实测复现)。

`--scope user` 写到 `~/.claude.json` **顶层**的 `mcpServers`,不挂 project key,
从根上绕开这个坑。

想换凭证:重跑 `claude mcp add` 同名覆盖即可,别的 MCP server 不受影响。

### Claude Desktop

`claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "appflowy": {
      "command": "appflowy",
      "args": ["serve"],
      "env": {
        "APPFLOWY_EMAIL": "you@example.com",
        "APPFLOWY_PASSWORD": "your-password"
      }
    }
  }
}
```

二进制不在 PATH 时写绝对路径(`C:\Users\you\AppData\Local\Appflowy\bin\appflowy.exe`)。
装了 `appflowy-mcp` 这个名字的话,`args` 可以省掉(启动即 MCP 模式)。

## Cursor

用户级 `~/.cursor/mcp.json`(所有项目可见,推荐带凭证的用途):

```json
{
  "mcpServers": {
    "appflowy": {
      "command": "appflowy",
      "args": ["serve"],
      "env": {
        "APPFLOWY_EMAIL": "you@example.com",
        "APPFLOWY_PASSWORD": "your-password"
      }
    }
  }
}
```

项目级 `.cursor/mcp.json` 结构相同,但**该文件会被 git 跟踪——严禁把账号密码放进去**(见顶部"凭证真相")。Settings → MCP 里能看到是否加载成功;验证让 Cursor 数一下工具(应为 114)。

## 需要 HTTP 而不是 stdio?(少数场景)

上面四个客户端都是**拉起本机进程走 stdio**,这是桌面 agent 的标准姿势。只有在
"客户端在别的机器 / 多人共用一个实例"时才上 Streamable HTTP:

```bash
appflowy serve --http 127.0.0.1:8000                 # 本机开发联调
appflowy serve --http 0.0.0.0:8000 --allow-remote    # 内网共享(见下)
```

端点是 `http://ADDR/mcp`,支持该协议的客户端直接填 URL。**护栏**:默认只回环 Host、
拒绝浏览器 Origin;非回环绑定必须 `--allow-remote`——任何能摸到这个端口的人都能用
你的 AppFlowy 凭证驱动全部 114 个工具,团队共享前请先想清楚鉴权(目前无 OAuth,放
可信内网/网关之后)。

## 排错

- **会话里没有 `appflowy_*` 工具但都显示 Connected**:Claude Code 的 project key
  正/反斜杠坑(见上),`--scope user` 重加。Cursor/Desktop 检查 JSON 是不是合法
  (逗号)、command 是否能在 PATH 找到(不行就写绝对路径)。
- **工具调用全报 401 / Not authenticated**:env 没传进 server 进程——客户端配置
  里的 `env` 写错键名(就是 `APPFLOWY_EMAIL/PASSWORD`),或者你又指望了
  `credentials.json`(server 不读,见"凭证真相")。改完配置**重启客户端**。
- **自托管上报 404 或奇怪的错**:`APPFLOWY_BASE_URL` 指向旧镜像;部分端点(评论的
  v2、分享详情、会议转写等)只有新商业云有,server 会降级或报明确的 404 错误——升级
  自托管镜像,或用回官方云。
- **想看协议发生了什么**:日志全在 stderr(stdout 是 JSON-RPC 通道,永远干净),
  加 `RUST_LOG=debug` 到客户端的 env 里。
- **验证工具集**:让客户端跑 `appflowy_list_workspaces`;正常应回你的工作区列表。
  再数工具,应为 **114**(老版本是 81,低于它说明连的是旧二进制)。

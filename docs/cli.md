# appflowy CLI 参考

`appflowy` 是 AppFlowy Cloud 的命令行客户端:为脚本和 cron 设计——整库备份、批量导入、快速查询。
与 MCP server(`appflowy serve`)**同一个二进制、同一套 HTTP 客户端与导出导入引擎**;
MCP 面向 AI agent,CLI 面向 shell/CI。安装见 [installation.md](installation.md)。

## 认证链(只认这两处,按序)

1. 环境变量 `APPFLOWY_EMAIL` + `APPFLOWY_PASSWORD`(可选 `.env` 文件、可选 `APPFLOWY_BASE_URL`)
2. `appflowy login` 保存的凭证文件(见下)

两都没有 → `error: Not authenticated. Run \`appflowy-cli login\`, or set APPFLOWY_EMAIL and APPFLOWY_PASSWORD.`,退出码 1。

```bash
appflowy login                     # 交互输入邮箱/密码;只存 token,不存密码
appflowy logout                    # 删除凭证文件
```

凭证文件:`~/.config/appflowy-cli/credentials.json`(Unix 下 0600;含 email/base_url/access/refresh token)。
**GoTrue 会轮换 refresh token**:每次命令跑完若发现 refresh token 变了就自动回写——所以长期不用也能续。
`appflowy login --base-url <url>` 指向自托管(优先级:env > 该文件)。

> 这个文件**只为 CLI 服务**;MCP server(`appflowy serve`)不读它,见 [mcp-connect.md](mcp-connect.md) 的"凭证真相"。

## 寻址模型

层级是 `workspace → space → page`。两条规则:

- workspace 用 `appflowy use <名字或UUID>` 存一次默认;每条命令也可以用 `-w` 临时覆盖。名字匹配**大小写不敏感**、多义会列出全部候选。
- workspace 之下的 space/page 用 **PATH**:`/` 分隔,每段是该层的**名字或 view_id**(UUID),逐级解析。段匹配"先 UUID 精确、再名字(大小写不敏感)";字歧义报错会附可消歧的 `name (view_id)` 清单。

```bash
appflowy use AI              # 默认 workspace
appflowy ls                  # workspace 根(空间列表)
appflowy ls 大语言模型        # 某空间的直接子项
appflowy tree 大语言模型/概念介绍
appflowy -w test ls          # 临时换 workspace
```

## 全局选项

| 选项 | 作用 |
| --- | --- |
| `--json` | 原始 JSON 输出(脚本/agent 用;人类输出是摘要表格) |
| `-w, --workspace <名字或ID>` | 覆盖 `use` 的默认 workspace |
| `-h, --help` / `-V, --version` | |

## 命令

### `login` / `logout` — 凭证

见上节。`login --email/--password` 也可一次给齐(注意会进 shell 历史)。Windows 下密码**明文回显**(限制,见 ROADMAP)。

### `use [名字]` — 默认 workspace

无参打印当前默认;有参则解析后保存(存的是名字,id 打在输出里)。注意它也要先过认证链(要解析候选列表)。

### `workspaces` — 列工作区

`*` 标记默认项。`--json` 给出完整字段(role/member_count/created_at 等)。

### `ls [PATH]` / `tree [PATH]` — 看结构

`ls` 只列 PATH 的直接子项(类型列:space/page/grid/board/calendar/chat);`tree` 打印整棵子树。不传 PATH 就是 workspace 根。

### `search <关键词> [--limit N]` — 全文搜索

```bash
appflowy search "transformer" --limit 5
```

命中页列出 object_id + 预览片段。新写入的内容要等服务端建完索引才搜得到。

### `export [PATH] -o <目标>` — 导出 Markdown(可读备份)

| PATH 是什么 | 导出结果 |
| --- | --- |
| 不传 | 整个 workspace → 目录树 |
| 空间 / 有子页的页面 | 该子树 → 目录树(有子页的页面变文件夹,其正文进 `README.md`) |
| 叶子页面 | 单个 `.md` 文件 |

```bash
appflowy export -o ./backup           # 全库 → ./backup/
appflowy export 大语言模型 -o ./llm    # 一个子树
appflowy export 大语言模型/概念介绍/Transformer -o t.md
```

- 行内格式(粗体/斜体/行内代码/链接)经 yjs 解码保留;表格、公式、嵌套列表、待办都支持。
- 取不到的页面(collab 丢失/拉取失败)按 warning 输出**不中止**;非文档视图(grid/board)跳过并报告。
- 目录型导出的目标**必须不存在或为空目录**(防覆盖);抓取并发 8。
- 导出的是 **Markdown**,不是官方备份格式。官方 ZIP 备份走 MCP 的 `appflowy_export_workspace_zip`(owner-only、邮件交付)——见 [mcp-connect.md](mcp-connect.md)/README。

### `import <PATH> <本地> [--no-assets] [--title T]` — 导入 Markdown

```bash
appflowy import demo ./notes          # 本地目录 → 页子树
appflowy import demo note.md          # 单文件 → 一页
```

- 目录映射:目录→页面、子目录→子页面;`README.md`/`index.md` 内容并入其目录页而不是单列一页。
- 本地图片相对 md 文件解析、**按文件去重后并行上传**(≤8 并发)、URL 替换为 AppFlowy file URL;远程图保留;缺失图进 warning 不中断。
- `--no-assets` 只建结构不上传图片。
- 页面创建保持文档内顺序(兄弟顺序确定);支持表格/嵌套列表/数学块/front-matter。
- PATH 不能是 workspace 根(根的直接子级只会成为 space)——报错文案会引导先 `ls` 选一个 space。

### `save <PATH> <标题> [--file F | --content C]` — 快速存一页

```bash
echo "# Note" | appflowy save demo "标题"      # stdin
appflowy save demo "标题" --file note.md
appflowy save demo "标题" --content "# 内联"
```

Python 时代的"把 AI 输出存进 AppFlowy"的 CLI 版;内容默认按 Markdown 解析。

### `serve [--http ADDR] [--allow-remote]` — MCP server

```bash
appflowy serve                               # stdio(客户端配置的标准方式)
appflowy serve --http 127.0.0.1:8000         # Streamable http @ /mcp
appflowy serve --http 0.0.0.0:8000 --allow-remote
```

把二进制改名/复制为 `appflowy-mcp`,不带参数启动即 serve(兼容老 `appflowy-mcp` 入口的客户端配置)。
护栏与共享风险见 [mcp-connect.md](mcp-connect.md)。

## CI / cron 示例

```cron
0 3 * * * APPFLOWY_EMAIL=... APPFLOWY_PASSWORD=... /home/me/.local/bin/appflowy -w AI export -o ~/backups/appflowy-$(date +\%F)
```

注意点:凭证用 env 就不用任何本地状态(完全无盘);`--json` 适合管道 jq;非零退出码一定会给出 `error: ...`(stderr)。

## 配置文件位置速查

| 文件 | 内容 |
| --- | --- |
| `~/.config/appflowy-cli/credentials.json` | CLI 登录 token(0600) |
| `~/.config/appflowy-cli/config.json` | `use` 存的默认 workspace |
| 启动目录的 `.env` | `APPFLOWY_EMAIL/PASSWORD/BASE_URL` |

## 排错

- `Not authenticated...`:两处凭证都没有,见"认证链"。
- `Unknown/Ambiguous workspace 'x'. Available: ...`:名字打错或同名;按提示用 UUID。
- `No 'seg' under X. Children: ...` / `Ambiguous segment ... — use a view_id`:PATH 某段没匹配上或多个同名,按清单用 view_id 消歧。
- `Destination directory is not empty`:导出目标的保护性拒绝;换空目录。
- Windows 终端中文乱码:CLI 恒输出 UTF-8,把终端代码页切到 65001(`chcp 65001`)或用 Windows Terminal。不推荐改 CLI 迁就终端。

## 相关文档

- 安装/卸载/一键脚本:[installation.md](installation.md)
- 接 MCP 客户端(Codex/Claude/Cursor):[mcp-connect.md](mcp-connect.md)
- 114 个工具的总目录:仓库根 [README.md](../README.md)

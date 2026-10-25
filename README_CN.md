<div align="center">

# Awpfel

**设备端 AI 补全，显示完整命令 ghost text。输入后看到命令，按 Tab 接受。**

<a href="https://github.com/beeemT/awpfel/releases"><img src="https://img.shields.io/github/v/tag/beeemT/awpfel?label=version&style=flat-square" alt="Version"></a>
<a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg?style=flat-square" alt="License"></a>
<br/>
<a href="README.md">English</a>
</div>

## 功能

Awpfel 在 zsh 输入时显示 ghost text。Ghost text 是完整命令，
来自设备端 AI，并以相关历史作为上下文。你接受或忽略它。
Awpfel 从不执行命令。

- 输入部分命令。Ghost text 显示完整命令。
- 按 **Tab** 把 ghost text 接受到命令行中编辑。
- 按 **Enter** 只运行已输入的内容。Enter 从不接受 ghost text。
- 没有 ghost text 时继续输入。没有东西会强加给你。

## 按键

| 按键 | 动作 |
| :-- | :--- |
| `Tab` / `→` | 把全部 ghost text 接受到命令行中编辑 |
| `Shift-Tab` / `Ctrl-→` 或 `Alt-→` | 接受 ghost text 的下一个 shell 词块，再按一次接受下一块 |
| `Enter` | 运行已输入的行（从不接受 ghost text） |
| `Ctrl-X` 然后 `A` | 把当前行直接替换为 AI 生成的命令，放在行内，不执行 |

注意：`Ctrl-X A` 指先按 `Ctrl-X`，松开，再按 `A`。
它不是 `Ctrl-X Ctrl-A`。它从不执行结果。
单独按 Enter 只运行行内已有内容。

## 补全显示

Ghost text 有两种形式：

- **后缀 ghost。** 显示文本从输入结尾处开始。一次接受全部，
  或只接受下一个 shell 词块，例如一次接受一个带引号的参数。
- **替换预览。** 最佳匹配不是输入的前缀。Awpfel 显示
  完整命令供检查。接受时替换整行，或先接受第一个建议块。

引号文本和转义空格保持为一块。光标在词中时，按块接受先补完该词。
接受只编辑行内内容，从不执行。只有 `Ctrl-X A` 在 ghost 流程之外
直接替换行内内容，它从不执行。

## 自然语言

输入 `#` 加一句描述。生成的命令以次行 ghost 预览显示
（`→ 完整命令`）。按 Tab 接受之前，行内内容不变。
它不执行。

```
# 找到所有今天修改过的 go 文件
```

按 `Ctrl-X` 然后 `A` 把当前行直接替换为生成的命令。
结果进入命令行等待编辑，不会直接执行。

## 安装

Awpfel 需要 Apple Silicon 上的 macOS 26（Tahoe）或更高版本。
它使用 Apfel CLI 提供设备端 AI。

### Homebrew

```bash
brew install beeemT/tap/awpfel
awpfel setup
```

`awpfel setup` 安装配置和 shell 集成并启动后台 daemon。
Daemon 管理自己的本地 Apfel。不需要运行
`brew services start apfel`。

---

重启 shell。Awpfel 随 shell 启动，首次启动自动导入 zsh 历史。

## 卸载

```bash
awpfel uninstall --yes
```

这会停止 daemon，从 `~/.zshrc` 删除 shell 集成，并删除
`~/.config/awpfel/` 和 `~/.local/share/awpfel/`。
不带 `--yes` 只做一次全局确认。

**Homebrew：**

```bash
brew uninstall awpfel
```

## CLI

```bash
awpfel setup              # 配置 shell 集成，启动 daemon
awpfel start              # 启动 daemon
awpfel stop               # 停止 daemon
awpfel status             # 查看状态（pid、运行时间、历史条数）
awpfel logs               # 查看最近日志
awpfel config             # 查看配置
awpfel context            # 查看当前上下文状态
awpfel history import     # 导入 zsh 历史
```

## 配置

配置文件在 `~/.config/awpfel/config.toml`，所有字段都有默认值。
默认值指向受管本地 Apfel。

```toml
[ai]
enabled = true
base_url = "http://127.0.0.1:11435/v1"
model = "apple-foundationmodel"
```

把 `ai.enabled = false` 可关闭 AI。

## 安全边界

- **从不执行。** 先接受 ghost text，Enter 才能运行它。`Ctrl-X A` 直接替换行内内容，但从不执行。
- **历史是上下文。** 历史条目作为不可信输入进入模型提示词。
  发送前会做脱敏。
- **从不读取敏感文件。** 不碰 `.env`、`.ssh` 或密钥文件。
- **隐私过滤。** 疑似密钥的值在离机前遮蔽。过滤是尽力而为。
- **设备端 AI。** 默认后端是受管本地 Apfel。不需要 API key，
  不需要云账号。

## 开发

```bash
make dev              # Debug 构建 + 同步 + 重启 daemon
make release          # Release 构建 + 同步 + 重启
make test             # cargo test + shellcheck + zsh 冒烟测试
make lint             # clippy + fmt check + shellcheck
make sync             # 只复制 plugin（不重新编译）
make status           # 查看 daemon 状态
make logs             # 查看 daemon 日志
```

## 致谢

Awpfel 基于两个开源项目：

- SaoNian（zzf2333）的 [Awen](https://github.com/zzf2333/Awen)，
  MIT 许可。Awpfel 是 Awen 的 fork，产品和后端改为设备端 AI。
- [Apfel](https://github.com/Arthur-Ficial/apfel)，MIT 许可，
  提供 Awpfel 查询的设备端模型。

Awen 原始 MIT 版权保留在 LICENSE 中。

Awpfel 是独立项目，与 Awen 作者、Apfel、Apple 均无关联，
也未获其背书或赞助。

## License

MIT

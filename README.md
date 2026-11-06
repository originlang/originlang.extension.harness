# OriginLang Extension Harness

宿主（`originlang.extension.host`）的第一个插件：把模型、工具与预算编排成一个可交付的 agent 循环。

它以**动态库插件**的形式发布——`crates/plugin` 编译出 `harness.dll`，由宿主的 dylib 后端加载进宿主进程。它不是第二个可执行文件，也不是链接进宿主的库：宿主不依赖它，装不装、装几个都不改变宿主的体积。

## 形态选择

| 方案 | 宿主体积 | 分发 | 结论 |
| --- | --- | --- | --- |
| 编进宿主二进制 | 每个插件 +3–5 MB | 一个 exe | 插件一多，宿主线性膨胀，且每次改插件都要重编发版 |
| 独立进程 | 不变 | 两个可执行文件 | 体积恒定，但多一层进程边界与部署成本 |
| **动态库插件** | **不变** | 一个宿主 + 一个插件包 | 采用 |

动态库和宿主共享地址空间，因此本插件可以自己发 HTTP 请求（QuickJS 沙箱不行），代价是它必须是一等方代码——详见宿主的 `docs/backends.md`。

## 结构

```
crates/harness   循环、模型、工具、预算、转录（不含任何 C ABI 代码）
crates/plugin    cdylib 外壳：八个导出符号，把 JSON 翻译成函数调用
examples/harness 可安装的包（extension.json + harness.dll）
examples/toolbox 演示用的工具提供方（QuickJS，贡献 3 个工具）
scripts/fake_model.py 端到端测试用的假模型端点
```

`crates/harness` 通过 `HostApi` trait 访问宿主，插件形态下由 `PluginHost` 走 C ABI 实现。本仓库不含命令行入口：驱动它的是独立仓库 `bootidea.cli` 的 `run` 与 `agent` 子命令。

## 插件暴露的方法

| 方法 | 作用 |
| --- | --- |
| `run` | 跑一次 agent 循环，返回完整 `RunResult` |
| `tools` | 列出循环当前能看到的工具，以及被跳过的原因 |
| `ping` | 存活探测，返回版本 |

`run` 的参数：

```json
{
  "goal": "what is 2+3?",
  "model": {
    "baseUrl": "https://api.example.com/v1",
    "model": "gpt-4o-mini",
    "apiKeyEnv": "OPENAI_API_KEY"
  },
  "limits": { "maxModelCalls": 20, "maxToolCalls": 50, "timeoutMs": 300000 }
}
```

凭据两种给法：`apiKey` 直接传（开发方便，但会出现在调用参数与日志里），或 `apiKeyEnv` 指定环境变量名（推荐）。都没给时回退到 `OPENAI_API_KEY`。

## 构建与安装

```sh
cargo build --release -p originlang-ext-harness-plugin
cp target/release/harness.dll examples/harness/
```

唯一的跨仓库依赖 `originlang-ext-plugin-abi` 以 git 依赖锁定在宿主仓库的一个 rev 上，因此本仓库可以单独检出，不要求宿主仓库放在同级目录；代价是首次构建需要能访问 GitHub，宿主 ABI 升级时改 `Cargo.toml` 里的 rev 再 `cargo update`。

包的清单声明 `runtime: dylib` 与入口文件。安装后宿主默认的 `--backend auto` 会把它路由到 dylib 后端：

```sh
bootidea ext install ./examples/harness --grant extension.call,log.write
bootidea ext install ./examples/toolbox --grant storage.read,storage.write,log.write
```

## 发布

两条线，互不干扰：

| 渠道 | 触发 | 版本 | tag | 产物 |
| --- | --- | --- | --- | --- |
| nightly | 推送到 `feature/dev` | `<version>-nightly.<yyyymmdd>.<当日第 n 次>` | 与版本同名 | Windows `.zip` + Linux `.tar.gz`，prerelease |
| stable | push `v<version>` 标签 | `<version>`（tag 去掉 v 前缀） | `v<version>` | 同上，正式 release |

`<version>` 以仓库根的 `version.txt` 为准，`Cargo.toml` 保持一致。打包时两者不一致会直接失败——版本没跟到位时，发错号比构建失败更贵。

nightly 的序号取自当日已有的 tag 数，所以一天内多次构建依次是 `.1`、`.2`、`.3`；日期按北京时间算，避免晚上 8 点后串到次日。包清单里的 `version` 会被改写成 nightly 版本，宿主据此区分同一分支的多次构建。

stable 走 **push tag 即发布**：把 `version.txt` 与 `Cargo.toml` 改成目标版本并提交，然后 `git tag v0.1.1 && git push origin v0.1.1`，tag 触发 `release.yml` 完成打包与发布。

## 验证

单元测试：

```sh
cargo test --workspace
```

端到端（不需要真实模型）：

```sh
python scripts/fake_model.py --port 8765 &

# 开发期：临时安装，一次跑完就丢弃
bootidea run examples/harness \
  --method run \
  --grant-all \
  --with examples/toolbox \
  --params '{"goal":"what is 2+3?","model":{"baseUrl":"http://127.0.0.1:8765/v1","model":"fake","apiKey":"test"}}'

# 安装之后：直接用 agent 子命令，密钥只从环境变量读
BOOTIDEA_API_KEY=test bootidea agent "what is 2+3?" \
  --model fake --base-url http://127.0.0.1:8765/v1
```

假模型第一轮返回 `add(2,3)` 的工具调用，收到观察结果后返回最终答案。预期 `status: completed`、`stats.model_calls: 2`、`stats.tool_calls: 1`，答案是 `The tool said {"sum":5}, so the answer is 5.`

`--with` 让这次运行同时装上 `toolbox`，否则 `tools` 只会看到空列表——这正好也是排查"模型为什么没用我的工具"的第一站：

```sh
bootidea run examples/harness --method tools --grant-all --with examples/toolbox
```

## 边界

- **不沙箱化工具。** 工具由宿主的其他插件提供，能力由宿主裁决；harness 只决定调哪一个。
- **不重试。** 模型调用失败就以该原因结束，重试是调用方的策略。
- **不流式。** 循环的工作单位是一次完整回合。
- **不持久化转录到宿主存储。** 目前的 `RunResult` 直接返回给调用方；写盘属于后续工作。

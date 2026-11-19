# md-sync

一个小而美的 Markdown 同步 / 备份工具依赖库（Rust，多 crates）。

面向文档同步，不是通用对象存储：默认只同步 `*.md` / `*.markdown`，
整文件读写（Markdown 文档都不大），用一份 `.md-sync.json` 清单做
“上次同步长什么样”的基线，于是新建 / 删除 / 冲突三件事都能判定，
而不是靠两边对拍猜。

```rust
use md_sync::{Engine, Operator, SyncMode};
use md_sync::services;

// opendal 风格：选一个 builder，得到一个 operator。
let a = Operator::new(services::Fs::default().root("/home/u/notes"))?;
let b = Operator::new(services::Fs::default().root("/home/u/notes-backup"))?;

a.write("hello.md", b"# Hello").await?;
let data = a.read("hello.md").await?;

// 任意两个后端之间同步：fs、S3、WebDAV、Drive、OneDrive 互相组合。
let report = Engine::default()
    .sync(a.backend(), b.backend(), SyncMode::TwoWay)
    .await?;
println!("applied {} ops, {} conflicts", report.ops_applied, report.conflicts.len());
```

示例 CLI（也是验证工具）：`cargo run -p md-sync-cli -- sync <A> <B>`，
见 `crates/md-sync-cli` 与 `demo/run-demo.sh`。

## Crates

| crate | 职责 | 直接依赖 |
|---|---|---|
| `md-sync-core` | `StorageBackend` trait、同步引擎、清单、冲突策略、内存后端 | serde, serde_json, sha2, thiserror, async-trait |
| `md-sync-fs` | 本地文件系统后端 | core, tokio（fs 部分） |
| `md-sync-http` | 远程后端共用的 HTTP 层：reqwest（rustls）+ 日期 / 编码小工具 | core, reqwest, serde, serde_json |
| `md-sync-s3` | S3 / MinIO：手写 SigV4 签名 | core, http, hmac, sha2, quick-xml |
| `md-sync-webdav` | WebDAV（Nextcloud / ownCloud / nginx …） | core, http, quick-xml |
| `md-sync-gdrive` | Google Drive API v3 | core, http, serde_json |
| `md-sync-onedrive` | OneDrive（Microsoft Graph） | core, http, serde_json |
| `md-sync` | 门面：统一导出 + feature 开关 | core + 各后端（可选） |
| `md-sync-cli` | 示例 CLI | md-sync, tokio, clap, serde_json |

依赖预算是有意压着的：core 没有 tokio、没有 HTTP 客户端；没有
chrono/time（日期解析是 60 行手写）、没有 url（路径编码是 20 行）、
没有 aws-sdk（SigV4 是 120 行手写）、没有 OAuth 库（见下）。

### feature 开关

```toml
md-sync = { version = "0.1", features = ["s3"] }   # 只要 S3
md-sync = { version = "0.1", features = ["full"] } # 五个后端全要
```

`fs` 是默认 feature；没开的后端在二进制里完全不存在——没有代码、
没有依赖、没有体积。CLI 同理：
`cargo build -p md-sync-cli --no-default-features --features fs`。

实测（2026-11-18，本机 release，`opt-level="z" + lto + strip`）：

| 构建 | 二进制 | 依赖（`cargo tree` 去重） |
|---|---|---|
| CLI，fs-only | 810 KiB | 51 |
| CLI，full（5 个后端） | 2.5 MiB | 162 |

## 同步模式

| 模式 | 语义 |
|---|---|
| `TwoWay` 双向 | 改动双向流动；删除会传播到另一侧；两边相对清单都改过同一文件 = 冲突 |
| `Bidirectional` 无状态双向 | opendal-sync 同款语义：独有文件双向复制；两边内容不同即冲突（哪怕只一边改过）；删除**永不**传播；决策不查清单 |
| `Mirror` 镜像 | 目标向源看齐：新 / 改过的文件复制过去，源里没有的文件从目标删除 |
| `OneWay` 单向镜像 | `Mirror` 的旧名；唯一区别是 `--no-delete`（`push`/`pull`）可关掉删除 |
| `Backup` 备份 | 单向、只复制、永不删除；目标文件比源新时不覆盖（会报告出来，不是悄悄跳过） |

CLI：`md-sync sync A B --mode bidirectional|mirror|backup`（默认 `two-way`），
`plan` 同样接受 `--mode`。

`Bidirectional` 与 `TwoWay` 的取舍：`TwoWay` 靠清单能分辨“只一边改了”
和“新建还是删除”，但需要一份基线；`Bidirectional` 不需要任何历史，
适合第一次同步或清单丢失后的合并，但单边改动也会报冲突、删过的文件
会被对方重新复制回来（而不是同步删除）。`Bidirectional` 跑完后仍会
把合并后的状态写进清单，这样下一次 `TwoWay` 有正确的基线。

另有 `plan`（dry-run：只算计划不动文件）和 `--json`（报告以 JSON 输出）。

### 冲突策略（`--conflict`）

`newer`（默认，按 mtime，时间相同取 A）、`keep-a`、`keep-b`、
`both`（胜者做正文，败者另存为 `name.conflict-<side>-<时间戳>.md`，
两侧各一份）、`skip`（两边都不动，且该文件不进新清单，下次还会再报，
不会把分歧悄悄“洗”掉）。

### 清单 `.md-sync.json`

每个后端根目录一份，记录上次成功同步时每个文件的 hash / etag /
大小 / mtime。它解决三个问题：

1. **新建 vs 删除**：只在一边出现的文件，是新建（复制过去）还是
   另一边删了（这边也删）——看它在不在清单里。
2. **冲突判定**：两边相对清单都变了，才是冲突；只一边变了，直接同步。
3. **mtime 抖动**：S3 / WebDAV 上传后 mtime 是上传时间，不是源文件的
   时间；清单记的是“实际存下来的状态”，下次比较不再误判为改过。

清单损坏或丢失不会让同步失败，只会退化成“按冲突策略处理所有差异
文件”，并在报告里给一条 warning。`Bidirectional` 模式决策时不查
清单，但跑完后仍会刷新清单，见“同步模式”。

### 比较规则

内容相同判定优先级：双方都有 SHA-256 → 比 hash；同种后端都有
etag → 比 etag；否则大小 + mtime（1 秒内算相同）。只有一侧有
hash 时，清单是主判断依据，不会因为 mtime 对不上就反复复制。

## 后端

| 后端 | 认证 | 说明 |
|---|---|---|
| fs | — | 根目录；hash 在 `list` 时本地算出；写后尽量保留源 mtime |
| S3 | access key / secret（可加 session token） | SigV4 手写；支持自定义 endpoint（MinIO）；单 PUT，无分片 |
| WebDAV | Basic 或 Bearer | `PROPFIND Depth: 1` 递归；`MKCOL` 建父目录 |
| Google Drive | 应用提供的 access token（见下） | 按文件夹 ID 逐级解析路径；删除进回收站；可设置 `modifiedTime` |
| OneDrive | 应用提供的 access token | Graph 路径寻址；简单上传 < 4 MB；可设置 mtime |

**OAuth 不在库里。** Drive / OneDrive 只接受一个 `TokenProvider`：
静态 token，或一个“返回新鲜 token”的回调。登录、刷新、存 token
是应用的事——这一个决定省掉了一个 OAuth 客户端及其全部依赖。
回调应当自己缓存 token：库每次 HTTP 请求都会调一次。

CLI 里 token / 密码一律走环境变量，不走命令行参数（不进 shell 历史）：
`AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` / `AWS_REGION` /
`AWS_ENDPOINT_URL`、`WEBDAV_USERNAME` / `WEBDAV_PASSWORD` /
`WEBDAV_TOKEN`、`GDRIVE_ACCESS_TOKEN`、`ONEDRIVE_ACCESS_TOKEN`。

后端地址写法：`fs:/path`（裸路径也行）、`s3://bucket/prefix`、
`webdav:https://host/path`、`gdrive:<folder-id>`、`onedrive:`。

## 加一个新后端（Dropbox / SFTP / FTP …）

1. 新建一个 crate（如 `md-sync-dropbox`），依赖 `md-sync-core`（+
   需要的传输库），实现 `StorageBackend`：`list / read / write /
   delete / stat` 五个方法，`mkdir` / `set_modified` 有默认实现。
2. 给它一个 `XxxBuilder` 并实现 core 的 `Builder` trait。
3. 在门面 `md-sync` 里加一个 feature + `services` 里一行 re-export。
4. CLI 里在 `parse_backend` 加一个 `#[cfg(feature = "...")]` 分支。

引擎、清单、冲突、CLI 命令都不用动——它们只认 `StorageBackend`。

## 验证

```bash
cargo test --workspace              # 16 个单元测试 + 1 个端到端测试，全部通过
cargo clippy --workspace --all-targets   # 零警告
demo/run-demo.sh                    # 端到端场景：双向、空跑、删除传播、
                                    #   newer/both 两种冲突、dry-run、push、备份保护
```

单元测试覆盖：引擎（双向复制、冲突、单向删多余、备份保护）、
`RelPath` 越界拒绝、清单序列化、SHA-256 已知向量、S3 SigV4
（对照 AWS 文档的 canonical request 例）、S3 / WebDAV 的 XML 解析、
Drive / OneDrive 的 JSON 解析、RFC 3339 / RFC 1123 日期。

**S3 已跑真实服务**（2026-11-18，用户 rustfs 私有服务
`fs.gns.sh`，bucket `museai`，`demo/test-s3-live.sh`，只用
`md-sync-test/` 与 `md-sync-backup/` 两个前缀，跑完已清空）：
首次双向同步、二次空跑（fs ↔ S3 无 mtime 抖动）、本地改动上 S3、
S3 侧改动回流本地、删除传播、dry-run、备份、CLI list——全部通过。
另外：2026-10-21 记录的 HeadObject 403 这次没有复现（现存 key 的
HEAD 正常返回），但引擎复制时取源端 mtime 已改为 best-effort——
stat 失败不再拖垮复制。WebDAV / Drive / OneDrive 仍只有解析与签名
层面的单测，没有跑真实网络——接上真实账号后的首次同步请先用
`plan` 看一遍。

## 诚实的限制

- 整文件进内存：适合文档，不适合大文件；OneDrive 简单上传上限 4 MB，
  S3 单 PUT（无分片上传）。
- S3 / WebDAV 不能保留源文件 mtime（靠清单兜底）；Drive / OneDrive 可以。
- 没有流式读写、断点续传、文件锁、加密、压缩——奥卡姆剃刀：先不要。
- 同步是顺序执行的（一个文件接一个文件），失败的操作记进报告继续
  跑，不会因为一个文件失败就停掉整批；失败文件的清单状态以实际
  为准，下次会重试。
- `.md-sync.json` 损坏会被当作空清单处理（见上），代价是下次同步
  可能把差异文件判为冲突。

# md-sync 与 opendal / opendal-sync 的兼容性分析

——以 Genius 编辑器 `crates/opendal-sync` 实际使用的接口为准

日期：2026-11-18。分析对象：`~/workspace/genius-editor/crates/opendal-sync`
（v1.1.0，opendal 0.59.5）与 `~/workspace/md-sync`（v0.1.0）。
Genius 主程序启用 opendal-sync 的 features：`opendal`、`sync-drives`、
`services-webdav`；主 crate 另直接依赖 opendal（services-s3/fs/webdav/
gdrive/onedrive/dropbox、blocking、internal-tokio-rt）。

> **2026-11-18 晚更新**：同步模式已补齐——`Bidirectional`（无状态双向）、
> `Mirror`（`OneWay` 的正式名，恒删）已落地（`SyncMode`、CLI `--mode`、
> 单元测试 4 个、demo 第 11/12 节）。`Backup` 本来就已对齐。
> 模式层面的剩余差异只剩：默认冲突策略（对方 `Fail`，md-sync
> `KeepNewer`）和 `KeepBoth` 的命名规则。§3 的模式行已相应订正，
> 其余结论不变。

## 结论先行

**md-sync 不是 opendal / opendal-sync 的直接替代品，现在换不上去。**
分两层说：

1. **无状态同步层**（`SyncEngine` + `Storage` trait）：md-sync 大体能对上，
   概念和接口一一对应，但有若干**会静默改变行为**的语义差（见 §3）。
2. **有状态同步层**（`StatefulEngine` + `Replica` + `protected::Remote`）：
   **这是 Genius 实际在跑的同步**，md-sync 完全没有对应物——条件写、
   tombstone、逐文件 checkpoint、引用发现、共享索引、OAuth 刷新，
   全部缺失。补齐它们基本等于把 opendal-sync 再写一遍，也就丢掉了
   md-sync“小”的意义。

建议：现阶段不替换。如果目标是减体积，先量化 opendal 在 Genius
二进制里的实际占比再决定（见 §6）。

## 1. 先澄清：Genius 实际在用什么接口

Genius 主程序**没有用到**无状态的 `SyncEngine` / `StorageConfig` /
`SyncConfig` / `BlockingSyncEngine`（`src/` 里 grep 不到调用，只有 UI
文案里有“配置同步…”字样）。实际调用链是：

| 用到的接口 | 位置 | 用途 |
|---|---|---|
| `stateful::StatefulEngine::new(&local, &remote, &store, &identity)` + `run(cancel, resolutions, progress)` | `src/sync/mod.rs:427-441` | 每次同步的主入口 |
| `stateful::{Replica, StateStore, SyncState, Conflict, Resolution, Choice, Report, Fingerprint, Revision}` | `src/sync/mod.rs:17`、`src/sync/replica.rs` | 本地/远端两个 Replica + checkpoint |
| `storage::protected::Remote<Documents>`（`ContentPolicy`） | `src/sync/replica.rs:415-421,427` | 远端 Replica：条件提交、tombstone、共享索引、引用发现 |
| `storage::protected::{test_connection, safe_key, remote_tombstone, RemoteCache}` | `src/sync/mod.rs:22-23,252`、`src/sync/replica.rs:415` | 建连探针、路径校验、浏览远端、缓存持久化 |
| `storage::protected::drives::{OAuth, ManagedNamespace, dropbox, onedrive, managed_gdrive}` | `src/storage/drives.rs:4` | Dropbox / OneDrive / Google Drive 原生协议 |
| `storage::backup::Backup` | `src/sync/backup.rs:2`、`src/app/sync_ui/backup.rs` | 手动备份上传/下载 |
| `opendal::Operator`（主 crate 直接建） | `src/sync/mod.rs:427`、`src/cloud/*` | S3 / R2 / WebDAV / 各云盘的 Operator 构造 |
| `CancellationToken` | `src/sync/mod.rs:25` | UI 取消同步 |

本地 Replica（`src/sync/replica.rs:260` 起）也不是简单的文件读写：
它有写锁（与编辑器保存互斥）、dirty buffer 检查（有未保存内容拒绝
下载覆盖）、root_stamp（目录实体变化即废弃 checkpoint）、symlink
拒绝、基于 size+mtime+inode 的本地缓存（git index 式，免重复哈希）、
Markdown 引用发现（图片/附件决定同步哪些非 md 文件）。

## 2. 传输层对照：`Storage` trait vs `StorageBackend`

| opendal-sync `Storage`（`storage/mod.rs`） | md-sync `StorageBackend`（core） | 结论 |
|---|---|---|
| `list_files(prefix: Option<&str>) -> Vec<FileInfo>`（递归，**含目录**，`is_dir`） | `list() -> Vec<Entry>`（递归，**仅文件**，目录隐式） | 部分对齐。md-sync 无法表达“目录”，也就做不了 Genius 的 file/dir 冲突硬错误 |
| `stat(path) -> Option<FileMeta>`（`""` 表示根，根必须是目录） | `stat(path) -> Option<Entry>`（无根概念） | 基本对齐 |
| `read(path) -> Vec<u8>` | `read(path) -> Vec<u8>` | 对齐 |
| `read_range(path, offset, length)`（短读 = 文件变了，报错） | **无** | 缺失。大文件哈希/复制依赖它 |
| `write(path, data)` | `write(path, data) -> Entry` | 对齐（返回值多一个 Entry，无害） |
| `write_stream(path, stream, total_size)`（8 MiB 分片、可并行上传） | **无** | 缺失。md-sync 整文件进内存 |
| `delete(path)` | `delete(path)` | 对齐 |
| `exists(path)`（默认用 stat 实现） | **无**（可用 stat 拼） | 可补，一行 |
| `create_dir(path)` | `mkdir(path)`（默认空实现） | 对齐 |
| `copy(from, to)`（同后端内复制，默认 read+write） | **无** | 可补，一行 |
| `name() -> &str` | `name() -> &'static str` + `describe()` | 对齐 |
| 错误：`anyhow::Result` | 错误：`thiserror::SyncError` | 不兼容。Genius 的 `Failure::from_error` 会 downcast `opendal::Error` 和 `std::io::Error` 做分类，md-sync 的错误进不了这个映射 |

另外：`Storage` 的方法契约更严——“列表不得把权限/网络错误伪装成
空目录”、“stat 只在不存在时返回 None，权限/认证/网络问题必须 Err”。
md-sync 的后端大体遵守，但没有对应的测试约束。

**小结**：把 md-sync 的后端包一层实现 opendal-sync 的 `Storage`
trait 是可行的（约 100 行适配器 + `read_range`/`write_stream`
用整文件读写兜底），但这只能接入**无状态** `SyncEngine`，而 Genius
主流程不用它。

## 3. 无状态引擎对照：`SyncEngine` vs md-sync `Engine`

| 维度 | opendal-sync `SyncEngine` | md-sync `Engine` | 影响 |
|---|---|---|---|
| 模式 | `Backup`（默认，保留目标独有文件）、`Mirror`（目标向源看齐，删多余）、`Bidirectional`（双向复制，**无历史状态时不传播删除**） | `Backup` / `Mirror` / `Bidirectional` / `TwoWay`（有清单基线的双向，传播删除）——**已对齐**（2026-11-18 补齐）。`Mirror` 与 `OneWay` 的区别只剩 `delete_extraneous` 开关：`Mirror` 恒删，`OneWay` 可关。`TwoWay` 是 md-sync 多出来的有状态模式，对方无状态层没有对应物 | 模式本身已对齐。注意默认冲突策略仍不同（见下行），且 `TwoWay` 依赖清单、不能直接替代对方的 `Bidirectional` |
| 冲突判定 | 共有文件大小或 BLAKE3 不同即冲突（无基线，分不清“只一边改了”） | `Bidirectional`：同款判定（两边内容不同即冲突）；`TwoWay`：相对 `.md-sync.json` 清单基线：两边都变才冲突 | `Bidirectional` 与对方一致；`TwoWay` 是更精确的判定，不是对方的语义 |
| 冲突策略 | `Fail`（默认，执行前整轮拒绝）、`PreferSource`、`PreferDestination`、`Skip` | `KeepNewer`（默认）、`KeepA`、`KeepB`、`KeepBoth`、`Skip` | **仍无交集的默认项**：md-sync 没有 `Fail`（整轮拒绝）；对方没有 `KeepNewer`/`KeepBoth` |
| KeepBoth 命名 | stateful 层：`name (conflict <hash前12>).ext`，确定性命名 | `name.conflict-<side>-<时间戳>.md` | 冲突副本文件名不兼容；stateful 的命名是内容确定的，可去重 |
| 哈希 | BLAKE3，**不信任后端 ETag/checksum**；同大小共有文件必读两端算 hash | SHA-256；**信任**后端 hash/etag（有就比，没有才 fallback 到大小+mtime） | md-sync 在远端 ETag 语义变化时可能误判；opendal-sync 流量更大但判断更稳 |
| 同步范围 | **全部文件类型**；目录参与比较；glob `exclude_patterns`（匹配相对路径及所有祖先） | 默认只同步 `md` / `markdown`，可配扩展名；无 glob | Genius 同步笔记 + 附件（图片等），md-sync 默认会漏掉附件；要改成“全部文件 + 排除规则” |
| 传输 | 8 MiB 分块 `read_range` + `write_stream`；执行阶段并发（默认 4）；先全部复制、再删除，复制失败则不删 | 整文件读写；**串行**；边复制边删（按计划顺序） | 大文件/大批量时 md-sync 内存和速度都差一截；失败语义也不同 |
| 预览 | `analyze() -> SyncPlan`（只读，可反复看；`run()` 会重新分析，旧计划不可执行） | 内部有 plan，CLI 的 `plan` = dry-run | 概念对齐，但 md-sync 的 plan 不是公开的稳定 API（`plan_two_way` 是 pub，可用） |
| 进度/取消 | `run_with_progress(cancel, callback)`；`SyncProgress{current_path, operations_total/completed, bytes_transferred, files_failed}`；`CancellationToken` | **无**（`Report` 只有最终统计） | Genius UI 依赖进度条和取消按钮，接 md-sync 要另做 |
| 报告 | `SyncReport{files_copied/deleted/skipped/failed, bytes_transferred, elapsed, cancelled, errors: Vec<SyncFailure{path, message}>}`；`is_success()` = 未取消且零失败（Skip 的冲突不算失败） | `Report{ops_applied, copied_bytes, deleted, conflicts, skipped, errors, warnings}`；`is_ok()` = 无 errors | 字段大体可映射，但 md-sync 的 `conflicts` 在 `Fail` 语义下没有对应物 |
| 错误 | `Error::{InvalidConfig, Storage(anyhow), Cancelled, Conflicts(usize), BlockingInAsyncContext, Runtime}`；执行期单文件失败进报告 | `SyncError`（thiserror 枚举） | 模型不同（见 §2 末） |
| 阻塞 API | `BlockingSyncEngine`（独立 runtime，禁在 Tokio 内调用） | **无** | Genius 目前没用上，但它是公开 API 的一部分 |
| 根校验 | 源必须是已存在目录；两本地根不得重叠；目标根非目录拒绝；文件/目录冲突拒绝 | `RelPath` 校验（防 `..`、绝对路径、反斜杠）；**无根重叠检查、无目录冲突概念** | md-sync 对“两个根是同一个目录”这种误配置没有防护 |

## 4. 有状态引擎对照：`StatefulEngine` / `Replica` vs md-sync

这是差距最大的地方。md-sync 的清单思想与 `SyncState` 同源（三路合并），
但协议深度完全不同：

| 维度 | opendal-sync stateful | md-sync | 影响 |
|---|---|---|---|
| 基线 | `SyncState{version, identity, entries: path -> Option<Fingerprint>}`；identity 绑定本地目录实体 + 远端绑定（换 bucket/prefix/目录实体必须换 identity，否则新目录会被当成大量删除）；`StateStore::save` 必须原子；**每完成一个文件就保存一次** checkpoint | `.md-sync.json`（version + files）；无 identity；成功后一次性写两端 | md-sync 的清单没有“这份基线属于哪一对”的概念；换目录/换桶后不会自动失效，存在把新目录当删除的风险（stateful 文档明确警告过这一点） |
| 内容身份 | `Fingerprint{size, BLAKE3}`；`Revision{content, token}`，token 是不透明的条件操作令牌（ETag） | `Entry{hash: Option<SHA-256>, etag: Option<String>, size, mtime}` | 概念接近，但 md-sync 的 etag 不参与条件写 |
| 三路合并 | `direction(base, local, remote, tombstone)`：一端等于基线则传播另一端；两端都变则冲突；无基线 + 本地有文件 + 远端有删除记录 → **冲突（防旧设备复活已删文件）** | 同款逻辑（`changed_since` + `same_content`），但**无 tombstone 输入** | md-sync 里“本地有、远端无、清单无”一律当新建复制；一台离线很久的设备会把已删文件复活 |
| 条件写（CAS） | `Replica::replace(path, expected, bytes)`：`expected=None` 仅不存在时创建；`bytes=None` 仅未变化时删除；比较与提交之间不许穿插；远程用 `If-Match` / `If-None-Match`；**不支持条件操作应拒绝执行，不能退化为覆盖** | **无**。`write` 是无条件覆盖 | **最关键的缺口**。多设备同时写同一文件时 md-sync 是 last-write-wins 静默覆盖；Genius 的协议保证要么提交成功、要么明确失败 |
| 读校验 | `read(path, revision)`：读指定版本，源变了就失败（`If-Match` + 哈希复核） | `read(path)` 无版本概念 | 同上 |
| 删除记录 | `deleted_paths()` + `remember_deletion()`；远端持久记录在 `.opendal-sync/deleted/<blake3(path)>.json`，**不按时间清理**；已删文件的 tombstone 也有内容型（WebDAV/Dropbox/OneDrive/GDrive 用带 `GENIUS-SYNC-TOMBSTONE/1` 头的 body）和 content-type 型两种 | **无** | 见上条复活问题；且 md-sync 的删除是真删，没有任何记录 |
| 冲突解决 | `Resolution` 绑定**精确的（path, local revision, remote revision）**；源或目标一变旧决定即失效；`Choice::{Local, Remote, Both}`；`Both` 保留本地原名、远端版本写确定性冲突副本；**编辑 vs 删除冲突保留存在的文件** | `ConflictPolicy` 是整轮配置，不是按冲突逐个的决定；`KeepBoth` 的胜者是 newer（mtime），败者副本名带时间戳 | md-sync 没有“用户在 UI 里逐个点选”的决议模型；`KeepNewer` 依赖 mtime，而 stateful 明确不用 wall clock 做决定 |
| 跳过路径 | `Replica::skipped()`：本地路径变成 symlink/特殊文件时，该路径记单项失败、checkpoint 不动、**不被误认成删除**，其余文件继续同步 | **无** | md-sync 的 fs 后端直接忽略 symlink；一个被替换成链接的已追踪文件会被当成删除传播 |
| 本地保护 | Local Replica 与编辑器写锁共享、dirty buffer 检查、root_stamp、原子写（同目录临时文件 + rename） | fs 后端是普通写，无锁、无 dirty 概念 | Genius 不能直接把 `Local` 换成 `FsBackend`，会丢“未保存内容不被下载覆盖”的保证 |
| 共享索引 | `Remote` 在 `.opendal-sync/index.json` 发布已验证对象（path → ETag + fingerprint）；新设备靠它播种缓存，下载一次而非两次；ETag 不变则信任缓存，**安静项目一次同步只有一次 list 请求** | **无**。每次同步都 list + 对无 hash 的远端文件靠大小/mtime 判断 | md-sync 在大项目上请求量和流量都大得多；且没有“另一台设备已验证”的信任链 |
| 缓存 | `RemoteCache`（对象 ETag/fingerprint、删除记录、index ETag/hash、dirty 位）+ 本地 `LocalCache`（size+mtime+inode stamp） | **无**。每次 list 都重新算 fs 文件的 SHA-256 | 同上；md-sync 的 fs `list()` 每次都读全部文件算 hash |
| 取消 | 可打断只读扫描；**进行中的单文件提交不会被丢弃**，提交 + checkpoint 完成后才处理下一次取消 | **无取消** | 接入 UI 要补 |
| 串行/缓冲 | 串行处理文件，内存中一次只缓冲一个完整文件 | 同样串行、整文件缓冲 | 这一点两者一致 |

## 5. 远端后端对照

| 后端 | Genius / opendal-sync 的做法 | md-sync 的做法 | 差距 |
|---|---|---|---|
| S3 | opendal `services-s3`：分片上传、presign、完善的错误分类；`Remote` 用 ETag 做条件写 | 手写 SigV4、**单 PUT**、无分片；ETag 只读不用于条件写 | 功能上能通（已在 rustfs 上实测），但大文件和条件写不行 |
| WebDAV | opendal `services-webdav` + protected 层：**要求 strong ETag**，`test_connection` 探针验证 `If-Match`/`If-None-Match`/tombstone 全套能力，不满足就拒绝同步 | PROPFIND + PUT；只要求 getetag/getlastmodified 存在 | md-sync 能在更多弱 WebDAV 服务上“跑起来”，但跑起来不等于受保护；Genius 的探针是故意设的门槛 |
| Google Drive | 两条路：opendal `services-gdrive`（普通文件）+ **原生 `managed_gdrive`**（不可变内容 + CAS manifest，显式不导入普通现有文件） | Drive API v3 直调：按文件夹 ID 解析路径、multipart 创建、`modifiedTime` 可设 | md-sync 只有普通文件路；没有 managed namespace / CAS manifest；OAuth 只有静态 token，没有 refresh |
| OneDrive | opendal `services-onedrive` + 原生 drives 服务：**单请求上传 250 MB** | Graph 简单上传，**< 4 MB** 上限（README 已注明） | 4 MB 以上文件 md-sync 直接拒绝 |
| Dropbox | opendal `services-dropbox` + 原生 drives 服务（strict revision 检查） | **未实现**（计划中） | 缺失 |
| 本地 fs | `LocalStorage`：原子提交（同目录临时文件 + rename）；`Storage` trait | `FsBackend`：直接写；有 `set_modified` 保留 mtime | md-sync 的写不是原子的；Genius 的编辑器保存依赖原子写 |

认证模型也不同：Genius 的 drives 是 `OAuth{client_id, client_secret,
refresh_token}` + 内置 token 刷新与过期管理；md-sync 只有
`TokenProvider`（静态 token 或应用回调），refresh 完全是应用的事。
这在 md-sync 的设计里是故意的（省依赖），但对 Genius 意味着接入方
要自己补一套 refresh。

## 6. 体积与依赖

这是 md-sync 存在的理由，也是唯一明确的优势：

- md-sync core 只有 5 个直接依赖（serde、serde_json、sha2、thiserror、
  async-trait），无 tokio、无 HTTP；实测 CLI fs-only **810 KiB**（51 个
  依赖）、full 五后端 **2.5 MiB**（162 个依赖）。
- opendal-sync 本身依赖 opendal 0.59.5（services、HTTP transport、
  layers）、tokio、futures、walkdir、globset、blake3、http、url、rand，
  再加主 crate 的 opendal 全套 services。两者不在一个量级——但**没有实测
  Genius 二进制里 opendal 到底占多少**，在没量之前，“换 md-sync 省体积”
  只是假设。建议先用 `cargo bloat` / `nm` 量一下再决定值不值得动。

其他工程约束也不同：opendal-sync 是 edition 2024、rust 1.91、forbid
unsafe、clippy pedantic；md-sync 是 edition 2021、rust 1.75+。Genius
主程序用 Rust 1.98+，md-sync 的版本下限不是问题。

## 7. 如果要让 md-sync 满足 Genius，需要补什么

按“缺了不能上”的顺序。P0 基本等于重写一个 stateful 层：

**P0（缺一不可）**
1. `Replica` trait：`scan / read(revision) / replace(expected, bytes) /
   deleted_paths / remember_deletion / skipped`，以及配套的
   `Fingerprint`（BLAKE3）、`Revision{content, token}`、`SyncState` +
   `StateStore`（identity 绑定、原子保存、每文件 checkpoint）。
2. 各后端条件写：S3 的 `If-Match`/`If-None-Match`（ETag）、WebDAV 的
   条件 PUT、Drive/OneDrive 的 etag 条件请求；不支持的后端要**明确拒绝**，
   不能静默退化为覆盖。
3. Tombstone：删除记录的格式、位置与读取（`.opendal-sync/deleted/`），
   以及 body/content-type 两种探测。
4. 全文件类型 + `ContentPolicy`（排除规则、Markdown 引用发现、附件选择），
   否则附件不同步。
5. 逐冲突决议模型（`Conflict::resolve(Choice)` 绑定精确版本对），替代
   整轮的 `ConflictPolicy`；以及 `test_connection` 探针。
6. 进度回调 + `CancellationToken`（取消不丢弃进行中的提交）。

**P1（上了才好用）**
7. `read_range` / `write_stream` 分块传输 + 执行并发。
8. `RemoteCache` + 共享 `.opendal-sync/index.json`（否则大项目每次全量扫描）。
9. `Backup` 的上传/下载（read-back 校验、目标只保留列表内文件）。
10. 目录建模与 file/dir 冲突、根重叠检查、本地写锁/dirty 检查/
    root_stamp/symlink 拒绝（这些在 Genius 的 `Local` 里，不在库里，
    但 md-sync 的 `FsBackend` 至少不能比它弱）。

**P2（再往后）**
11. OAuth refresh（Dropbox/OneDrive/GDrive 原生协议）、managed_gdrive
    的 CAS manifest、强 ETag 校验。

**更小的替代路线**（如果只是想用 md-sync 的后端，不动同步语义）：
写一个约 100 行的适配器，让 md-sync 的 `StorageBackend` 实现
opendal-sync 的 `Storage` trait（`read_range`/`write_stream` 用整文件
读写兜底）。这能把 md-sync 的后端接进**无状态** `SyncEngine`，但
Genius 主流程用的是 stateful 引擎，所以这条路只能服务备份、导入等
次要场景，不能替代主同步。

**另一条更现实的路线**：保持 opendal-sync 不动。md-sync 继续作为独立
小工具（CLI、嵌入式、只需要备份/镜像的场景）。Genius 的同步是多设备
一致性协议，不是“把文件从 A 拷到 B”——它的复杂度（条件写、tombstone、
checkpoint、引用发现）正是 opendal-sync 存在的理由，也是 md-sync
为“小”而主动不要的东西。两者目标不同，硬凑只会两头不讨好。

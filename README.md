# Binary Format Workbench

单机运行的二进制文件格式多版本工作台。协议设计者用结构化 JSON 声明 magic、整数字段、
位域、对齐、定长/变长字节、条件分支、校验和范围以及 TLV 扩展块，导入样本字节后按指定
版本解析，保留未识别区域；规则（解析、迁移、冲突检测、冻结指纹、批量转换）全部在服务端
执行，网页只是规则的可见入口。

## 运行

```bash
cargo fetch
cargo test --quiet
cargo run --bin server -- --listen 127.0.0.1:5219
# 打开 http://127.0.0.1:5219
```

可选参数：`--data-dir <目录>`（默认 `./bfw-data`）。首次启动会写入一套内置演示格式
（`img@v1`、`img@v2`、继承演示 `img-ext@v1`）、一个样本与一条有损迁移规则。

## 数据模型（`src/model.rs`）

格式定义 `FormatDoc`：`id`、`version`、`endian`、`magic[]`（带绝对偏移）、
`inherits`（父格式引用）、`layout[]`。条目类型：

- `int`：u8/u16/u32/u64 与有符号版本，可用 `offset` 指定绝对位置（用于 magic 之后的字段
  或历史遗留覆盖式布局）。
- `bits`：一个底层整数 + 多个 `(name, lsb, bits)` 位部件。
- `align`：按边界对齐；`pad = zero` 时非零填充字节直接报错并给出精确偏移，`preserve`
  时保留原填充。
- `bytes`：定长 `fixed`，或长度引用其它整数字段 `field`。
- `branch`：若干条件臂（eq/ne/gt/lt/ge/le/has_bit），可给无条件默认臂；没有臂命中时报
  `unmatched_branch`。
- `checksum`：xor8 / sum16 / crc32（IEEE 0xEDB88320），`range` 用 `start`/`end` 两个边
  （`start`、`end`、`field_end{path}` + 有符号偏移）描述。缺省范围是
  `[文件起点, 校验字段起点)`，因此默认天然不包含自身。
- `ext`：TLV 扩展块序列，数量可固定或引用整数；`header` 描述 tag/len 的宽度、字节序与
  倍数；`known` 按 tag 映射到子布局，未知 tag 原样保留其 payload，写出时逐字节重建。

继承把祖先的 magic 与 layout 前置拼接到当前定义前。编译器静态拒绝：

- 继承环（含自环）；
- 同名字段/路径遮蔽（祖先字段与当前字段重名、位部件重名等）；
- 校验范围**静态自指**（范围边界显式引用自身路径）。

解析期还会检测**范围自覆盖**（区间实际包住校验字段本身）。

## 解析与精确错误（`src/parser.rs`）

解析结果是节点树，每个节点带 `path/kind/start/end/value`，未知区域标记
`identified=false`（magic 与第一个字段之间的空洞是 `unidentified.gap.*`，文件尾部剩余字节
是 `unidentified.trailer`）。所有错误携带**准确字节偏移**与字段路径：

- `short_read`：需要的字节越过文件末尾；
- `length_out_of_bounds`：变长字段/扩展 payload 越过所在作用域（如扩展块边界）；
- `overlap`：绝对偏移字段与已顺序定位的字段重叠（按精确偏移报告）；
- `bitfield_out_of_bounds`、`alignment_nonzero`、`checksum_self_reference` 等；
- magic 不匹配与校验和不符是**告警**（`bad_magic` / `checksum_mismatch`），解析仍返回树，
  但 HTTP 状态码为 422，前端红色/黄色分别展示。

## 写回与字节一致性（`src/writer.rs`）

写出是一次**全量重建**，但按节点树重建为与输入完全相同的字节：

- 未修改样本直接写回**逐字节一致**（有测试保证，含未知尾部与未知 TLV payload）；
- 长度字段、扩展块数量字段、TLV len、校验和都在两遍发射后**自动回填**，并在
  `auto_fields` / 每条 write-range 的 `auto` 标记中声明；
- 间隙字节从原始输入复制，绝不“顺手”重排或挪动未知扩展；
- 返回每条字段的**写出范围**，与解析节点的**读取范围**一起在页面点击字段时展示。

## 迁移（`src/migration.rs`）

规则 `RuleDoc` 声明 `from/to` 格式引用与绑定列表，绑定来源为字段引用、常量或默认。
对样本运行 dry-run 后输出：

- 每个目标字段的**来源**（field/constant/default）、默认值清单、来源路径；
- 输出字节与输出布局（写范围、自动回填字段）；
- **损失**：源格式存在但没有被任何绑定带走的叶字段；
- 反向验证三档：
  - `strict`：转发字节与输入严格相同（同格式恒等迁移）；
  - `semantic`：有布局/校验差异但所有携带字段语义相等、无损失；
  - `lossy`：存在损失或携带值改变。
- 接受有损项必须绑定**字段路径 + 规则 id + 规则版本**（`AcceptedLoss`），发布与批量转换
  都会严格核对；多报（stale）或漏报（unaccepted）都被拒绝。

## 持久化与并发（`src/store.rs`）

- 只追加 WAL（帧 = magic + 长度 + CRC32 + JSON 事件），每帧 `fsync`；每 50 个事件原子
  生成快照并截断 WAL。重放时 CRC 失败的尾部帧被丢弃，模拟半写（进程被 `kill -9`）。
- 规则与计划是**版本号乐观并发**：PUT 携带 `rev`，落后写入返回 **409 + 双方差异**
  （服务端版本、客户端版本、服务端文档、逐字段 diff），绝不静默覆盖。
- 格式 `(id, version)` 不可变；规则通过“新版本 + rev 检查”演进，冲突返回双方差异。
- 幂等：写请求可带 `Idempotency-Key` 头，重复请求返回首次缓存的状态码与响应体
  （批量转换同样幂等）。
- **导入内容保留原文**：样本是不可变文档；规范化或修订通过
  `POST /api/samples/revision` 另存为新样本（`derived_from` 指回原样本），原始 hex 不被
  覆盖。

## 计划冻结与批量转换

- `POST /api/plans/publish` 对样本集逐个 dry-run、核对损失绑定，任一失败即 422，计划停留
  在 `draft`；成功后状态转 `published`，并冻结**定义指纹**（规则、from/to 格式的
  FNV-1a64 指纹，基于键排序的紧凑规范 JSON）。
- 只有已发布计划可运行批次；批量转换重新核对指纹（定义变更 → 409 `fingerprint_drift`），
  任一文件失败则整体 422，**不写入任何可见批次**；成功才原子追加一个 BatchDoc。
- `draft → published` 之外的跳转（例如再次 publish 已发布计划、对草稿跑批次）返回
  409 `illegal_transition`。
- `GET /api/export` 导出确定性 canonical JSON 包并附指纹，重复导出字节稳定。

## HTTP API

| 方法 路径 | 作用 |
| --- | --- |
| `GET /api/formats` / `PUT /api/formats` | 列出 / 创建（校验并返回指纹）不可变格式版本 |
| `GET /api/samples` / `PUT /api/samples` | 列出 / 导入不可变样本 |
| `POST /api/samples/revision` | 规范化/编辑后另存新样本 |
| `POST /api/parse` | 按版本解析（支持 `sample_id` 或 `format+hex`） |
| `POST /api/emit` | 用编辑集重写，返回字节、逐字节一致性、读/写范围 |
| `GET /api/diff?...` | 同一样本在两个版本下的树与逐字段双版本差异 |
| `GET /api/rules` / `PUT /api/rules` | 规则列表 / 乐观并发更新（body 带 `rev`） |
| `POST /api/migrate/dry-run` | 迁移预演：来源、默认、损失、布局、三档等价 |
| `GET/PUT /api/plans`、`POST /api/plans/publish`、`POST /api/plans/batch` | 计划、冻结、批次 |
| `GET /api/export` | 确定性导出 |

## 页面（`src/web/`，无构建步骤的原生 JS）

- **解析检查**：结构树、十六进制覆盖图（字段/未识别/错误三色）、点击字段查看读取与写出
  范围、精确偏移的告警/错误列表。
- **双版本差异**：左右两棵树 + 逐字段 changed/added/removed 表与各自定义指纹。
- **迁移工作台**：选规则/样本做 dry-run，展示来源与损失；一键把 dry-run 损失绑定到
  规则版本后发布冻结，并运行原子批量转换。

## 测试

`cargo test --quiet`（共 30+ 用例，全部在进程内或真实子进程运行）：

- 解析/往返：逐字节一致、短读与越界精确偏移、绝对字段重叠、校验范围静态/动态自指、
  编辑后长度与校验自动回填、未知尾部保留。
- 扩展：未知 TLV payload 逐字节往返、截断时精确偏移。
- 迁移：lossy/semantic/strict 分档、损失必须绑定规则版本、同格式严格等价。
- 模型：继承环、同名遮蔽、重复位部件、指纹随定义变化。
- 存储：幂等重复请求、落后写入 409 双方差异、重开恢复、快照恢复、确定性导出。
- HTTP：非法状态跳转、批次失败不落任何批次、批次幂等、非法 hex 不 panic。
- **进程异常退出恢复**：启动真实 server 子进程写入数据后 `kill -9`，重新启动验证格式、
  样本、幂等记录全部从 WAL 恢复。

## 架构取舍

- 依赖刻意最小：只使用 `serde` / `serde_json`，HTTP/1.1、CRC32、FNV-1a、canonical JSON
  均手写，便于 `cargo fetch` 后离线验收，也便于审计每条字节规则。
- 单进程 `Mutex<State>`：单机工作台无需多副本；锁内完成“检查 + WAL 追加”保证串行化与
  崩溃一致性，避免引入嵌入式数据库带来的格式/版本面。
- 定义指纹用 FNV-1a64 而非加密哈希：目标是检测意外改动与冻结漂移，不提供抗碰撞安全承诺。
- 写回采用全量重建而非差量补丁：规则集中、可测试，并能保证未知区域原样复制；代价是
  超大文件（GB 级）会全量驻留内存。

## 已知限制

- 单文件内存处理，超大样本不适用；位域不支持跨字节的非对齐拆分。
- 条件表达式只支持单个字段的比较/has_bit，不支持任意布尔公式。
- 扩展块已知子布局内的字段不能作为外层条件/长度引用；未知 payload 不解析、仅保真往返。
- 校验和范围默认排除自身字段；需要把自身按 0 参与等特殊算法时需显式声明且不能自覆盖。
- 单机无鉴权/TLS，绑定本地地址使用；多用户协作场景需要在外层加网关与鉴权。

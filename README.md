# DeskZen（桌面众生）

## 产品定位与核心理念

DeskZen 是一款拥有“独立生活节律”的桌面伴生 AI 软件。

不同于传统的“指令驱动型”桌面宠物或聊天机器人，DeskZen 中的每一个角色（Persona）都是一个“活在屏幕里的室友”。它们拥有自己的 24 小时作息时间表，会根据当前状态（待机、跑步、挥手、跳跃、复习等）做出相应的动画表现，并能基于时间节点轻度、非打扰式地主动发起气泡对话；对话时的人设与回复风格也随当前状态动态变化。

## 功能总览

| 功能 | 说明 |
|---|---|
| 生活状态引擎 | 每个角色独立的状态机：默认按 `loop`（逐状态 `{state,duration}`）循环；`time` 时段内固定为对应状态，每 30 秒计算并广播状态变化 |
| 多角色运行时切换 | 内置林克；托盘「更换角色」子菜单即时切换（✓ 标记当前角色） |
| Petdex 角色导入 | 从 petdex.dev 链接下载 zip 包（或直接资源），自动生成角色配置并持久化 |
| 角色删除 | 设置界面一键删除导入角色，删除当前角色时自动回退到默认角色 |
| 桌面角色动画 | spritesheet + CSS steps 帧动画（内置林克 20×6，6 种状态各占一行） |
| 对话面板 | 双击角色打开，跟随角色的对话气泡（尖角指向角色、高度自适应），接入 OpenAI 兼容大模型 |
| 状态气泡 | 状态切换 / 收到回复时角色头顶弹出轻量气泡，超时自动消失 |
| 角色右键菜单 | 右键角色弹出「下个状态 / 隐藏」：循环切换角色动画状态（按 spritesheet 行序），或隐藏角色窗口 |
| 点击穿透 | 可选整窗点击穿透，不遮挡下层操作 |
| 托盘控制 | 显示/隐藏角色、更换角色、打开设置、退出 |

## 核心功能模块与业务逻辑

### 角色与作息系统（Life Schedule Engine）

核心引擎，决定角色的行为与回复逻辑，全部在 Rust 主进程运行。

- 多角色支持：内置角色 + 用户导入角色，运行时切换。
- 状态机驱动：默认按 `loop` 列表（每项 `{state,duration}` 分钟）循环；`time` 配置的时段内固定为对应状态（支持跨午夜），时段外回到循环。
- 内置角色状态由 persona.json 定义（林克：`walking`/`motorcycle`/`idle`）；Petdex 导入角色默认 9 个标准状态（`Idle`、`RunRight`、`RunLeft`、`Waving`、`Jumping`、`Failed`、`Waiting`、`Running`、`Review`，与 Petdex 规范一致，spritesheet 每状态一行）。
- 状态与回复绑定：对话时 system prompt 注入「角色定义 + 回复风格 + 当前状态约束」，同一角色在不同状态下回复风格不同。
- 状态切换气泡：状态变化时从该状态的气泡文本池随机弹出一条。

### 主动交互与防打扰机制

- 气泡机制：角色头顶轻量气泡，带淡出动画，不抢焦点、不弹系统窗口；用户可无视（超时消失）。
- 桌面常驻：角色窗口透明无边框、置顶、不占任务栏；隐藏/销毁对话窗不影响角色状态。
- 右键菜单：右键角色弹出原生菜单「下个状态 / 隐藏」。「下个状态」切到循环列表的下一个状态；若处于 `time` 时段内，则转入循环模式直到该时段结束，再回到 time 配置（期间头顶气泡提示新状态）。

### 角色导入与删除（Petdex）

- 导入：设置界面「角色设置 → 从 Petdex 导入」，输入 `https://petdex.dev/pets/{slug}` 链接，流程见下文「核心流程」。
- 删除：设置界面「已导入角色」列表可删除；同时移除磁盘文件与托盘菜单项；删除当前角色时自动回退到内置默认角色（林克）；内置角色不可删除。

### 聊天窗口

- 双击角色打开，无边框透明**对话气泡**：约 340px 宽、高度自适应内容、底部尖角指向角色；米白/浅暖黄底、细描边、轻阴影；头像与名字置于气泡左上角、右上角小关闭圆点。
- 头部：角色头像（名字首字）、角色名、当前状态胶囊、关闭按钮；按住头部可拖动窗口。
- 消息区：用户消息暖金色靠右、角色消息白色靠左，输入中显示动态省略号动画，支持空状态提示。
- 对话窗跟随角色移动（拖动角色时自动贴到角色旁边）。

## 视觉表现与桌面渲染

- 渲染层：WebView2 + Vite + TypeScript（无框架），spritesheet + CSS `steps()` 帧动画。
- 角色窗口：无边框透明、置顶、跳过任务栏，仅角色精灵区域可见。
- 精灵图规范：8 列 × 9 行、每帧 192×208（Petdex 规范）；内置角色位于 `resources/characters/`，导入角色位于用户数据目录，通过 Tauri asset 协议加载。

## 技术架构

```
┌─ 渲染层（WebView2 + Vite + TypeScript，无框架）────────────────┐
│ 角色动画（spritesheet + CSS steps）│气泡│对话/设置页 │
└───────────────▲───────────────────────────────▲──────────────┘
   Tauri IPC（command / event）                   │
┌───────────────┴───────────────────────────────┴──────────────┐
│ Rust 核心进程（常驻，低占用）                                 │
│ 生活状态引擎 │ 气泡调度 │ LLM 网关(OpenAI 兼容) │ Petdex 导入     │
│ 窗口管理 │ 托盘 │ 角色持久化（用户数据目录扫描）                │
└───────────────▲─────────────────────────────────────────────┘
   Tauri 窗口能力：透明无边框、点击穿透、按需创建/销毁窗口、asset 协议
```

设计原则：

- **逻辑与渲染分离**：所有“活着”的逻辑（状态、时间、气泡、LLM、角色注册）都在 Rust 主进程，WebView 只负责表现；隐藏/销毁窗口不影响角色状态。
- **按需创建窗口**：设置窗需要时创建、关闭即销毁；对话窗首次创建后关闭改为隐藏，保留本会话内的对话历史，再次打开即时恢复。
- **配置驱动角色**：作息、状态、动画、气泡、提示词全部来自 persona JSON；新增角色 = 新增一个角色目录，无需改代码。
- **导入角色运行时注册**：导入角色持久化到 `%APPDATA%\com.deskzen.app\characters\`，启动时自动扫描加载，托盘「更换角色」菜单动态增删。

### 目录结构

```
DeskZen/
├─ index.html / chat.html / settings.html   三个页面入口
├─ src/                                     前端（TS）
│  ├─ main.ts       角色窗：动画、拖拽、气泡、右键菜单
│  ├─ chat.ts       对话窗：历史、发送、状态展示
│  ├─ settings.ts   设置窗（侧边栏：角色设置 / 大模型设置）
│  └─ styles.css
├─ resources/                                资源目录（Vite publicDir）
│  ├─ characters/link/
│  │  ├─ persona.json     角色配置（作息/状态/气泡/提示词）
│  │  └─ spritesheet.webp 角色精灵图（20×6）
│  └─ icons/              应用图标（32/128/256 + ico）
├─ scripts/                                  开发与测试脚本
│  ├─ gen_icons.py    从源 PNG 生成应用图标
│  ├─ smoke_test.ps1 / llm_test.ps1
│  └─ petdex_e2e.mjs / petdex_ui_e2e.mjs / petdex_delete_chat_e2e.mjs / settings_sidebar_e2e.mjs
└─ src-tauri/
   ├─ src/
   │  ├─ main.rs / lib.rs   入口、窗口、托盘、命令
   │  ├─ engine.rs          生活状态引擎 + persona 注册/持久化
   │  ├─ petdex.rs          Petdex 导入/删除命令
   │  └─ llm.rs             LLM 网关（OpenAI 兼容）
   ├─ capabilities/         Tauri 权限
   └─ tauri.conf.json       窗口、asset 协议、打包配置
```

## 交互约定

| 操作 | 行为 |
|---|---|
| 按住角色拖动 | 移动角色窗口（移动超过 4px 判定为拖动） |
| 左键双击角色 | 打开对话窗（贴在角色旁边，拖动角色时跟随） |
| 右键点击角色 | 弹出「下个状态 / 隐藏」菜单 |
| 托盘左键单击 | 角色隐藏时显示角色 |
| 托盘左键双击 | 打开设置界面 |
| 托盘右键 | 菜单：设置 / 显示隐藏角色 / 更换角色 / 退出 |

## 核心流程

### 状态引擎流程

1. 启动时加载角色：编译期内嵌的内置角色（`resources/characters/link/persona.json`）+ 扫描用户数据目录（`%APPDATA%\com.deskzen.app\characters\*\persona.json`）中已导入的角色。
2. Rust 引擎每 30 秒按本地时间计算当前状态，变化时广播 `state-changed` 事件并弹出对应气泡。
3. 前端收到事件后切换 spritesheet 行（动画）；切换角色时广播 `persona-changed` 让前端重新渲染。

### 角色切换

托盘「更换角色」子菜单 → `switch_persona`：更新引擎当前角色并广播 `persona-changed` / `state-changed`，动画、作息、LLM 人设同步切换；对话窗同步清空上一角色的对话历史并显示「开始新对话」提示，避免旧上下文混入新角色的 system prompt。

### Petdex 导入流程

1. 前端校验链接格式（`https://petdex.dev/pets/{slug}`），调用 `import_petdex_pet`。
2. 后端通过官方接口 `GET /api/install-pet/{slug}` 解析角色资源地址。
3. 优先下载 zip（`pets/…/zip.zip`，社区角色为 `{slug}.zip`）并解压出 `pet.json` + `spritesheet.webp/png`；zip 不可用时回退为直接下载 `petjson.json` + `sprite.webp`。
4. 按 8×9 网格规范生成 DeskZen `persona.json`（`description` 作为 LLM 角色定义，作息/气泡/提示词取默认值）。
5. 写入 `%APPDATA%\com.deskzen.app\characters\petdex-{slug}\`，注册进状态引擎、追加托盘菜单项并立即切换。
6. 精灵图通过 Tauri asset 协议加载（`convertFileSrc`，scope 为 `$APPCONFIG/characters/**`）。

### 删除角色流程

设置界面「已导入角色」→ `delete_persona`：校验仅允许 `petdex-` 前缀的导入角色 → 删除磁盘目录 → 移除引擎注册与托盘菜单项；若删除的是当前角色，自动切换到内置默认角色。

### 对话流程

1. 双击角色 → `open_chat` 创建无边框透明气泡窗口并定位到角色上方，随内容自适应高度、跟随角色移动。
2. 发送消息 → 前端把最近 20 条历史一起发给 `chat_send`。
3. Rust 组装 system prompt（角色定义 + 回复风格 + 当前状态约束），调用配置的大模型。
4. 回复显示在对话窗，角色头顶弹出「收到！」气泡。

### 问屏幕（截图问答）

聊天框右下角「📷」按钮把当前屏幕截图发给大模型，让角色直接回答“屏幕上现在是什么”这类问题：

1. 前端点击「📷」→ `capture_screen` 截屏并转成图片预览，发送时随消息提交。
2. 截图前 Rust 先临时隐藏角色窗与对话窗（避免它们挡住被截内容），再用 `xcap` 抓取角色/对话窗所在显示器，随后恢复窗口显示。
3. 把图片作为 `image_url` 追加到最后一条用户消息（多模态 `content` 数组）并调用当前配置的模型（需支持图像输入）。
4. 回复显示在对话窗；若问题为空，默认询问“请看看我当前屏幕上的内容”。

> 发送图片（截图或粘贴）时，请使用支持图像输入的模型。

### 窗口生命周期

- 对话窗首次创建（async 命令），关闭时仅隐藏、保留会话与历史，双击再次打开即时恢复；设置窗关闭即销毁、按需重建。
- 注意：Windows 上在同步命令里创建窗口会导致窗口无法响应关闭，因此一律使用 `async` 命令。

## 设置界面

设置窗口（560×620）为侧边栏布局，两个板块：

- **角色设置**：点击穿透开关；从 Petdex 导入（链接输入 + 前端校验 + 导入状态）；已导入角色列表（删除）。
- **大模型设置**：接口地址、模型、API Key、保存按钮与保存状态。

## 配置

### LLM 大模型配置（OpenAI 兼容）

支持任意 OpenAI 兼容接口（如 DeepSeek、OpenAI、Ollama 等），配置文件位于 `%APPDATA%\com.deskzen.app\llm.json`。设置界面中 API Key 以打码形式显示（保留前 3 后 4 位），直接保存不会覆盖已存 Key，重新输入完整 Key 才会替换。

```json
{
  "base_url": "https://api.deepseek.com",
  "model": "deepseek-v4-flash",
  "api_key": "sk-..."
}
```

配置优先级：环境变量 `DESKZEN_API_KEY` > `llm.json` > 默认值。Key 不会进入前端代码和安装包，可通过设置界面填写保存。

### 角色

内置角色：`resources/characters/<id>/`（随应用打包），含 `persona.json` 与 `spritesheet`：

- `system_prompt`：角色定义、回复风格、各状态行为约束（注入 LLM）；
- `spritesheet` / `cols` / `rows` / `pixel_art` / `display_w` / `display_h`：精灵图与渲染参数（`display_w`/`display_h` 可省略，缺省时按精灵图实际尺寸 ÷ `cols`/`rows` 自动计算）；
- `states`：状态中文名、spritesheet 行/帧/帧时长、气泡文本池；
- `schedule`：状态机配置 `{ "loop": [ {state,duration 分钟}... ], "time": [ {start,end,state}... ] }`——默认按 `loop` 逐状态循环，`time` 时段内固定为对应状态（支持跨午夜）。

导入角色：`%APPDATA%\com.deskzen.app\characters\petdex-{slug}\`，与内置角色同构（`persona.json` + `spritesheet` + 原始 `pet.json`），启动时自动扫描加载。

## 运行与构建

前置要求：Node.js 18+、Rust stable（MSVC 工具链）、WebView2（Windows 10/11 自带）。

```bash
npm install          # 安装前端依赖
npm run tauri dev    # 开发模式（热更新）
npm run tauri build  # 生产构建，生成安装包（NSIS/MSI）
npm run tauri build -- --no-bundle  # 仅生成 deskzen.exe，不打包安装包
```

> 注意：不要用裸 `cargo build` 代替 `tauri build` —— 前者是 dev 模式，会去连接 vite 开发服务器，页面无法独立运行。

## 测试

- Rust 单元测试：在 `src-tauri/` 下运行 `cargo test --lib`，覆盖 Petdex 链接解析、zip 解压、persona 生成、LLM 网关。
- 端到端冒烟测试（`scripts/`，需先以 CDP 调试端口启动应用）：
  - `petdex_e2e.mjs`：导入命令 + 精灵图加载；
  - `petdex_ui_e2e.mjs`：设置界面导入流程 + 前端校验；
  - `petdex_delete_chat_e2e.mjs`：删除角色 + 聊天窗口 UI；
  - `settings_sidebar_e2e.mjs`：设置侧边栏切换。

## MVP 交付状态

| MVP 目标 | 状态 |
|---|---|
| 透明穿透窗口基础框架 | ✅ 完成（透明无边框小窗 + 整窗穿透开关） |
| 基础角色 + 多状态帧动画 | ✅ 完成（林克，3 种动画状态） |
| 状态机按本地时间切换 | ✅ 完成 |
| 状态切换随机气泡 | ✅ 完成 |
| 对话面板 + LLM + 状态注入 | ✅ 完成（OpenAI 兼容大模型） |
| 多角色运行时切换 | ✅ 完成（托盘「更换角色」子菜单） |
| Petdex 角色导入 / 删除 | ✅ 完成（zip 下载解压 + 前端校验 + 持久化） |
| 设置界面侧边栏 | ✅ 完成（角色设置 / 大模型设置） |

## 后续规划

- LLM 流式输出、对话历史持久化
- 环境贴合：屏幕边缘吸附、站立在活动窗口标题栏
- 便利贴（Notes）机制、免打扰时段
- API Key 改用 Windows 凭据管理器存储
- 导入角色的自定义编辑（作息 / 气泡 / 提示词）

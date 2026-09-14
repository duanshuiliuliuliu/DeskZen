# 仓库约定（给 AI 助手）

## persona.json 的排版（重要）

角色配置按「结构对象逐字段、叶子对象内联」来排，**不要把整段压成一行长字符串**：

- 缩进 2 空格；`:` 与 `,` 后面各留一个空格；行尾不留空格；
- **结构对象 / 结构数组**（`states.<状态>`、`chains[]` 里的链、`segments[]` 里的段、`clips.<动作>`、
  `system_prompt`、`schedule`、`acknowledge`）→ 每个字段或元素一行；
- **叶子对象**（一拍 `{"clip": "walk", "seconds": [18, 30]}`、`{"state": "routine", "duration": 30}`、
  `{"cooldown_min": 45}` 之类）→ 单行内联；
- **短数组**（`"seconds": [30, 50]`、`"tags": ["explore"]`、`"bubbles": [三条文案]`、
  单元素的 `"steps": [{...}]`）→ 单行内联；元素多到一行超宽（约 110 显示宽度，中文按 2 列计）再一项一行。

```jsonc
"chains": [
  {
    "id": "patrol_round",
    "weight": 5,
    "segments": [
      {"steps": [{"clip": "observe", "seconds": [30, 50]}]},   // 单段、单拍：整段内联
      {
        "label": "巡视一圈",
        "steps": [
          {"clip": "walk", "seconds": [18, 30]},
          {"clip": "observe", "chance": 0.7}
        ]
      }
    ],
    "tags": ["explore"]
  },
  // 单段链用 steps 简写：label 写在链上，就是这一段的段名
  {"id": "notice_viewer", "weight": 2, "label": "打招呼", "steps": [{"clip": "greet_wave", "seconds": 30}], "tags": ["social"]}
]
```

改完自检：解析得通（`JSON.parse` 或 `cargo test --lib`，内置角色的解析被单测覆盖）——手改很容易把 key 写坏
（例如 `"frames "` 多了个空格，会让整个 persona 解析失败）。

## 其他既有约定

- 注释、README、提交信息一律中文；提交信息用「类型: 一句话」+ 要点正文 + 验证结论。
- 产品尚未正式发布：**不做向后兼容**，旧字段/旧格式直接删干净，不留兼容分支，怎么最优怎么来。
- 改完至少跑：`cargo test --lib`、`cargo clippy --all-targets`、`npm run build`；
  涉及运行时行为（状态机 / 编排 / 气泡 / 窗口）再用 `scripts/` 里的 CDP e2e 脚本跑一遍。

## 排查问题先看日志

- 日志落在 `%LOCALAPPDATA%\com.deskzen.desktop\logs\deskzen.log`；设置页有独立「日志」板块（级别 + 单文件大小 + 打开目录），
  界面里不展示日志内容；级别默认 `debug`，单份默认 10MB（可选 1/5/10/30M），都即时生效并写入 prefs.json；
  滚动保留最近 7 份；
- 加功能时顺手补日志：**决策点记 info**（状态为什么切、链为什么被选中、气泡说了什么/为什么没说、AI 生成为什么失败），
  **细节记 debug**（每拍推进、偏置倍率、跳过原因），失败记 warn；用 `log::info!/debug!/warn!`，不要 `eprintln!`；
- 不要记录聊天正文或用户内容，只记规模与结果（model / 条数 / 耗时 / 错误）。

---
name: project-docs
description: "Write and maintain VitePress project documentation with the shared book/part/chapter/article/section hierarchy, generated four-level sidebars, and anchor/link verification."
whenToUse: "Use when creating or updating a project documentation site, adding a chapter or article, restructuring navigation, or verifying section anchors and links."
user-invocable: true
disable-model-invocation: false
---

# 项目文档

写文档前先找约定文件（本机示例：`D:\code\markdown\nexus-docs\文档层级命名约定.md`；其他机器按 `<nexus-docs>\文档层级命名约定.md` 查找）。它定义称呼与层级，本技能只给可执行步骤；两者冲突时以约定为准。

## 层级

```text
书 ⊃ 部 ⊃ 章 ⊃ 篇 ⊃ 节
```

| 级 | 站点实体 | 规则 |
| --- | --- | --- |
| 部 | 侧边栏顶层分组 | 手写标签，语义与章内容相符 |
| 章 | `docs/zh/NN_topic/` 目录 + `readme.md` 导读 | 两位数字 + 下划线 + 短名 |
| 篇 | `docs/zh/NN_topic/NN.md` | 一个页面；标题取自 H1 |
| 节 | 页内 `##` / `###` | 最小目录级；段及以下不入目录 |

侧边栏固定四级（部 → 章 → 篇 → 节），全部默认展开。侧边栏文字就是实际标题，不要加「第 X 部 / 第 X 章」前缀。

## 文件布局

```text
docs/
├─ index.md                 首页
├─ zh/
│  ├─ index.md              中文入口
│  └─ 01_guide/
│     ├─ readme.md          章导读（H1 = 章名）
│     └─ 01.md … 06.md      篇（H1 = 篇名）
└─ .vitepress/config.mts    四级侧边栏生成
```

## 侧边栏

- 在 `config.mts` 里扫描 `docs/zh/NN_*` 目录与 `NN.md` 文件，读取 H1、`##`、`###` 生成四级条目。
- 每一级都写 `collapsed: false`；篇条目同时带 `link` 与 `items`，节链接到 `页面#锚点`。
- 锚点必须与 VitePress 完全一致：

```js
const rControl = /[\u0000-\u001f]/g
const rSpecial = /[\s~`!@#$%^&*()\-_+=[\]{}|\\;:"'“”‘’<>,.?/]+/g
const rCombining = /[\u0300-\u036F]/g
const slug = (str) => str.normalize('NFKD')
  .replace(rCombining, '')
  .replace(rControl, '')
  .replace(rSpecial, '-')
  .replace(/-{2,}/g, '-')
  .replace(/^-+|-+$/g, '')
  .replace(/^(\d)/, '_$1')
  .toLowerCase()
```

全角标点会先被 `NFKD` 归一成 ASCII，再替换成 `-`；直接用 `toLowerCase().replace(/\s+/g,'-')` 会生成错误锚点。

## 新增一篇

1. 在章目录里取下一个编号：`07.md`。
2. 写 H1（篇名）与 `##` / `###`（节），不要用编号前缀。
3. 在章 `readme.md` 的篇列表里补一行。
4. 如果需要顶部导航，更新 `themeConfig.nav`。
5. 构建并检查（见下）。

## 新增一章

1. 新建 `docs/zh/NN_topic/`，写 `readme.md`（H1 = 章名）。
2. 写 `01.md`、`02.md`……，每篇一个 H1。
3. 若需要新的部，在 `config.mts` 的 `guide()` 里增加分组；否则自动归入现有部。
4. 更新 `docs/zh/index.md` 的入口表与 `readme.md` 的结构说明。

## 验证

```bash
npm install
npm run docs:build
npm run docs:check
```

`docs:check` 应在 `docs/.vitepress/dist` 上核对：每个 `##`/`###` 的锚点都存在、每个页内相对链接都指向真实文件，输出 `problems=0`。

再抽查构建后的 HTML：

- 侧边栏包含 `VPSidebarItem level-0` 到 `level-3` 四层；
- 没有 `collapsed` 类（表示默认展开）；
- 抽查一个中文标点标题的 `id` 与配置生成的链接一致。

## 内容规则

- 只写验证过的行为；引用源码时给出文件路径与类/方法/行号，不写猜测。
- 命令示例默认只读；涉及数据库用只读连接，不输出密钥。
- 每页 H1 只有一个；`##` 是节，`###` 是节的下级，不再往侧边栏加 `####`。
- 每次结构性修改都追加到 `docs-maintenance.md`：日期、改了什么、验证命令与结果。

## 发布

- `git init` 的默认分支可能是 `master`；推送前 `git branch -m main`。
- `node_modules/`、`docs/.vitepress/dist/`、`docs/.vitepress/cache/` 不入库。
- 远端由用户指定；未经确认不要推送。

## 已知坑

- PowerShell `Set-Content -Encoding UTF8` 会写 BOM。Node 的 `readFileSync(..., 'utf8')` 会保留开头的 `\uFEFF`，`/^#\s+/` 匹配不到 H1，侧边栏就回退成文件名（`03.md`、`04.md`）。读取 Markdown 一律先 `replace(/^\uFEFF/, '')`，批量改文件优先用不会写 BOM 的工具。
- 全角标点会先被 `NFKD` 归一成 ASCII，再按 `rSpecial` 转 `-`；不要用简化的 `toLowerCase().replace(/\s+/g, '-')`，否则中文标点标题的锚点对不上。
- `readme.md` 不是 VitePress 的目录索引（只有 `index.md` 是）；章导读链接写 `/zh/NN_topic/readme`。
- `git init` 默认分支可能是 `master`，推送前 `git branch -m main`。
- 改 `.vitepress/config.mts` 后开发服务器通常会自动重启；侧边栏没变化时先重启 `npm run docs:dev` 再判断。

## 参考实现

- 当前站点（本机示例）：`D:\code\project\project-llm-proxy\java-llm-proxy-docs`
- 约定来源：`<nexus-docs>\文档层级命名约定.md`（本机示例 `D:\code\markdown\nexus-docs\...`）
- 同构站点（本机示例）：`D:\code\markdown\nexus-docs\tio-boot-docs`

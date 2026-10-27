# TUI 终端体系与组件运行时

`pi-tui` 基于 `ratatui` 与 `crossterm` 构建，负责交互模式下的终端展示，设计对应上游 `packages/tui`。

## 组件化设计 (`Component` Trait)

所有 UI 单元均实现统一的 `Component` 抽象：
- `handle_event(&mut self, event: &crossterm::event::Event) -> EventResult`：事件响应。
- `render(&self, area: Rect, buf: &mut Buffer)`：无副作用视图渲染。

## 关键组件实现

### 1. Markdown 渲染引擎 (`components/markdown.rs`)
- 基于 `pulldown-cmark` 驱动的零拷贝 Markdown 解析。
- 支持语义：
  - 标题、粗体、斜体、删除线。
  - 围栏代码块（带边界框高亮）。
  - 列表、引用区块。
  - GFM 表格排版。
  - 行内代码与超链接。
- 根据终端视口宽度动态折行（Word Wrap），保留语义缩进。

### 2. 多行编辑器 (`components/editor.rs`)
- 支持 Emacs / Readline 标准快捷键流派。
- 支持 `\` 续行语法，避免误按 Enter 导致提早提交未完提示词。
- 维护光标位置、词边界跳转、剪切板（Kill Ring）与撤销恢复。

### 3. 模态浮层与模糊匹配 (`components/select_list.rs`, `fuzzy.rs`)
- 提供类似 Sublime / VS Code 的模糊过滤算法。
- 应用场景：`/model` 模型动态选择弹窗。

### 4. 滚动视口 (`components/scroll_view.rs`)
- 虚拟化平滑滚动，适应长篇多轮会话浏览。
- 支持鼠标滚轮事件和 `PgUp` / `PgDn` 翻页。

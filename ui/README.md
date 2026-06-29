# Slint UI Split Layout

原始文件被拆成：

- `common.slint`：公共 `struct`、`global Theme/I18n/ColW`、表格 Cell、按钮、Ribbon、图标、仿真控件等复用组件。
- `AppWindow.slint`：主窗口。
- `SignalSelectWindow.slint`、`ChartWindow.slint`、`TxWindow.slint` 等：每个弹窗/页面一个文件。
- `main.slint`：主窗口预览入口。
- `logo.svg`：占位图标，避免 `@image-url("logo.svg")` 找不到导致预览失败。

## Slint Live 打开方式

主窗口：

```bash
slint-viewer main.slint
```

单独预览某个窗口：

```bash
slint-viewer TxWindow.slint
slint-viewer ChartWindow.slint
slint-viewer ChannelConfigWindow.slint
```

## Rust / C++ 后端引用建议

后端只需要加载你真正的入口组件，例如主程序加载 `main.slint` 或直接加载 `AppWindow.slint`。
其他窗口由后端按需创建，或者在构建脚本中按文件分别 include。

## 注意

拆分后公共小控件从原来的本文件私有组件改成了 `export component`，供每个页面文件 import 使用。


## Rust build entry

Use `app.slint` for `slint_build::compile`, because Rust code references `AppWindow`, dialog windows, row structs and globals directly.

```rust
fn main() {
    slint_build::compile("ui/app.slint").unwrap();
}
```

`main.slint` is only a lightweight preview wrapper that exports `MainWindow`; it is not suitable as the Rust build entry for this project.

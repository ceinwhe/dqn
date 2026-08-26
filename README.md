# dqn

终端音乐下载器


## 构建与运行

要求 Rust 1.88 或更高版本.若版本较旧,请先执行 `rustup update stable`.

```
cargo run
```

或构建发布版本:

```
cargo build --release
```



## 常用操作

| 按键 | 功能 |
| --- | --- |
| 普通文字键 | 直接编辑搜索词 |
| `Enter` | 搜索 |
| `Backspace` / `Delete` | 修改或清空搜索词 |
| `Up` / `Down` | 移动当前行 |
| `Insert` | 选择/取消当前歌曲 |
| `Ctrl+A` | 选择/取消当前页全部歌曲 |
| `Left` / `Right` | 上一页/下一页 |
| `Ctrl+P` 或 `Tab` | 切换 |
| `Ctrl+T` 或 `Ctrl+X` | 切换音质 |
| `Ctrl+D` | 下载已选歌曲;未多选时下载当前行 |
| `Ctrl+L` | 扫码登录 |
| `F8` | 清理完成/失败的下载记录 |
| `F1` 或 `Ctrl+H` | 显示/隐藏帮助 |
| `Ctrl+Q` | 无活动下载时安全退出 |
| `Ctrl+C` | 强制退出 |


## 使用说明
自动生成Downloads文件夹在于同级目录


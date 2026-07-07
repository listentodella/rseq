# rseq-lsp

`rseq-lsp` 是 rseq DSL 的 Language Server。它通过标准 LSP stdio
工作, 可被 VS Code、Zed、Neovim 等编辑器接入。

第一版能力:

- `.rseq` 语法诊断, 复用 `rseq::parse_detailed` 和真实编译器诊断;
- DSL 内置关键词/宏补全: `read!`, `write!`, `irq!`, `report!`,
  `report_format!`, `bus!`, `bus_probe!` 等;
- chip YAML 补全: page、register、field、interrupt event;
- hover 文档: 寄存器地址、access、width、desc、bit field 和 event 来源;
- 支持 `chip!("...")` 自动加载 metadata, 也支持启动参数传入 `--chip`。

## 运行

```bash
cargo run -p rseq-lsp -- --chip qmi8660.yaml
```

`--chip` 可重复提供。相对路径会优先按 LSP workspace root 和当前文档目录解析。
如果 `.rseq` 文件里已经写了 `chip!("qmi8660.yaml");`, 不传 `--chip` 也能获得
该 YAML 里的寄存器/字段/事件补全。

也可以通过 LSP `initializationOptions` 传入:

```json
{
  "chips": ["qmi8660.yaml"]
}
```

## Neovim 示例

```lua
vim.api.nvim_create_autocmd({ "BufRead", "BufNewFile" }, {
  pattern = "*.rseq",
  callback = function()
    vim.lsp.start({
      name = "rseq-lsp",
      cmd = {
        "cargo", "run", "-q", "-p", "rseq-lsp", "--",
        "--chip", "qmi8660.yaml",
      },
      root_dir = vim.fs.root(0, { "Cargo.toml", ".git" }),
    })
  end,
})
```

## 设计边界

LSP 只运行在 host 侧, 不改变 MCU 字节码、帧协议或固件行为。chip YAML 只作为
编辑器提示和编译期解析 metadata, MCU 仍然保持通用总线桥/VM。
